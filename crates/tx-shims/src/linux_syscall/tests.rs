//! Phase 2a syscall-dispatch tests.
//!
//! Per the Trio plan §"Part 2 — Tests" (lines 247-258) restricted to
//! Phase 2a scope (`NR_WRITE`, `NR_EXIT`, `NR_EXIT_GROUP`, `NR_GETPID`,
//! and the `-ENOSYS` default arm). The tests call `dispatch` directly;
//! Plan B writeback discipline is asserted by checking the
//! `SyscallResult` shape, not by exercising a userspace round-trip.
//!
//! Test isolation: each test takes the shared `SHIMS_TEST_LOCK` to
//! serialise zone init and TTY-registry mutations, then resets the
//! `INIT_PROCESS` slot and pid/tid counters via the `test-support`
//! feature on `tx-subsystems`.
//!
//! Doc anchors:
//! - `txdoc:PROCESS-STEP-THREAD-EXIT-1` (PROCESS_v1 §7.3.1) — the
//!   single-threaded-`exit` chain assertion.
//! - `txdoc:PROCESS-STEP-EXIT-GROUP-1` (PROCESS_v1 §7.3.2).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::{Arc, Mutex};
use std::task::Wake;

use tx_reactor::userspace::SyscallRequest;
use tx_substrate::zone::Cap;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT};
use tx_subsystems::execution::{Guard, StepOutcome};
use tx_subsystems::process::{bootstrap_init_process, ExitStatus, Pid, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::tty::execution::{register_console_alias, register_hardware};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

use super::{
    dispatch, SyscallCtx, SyscallResult, NR_BRK, NR_EXIT, NR_EXIT_GROUP, NR_GETPID, NR_READ,
    NR_RT_SIGACTION, NR_RT_SIGPROCMASK, NR_WRITE,
};

// ---------------------------------------------------------------------------
// Test platform — minimal `PmapIf` impl for `AddressSpace` construction.
// ---------------------------------------------------------------------------
//
// `tx-subsystems::vm::pmap::TestPmap` is `pub(crate)` — not accessible
// from this crate. Phase 2a's tests don't exercise mmap/fault paths,
// so the only required behaviour is `create_pmap_root`. The remaining
// trait methods take defaults (which are never invoked because
// bootstrap_init_process only constructs the AddressSpace).

use std::collections::BTreeMap;
use std::sync::LazyLock;
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot,
    PmapUnmapResult, PtNode, VirtAddr,
};
use tx_subsystems::vm::USER_PAGE_SIZE;

struct ShimsTestPmap;

#[derive(Default)]
struct ShimsTestPmapState {
    next_root: usize,
    mappings: BTreeMap<(usize, usize), PhysAddr>,
}

static SHIMS_TEST_PMAP_STATE: LazyLock<Mutex<ShimsTestPmapState>> = LazyLock::new(|| {
    Mutex::new(ShimsTestPmapState {
        next_root: 1,
        mappings: BTreeMap::new(),
    })
});

fn root_key(root: &PmapRoot) -> usize {
    root.phys().0
}

impl PmapIf for ShimsTestPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = SHIMS_TEST_PMAP_STATE.lock().expect("shims pmap lock");
        let root_id = state.next_root;
        state.next_root += 1;
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(root_id * USER_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(root: PmapRoot) {
        let mut state = SHIMS_TEST_PMAP_STATE.lock().expect("shims pmap lock");
        let key = root.phys().0;
        state.mappings.retain(|(r, _), _| *r != key);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        let state = SHIMS_TEST_PMAP_STATE.lock().expect("shims pmap lock");
        if state.mappings.contains_key(&(root_key(root), virt.0)) {
            return Err(PmapError::AlreadyMapped);
        }
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

    fn commit_mapping(
        root: &PmapRoot,
        reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
        let mut state = SHIMS_TEST_PMAP_STATE.lock().expect("shims pmap lock");
        state
            .mappings
            .insert((root_key(root), reservation.virt().0), reservation.phys());
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        let mut state = SHIMS_TEST_PMAP_STATE.lock().expect("shims pmap lock");
        let Some(phys) = state.mappings.remove(&(root_key(root), virt.0)) else {
            return Ok(None);
        };
        Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
    }
}

// ---------------------------------------------------------------------------
// Shared per-test setup.
// ---------------------------------------------------------------------------

/// Serialise zone init, TTY-registry mutations, and process-static
/// reset across all Phase 2a syscall tests. These resources are
/// process-wide singletons; concurrent tests that bootstrap init or
/// register hardware would race without this lock.
static SHIMS_TEST_LOCK: Mutex<()> = Mutex::new(());

struct TestSetup {
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn setup() -> TestSetup {
    let lock = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_substrate::testing::init_host_for_test_once();
    let _ = zones::register_all();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    TestSetup { _lock: lock }
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<ShimsTestPmap>().expect("fresh aspace")
}

fn bootstrap() -> Cap<ProcessIdentity> {
    bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
}

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    proc_cap
        .nth_thread(0)
        .expect("alive process has leader thread")
}

fn make_ctx(process: Cap<ProcessIdentity>, thread: Cap<ThreadIdentity>) -> SyscallCtx<'static> {
    let aspace = process.aspace_cap().expect("alive aspace");
    SyscallCtx::new(process, thread, aspace)
}

// ---------------------------------------------------------------------------
// Capturing console hardware: a CharDeviceOps impl that records bytes
// the underlying transport sees. The TTY's N_TTY ldisc applies
// OPOST|ONLCR before the bytes hit this binding, so the captured
// snapshot reflects post-output-processing.
// ---------------------------------------------------------------------------

struct CapturingOps {
    captured: Mutex<Vec<u8>>,
}

impl CapturingOps {
    fn new() -> Self {
        Self {
            captured: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.captured.lock().expect("capture lock").clone()
    }
}

impl CharDeviceOps for CapturingOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        self.captured
            .lock()
            .expect("capture lock")
            .extend_from_slice(bytes);
        StepOutcome::Done(bytes.len())
    }
}

fn install_capturing_console() -> &'static CapturingOps {
    let ops_static: &'static CapturingOps = Box::leak(Box::new(CapturingOps::new()));
    let binding = Box::leak(Box::new(CharDeviceBinding {
        devt: DevT::new(4, 64),
        name: "shims-console-test",
        ops: ops_static,
    }));
    let guard = tx_substrate::epoch::guard();
    let tty = match register_hardware("shims-console-hw", 0, binding, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty),
        StepOutcome::Done(())
    );
    ops_static
}

// ---------------------------------------------------------------------------
// Minimal future-driver: spin-poll a future to completion.
//
// Phase 2a's `dispatch` returns synchronously today (no `.await`
// points are reached in the implemented arms because step_write
// against the test TTY produces `StepOutcome::Done` immediately).
// The loop is defensive — if a future lane introduces blocking
// behaviour the test will see Pending and fail rather than silently
// skip the await.
// ---------------------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

fn block_on<F: Future>(mut fut: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    // SAFETY: the future stays on the stack for the duration of the
    // poll loop; we never move it after pinning.
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("block_on: future did not resolve in 1024 polls");
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// `write(1, "hello\n", 6)` resolves through the dispatcher into
/// `OpenFile::step_write`, which routes via `RNodeBacking::StructBacked
/// { Tty }` → `tty::execution::step_write` → the capturing
/// CharDeviceBinding. Returns the number of *input* bytes consumed
/// (matches Linux semantics for `write(2)`); the captured byte stream
/// reflects post-OPOST `\n→\r\n` expansion.
#[test]
fn dispatch_write_one_to_console_returns_byte_count() {
    let _setup = setup();
    let ops = install_capturing_console();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    // Phase 3a's `open_console_for_init` resolves the registered
    // `console` alias and constructs an `OpenFile` directly.
    let console: Cap<OpenFile> = tx_fs::devfs::open_console_for_init();
    proc_cap.set_fd(1, Some(console));

    let ctx = make_ctx(proc_cap, thread);
    let buf: &[u8] = b"hello\n";
    let req = SyscallRequest::new(NR_WRITE, [1, buf.as_ptr() as u64, 6, 0, 0, 0]);

    let result = block_on(dispatch(req, &ctx));

    assert_eq!(result, SyscallResult::Return(6));
    assert_eq!(
        ops.snapshot(),
        b"hello\r\n",
        "OPOST should expand LF to CRLF before reaching the device transport"
    );
}

/// `exit_group(0)` zombifies the process at once and records
/// `ExitStatus::Exited(0)` per `PROCESS_v1` §7.3.2.
#[test]
fn dispatch_exit_group_marks_process_zombie() {
    let _setup = setup();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_EXIT_GROUP, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch(req, &ctx));

    assert_eq!(result, SyscallResult::NoReturn);
    assert!(
        proc_cap.is_zombie(),
        "exit_group should zombify the process"
    );
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(0)));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

/// For a single-threaded process, `exit(7)` chains internally inside
/// `step_thread_exit` to `step_process_exit` (per `PROCESS_v1` §7.3.1
/// step 3: "If `thread_count == 0`: trigger step_process_exit"). The
/// dispatcher therefore only calls `step_thread_exit`; the process
/// observes `ExitStatus::Exited(7)` after the chain.
#[test]
fn dispatch_exit_for_single_thread_chains_to_exit_group() {
    let _setup = setup();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_EXIT, [7, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch(req, &ctx));

    assert_eq!(result, SyscallResult::NoReturn);
    assert!(
        proc_cap.is_zombie(),
        "single-threaded exit should zombify the process via step_thread_exit's last-thread cascade"
    );
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(7)));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

/// `getpid()` for the bootstrap init process returns pid 1.
#[test]
fn dispatch_getpid_returns_init_pid_for_init_process() {
    let _setup = setup();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    assert_eq!(proc_cap.pid, Pid::INIT);

    let ctx = make_ctx(proc_cap, thread);
    let req = SyscallRequest::new(NR_GETPID, [0; 6]);
    let result = block_on(dispatch(req, &ctx));

    assert_eq!(result, SyscallResult::Return(1));
}

/// Any nr not in the Phase 2a/2b table returns `-ENOSYS` (positive
/// magnitude 38; the userspace-entry shim negates before writing).
#[test]
fn dispatch_unknown_nr_returns_neg_enosys() {
    let _setup = setup();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(9999, [0; 6]);
    let result = block_on(dispatch(req, &ctx));

    assert_eq!(result, SyscallResult::Error(38));
}

// ---------------------------------------------------------------------------
// Phase 2b — read / brk / rt_sigprocmask / rt_sigaction.
// ---------------------------------------------------------------------------

/// `read(0, buf, 64)` against a console with no buffered input
/// resolves through `tty::execution::step_read`'s `Blocked` shape,
/// which the dispatcher translates to `Done(0)` per the Trio plan
/// §"Open questions #5" non-blocking-slice semantic.
#[test]
fn dispatch_read_zero_when_console_empty_returns_zero() {
    let _setup = setup();
    let _ops = install_capturing_console();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    let console: Cap<OpenFile> = tx_fs::devfs::open_console_for_init();
    proc_cap.set_fd(0, Some(console));

    let ctx = make_ctx(proc_cap, thread);
    let mut buf = [0u8; 64];
    let req = SyscallRequest::new(NR_READ, [0, buf.as_mut_ptr() as u64, 64, 0, 0, 0]);

    let result = block_on(dispatch(req, &ctx));

    assert_eq!(
        result,
        SyscallResult::Return(0),
        "empty TTY input queue should surface as Done(0) per non-blocking slice"
    );
}

/// `brk(0)` reports the current break, then `brk(>current)` grows,
/// then `brk(<current)` shrinks. Bootstrap init's break starts at
/// `BOOTSTRAP_BRK_BASE = 0x6000_0000`. Page granularity: arguments
/// must be page-aligned (4 KiB on RV64) for `brk_script` to accept
/// them per `txdoc:VM-5-8-BRK`.
#[test]
fn dispatch_brk_grow_then_shrink_round_trip() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // brk(0) → reports current break (== brk_base for init).
    let r0 = block_on(dispatch(
        SyscallRequest::new(NR_BRK, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r0, SyscallResult::Return(0x6000_0000));

    // brk(grow) → returns new break.
    let grow_target = 0x6000_1000u64;
    let r1 = block_on(dispatch(
        SyscallRequest::new(NR_BRK, [grow_target, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(grow_target as i64));
    assert_eq!(proc_cap.current_brk(), grow_target);

    // brk(shrink back to base) → returns base.
    let r2 = block_on(dispatch(
        SyscallRequest::new(NR_BRK, [0x6000_0000, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(0x6000_0000));
    assert_eq!(proc_cap.current_brk(), 0x6000_0000);
}

/// brk(addr) for `addr < brk_base` is `InvalidRange` from
/// `brk_script`; per Linux semantics the dispatcher returns the
/// **unchanged** current break, never a negative errno.
#[test]
fn dispatch_brk_invalid_range_returns_unchanged_current() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // brk(very-low) — below brk_base. Linux: no errno, just report
    // the unchanged current break.
    let r = block_on(dispatch(
        SyscallRequest::new(NR_BRK, [0x1000, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0x6000_0000));
    assert_eq!(
        proc_cap.current_brk(),
        0x6000_0000,
        "invalid brk must not have moved the break"
    );
}

/// `rt_sigprocmask(SIG_BLOCK, set, oldset, 8)` round-trip:
/// SIG_BLOCK installs SIGUSR1, then SIG_UNBLOCK removes it. Each call
/// observes the previous mask through `oldset_ptr`.
///
/// SIGUSR1 is signum 10 in Linux generic ABI (bit 9 in the 64-bit
/// bitset). Day-1 `Signum` constants don't include SIGUSR1, but we
/// emit raw bits directly — `step_sigprocmask` consumes a `SignalMask`
/// regardless of which signum produced the bit.
#[test]
fn dispatch_rt_sigprocmask_block_then_unblock_round_trip() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIGUSR1_BIT: u64 = 1u64 << 9; // signum 10 → bit 9

    let mut set: u64 = SIGUSR1_BIT;
    let mut oldset: u64 = 0xdead_beefu64;

    // SIG_BLOCK SIGUSR1; oldset should be 0 (init starts with empty mask).
    let r1 = block_on(dispatch(
        SyscallRequest::new(
            NR_RT_SIGPROCMASK,
            [
                SIG_BLOCK,
                &mut set as *mut u64 as u64,
                &mut oldset as *mut u64 as u64,
                8,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(0));
    assert_eq!(oldset, 0, "initial mask was empty");

    // SIG_UNBLOCK SIGUSR1; oldset should observe the previously-set bit.
    let mut oldset2: u64 = 0xdead_beefu64;
    let r2 = block_on(dispatch(
        SyscallRequest::new(
            NR_RT_SIGPROCMASK,
            [
                SIG_UNBLOCK,
                &mut set as *mut u64 as u64,
                &mut oldset2 as *mut u64 as u64,
                8,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(0));
    assert_eq!(
        oldset2, SIGUSR1_BIT,
        "second call should observe SIGUSR1 still in the mask"
    );
}

/// `rt_sigprocmask` with `sigsetsize != 8` is rejected with `-EINVAL`
/// per Linux generic ABI / `SIGNAL_v1` §3 (sigset is always 64 bits
/// on RV64).
#[test]
fn dispatch_rt_sigprocmask_rejects_wrong_sigsetsize() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch(
        SyscallRequest::new(NR_RT_SIGPROCMASK, [0, 0, 0, 16, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(22));
}

/// `rt_sigaction(SIGUSR1, act, oldact, 8)` round-trip: install a
/// custom handler for SIGUSR1 (signum 10), then query it back via
/// oldact in a follow-up call. The handler value is preserved
/// across the read.
#[test]
fn dispatch_rt_sigaction_install_then_query_round_trip() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    const HANDLER_ADDR: u64 = 0xCAFE_F00D_DEAD_BEEFu64;
    let act: [u64; 4] = [HANDLER_ADDR, 0, 0, 0]; // handler/flags/restorer/mask
    let mut oldact: [u64; 4] = [0xDEADu64; 4];

    // Install: oldact reports prev (Default == 0).
    let r1 = block_on(dispatch(
        SyscallRequest::new(
            NR_RT_SIGACTION,
            [
                10, // SIGUSR1
                act.as_ptr() as u64,
                oldact.as_mut_ptr() as u64,
                8,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(0));
    assert_eq!(
        oldact[0], 0,
        "previous disposition was Default (== SIG_DFL == 0)"
    );

    // Query (act == NULL): oldact reports the just-installed handler.
    let mut oldact2: [u64; 4] = [0xDEADu64; 4];
    let r2 = block_on(dispatch(
        SyscallRequest::new(
            NR_RT_SIGACTION,
            [10, 0, oldact2.as_mut_ptr() as u64, 8, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(0));
    assert_eq!(
        oldact2[0], HANDLER_ADDR,
        "query should observe the previously-installed handler"
    );
}

/// `rt_sigaction` with `sigsetsize != 8` is rejected with `-EINVAL`
/// per Linux generic ABI / `SIGNAL_v1` §15.1.
#[test]
fn dispatch_rt_sigaction_rejects_wrong_sigsetsize() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch(
        SyscallRequest::new(NR_RT_SIGACTION, [10, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(22));
}
