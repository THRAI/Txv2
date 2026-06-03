//! Tests for the production thread future + per-hart slot adapter.
//!
//! Strategy: drive `prepare_userspace_entry_payload` and
//! `linux_syscall::dispatch` separately rather than reaching the
//! `enter_userspace_with_context` divergent call site (which would
//! either panic via the host TestPlatform's default TrapIf impl or
//! require a divergent test-only override). The two pieces under test
//! here are the per-hart slot adapter (`PerHartSlotted`) and the slot
//! semantics around `start_request` / `complete_interesting_trap` that
//! the future relies on.

use core::future::Future;
use core::mem::size_of;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use std::sync::Mutex;

use crate::adapter::boot_runtime::userspace::{
    PageFaultAccess, PageFaultInfo, SyscallRequest, UserAddr, UserspaceTrapInfo,
};
use crate::adapter::step_engine::PayloadCap;
use tx_hal::{
    AllocError, Arch, Asid, BootHandoff, BootInfo, BootPlatformIf, BootProtocol, ConsoleIf, InitIf,
    ObserverIf, PhysAddr, PlatformConfig, PlatformInfo, PmapError, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PtNode,
};
use tx_shims::linux_syscall::{
    dispatch, dispatch_cap_only_immediate, SyscallCtx, SyscallResult, FUTEX_PRIVATE_FLAG,
    FUTEX_WAKE, FUTEX_WAKE_BITSET, NR_CLONE, NR_EXIT_GROUP, NR_FUTEX, NR_GETPPID, NR_PSELECT6,
    NR_WRITE,
};
use tx_subsystems::process::ExitStatus;
use tx_subsystems::reactor_submit::SubmitChildThreadStatus;
use tx_subsystems::signal::{SigDisposition, Signum};
use tx_subsystems::thread_runtime::{
    clear_current_thread_payload, current_thread_payload, drain_pending_syscall_return,
    ThreadPayload,
};
use tx_subsystems::vm::{
    AccessMode, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmFault,
    VmFaultError, VmMapRequest,
};

use crate::thread_future::{
    pf_access_to_vm_access, restore_sigreturn_frame, run_thread, siginfo_to_user_abi,
    syscall_return_may_publish_wake_handoff, syscall_return_needs_handoff, PerHartSlotted,
};
use crate::trap::direct_trap_syscall_needs_wake_handoff;

const TEST_PAGE_SIZE: usize = 4096;

/// Serialise against every other tx-kernel host test that touches
/// global INIT_PROCESS, the per-hart slot table, and the epoch
/// domain. The shared lock lives in `crate::test_serialise`.
use crate::test_serialise::KERNEL_TEST_LOCK as THREAD_FUTURE_TEST_LOCK;

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-kernel-thread-future-test";
}

impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for TestPlatform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

static EMPTY_BOOT_INFO: BootInfo = BootInfo::empty();
static TEST_PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: TestPlatform::BOARD,
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

impl tx_hal::BootInfoIf for TestPlatform {
    fn boot_info() -> &'static BootInfo {
        &EMPTY_BOOT_INFO
    }
}

impl tx_hal::PlatformInfoIf for TestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &TEST_PLATFORM_INFO
    }
}

impl tx_hal::AuxvIf for TestPlatform {}

impl ConsoleIf for TestPlatform {
    fn write_bytes(_bytes: &[u8]) {}
}

static USERSPACE_A0_LOG: Mutex<std::vec::Vec<usize>> = Mutex::new(std::vec::Vec::new());

impl tx_hal::TrapIf for TestPlatform {
    fn enter_userspace_with_context(ctx: &tx_hal::UserTrapContext, _root: &tx_hal::PmapRoot) {
        USERSPACE_A0_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ctx.regs[10]);
    }
}
impl tx_hal::SignalFrameIf for TestPlatform {
    fn signal_frame_size() -> usize {
        size_of::<tx_hal::SavedSignalFrame>()
    }

    fn decode_signal_frame_bytes(
        user_sp: tx_hal::UserPtr<u8>,
        bytes: &[u8],
    ) -> Result<tx_hal::SavedSignalFrame, tx_hal::FaultInfo> {
        if bytes.len() != size_of::<tx_hal::SavedSignalFrame>() {
            return Err(tx_hal::FaultInfo {
                address: tx_hal::VirtAddr(user_sp.addr()),
                write: false,
                instruction: false,
                from_user: true,
            });
        }
        let mut frame = core::mem::MaybeUninit::<tx_hal::SavedSignalFrame>::uninit();
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                frame.as_mut_ptr().cast::<u8>(),
                size_of::<tx_hal::SavedSignalFrame>(),
            );
            Ok(frame.assume_init())
        }
    }
}
impl tx_hal::IrqIf for TestPlatform {}

impl tx_hal::TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl tx_hal::PercpuIf for TestPlatform {}
impl tx_hal::CacheIf for TestPlatform {}
impl tx_hal::DmaIf for TestPlatform {}
impl tx_hal::SmpIf for TestPlatform {}
impl tx_hal::EntropyIf for TestPlatform {}
impl ObserverIf for TestPlatform {}

impl tx_hal::PowerIf for TestPlatform {
    fn system_off() -> ! {
        #[allow(clippy::empty_loop)]
        loop {}
    }
}

static TEST_PMAP_NEXT_ROOT: AtomicUsize = AtomicUsize::new(1);

impl tx_hal::PmapIf for TestPlatform {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let root_id = TEST_PMAP_NEXT_ROOT.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(root_id * TEST_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

    fn reserve_mapping(
        _root: &PmapRoot,
        virt: tx_hal::VirtAddr,
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

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        Err(AllocError::Exhausted)
    }
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = THREAD_FUTURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = tx_subsystems::zones::register_all();
    tx_subsystems::cross_crate_test_support::reset_init_process();
    tx_subsystems::cross_crate_test_support::reset_pid_counter();
    tx_subsystems::cross_crate_test_support::reset_tid_counter();
    // Ensure the per-hart slot is empty across tests.
    let _ = clear_current_thread_payload(0);
    USERSPACE_A0_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    guard
}

fn bootstrap_payload() -> PayloadCap<ThreadPayload> {
    let aspace = tx_subsystems::vm::AddressSpace::new_cap_for_platform::<TestPlatform>()
        .expect("test aspace");
    let init = tx_subsystems::process::bootstrap_init_process(aspace).expect("bootstrap init");
    let leader = init.nth_thread(0).expect("leader thread post-bootstrap");
    leader.payload_cap_for_test().expect("leader payload alive")
}

#[test]
fn clone_return_needs_child_publish_handoff_after_successful_publish() {
    let clone = SyscallRequest::new(NR_CLONE, [0; 6]);
    let futex_wake = SyscallRequest::new(
        NR_FUTEX,
        [0x1000, (FUTEX_WAKE | FUTEX_PRIVATE_FLAG) as u64, 1, 0, 0, 0],
    );
    let futex_wake_bitset = SyscallRequest::new(
        NR_FUTEX,
        [
            0x1000,
            (FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG) as u64,
            1,
            0,
            0,
            u32::MAX as u64,
        ],
    );
    let futex_wait = SyscallRequest::new(NR_FUTEX, [0x1000, 0, 1, 0, 0, 0]);
    let pselect = SyscallRequest::new(NR_PSELECT6, [0; 6]);
    let write = SyscallRequest::new(NR_WRITE, [0; 6]);

    assert!(syscall_return_needs_handoff(
        &clone,
        &SyscallResult::Return(123)
    ));
    assert!(syscall_return_needs_handoff(
        &clone,
        &SyscallResult::CloneReturn {
            value: 123,
            child_submit: SubmitChildThreadStatus::QueuedFallback,
        }
    ));
    assert!(syscall_return_needs_handoff(
        &clone,
        &SyscallResult::CloneReturn {
            value: 123,
            child_submit: SubmitChildThreadStatus::Published,
        }
    ));
    assert!(!syscall_return_needs_handoff(
        &clone,
        &SyscallResult::Error(22)
    ));
    assert!(!syscall_return_needs_handoff(
        &clone,
        &SyscallResult::NoReturn
    ));
    assert!(!syscall_return_needs_handoff(
        &write,
        &SyscallResult::Return(1)
    ));
    assert!(!syscall_return_needs_handoff(
        &futex_wake,
        &SyscallResult::Return(1)
    ));
    assert!(!syscall_return_needs_handoff(
        &futex_wake_bitset,
        &SyscallResult::Return(1)
    ));
    assert!(!syscall_return_needs_handoff(
        &futex_wake,
        &SyscallResult::Return(0)
    ));
    assert!(!syscall_return_needs_handoff(
        &futex_wait,
        &SyscallResult::Return(0)
    ));
    assert!(!syscall_return_needs_handoff(
        &pselect,
        &SyscallResult::Return(0)
    ));
    assert!(!syscall_return_needs_handoff(
        &pselect,
        &SyscallResult::Return(1)
    ));
}

#[test]
fn positive_futex_wake_return_may_publish_wake_handoff() {
    let futex_wake = SyscallRequest::new(
        NR_FUTEX,
        [0x1000, (FUTEX_WAKE | FUTEX_PRIVATE_FLAG) as u64, 1, 0, 0, 0],
    );
    let futex_wake_bitset = SyscallRequest::new(
        NR_FUTEX,
        [
            0x1000,
            (FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG) as u64,
            1,
            0,
            0,
            u32::MAX as u64,
        ],
    );
    let futex_wait = SyscallRequest::new(NR_FUTEX, [0x1000, 0, 1, 0, 0, 0]);
    let clone = SyscallRequest::new(NR_CLONE, [0; 6]);

    assert!(syscall_return_may_publish_wake_handoff(
        &futex_wake,
        &SyscallResult::Return(1)
    ));
    assert!(syscall_return_may_publish_wake_handoff(
        &futex_wake_bitset,
        &SyscallResult::Return(1)
    ));
    assert!(!syscall_return_may_publish_wake_handoff(
        &futex_wake,
        &SyscallResult::Return(0)
    ));
    assert!(!syscall_return_may_publish_wake_handoff(
        &futex_wake,
        &SyscallResult::Error(11)
    ));
    assert!(!syscall_return_may_publish_wake_handoff(
        &futex_wait,
        &SyscallResult::Return(0)
    ));
    assert!(!syscall_return_may_publish_wake_handoff(
        &clone,
        &SyscallResult::Return(123)
    ));
}

#[test]
fn positive_direct_futex_wake_needs_wake_handoff_boundary() {
    let futex_wake = SyscallRequest::new(
        NR_FUTEX,
        [0x1000, (FUTEX_WAKE | FUTEX_PRIVATE_FLAG) as u64, 1, 0, 0, 0],
    );
    let futex_wake_bitset = SyscallRequest::new(
        NR_FUTEX,
        [
            0x1000,
            (FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG) as u64,
            1,
            0,
            0,
            u32::MAX as u64,
        ],
    );
    let futex_wait = SyscallRequest::new(NR_FUTEX, [0x1000, 0, 1, 0, 0, 0]);
    let clone = SyscallRequest::new(NR_CLONE, [0; 6]);

    assert!(direct_trap_syscall_needs_wake_handoff(
        &futex_wake,
        &SyscallResult::Return(1)
    ));
    assert!(direct_trap_syscall_needs_wake_handoff(
        &futex_wake_bitset,
        &SyscallResult::Return(1)
    ));
    assert!(!direct_trap_syscall_needs_wake_handoff(
        &futex_wake,
        &SyscallResult::Return(0)
    ));
    assert!(!direct_trap_syscall_needs_wake_handoff(
        &futex_wait,
        &SyscallResult::Return(0)
    ));
    assert!(!direct_trap_syscall_needs_wake_handoff(
        &clone,
        &SyscallResult::Return(123)
    ));
}

#[test]
fn getppid_uses_cap_only_immediate_fast_path() {
    let _guard = setup();
    let aspace = tx_subsystems::vm::AddressSpace::new_cap_for_platform::<TestPlatform>()
        .expect("test aspace");
    let init = tx_subsystems::process::bootstrap_init_process(aspace).expect("bootstrap init");
    let getppid = SyscallRequest::new(NR_GETPPID, [0; 6]);
    let write = SyscallRequest::new(NR_WRITE, [0; 6]);

    assert_eq!(
        dispatch_cap_only_immediate(&getppid, &init),
        Some(SyscallResult::Return(0))
    );
    assert_eq!(dispatch_cap_only_immediate(&write, &init), None);
}

fn noop_waker() -> Waker {
    Waker::noop().clone()
}

/// Drive `fut` to completion via a spin-poll loop. Mirrors the
/// `block_on` shape used in `crates/tx-kernel/src/init/tests.rs` so
/// the dispatcher's `async` shape is exercised even on its
/// synchronous arms.
fn block_on<F: Future>(mut fut: F) -> F::Output {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` lives on the stack for the duration of the loop;
    // we never move it after `Pin::new_unchecked`.
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("block_on: future did not resolve in 1024 polls");
}

/// A successful FUTEX_WAKE is just a syscall return. It must not park
/// the issuing thread on its own mailbox; musl's pthread-exit path
/// does `__wake(&self->detach_state)` and then immediately reaches
/// `SYS_exit`, where `CLONE_CHILD_CLEARTID` clears
/// `__thread_list_lock`. Parking between those two instructions
/// leaves sibling threads blocked in `__tl_lock`.
#[test]
fn run_thread_future_stays_within_clone_submit_budget() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");

    let future = run_thread::<TestPlatform>(leader.clone(), payload.clone());
    let wrapped = PerHartSlotted::<TestPlatform, _>::new(leader, payload, future);

    assert!(
        core::mem::size_of_val(&wrapped) <= 2048,
        "run_thread task future grew past the clone-submit budget: {} bytes",
        core::mem::size_of_val(&wrapped)
    );
}

#[test]
fn futex_wake_return_reenters_userspace_without_mailbox_event() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");

    let mut initial_ctx = tx_hal::UserTrapContext {
        regs: [0; 32],
        pc: 0,
        status: 0,
        fp: tx_hal::UserFpContext::empty(),
    };
    initial_ctx.regs[10] = 99;
    payload.store_saved_user_context(Some(initial_ctx));

    let future = run_thread::<TestPlatform>(leader.clone(), payload.clone());
    let wrapped = PerHartSlotted::<TestPlatform, _>::new(leader, payload.clone(), future);
    let reactor = crate::adapter::boot_runtime::Reactor::new();
    let _task = reactor.submit_task(wrapped);

    let first = reactor.run_until_idle();
    assert_eq!(first.completed, 0);
    assert_eq!(
        *USERSPACE_A0_LOG.lock().unwrap_or_else(|e| e.into_inner()),
        std::vec![99],
        "first userspace dive uses the seeded baseline return register"
    );

    let active = payload
        .active_userspace_request()
        .expect("run_thread published a userspace wait");
    let futex_wake = UserspaceTrapInfo::Syscall(SyscallRequest::new(
        NR_FUTEX,
        [0x1000, (FUTEX_WAKE | FUTEX_PRIVATE_FLAG) as u64, 1, 0, 0, 0],
    ));
    payload
        .userspace_slot()
        .complete_interesting_trap(active, futex_wake)
        .expect("resolve wait with futex wake");

    let second = reactor.run_until_idle();
    assert_eq!(second.completed, 0);
    assert_eq!(
        second.polled, 1,
        "FUTEX_WAKE must return to userspace in the same thread-future poll"
    );
    assert_eq!(
        *USERSPACE_A0_LOG.lock().unwrap_or_else(|e| e.into_inner()),
        std::vec![99, 0],
        "FUTEX_WAKE return must be written back and immediately re-enter userspace"
    );
}

/// `PerHartSlotted` sets the per-hart slot before delegating to the
/// inner future, and clears it after the inner poll returns. The
/// inner future asserts the slot is `Some` mid-poll; we then assert
/// the slot is `None` after the wrapper's poll returns.
#[test]
fn per_hart_slotted_sets_and_clears_slot_around_poll() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");

    let payload_for_inner = payload.clone();
    let inner = async move {
        // The wrapper installed `payload_for_inner` on hart 0 before
        // entering this future; the slot must reflect it.
        let slot = current_thread_payload(0).expect("slot installed during poll");
        assert_eq!(
            slot.key(),
            payload_for_inner.key(),
            "slot must hold the wrapper's payload"
        );
    };

    let mut wrapped = PerHartSlotted::<TestPlatform, _>::new(leader, payload.clone(), inner);

    // Pre-poll: slot empty.
    assert!(
        current_thread_payload(0).is_none(),
        "slot empty before any poll"
    );

    // SAFETY: `wrapped` lives on the stack for the duration of the
    // single poll call; we never move it after pinning.
    let mut pinned = unsafe { Pin::new_unchecked(&mut wrapped) };
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let out = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(out, Poll::Ready(())),
        "trivial inner future must complete in one poll"
    );

    // Post-poll: slot cleared.
    assert!(
        current_thread_payload(0).is_none(),
        "slot cleared after wrapper poll exit"
    );
}

/// `PerHartSlotted` clears the slot on `Pending` exit too — the
/// per-hart slot must not leak across yields.
#[test]
fn per_hart_slotted_clears_slot_on_pending_exit() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");

    // Inner future that returns Pending the first time it's polled.
    struct PendOnce {
        polled: bool,
    }
    impl Future for PendOnce {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            let this = self.get_mut();
            if !this.polled {
                this.polled = true;
                // Verify the wrapper installed the slot.
                assert!(
                    current_thread_payload(0).is_some(),
                    "slot must be set during inner poll"
                );
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }

    let mut wrapped =
        PerHartSlotted::<TestPlatform, _>::new(leader, payload.clone(), PendOnce { polled: false });
    // SAFETY: stack-pinned for the call.
    let mut pinned = unsafe { Pin::new_unchecked(&mut wrapped) };
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let out = pinned.as_mut().poll(&mut cx);
    assert!(matches!(out, Poll::Pending), "inner returned Pending");

    // Slot cleared even on Pending exit.
    assert!(
        current_thread_payload(0).is_none(),
        "slot cleared after Pending poll exit",
    );
}

/// `PerHartSlotted` also binds the reactor task mailbox into the
/// thread payload while the task is polled. Signal delivery posts its
/// wake hint through this weak handle, so a syscall parked inside
/// `drive()` must have the binding installed before it blocks.
#[test]
fn per_hart_slotted_binds_current_task_mailbox() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");
    assert!(
        payload.mailbox_handle().is_none(),
        "payload starts without a bound task mailbox",
    );

    let observed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let observed_for_inner = std::sync::Arc::clone(&observed);
    let payload_for_inner = payload.clone();
    let inner = async move {
        *observed_for_inner.lock().unwrap_or_else(|e| e.into_inner()) = payload_for_inner
            .mailbox_handle()
            .and_then(|mailbox| mailbox.upgrade())
            .is_some();
    };

    let wrapped = PerHartSlotted::<TestPlatform, _>::new(leader, payload.clone(), inner);
    let reactor = crate::adapter::boot_runtime::Reactor::new();
    let _task = reactor.submit_task(wrapped);
    let result = reactor.run_until_idle();

    assert_eq!(result.completed, 1);
    assert!(
        *observed.lock().unwrap_or_else(|e| e.into_inner()),
        "inner future observes a live task mailbox binding",
    );
    assert!(
        payload
            .mailbox_handle()
            .and_then(|mailbox| mailbox.upgrade())
            .is_some(),
        "payload retains a weak handle to the task mailbox after poll",
    );
}

/// End-to-end host-driven slice that mirrors what the production
/// thread future does for a `Syscall(write)` resolution: open the
/// slot, resolve it with a `Syscall` trap, drive `linux_syscall::dispatch`,
/// stash the return into `pending_syscall_return`. Asserts the
/// dispatcher round-trips correctly. We do **not** drive
/// `prepare_userspace_entry_payload` + `enter_userspace_with_context`
/// here because the latter would diverge into TestPlatform's
/// default-panic impl; that path is exercised in
/// `tx-subsystems/src/thread_runtime/tests.rs` separately.
#[test]
fn thread_future_dispatches_syscall_then_yields_for_userspace_entry() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");
    let aspace = init.aspace_cap().expect("aspace alive");

    // Open the slot the way the future does.
    let wait = payload
        .userspace_slot()
        .start_request()
        .expect("start_request");
    let req_token = wait.request();
    payload.set_active_userspace_request(Some(req_token));

    // Synthesise a `getpid` syscall (no buffers required, no console
    // wiring needed) — it returns the process pid.
    let req = SyscallRequest::new(tx_shims::linux_syscall::NR_GETPID, [0, 0, 0, 0, 0, 0]);
    payload
        .userspace_slot()
        .complete_interesting_trap(req_token, UserspaceTrapInfo::Syscall(req))
        .expect("resolve wait");

    // Drain the wait future to consume the resolution. Mirrors the
    // `wait.await` step inside `run_thread`.
    drop(wait);
    payload.set_active_userspace_request(None);

    // Drive the dispatcher (NR_GETPID is sync but the dispatcher is
    // `async`, so wrap in `block_on`).
    let ctx = SyscallCtx::new(init.clone(), leader.clone(), aspace);
    let result = block_on(dispatch::<TestPlatform>(req, &ctx));
    drop(ctx);

    let v = match result {
        SyscallResult::Return(v) => v,
        other => panic!("expected Return; got {other:?}"),
    };
    payload.store_pending_syscall_return(Some(Ok(v)));

    let drained = drain_pending_syscall_return(&payload);
    assert_eq!(
        drained,
        Some(Ok(init.pid.0 as i64)),
        "getpid result lands in pending_syscall_return"
    );
}

/// `exit_group` returns `SyscallResult::NoReturn` and zombifies the
/// process. Mirrors the `NoReturn` arm of `run_thread`'s match.
#[test]
fn thread_future_terminates_on_exit_group() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader");
    let aspace = init.aspace_cap().expect("aspace alive");

    let wait = payload
        .userspace_slot()
        .start_request()
        .expect("start_request");
    let req_token = wait.request();
    payload.set_active_userspace_request(Some(req_token));
    let req = SyscallRequest::new(NR_EXIT_GROUP, [0, 0, 0, 0, 0, 0]);
    payload
        .userspace_slot()
        .complete_interesting_trap(req_token, UserspaceTrapInfo::Syscall(req))
        .expect("resolve wait");
    drop(wait);
    payload.set_active_userspace_request(None);

    let ctx = SyscallCtx::new(init.clone(), leader.clone(), aspace);
    let result = block_on(dispatch::<TestPlatform>(req, &ctx));
    drop(ctx);

    assert!(
        matches!(result, SyscallResult::NoReturn),
        "exit_group → NoReturn"
    );
    assert!(init.is_zombie(), "exit_group zombifies init");
    assert_eq!(
        init.exit_status(),
        Some(ExitStatus::Exited(0)),
        "exit_group(0) records Exited(0)"
    );

    // No pending_syscall_return write happened.
    assert!(
        drain_pending_syscall_return(&payload).is_none(),
        "NoReturn does not write pending_syscall_return"
    );

    // Touch NR_WRITE so the import is exercised by some test in this
    // file (silences dead-code lint from the use list above).
    let _ = NR_WRITE;
}

// ----- Page-fault dispatch (Phase 3) -----

/// Unit-coverage for the local `pf_access_to_vm_access` helper that
/// translates the reactor's `PageFaultAccess` into the VM subsystem's
/// `AccessMode`. The two enums do not converge today; the mapping
/// must collapse `Unknown` to `Read` (defensive, since the canonical
/// fault script re-derives the protection requirement from the
/// recipe).
#[test]
fn pf_access_translates_to_vm_access_mode() {
    assert_eq!(
        pf_access_to_vm_access(PageFaultAccess::Read),
        AccessMode::Read
    );
    assert_eq!(
        pf_access_to_vm_access(PageFaultAccess::Write),
        AccessMode::Write
    );
    assert_eq!(
        pf_access_to_vm_access(PageFaultAccess::Execute),
        AccessMode::Execute
    );
    // Defensive collapse: `Unknown` falls to `Read` so the recipe
    // lookup still runs without falsely upgrading a load to a store.
    assert_eq!(
        pf_access_to_vm_access(PageFaultAccess::Unknown),
        AccessMode::Read
    );
}

/// Initialise the host page allocator's zero frame so
/// `materialize_pagebacked` (private-anon read fault) can publish
/// the global zero PPN. Idempotent across tests.
fn ensure_zero_frame_claimed() {
    use crate::adapter::step_engine::page_allocator;
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for thread-future tests: {error:?}"),
    }
}

fn saved_signal_frame_bytes(
    frame: &tx_hal::SavedSignalFrame,
) -> [u8; size_of::<tx_hal::SavedSignalFrame>()] {
    let mut bytes = [0u8; size_of::<tx_hal::SavedSignalFrame>()];
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::from_ref(frame).cast::<u8>(),
            bytes.as_mut_ptr(),
            bytes.len(),
        );
    }
    bytes
}

/// `PageFault` Ok path: the fault script publishes a recipe and the
/// loop body falls through to AST drain + entry without writing any
/// `pending_syscall_return`. The merged-context `a0` would come from
/// `saved_user_context` (Plan B writeback discipline for fault
/// returns).
///
/// Mirrors the run_thread loop iteration step-by-step rather than
/// invoking `run_thread` (which would diverge into the host
/// TestPlatform's default `enter_userspace_with_context` panic). The
/// test asserts:
///
/// 1. The slot resolves with `PageFault(...)`.
/// 2. `aspace.fault_script(VmFault).await` returns `Ok(_)`.
/// 3. `pending_syscall_return` is unchanged (no write on Ok).
/// 4. The next iteration's `start_request` succeeds (slot is back to
///    idle), demonstrating the loop is ready to continue.
#[test]
fn thread_future_pf_ok_loops_back_to_userspace_entry() {
    let _g = setup();
    ensure_zero_frame_claimed();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let aspace = init.aspace_cap().expect("aspace alive");

    // Seed a private-anon mapping so the fault at FAULT_ADDR has a
    // recipe and the script's first poll publishes via the test pmap
    // (which accepts every reservation).
    const FAULT_ADDR: usize = 0x1000;
    aspace
        .try_mmap(VmMapRequest::fixed(
            UserRange::new_aligned(UserVirtAddr(FAULT_ADDR), TEST_PAGE_SIZE).expect("range"),
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("seed recipe");

    // Open the userspace-run wait the way `run_thread` does.
    let wait = payload
        .userspace_slot()
        .start_request()
        .expect("start_request");
    let req_token = wait.request();
    payload.set_active_userspace_request(Some(req_token));

    // Resolve the wait with a from-user PageFault for FAULT_ADDR
    // (Read access — exercises the zero-frame materialisation path).
    let pf = PageFaultInfo {
        addr: UserAddr::new(FAULT_ADDR as u64),
        access: PageFaultAccess::Read,
        present: false,
    };
    payload
        .userspace_slot()
        .complete_interesting_trap(req_token, UserspaceTrapInfo::PageFault(pf))
        .expect("resolve wait with PageFault");
    drop(wait);
    payload.set_active_userspace_request(None);

    // Drive the canonical fault script — what run_thread's PageFault
    // arm awaits.
    let fault = VmFault::new(
        UserVirtAddr::new(pf.addr.raw() as usize),
        pf_access_to_vm_access(pf.access),
    );
    let result = block_on(aspace.fault_script(fault));
    assert!(
        result.is_ok(),
        "fault_script(Read on private-anon) must succeed; got {result:?}"
    );

    // Ok path does NOT write pending_syscall_return.
    assert!(
        drain_pending_syscall_return(&payload).is_none(),
        "PageFault Ok must not write pending_syscall_return — Plan B \
         writeback discipline reuses saved_user_context.a0"
    );

    // The loop is ready to continue: a fresh start_request succeeds
    // (slot returned to idle when the wait was dropped above).
    let next_wait = payload
        .userspace_slot()
        .start_request()
        .expect("loop continues — next iteration starts a fresh request");
    drop(next_wait);

    // Process is still live; no SIGSEGV was routed.
    assert!(!init.is_zombie(), "Ok path must leave process live");
}

/// `PageFault` Err path: the fault script returns `VmFaultError`
/// (here: `NoRecipe` for an address with no mapping) and the future
/// routes default-action SIGSEGV per `SIGNAL_v1` §15.1.
#[test]
fn thread_future_pf_err_routes_sigsegv_and_zombifies() {
    let _g = setup();
    ensure_zero_frame_claimed();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let aspace = init.aspace_cap().expect("aspace alive");

    // No mapping installed at FAULT_ADDR — fault_script must return
    // VmFaultError::NoRecipe.
    const FAULT_ADDR: usize = 0xdead_0000;

    let wait = payload
        .userspace_slot()
        .start_request()
        .expect("start_request");
    let req_token = wait.request();
    payload.set_active_userspace_request(Some(req_token));

    let pf = PageFaultInfo {
        addr: UserAddr::new(FAULT_ADDR as u64),
        access: PageFaultAccess::Write,
        present: false,
    };
    payload
        .userspace_slot()
        .complete_interesting_trap(req_token, UserspaceTrapInfo::PageFault(pf))
        .expect("resolve wait with PageFault");
    drop(wait);
    payload.set_active_userspace_request(None);

    let fault = VmFault::new(
        UserVirtAddr::new(pf.addr.raw() as usize),
        pf_access_to_vm_access(pf.access),
    );
    let result = block_on(aspace.fault_script(fault));
    assert_eq!(
        result,
        Err(VmFaultError::NoRecipe),
        "fault on unmapped address must surface NoRecipe"
    );

    // Phase B: run_thread routes VM-fault Err through
    // deliver_synchronous_fault per SIGNAL_v1 §20. The default-action
    // path in deliver_synchronous_fault calls step_exit_group_with_signal,
    // which is what this test exercises directly.
    tx_subsystems::process::execution::step_exit_group_with_signal(&init, Signum::SIGSEGV);

    assert!(init.is_zombie(), "SIGSEGV routing zombifies process");
    assert_eq!(
        init.exit_status(),
        Some(ExitStatus::Signaled(Signum::SIGSEGV)),
        "SIGSEGV records Signaled(SIGSEGV)"
    );

    // No pending_syscall_return write on Err either.
    assert!(
        drain_pending_syscall_return(&payload).is_none(),
        "PageFault Err must not write pending_syscall_return"
    );
}

// ----- ExecCommitted dispatch (Wave 4 / Phase 6 of the ELF-loader plan) -----

/// `SyscallResult::ExecCommitted` is the new variant returned from
/// `dispatch::<P>` after a successful `execve`. The thread future
/// must:
///
/// 1. **Not** drain `pending_syscall_return` for this iteration (the
///    new image's `_start` reads from the freshly-seeded
///    `saved_user_context.regs[10]`, which the script left at zero).
/// 2. Loop back to AST drain + userspace-entry rather than terminate.
///
/// We mirror `run_thread`'s syscall-arm match here without invoking
/// `run_thread` itself (the loop ends with a divergent
/// `enter_userspace_with_context` that would panic in TestPlatform).
/// The test scripts the dispatcher's outcome directly with
/// `SyscallResult::ExecCommitted` and asserts the two key contract
/// points: no `pending_syscall_return` write, and the slot is back
/// to idle so the next iteration can `start_request` afresh.
///
/// Cite: `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`,
/// `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`.
#[test]
fn thread_future_execve_continues_loop_without_writing_pending_return() {
    let _g = setup();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");

    // Open a userspace-run wait the way `run_thread` does.
    let wait = payload
        .userspace_slot()
        .start_request()
        .expect("start_request");
    let req_token = wait.request();
    payload.set_active_userspace_request(Some(req_token));

    // Synthesise a syscall trap (the actual nr is irrelevant — we
    // are scripting the dispatcher's *outcome* below). NR_GETPID
    // resolves the wait without exercising the real execve path.
    let req = SyscallRequest::new(tx_shims::linux_syscall::NR_GETPID, [0, 0, 0, 0, 0, 0]);
    payload
        .userspace_slot()
        .complete_interesting_trap(req_token, UserspaceTrapInfo::Syscall(req))
        .expect("resolve wait with Syscall");
    drop(wait);
    payload.set_active_userspace_request(None);

    // Script the dispatcher's outcome. In production this `result`
    // would be returned from `dispatch::<P>(execve_req, &ctx).await`
    // after the exec_script Phase-6 swap. We pin the variant
    // directly because the test scaffold's `ShimsTestPmap` is not
    // available here and a full execve round-trip is the
    // dispatcher's responsibility (covered by tx-shims tests). What
    // we exercise here is the thread-future's match-arm behaviour.
    let result = SyscallResult::ExecCommitted;

    // Mirror run_thread's match: ExecCommitted → no
    // store_pending_syscall_return write; no early return.
    let early_terminate = match result {
        SyscallResult::Return(v) => {
            payload.store_pending_syscall_return(Some(Ok(v)));
            false
        }
        SyscallResult::CloneReturn { value, .. } => {
            payload.store_pending_syscall_return(Some(Ok(value)));
            false
        }
        SyscallResult::Error(e) => {
            payload.store_pending_syscall_return(Some(Err(e)));
            false
        }
        SyscallResult::NoReturn => true,
        SyscallResult::ExecCommitted => false,
        SyscallResult::SigreturnRestored => false,
    };

    assert!(
        !early_terminate,
        "ExecCommitted must NOT terminate the loop (vs NoReturn which terminates)"
    );

    // Critical Phase-6 contract: the dispatcher did NOT write a
    // pending_syscall_return; the next userspace re-entry runs from
    // the new image's freshly-seeded saved_user_context.
    assert!(
        drain_pending_syscall_return(&payload).is_none(),
        "ExecCommitted must NOT write pending_syscall_return — the new \
         image's _start expects fresh state"
    );

    // The loop is ready to continue: a fresh start_request succeeds
    // (the wait was dropped above), demonstrating the slot is back
    // to idle for the userspace-entry checkpoint.
    let next_wait = payload
        .userspace_slot()
        .start_request()
        .expect("loop continues — next iteration starts a fresh request");
    drop(next_wait);

    // Process is still live; ExecCommitted is a successful path.
    assert!(
        !init.is_zombie(),
        "ExecCommitted is success — process must remain live"
    );
}

/// `rt_sigreturn` must restore the user-edited signal frame. musl's
/// pthread cancellation handler rewrites `ucontext_t.uc_mcontext.MC_PC`
/// to `__cp_cancel`; resuming the parked pre-handler snapshot would
/// lose that rewrite and keep returning to the interrupted syscall.
#[test]
fn thread_future_sigreturn_restores_user_edited_frame_context_and_mask() {
    let _g = setup();
    ensure_zero_frame_claimed();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader thread post-bootstrap");
    let aspace = init.aspace_cap().expect("aspace alive");

    let mut parked = tx_hal::UserTrapContext::empty();
    parked.pc = 0x1234_5678;
    parked.regs[10] = 0xdead_beef;
    payload.store_saved_signal_context(Some(parked));

    let mut handler_ctx = tx_hal::UserTrapContext::empty();
    handler_ctx.regs[2] = 0x7000;
    payload.store_saved_user_context(Some(handler_ctx));
    payload.store_signal_mask(tx_subsystems::signal::SignalMask::new(
        Signum::SIGTERM.bit(),
    ));

    let mut edited = tx_hal::UserTrapContext::empty();
    edited.pc = 0x8765_4321;
    edited.regs[2] = 0x8000;
    edited.regs[10] = 0xfeed_face;
    let frame = tx_hal::SavedSignalFrame {
        saved_mask: tx_hal::UserSignalMaskAbi { bits: 0 },
        user_context: edited,
    };
    let frame_bytes = saved_signal_frame_bytes(&frame);
    aspace
        .try_mmap(VmMapRequest::fixed(
            UserRange::new_aligned(UserVirtAddr(0x7000), TEST_PAGE_SIZE).expect("frame range"),
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("seed signal frame mapping");
    let guard = crate::adapter::step_engine::guard();
    let copied = aspace.copy_to_user(tx_hal::UserPtr::new(0x7000), &frame_bytes, &guard);
    assert_eq!(
        copied,
        crate::adapter::step_engine::StepOutcome::Done(frame_bytes.len())
    );
    drop(guard);

    restore_sigreturn_frame::<TestPlatform>(&leader, &aspace, &payload, &handler_ctx)
        .expect("valid edited frame restores");

    let restored = payload
        .saved_user_context()
        .expect("sigreturn arm must store restored context");
    assert_eq!(restored.pc, 0x8765_4321);
    assert_eq!(restored.regs[10], 0xfeed_face);
    assert_eq!(payload.signal_mask().raw_bits(), 0);
    assert!(
        !init.is_zombie(),
        "valid sigreturn frame keeps process live"
    );
}

#[test]
fn thread_future_sigreturn_preserves_user_blocked_sigcancel_mask() {
    let _g = setup();
    ensure_zero_frame_claimed();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader thread post-bootstrap");
    let aspace = init.aspace_cap().expect("aspace alive");

    let mut parked = tx_hal::UserTrapContext::empty();
    parked.pc = 0x1234_5678;
    parked.regs[10] = 0xdead_beef;
    payload.store_saved_signal_context(Some(parked));

    let mut handler_ctx = tx_hal::UserTrapContext::empty();
    handler_ctx.regs[2] = 0x7000;
    payload.store_saved_user_context(Some(handler_ctx));
    payload.store_signal_mask(tx_subsystems::signal::SignalMask::EMPTY);

    let mut edited = tx_hal::UserTrapContext::empty();
    edited.pc = 0x8765_4321;
    edited.regs[2] = 0x8000;
    edited.regs[10] = 0xfeed_face;
    let frame = tx_hal::SavedSignalFrame {
        saved_mask: tx_hal::UserSignalMaskAbi {
            bits: 1u64 << (33 - 1),
        },
        user_context: edited,
    };
    let frame_bytes = saved_signal_frame_bytes(&frame);
    aspace
        .try_mmap(VmMapRequest::fixed(
            UserRange::new_aligned(UserVirtAddr(0x7000), TEST_PAGE_SIZE).expect("frame range"),
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("seed signal frame mapping");
    let guard = crate::adapter::step_engine::guard();
    let copied = aspace.copy_to_user(tx_hal::UserPtr::new(0x7000), &frame_bytes, &guard);
    assert_eq!(
        copied,
        crate::adapter::step_engine::StepOutcome::Done(frame_bytes.len())
    );
    drop(guard);

    restore_sigreturn_frame::<TestPlatform>(&leader, &aspace, &payload, &handler_ctx)
        .expect("valid edited frame restores");

    assert!(
        payload.signal_mask().raw_bits() & (1u64 << (33 - 1)) != 0,
        "sigreturn must preserve a user-blocked SIGCANCEL mask"
    );
    let restored = payload
        .saved_user_context()
        .expect("sigreturn arm must store restored context");
    assert_eq!(restored.pc, 0x8765_4321);
    assert_eq!(restored.regs[10], 0xfeed_face);
    assert!(
        !init.is_zombie(),
        "valid sigreturn frame keeps process live"
    );
}

#[test]
fn thread_future_sigreturn_recomputes_summary_for_restored_blocked_sigcancel() {
    let _g = setup();
    ensure_zero_frame_claimed();
    let payload = bootstrap_payload();
    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS populated post-bootstrap");
    let leader = init.nth_thread(0).expect("leader thread post-bootstrap");
    let aspace = init.aspace_cap().expect("aspace alive");
    let sigcancel = Signum::new(33).expect("musl SIGCANCEL");

    tx_subsystems::signal::step_sigaction(&init, sigcancel, SigDisposition::Handler(0xCAFE));
    tx_subsystems::thread_runtime::execution::post_signal(
        &leader,
        sigcancel,
        tx_subsystems::signal::adapter::step_engine::SignalRouting::ThreadDirected {
            tid: leader.tid.0 as u64,
        },
        None,
    );
    assert!(
        payload.interrupt_summary().deliverable_signal,
        "unmasked pending SIGCANCEL should start deliverable"
    );

    let mut handler_ctx = tx_hal::UserTrapContext::empty();
    handler_ctx.regs[2] = 0x7000;
    payload.store_saved_user_context(Some(handler_ctx));
    payload.store_signal_mask(tx_subsystems::signal::SignalMask::EMPTY);

    let frame = tx_hal::SavedSignalFrame {
        saved_mask: tx_hal::UserSignalMaskAbi {
            bits: sigcancel.bit(),
        },
        user_context: tx_hal::UserTrapContext::empty(),
    };
    let frame_bytes = saved_signal_frame_bytes(&frame);
    aspace
        .try_mmap(VmMapRequest::fixed(
            UserRange::new_aligned(UserVirtAddr(0x7000), TEST_PAGE_SIZE).expect("frame range"),
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("seed signal frame mapping");
    let guard = crate::adapter::step_engine::guard();
    let copied = aspace.copy_to_user(tx_hal::UserPtr::new(0x7000), &frame_bytes, &guard);
    assert_eq!(
        copied,
        crate::adapter::step_engine::StepOutcome::Done(frame_bytes.len())
    );
    drop(guard);

    restore_sigreturn_frame::<TestPlatform>(&leader, &aspace, &payload, &handler_ctx)
        .expect("valid edited frame restores");

    assert!(
        !payload.interrupt_summary().deliverable_signal,
        "restoring a mask that blocks pending SIGCANCEL must clear deliverability"
    );
}

#[test]
fn thread_future_siginfo_to_user_abi_matches_musl_siginfo_prefix() {
    let info = tx_subsystems::signal::SigInfo {
        si_signo: Signum::SIGTERM.raw() as u32,
        si_code: tx_subsystems::signal::SI_USER,
        si_pid: 123,
        si_uid: 456,
    };

    let abi = siginfo_to_user_abi(info);
    assert_eq!(u32::from_le_bytes(abi.bytes[0..4].try_into().unwrap()), 15);
    assert_eq!(i32::from_le_bytes(abi.bytes[8..12].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(abi.bytes[16..20].try_into().unwrap()),
        123
    );
    assert_eq!(
        u32::from_le_bytes(abi.bytes[20..24].try_into().unwrap()),
        456
    );
}
