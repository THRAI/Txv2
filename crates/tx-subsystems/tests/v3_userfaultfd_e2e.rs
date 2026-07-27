//! PR-10 phase 6 — end-to-end userfaultfd canary.
//!
//! This is **the** canary for the OnAgent runtime. It exercises the
//! complete Linux-style userfaultfd loop end-to-end:
//!
//!   1. Setup: create a process, register a page-backed memory region,
//!      open a userfaultfd, perform `UFFDIO_API`, register the region
//!      via `UFFDIO_REGISTER`. The ufd is installed into the process's
//!      fd table as `OpenFileBacking::Ufd { ufd }` so the production
//!      `ProcessUfdDispatch` can resolve it.
//!   2. Spawn a handler "thread" (a separate future) that blocks on
//!      `ufd.read()` waiting for a fault message.
//!   3. The faulting "thread" touches a page in the registered region
//!      → drives `AddressSpace::fault_script_for_process_with_post` with the
//!      production `ProcessUfdDispatch` → `dispatch_ufd_fault`
//!      installs an OnAgent request, pushes the fault message onto
//!      the per-ufd queue, parks the faulting future on
//!      `await_agent_reply`.
//!   4. Handler thread wakes: drains the pending queue, decodes the
//!      `fault_addr`, drives `UFFDIO_COPY` against the ufd's
//!      `DelegateRegistry` (the same `delegate reply transition` path the
//!      production shim ioctl arms use).
//!   5. Faulting thread resumes: the `await_agent_reply` helper sees
//!      the `AgentReplied` event, the fault future completes the
//!      materialize-and-publish tail under fresh epoch guard.
//!   6. Verify per-ufd `DelegateRegistry` shows the token transitioned
//!      `Pending → Replied`; the fault future's result is `Ok(_)`.
//!
//! If this canary works end-to-end, PR-7's substrate runtime +
//! PR-7B's mailbox layering + PR-9's `SubjectContext` threading +
//! PR-10 phases 0-5 are all validated together.
//!
//! ## What this canary is **not**
//!
//! - Not a multi-thread real-OS-thread test. The "faulting thread"
//!   and "handler thread" are both async futures driven by a single
//!   bounded driver loop (mirrors the pattern in
//!   `v3_userfaultfd_fault_path.rs` and `v3_userfaultfd_ioctl_reply.rs`).
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//!   §11 (success criteria), §6 phase plan P-10.6.
//! - `docs/Txv3/05_DELEGATE_v1.md` §8.1 (userfaultfd worked example).
//! - `docs/Txv3/07_BLAST_RADIUS.md` §5.2 PR-10 row.

extern crate alloc;

use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_substrate::step::{InterestMask, WaitSourceId};
use tx_substrate::wake::{
    MailboxEvent, MailboxSchedulerHint, WaitGeneration, WaitRegistrationGuard, WaitSource,
};
use tx_subsystems::vm::adapter::step_engine::{
    page_allocator, Cap, DelegateReply, DelegateState, DelegateTokenId, TaskMailbox,
    TransitionOutcome, UfdReply,
};

use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::userfaultfd::{reset_ufd_id_counter_for_test, ProcessUfdDispatch, UserfaultFd};
use tx_subsystems::vfs::{OpenFile, OpenFileFlags};
use tx_subsystems::vm::{
    AccessMode, AddressSpace, MapPlacement, Prot, UfdRegistration, UserRange, UserVirtAddr,
    VmBacking, VmEntryFlags, VmFault, VmMapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::zones;

// -------- Test serialization + stub PMAP ---------------------------

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static UFD_REF_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

struct StubPmap;

static NEXT_ROOT_ID: AtomicUsize = AtomicUsize::new(1);

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let id = NEXT_ROOT_ID.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * 4096)),
            Asid(id as u16),
        ))
    }
    fn destroy_pmap_root(_root: PmapRoot) {}
    fn reserve_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }
    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}
    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
    }
    fn unmap_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Ok(Some(PmapUnmapResult::new(virt, PhysAddr(virt.0), kind)))
    }
    fn protect_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        Ok(Some(PmapInvalidation::new(virt, kind.size())))
    }
    fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {}
    fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {}
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    // The materialize_pagebacked tail of fault_script needs a zero
    // frame for the private-anon read fallback; mirror the vm-tests
    // setup so the resume tail can complete.
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame: {error:?}"),
    }
    tx_test_support::drain_to_quiescence();
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    reset_ufd_id_counter_for_test();
    guard
}

// -------- Bounded reactor driver -----------------------------------

fn noop_raw_waker() -> RawWaker {
    fn clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop_(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_);
    RawWaker::new(core::ptr::null(), &VTABLE)
}

fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(noop_raw_waker()) }
}

/// Poll a future once. Returns `Some(out)` on `Ready`, `None` on
/// `Pending`. Mirrors the pattern in
/// `v3_userfaultfd_fault_path.rs::poll_once`.
fn poll_once<F: Future>(future: Pin<&mut F>) -> Option<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.poll(&mut cx) {
        Poll::Ready(out) => Some(out),
        Poll::Pending => None,
    }
}

/// Maximum reactor ticks before declaring the canary stuck. Per the
/// phase-6 prompt constraint #4: "cap the driver loop at N polls
/// (e.g., 1000); fail the test if it doesn't complete within bounds."
const MAX_DRIVER_TICKS: usize = 1000;

fn counting_ufd_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    hint: MailboxSchedulerHint,
) -> bool {
    assert_eq!(hint, MailboxSchedulerHint::Normal);
    UFD_REF_POST_COUNT.fetch_add(1, Ordering::AcqRel);
    mailbox.post_with_scheduler_hint(event, hint)
}

fn direct_ufd_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    hint: MailboxSchedulerHint,
) -> bool {
    mailbox.post_with_scheduler_hint(event, hint)
}

fn direct_delegate_mailbox_post(mailbox: std::sync::Weak<TaskMailbox>, event: MailboxEvent) {
    if let Some(mailbox) = mailbox.upgrade() {
        let _ = mailbox.post(event);
    }
}

fn register_waiter<'a>(
    source: &'a Arc<WaitSource>,
    mailbox: &Arc<TaskMailbox>,
    interests: u64,
) -> (WaitRegistrationGuard<'a>, WaitGeneration) {
    let generation = mailbox.next_generation();
    let prep = source.prepare(
        Arc::downgrade(mailbox),
        generation,
        InterestMask::new(interests),
    );
    let guard = prep.install_if(|| true).expect("registration installed");
    (guard, generation)
}

fn assert_source_fired(
    mailbox: &TaskMailbox,
    source: WaitSourceId,
    generation: WaitGeneration,
    expected_overlap: u64,
) {
    let event = mailbox.poll().expect("mailbox should receive SourceFired");
    match event {
        MailboxEvent::SourceFired {
            generation: actual_generation,
            source: actual_source,
            interests,
        } => {
            assert_eq!(actual_generation, generation);
            assert_eq!(actual_source, source);
            assert_ne!(interests.raw() & expected_overlap, 0);
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }
}

// -------- Canary harness -------------------------------------------

/// Build a fresh init process with a ufd installed at fd 3 and the
/// VMA at `vma_base` registered against that ufd.
///
/// Returns the process cap, the ufd-fd value, the ufd cap clone (for
/// direct registry inspection from the test), and the `Arc<TaskMailbox>`
/// the faulting thread will be parked against.
fn setup_proc_with_registered_ufd(
    vma_base: u64,
) -> (
    Cap<ProcessIdentity>,
    u32,
    Cap<UserfaultFd>,
    Arc<TaskMailbox>,
) {
    // Build the process.
    let aspace_cap = AddressSpace::new_cap_for_platform::<StubPmap>().expect("aspace cap");
    let proc_cap = bootstrap_init_process(aspace_cap).expect("bootstrap init");
    let aspace = proc_cap.aspace_cap().expect("aspace");

    // Map a single private-anon page at vma_base.
    let range = UserRange::new_aligned(UserVirtAddr::new(vma_base as usize), USER_PAGE_SIZE)
        .expect("aligned");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::RequireFree,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    aspace.try_mmap(request).expect("mmap private anon");

    // Build the ufd cap and install it as fd 3.
    let ufd_cap = UserfaultFd::new_cap().expect("ufd cap");
    let ufd_id = ufd_cap.ufd_id();
    let open_file = OpenFile::new_userfaultfd_cap(ufd_cap.clone(), OpenFileFlags::default())
        .expect("ufd OpenFile");
    let _prev = proc_cap.set_fd(3, Some(open_file));

    // Tag the VMA with the ufd registration.
    aspace
        .tag_ufd_registration(range, UfdRegistration { ufd_id, mode: 0 })
        .expect("tag ufd registration");

    // Faulting thread's mailbox.
    let mailbox = Arc::new(TaskMailbox::new());

    (proc_cap, 3, ufd_cap, mailbox)
}

// =========================================================================
// Canary test — full OnAgent loop end-to-end.
// =========================================================================

/// PR-10 phase 6 canary.
///
/// Six-step scenario per D7 §11 success criteria:
///
/// 1. **Setup.** `setup_proc_with_registered_ufd` builds the process,
///    maps a private-anon VMA, opens a ufd, installs it as fd 3, and
///    tags the VMA with `UfdRegistration { ufd_id, mode: 0 }`.
/// 2. **Spawn handler.** The handler is modelled as a checkpoint in the
///    driver loop — when the fault future has parked on the mailbox,
///    we (the test) drain the per-ufd `pending_faults` queue, decode
///    the fault address, and drive `delegate reply transition` against the per-ufd
///    `DelegateRegistry`. This is exactly what the production handler
///    thread's `UFFDIO_COPY` arm does (see the
///    `v3_userfaultfd_ioctl_reply` tests' `dispatch_ioctl(UFFDIO_COPY)`
///    path, which drives `delegate reply transition` against the same registry).
/// 3. **Fault.** We drive `aspace.fault_script_for_process_with_post(fault,
///    &proc, mailbox.weak(), direct_post)` — the **production** entrypoint that
///    builds a `ProcessUfdDispatch` and walks the fd table to resolve
///    the ufd. This exercises the same code path `thread_future`
///    will call once the phase-6 production loop wires it.
/// 4. **Handler wakes.** Inside the driver loop, on the first
///    `Pending` poll of the fault future, the test inspects the ufd's
///    `pending_faults` queue (the production handler's `read(uffd_fd,
///    ...)` arm drains this queue via `step_ufd_read`), pulls the
///    front message, and drives the delegate reply transition for `token_id`
///    DelegateReply::Ufd(UfdReply::Copy { ... }))` — exactly what
///    `step_uffdio_copy` does at the shim layer.
/// 5. **Resume.** The next driver tick re-polls the fault future,
///    `await_agent_reply` drains the `AgentReplied` event from the
///    mailbox, the OnAgent branch returns through `dispatch_ufd_fault`,
///    and the fault-script tail runs the canonical
///    `materialize_pagebacked` + `publish_page_with_replacement`
///    chain. The fault future resolves with `Ok(_)`.
/// 6. **Verify token state.** After the resume, the per-ufd registry
///    shows the token in `Replied` state — pinning the
///    `Pending → ReplyInstalling → Replied` transition end-to-end.
#[test]
fn pr_10_phase_6_oneagent_canary_full_loop() {
    let _g = setup();

    // ----- Step 1: Setup ---------------------------------------------
    let vma_base = 0x4000_0000u64;
    let (proc_cap, _ufd_fd, ufd_cap, mailbox) = setup_proc_with_registered_ufd(vma_base);

    // The first install_request mints DelegateTokenId(1) per the
    // registry's monotonic counter starting at 1. The canary asserts
    // against this stable id.
    let expected_token_id = DelegateTokenId::new(1);

    // Cache the per-ufd registry handle for end-of-test assertions
    // (single owning Cap clone — drops after the test body).
    let registry = ufd_cap.delegate_registry();

    // ----- Step 2 (preamble): handler "thread" closure ----------------
    //
    // The "handler thread" in the canary is the test-driven
    // delegate reply transition step. We don't spawn a real Rust thread — the
    // production handler is a userspace agent that runs on its own
    // reactor task and drains `read(uffd_fd, ...)`. Here we just
    // drive the substrate-side equivalent (`pop_fault_msg` +
    // `delegate reply transition`) from the driver loop between fault-future polls.
    // This is the same shape `v3_userfaultfd_ioctl_reply.rs` uses to
    // exercise the reply ioctls.
    let handler_src_pattern: u8 = 0xAB;
    // Synthetic kernel-addressable source buffer the handler "copies
    // from." The substrate's reply payload carries the kernel VA of
    // this buffer; phase 6 wires the actual byte-move from `src` to
    // `dst` during the resume tail (see
    // `crates/tx-subsystems/src/vm/execution.rs::materialize_ufd_copy`).
    // After the canary finishes we walk the published page's PPN and
    // assert every byte equals `handler_src_pattern` — the load-bearing
    // end-to-end check that the agent's bytes landed in the user's page.
    let mut handler_src_buf = alloc::vec![handler_src_pattern; USER_PAGE_SIZE];
    let src_kernel_addr = handler_src_buf.as_mut_ptr() as u64;

    // ----- Step 3: Drive the faulting thread -------------------------
    let aspace = proc_cap.aspace_cap().expect("aspace");
    let fault = VmFault::new(UserVirtAddr::new(vma_base as usize), AccessMode::Read);
    let mailbox_weak = Arc::downgrade(&mailbox);

    // The production entrypoint that builds `ProcessUfdDispatch` from
    // the process cap and walks the fd-table.
    let aspace_ref = aspace.clone();
    let proc_ref = proc_cap.clone();
    let fault_fut = async move {
        aspace_ref
            .fault_script_for_process_with_post(
                fault,
                &proc_ref,
                mailbox_weak,
                direct_ufd_ref_post_with_hint,
            )
            .await
    };
    let mut fault_fut = Box::pin(fault_fut);

    // ----- Driver loop ------------------------------------------------
    //
    // Bounded reactor-ticks per phase-6 prompt constraint #4 (max
    // 1000; canary must finish in << 10 ticks).
    let mut handler_fired = false;
    let mut fault_result = None;
    for tick in 0..MAX_DRIVER_TICKS {
        // Poll the faulting future.
        match poll_once(fault_fut.as_mut()) {
            Some(out) => {
                fault_result = Some(out);
                break;
            }
            None => {
                // Faulting future is parked. If the handler hasn't
                // yet fired, drain the pending queue + delegate reply transition.
                if !handler_fired {
                    // ----- Step 4: handler thread wakes -------------------
                    if let Some(msg) = ufd_cap.pop_fault_msg() {
                        assert_eq!(
                            msg.fault_addr, vma_base,
                            "fault message carries the faulting addr",
                        );
                        assert_eq!(
                            msg.token_id, expected_token_id,
                            "first installed token is DelegateTokenId(1)",
                        );
                        // The token must be Pending at this point.
                        assert_eq!(
                            registry.state(msg.token_id),
                            Some(DelegateState::Pending),
                            "token is Pending after install_request",
                        );

                        // Drive UFFDIO_COPY semantically — the same
                        // `delegate reply transition` path `step_uffdio_copy` drives.
                        let reply_outcome = registry.mark_replied_with_post(
                            msg.token_id,
                            DelegateReply::Ufd(UfdReply::Copy {
                                src_kernel_addr,
                                dst_uaddr: msg.fault_addr,
                                len: USER_PAGE_SIZE as u64,
                            }),
                            direct_delegate_mailbox_post,
                        );
                        assert_eq!(
                            reply_outcome,
                            TransitionOutcome::Applied,
                            "delegate reply transition transitions Pending → Replied",
                        );
                        handler_fired = true;
                    }
                }
            }
        }
        if tick > 100 && !handler_fired {
            panic!("driver loop ran 100 ticks without the fault parking on the mailbox");
        }
    }

    // ----- Step 5 + 6: resume + verify --------------------------------
    let result = fault_result.expect("fault future must resolve within MAX_DRIVER_TICKS");
    assert!(
        result.is_ok(),
        "fault_script_for_process_with_post must succeed end-to-end, got {result:?}",
    );

    // Token must be in Replied state (the registry's
    // `Pending → ReplyInstalling → Replied` end-state).
    assert_eq!(
        registry.state(expected_token_id),
        Some(DelegateState::Replied),
        "DelegateRegistry pins the token as Replied after delegate reply transition",
    );

    // The reply payload was consumed exactly once on the resume path
    // (the fault-script's await_agent_reply calls `take_reply`).
    assert!(
        registry.take_reply(expected_token_id).is_none(),
        "reply payload is consumed exactly once on resume",
    );

    // Handler must have fired (test would have stalled otherwise — but
    // pin the invariant explicitly).
    assert!(handler_fired, "handler-thread step must have run");

    // ----- Step 7 (phase 6 byte-move): verify the page contents ------
    //
    // The agent supplied a 0xAB-filled `src` buffer via UFFDIO_COPY's
    // `src_kernel_addr`. After the resume tail's `materialize_ufd_copy`
    // landed the bytes into the freshly-installed private page and
    // the pmap published it, the user-visible page contents must
    // match the handler's pattern.
    //
    // Look up the published PPN through the address-space's pmap
    // `walk_range` snapshot, then read the frame's bytes via the
    // page-allocator test backend's direct-map read helper.
    let walked = aspace.pmap().walk_range(
        UserRange::new_aligned(UserVirtAddr::new(vma_base as usize), USER_PAGE_SIZE)
            .expect("aligned range"),
    );
    assert_eq!(
        walked.len(),
        1,
        "exactly one published page covers the faulting range, got {walked:?}",
    );
    let (_user_page, snapshot) = walked[0];
    let mut readback = alloc::vec![0u8; USER_PAGE_SIZE];
    page_allocator::testing::read_frame_bytes_for_test(snapshot.ppn, 0, &mut readback);
    assert!(
        readback.iter().all(|byte| *byte == handler_src_pattern),
        "every byte of the published page must equal the handler's \
         src pattern (0x{:02X}); first mismatch at index {:?}",
        handler_src_pattern,
        readback.iter().position(|b| *b != handler_src_pattern),
    );

    // Drop the buffer last — its `src_kernel_addr` stayed live for
    // the duration of the COPY reply and the substrate's
    // `materialize_ufd_copy` dereferenced it during the resume tail.
    drop(handler_src_buf);
}

// =========================================================================
// Companion canary — `ProcessUfdDispatch::resolve` walks the fd table.
// =========================================================================
//
// Pin the small structural invariant that the new dispatcher actually
// walks the calling process's fd table to find the ufd cap. This is
// the production replacement for `NullUfdDispatch` and the missing
// piece phase 6 lands.

#[test]
fn process_ufd_dispatch_resolves_via_process_fd_table() {
    use tx_subsystems::vm::UfdDispatch;

    let _g = setup();
    let vma_base = 0x5000_0000u64;
    let (proc_cap, _fd, ufd_cap, mailbox) = setup_proc_with_registered_ufd(vma_base);
    let mailbox_weak = Arc::downgrade(&mailbox);

    let dispatch =
        ProcessUfdDispatch::new_with_post(&proc_cap, mailbox_weak, direct_ufd_ref_post_with_hint);

    // Hit: the ufd is installed at fd 3 with matching ufd_id.
    let target = dispatch.resolve(ufd_cap.ufd_id());
    assert!(
        target.is_some(),
        "dispatcher resolves a live ufd via fd table"
    );

    // Miss: a bogus ufd_id falls through to None.
    let dispatch2 = ProcessUfdDispatch::new_with_post(
        &proc_cap,
        Arc::downgrade(&mailbox),
        direct_ufd_ref_post_with_hint,
    );
    let miss = dispatch2.resolve(0xDEAD_BEEF);
    assert!(miss.is_none(), "dispatcher returns None for unknown ufd_id");
}

#[test]
fn process_fault_push_uses_injected_mailbox_ref_post_for_ufd_readable_wake() {
    let _g = setup();
    UFD_REF_POST_COUNT.store(0, Ordering::Release);

    let vma_base = 0x6000_0000u64;
    let (proc_cap, _fd, ufd_cap, fault_mailbox) = setup_proc_with_registered_ufd(vma_base);
    let handler_mailbox = Arc::new(TaskMailbox::new());
    let (_registration, generation) = register_waiter(ufd_cap.wait_source(), &handler_mailbox, 1);

    let aspace = proc_cap.aspace_cap().expect("aspace");
    let fault = VmFault::new(UserVirtAddr::new(vma_base as usize), AccessMode::Read);
    let fault_mailbox_weak = Arc::downgrade(&fault_mailbox);
    let aspace_ref = aspace.clone();
    let proc_ref = proc_cap.clone();
    let fault_fut = async move {
        aspace_ref
            .fault_script_for_process_with_post(
                fault,
                &proc_ref,
                fault_mailbox_weak,
                counting_ufd_ref_post_with_hint,
            )
            .await
    };
    let mut fault_fut = Box::pin(fault_fut);

    assert!(
        poll_once(fault_fut.as_mut()).is_none(),
        "fault future should park after enqueueing the userfaultfd message",
    );
    assert_eq!(
        UFD_REF_POST_COUNT.load(Ordering::Acquire),
        1,
        "userfaultfd pending-fault publication should use injected mailbox-ref post"
    );
    assert_eq!(ufd_cap.pending_fault_count(), 1);
    assert_source_fired(
        &handler_mailbox,
        WaitSourceId::new(ufd_cap.wait_source_id()),
        generation,
        1,
    );
}
