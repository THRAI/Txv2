//! PR-10 phase 4 — fault-script OnAgent branch + `await_agent_reply`
//! pin tests.
//!
//! Pins the wiring that PR-10 phase 4 lands:
//!
//! 1. `fault_script_with_ufd_dispatch` reads the
//!    `VmEntry::ufd_registration` tag at the VMA recipe lookup point
//!    (W-V's catchup recommendation) and, when present, installs a
//!    `DelegateRequest::Ufd(UfdRequest::PageFault)` into the per-ufd
//!    `DelegateRegistry`.
//! 2. The faulting future parks on the `await_agent_reply` helper —
//!    matching the bound `TaskMailbox` against the freshly-minted
//!    `DelegateTokenId`. Spurious mailbox events for other tokens
//!    are re-posted (not consumed).
//! 3. When the agent thread drives `mark_replied(token_id,
//!    DelegateReply::Ufd(...))`, the faulting future resumes and
//!    completes the publish path.
//! 4. When the agent fails to reply and `mark_endpoint_died(ufd_id)`
//!    fires (the ufd was closed), the faulting future surfaces the
//!    abort as `VmFaultError::WouldBlock` (phase 4 stub mapping;
//!    phase 6 may add a dedicated `AgentDied` variant).
//! 5. `NullUfdDispatch` makes the legacy `fault_script(VmFault)`
//!    entrypoint behave exactly as it did pre-phase-4 (graceful
//!    fall-through when no ufd is registered, or when the dispatcher
//!    cannot resolve the id).
//!
//! Phase-4 stub: actual byte-level `src_kernel_addr → dst_uaddr`
//! copy is deferred to phase 5. Tests here pin the **plumbing**
//! (interception + delegation + reply consumption + abort routing),
//! not the I/O.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//!   §3.4 (gap #2 await_agent_reply), §3.6 (fault-path interception),
//!   §6 phase plan P-10.4.
//! - `docs/Txv3/05_DELEGATE_v1.md` §7 step 4 (post WakeHint on
//!   Applied), §8.1 (userfaultfd worked example).

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
use tx_substrate::epoch;
use tx_substrate::step_v3::{
    DelegateReply, DelegateState, TransitionOutcome, UfdReply,
};
use tx_substrate::testing::init_host_for_test_once;
use tx_substrate::wake::TaskMailbox;

use tx_subsystems::userfaultfd::UserfaultFd;
use tx_subsystems::vm::{
    AddressSpace, MapPlacement, NullUfdDispatch, Prot, UfdDispatch, UfdDispatchTarget,
    UfdRegistration, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmFault, VmFaultError,
    VmMapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// -------- Stub PMAP -------------------------------------------------

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
    init_host_for_test_once();
    let _ = zones::register_all();
    // The materialize_pagebacked path needs a zero frame for the
    // private-anon read fallback; mirror the vm-tests setup so the
    // tail of fault_script can complete.
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame: {error:?}"),
    }
    drain_to_quiescence();
    guard
}

fn drain_to_quiescence() {
    let mut quiet = 0u32;
    while quiet < 2 {
        let stats = epoch::drain_with_budget(usize::MAX);
        if stats.reclaimed == 0 {
            quiet += 1;
        } else {
            quiet = 0;
        }
    }
}

// -------- Single-cap UfdDispatch ------------------------------------
//
// Real production callers will route through the owning process's
// fd-table. Phase 4 tests use this tiny single-ufd dispatcher so the
// fault-script OnAgent branch can be exercised in isolation.

struct SingleUfdDispatch<'a> {
    ufd: &'a UserfaultFd,
    mailbox: alloc::sync::Weak<TaskMailbox>,
}

impl<'a> UfdDispatch for SingleUfdDispatch<'a> {
    fn resolve(&self, ufd_id: u64) -> Option<UfdDispatchTarget<'_>> {
        if self.ufd.ufd_id() != ufd_id {
            return None;
        }
        Some(UfdDispatchTarget {
            registry: self.ufd.delegate_registry(),
            mailbox: self.mailbox.clone(),
            // Phase-4 tests don't exercise the read-queue arm.
            fault_pusher: None,
        })
    }
}

// -------- Local async executor ------------------------------------
//
// Single-task `poll`-based executor that runs the fault future to
// completion (or yields back to the test thread on `Pending`). Lets
// the test drive `mark_replied` between polls — modelling the
// agent-side handler running on a separate reactor task. Mirrors the
// pattern used by `crates/tx-shims/tests/v3_userfaultfd_register.rs`.

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

/// Poll the future once and return `Some(out)` on `Ready`; `None` on
/// `Pending`. Used to step the fault future across the agent's
/// `mark_replied`.
fn poll_once<F: Future>(future: Pin<&mut F>) -> Option<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.poll(&mut cx) {
        Poll::Ready(out) => Some(out),
        Poll::Pending => None,
    }
}

// -------- Helper: build an aspace with one ufd-tagged private-anon VMA --

fn fresh_aspace_with_ufd_tagged_vma(
    ufd: &UserfaultFd,
    range_start: u64,
) -> Arc<AddressSpace> {
    let aspace =
        AddressSpace::new_for_platform::<StubPmap>().expect("fresh aspace");
    let range = UserRange::new_aligned(
        UserVirtAddr::new(range_start as usize),
        USER_PAGE_SIZE,
    )
    .expect("aligned range");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::RequireFree,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    aspace
        .try_mmap(request)
        .expect("mmap private anon page");
    aspace
        .tag_ufd_registration(
            range,
            UfdRegistration {
                ufd_id: ufd.ufd_id(),
                mode: 0,
            },
        )
        .expect("tag ufd registration");
    Arc::new(aspace)
}

// =========================================================================
// 1. Reply path: agent's mark_replied wakes the parked fault future.
// =========================================================================

#[test]
fn fault_script_yields_on_agent_and_resumes_on_mark_replied() {
    let _g = setup();
    let ufd_cap = UserfaultFd::new_cap().expect("ufd cap");
    let _ufd_id = ufd_cap.ufd_id();
    let aspace = fresh_aspace_with_ufd_tagged_vma(&ufd_cap, 0x4000_0000);
    let mailbox = Arc::new(TaskMailbox::new());

    let dispatcher = SingleUfdDispatch {
        ufd: &ufd_cap,
        mailbox: Arc::downgrade(&mailbox),
    };
    let fault = VmFault::new(UserVirtAddr::new(0x4000_0000), tx_subsystems::vm::AccessMode::Read);

    let aspace_ref = aspace.clone();
    let future = aspace_ref.fault_script_with_ufd_dispatch(fault, dispatcher);
    let mut fut = Box::pin(future);

    // First poll: should yield Pending — the helper installed a
    // request and parked on the mailbox.
    assert!(
        poll_once(fut.as_mut()).is_none(),
        "fault_script must park on the mailbox awaiting the agent reply",
    );
    // The registry should now have exactly one Pending token bound to
    // the ufd's endpoint marker.
    let registry = ufd_cap.delegate_registry();
    assert_eq!(registry.tracked_count(), 1, "one in-flight token");

    // Walk the slot to recover the token id. The registry has no
    // public "iter" today; we know id starts at 1 per
    // DelegateRegistry::new docs.
    use tx_substrate::step_v3::DelegateTokenId;
    let token_id = DelegateTokenId::new(1);
    assert_eq!(
        registry.state(token_id),
        Some(DelegateState::Pending),
        "token must be Pending after install_request",
    );

    // Drive the agent-side reply.
    let outcome = registry.mark_replied(
        token_id,
        DelegateReply::Ufd(UfdReply::ZeroPage {
            dst_uaddr: 0x4000_0000,
            len: USER_PAGE_SIZE as u64,
        }),
    );
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(token_id), Some(DelegateState::Replied));
    // The mailbox should now hold the AgentReplied wake hint.
    assert_eq!(mailbox.len(), 1, "mark_replied posts exactly one event");

    // Poll the future again: drains the mailbox, takes the reply,
    // runs the materialize-and-publish tail.
    let result = poll_once(fut.as_mut())
        .expect("future must resolve once the agent reply is drained");
    assert!(result.is_ok(), "fault must succeed end-to-end, got {result:?}");
    // The take_reply CAS should have drained the reply slot.
    assert!(
        registry.take_reply(token_id).is_none(),
        "reply payload is consumed exactly once",
    );
}

// =========================================================================
// 2. Spurious mailbox events for unrelated tokens are not consumed.
// =========================================================================

#[test]
fn await_agent_reply_repost_spurious_events_for_other_tokens() {
    use tx_substrate::step_v3::DelegateTokenId;
    use tx_substrate::wake::MailboxEvent;

    let _g = setup();
    let ufd_cap = UserfaultFd::new_cap().expect("ufd cap");
    let aspace = fresh_aspace_with_ufd_tagged_vma(&ufd_cap, 0x5000_0000);
    let mailbox = Arc::new(TaskMailbox::new());

    let dispatcher = SingleUfdDispatch {
        ufd: &ufd_cap,
        mailbox: Arc::downgrade(&mailbox),
    };
    let fault = VmFault::new(UserVirtAddr::new(0x5000_0000), tx_subsystems::vm::AccessMode::Read);

    let aspace_ref = aspace.clone();
    let future = aspace_ref.fault_script_with_ufd_dispatch(fault, dispatcher);
    let mut fut = Box::pin(future);

    // Park.
    assert!(poll_once(fut.as_mut()).is_none(), "must park");
    assert_eq!(mailbox.len(), 0);

    // Inject a spurious AgentReplied for a different (unminted) token.
    let other_token = DelegateTokenId::new(999);
    let _ = mailbox.post(MailboxEvent::AgentReplied { token_id: other_token });
    assert_eq!(mailbox.len(), 1);

    // Polling should drain and re-post the spurious event (not match
    // our active wait), then return Pending.
    assert!(
        poll_once(fut.as_mut()).is_none(),
        "spurious event for another token must not resolve our wait",
    );
    assert_eq!(
        mailbox.len(),
        1,
        "spurious event is re-posted, not silently dropped",
    );
}

// =========================================================================
// 3. mark_endpoint_died aborts the fault with AgentDied.
// =========================================================================

#[test]
fn fault_script_resolves_to_would_block_on_agent_died() {
    use tx_substrate::step_v3::DelegateTokenId;

    let _g = setup();
    let ufd_cap = UserfaultFd::new_cap().expect("ufd cap");
    let ufd_id = ufd_cap.ufd_id();
    let aspace = fresh_aspace_with_ufd_tagged_vma(&ufd_cap, 0x6000_0000);
    let mailbox = Arc::new(TaskMailbox::new());

    let dispatcher = SingleUfdDispatch {
        ufd: &ufd_cap,
        mailbox: Arc::downgrade(&mailbox),
    };
    let fault = VmFault::new(UserVirtAddr::new(0x6000_0000), tx_subsystems::vm::AccessMode::Read);

    let aspace_ref = aspace.clone();
    let future = aspace_ref.fault_script_with_ufd_dispatch(fault, dispatcher);
    let mut fut = Box::pin(future);
    assert!(poll_once(fut.as_mut()).is_none(), "must park");

    let registry = ufd_cap.delegate_registry();
    // Walk the registry to confirm one in-flight token, then fire the
    // endpoint-death routing — the same call path the ufd-close arm
    // will drive in phase 5.
    assert_eq!(registry.tracked_count(), 1);
    let transitioned = registry.mark_endpoint_died(ufd_id);
    assert_eq!(transitioned, 1, "the one in-flight token must abort");
    assert_eq!(
        registry.state(DelegateTokenId::new(1)),
        Some(DelegateState::AgentDied),
    );

    // The mailbox should now hold the Abort event.
    assert_eq!(mailbox.len(), 1);
    // Polling resolves the future with WouldBlock (phase 4 mapping
    // for AgentDied / Canceled / TimedOut).
    let result =
        poll_once(fut.as_mut()).expect("Abort must resolve the future");
    assert!(
        matches!(result, Err(VmFaultError::WouldBlock)),
        "AgentDied must surface as WouldBlock in phase 4, got {result:?}",
    );
}

// =========================================================================
// 4. NullUfdDispatch falls through — legacy fault_script path unchanged.
// =========================================================================

#[test]
fn null_ufd_dispatch_falls_through_to_normal_materialize() {
    let _g = setup();
    let ufd_cap = UserfaultFd::new_cap().expect("ufd cap");
    let aspace = fresh_aspace_with_ufd_tagged_vma(&ufd_cap, 0x7000_0000);
    let fault = VmFault::new(UserVirtAddr::new(0x7000_0000), tx_subsystems::vm::AccessMode::Read);

    // Even though the VMA is tagged, NullUfdDispatch::resolve returns
    // None, so fault_script_with_ufd_dispatch must fall through to
    // the normal materialize-publish tail.
    let aspace_ref = aspace.clone();
    let future = aspace_ref.fault_script_with_ufd_dispatch(fault, NullUfdDispatch);
    let mut fut = Box::pin(future);
    let result = poll_once(fut.as_mut())
        .expect("null-dispatch must run to completion in one poll");
    assert!(result.is_ok(), "null-dispatch fall-through must succeed");
}
