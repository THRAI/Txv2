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
    dispatch, SyscallCtx, SyscallResult, FD_CLOEXEC, F_GETFD, F_SETFD, NR_BRK, NR_CLONE, NR_EXECVE,
    NR_EXIT, NR_EXIT_GROUP, NR_FCNTL, NR_GETPGID, NR_GETPGRP, NR_GETPID, NR_GETPPID, NR_GETSID,
    NR_READ, NR_RT_SIGACTION, NR_RT_SIGPROCMASK, NR_SETPGID, NR_SETSID, NR_SET_ROBUST_LIST,
    NR_SET_TID_ADDRESS, NR_WAIT4, NR_WRITE, SIGCHLD, WNOHANG,
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

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Error(38));
}

// ---------------------------------------------------------------------------
// Phase 2b — read / brk / rt_sigprocmask / rt_sigaction.
// ---------------------------------------------------------------------------

/// `read(0, buf, len)` against a console with no buffered input
/// blocks until `tty::execution::step_ingest` queues a byte and fires
/// the registered `wait_carrier` channel, then returns the byte.
///
/// Pre-ELF Phase 5 (item 9): the dispatcher used to short-circuit
/// `Blocked` to `Done(0)` per the trio's non-blocking slice. With the
/// IRQ-driven UART RX path landed, the dispatcher actually awaits
/// the wait carrier and re-polls. The test models the IRQ side by
/// calling `step_ingest` directly between manual `Future::poll`
/// invocations — same effect a real `uart_rx_irq_handler` call has
/// from inside the trap shell.
#[test]
fn dispatch_read_blocks_until_tty_input_then_returns_byte() {
    let _setup = setup();
    let _ops = install_capturing_console();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);

    let console: Cap<OpenFile> = tx_fs::devfs::open_console_for_init();
    proc_cap.set_fd(0, Some(console));

    // Resolve the registered console TTY so the test can drive
    // step_ingest directly. `tty::project::resolve_devfs_alias` keeps
    // the alias-to-cap mapping that `register_console_alias`
    // populated.
    let console_tty = tx_subsystems::tty::project::resolve_devfs_alias(b"console")
        .expect("console alias must resolve after install_capturing_console");

    let ctx = make_ctx(proc_cap, thread);
    let mut buf = [0u8; 64];
    let req = SyscallRequest::new(NR_READ, [0, buf.as_mut_ptr() as u64, 64, 0, 0, 0]);

    // Manually drive the future: first poll should observe an empty
    // TTY input queue and return Pending after registering a waker
    // on the wait-carrier `Channel`.
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    let first = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first, Poll::Pending),
        "blocked read on empty TTY should park; got {first:?}"
    );

    // Now drive the IRQ side: ingest a complete line. The boot
    // console TTY runs in cooked mode (ICANON), so a newline is
    // required to flush bytes into the user-visible input queue.
    // step_ingest fires both the BIF-5 readiness wire AND the
    // registered wait-carrier channel, so the next poll should
    // observe the bytes and complete.
    {
        let guard = tx_substrate::epoch::guard();
        let outcome = tx_subsystems::tty::execution::step_ingest(&console_tty, b"X\n", &guard);
        assert!(
            matches!(outcome, StepOutcome::Done(_)),
            "step_ingest should accept the bytes; got {outcome:?}"
        );
    }

    // Spin-poll a bounded number of times so a stuck future fails
    // fast rather than hanging the test runner.
    let mut result = Poll::Pending;
    for _ in 0..256 {
        result = pinned.as_mut().poll(&mut cx);
        if let Poll::Ready(value) = result {
            assert_eq!(
                value,
                SyscallResult::Return(2),
                "read should return the ingested line (X + LF)",
            );
            assert_eq!(
                &buf[..2],
                b"X\n",
                "the ingested bytes should land in the user buffer",
            );
            return;
        }
    }
    panic!("dispatch did not resolve after step_ingest woke the carrier; last poll = {result:?}");
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
    let r0 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_BRK, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r0, SyscallResult::Return(0x6000_0000));

    // brk(grow) → returns new break.
    let grow_target = 0x6000_1000u64;
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_BRK, [grow_target, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(grow_target as i64));
    assert_eq!(proc_cap.current_brk(), grow_target);

    // brk(shrink back to base) → returns base.
    let r2 = block_on(dispatch::<ShimsTestPmap>(
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
    let r = block_on(dispatch::<ShimsTestPmap>(
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
    let r1 = block_on(dispatch::<ShimsTestPmap>(
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
    let r2 = block_on(dispatch::<ShimsTestPmap>(
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

    let r = block_on(dispatch::<ShimsTestPmap>(
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
    let r1 = block_on(dispatch::<ShimsTestPmap>(
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
    let r2 = block_on(dispatch::<ShimsTestPmap>(
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

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_RT_SIGACTION, [10, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(22));
}

// ---------------------------------------------------------------------------
// Wave 2 ELF-loader plan: fcntl(F_GETFD/F_SETFD) against the per-process
// CLOEXEC bitmap (`ProcessPayload.fd_cloexec`).
// ---------------------------------------------------------------------------

/// `fcntl(fd, F_GETFD, _)` returns `0` for an fd whose CLOEXEC bit is
/// unset (the bootstrap default — `bootstrap_init_process` initialises
/// every bit to 0).
#[test]
fn dispatch_fcntl_getfd_returns_zero_for_unset_bit() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_GETFD as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `fcntl(fd, F_SETFD, FD_CLOEXEC)` then `fcntl(fd, F_GETFD, _)`
/// observes `FD_CLOEXEC` — the bitmap round-trips through the syscall
/// surface.
#[test]
fn dispatch_fcntl_setfd_then_getfd_round_trip() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // F_SETFD with FD_CLOEXEC.
    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_SETFD as u64, FD_CLOEXEC as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));
    assert!(
        proc_cap.fd_cloexec(3),
        "F_SETFD must set the per-process CLOEXEC bit"
    );

    // F_GETFD reads it back as FD_CLOEXEC.
    let get = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_GETFD as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(get, SyscallResult::Return(FD_CLOEXEC as i64));

    // F_SETFD with `arg == 0` clears the bit again (POSIX: any arg
    // value missing FD_CLOEXEC clears).
    let clear = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_SETFD as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(clear, SyscallResult::Return(0));
    assert!(!proc_cap.fd_cloexec(3));
}

/// `F_SETFD` on one fd does not perturb other fds' CLOEXEC bits.
#[test]
fn dispatch_fcntl_setfd_clears_other_bits() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Pre-mark fd 5 CLOEXEC at the API level so we can confirm
    // F_SETFD on fd 3 does not touch it.
    proc_cap.set_fd_cloexec(5, true);
    assert!(proc_cap.fd_cloexec(5));

    // Set CLOEXEC on fd 3.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, F_SETFD as u64, FD_CLOEXEC as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));

    // Both bits independent.
    assert!(proc_cap.fd_cloexec(3));
    assert!(proc_cap.fd_cloexec(5));
    assert!(!proc_cap.fd_cloexec(4));
    assert!(!proc_cap.fd_cloexec(0));
    assert!(!proc_cap.fd_cloexec(2));
}

/// Unknown `cmd` values return `-ENOSYS` per the Wave 2 plan
/// (`F_DUPFD`, `F_GETFL`, `F_SETFL`, etc. are
/// `TODO(phase-fcntl-extension)`).
#[test]
fn dispatch_fcntl_unknown_cmd_returns_neg_enosys() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // F_DUPFD = 0 is not in the Wave 2 surface.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(38));
}

/// fd outside the day-1 fixed-size fd table (`FD_TABLE_SIZE = 8`)
/// returns `-EBADF`.
#[test]
fn dispatch_fcntl_invalid_fd_returns_neg_ebadf() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // FD_TABLE_SIZE = 8: fd 8 is out of range.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [8, F_GETFD as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(9));

    // Same for F_SETFD.
    let r2 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [99, F_SETFD as u64, FD_CLOEXEC as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Error(9));
}

// ===========================================================================
// Wave 4 / Phase 6 — NR_EXECVE syscall arm.
//
// Reuses the same ScriptsTestFs / minimal ELF fixture pattern as
// `crates/tx-scripts/src/process/exec/script/tests.rs` but kept
// inline here so the test file is self-contained. Each test follows
// the same setup discipline: serialise on `SHIMS_TEST_LOCK`, reset
// the init-process slot + pid/tid counters, register a fresh tmpfs
// at `/`, then issue an `NR_EXECVE` request through `dispatch`.
//
// Doc anchors: `txdoc:EXEC-7-EIGHT-PHASES`,
// `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`.
// ===========================================================================

mod execve {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;
    use std::collections::BTreeMap;

    use tx_substrate::{page_allocator, zone, SpinMutex};
    use tx_subsystems::execution::Errno;
    use tx_subsystems::mount::{
        DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
    };
    use tx_subsystems::page_backed::{
        AnonSwapPolicy, Frame, FsPageBacking, MaterializeAccess, PageContainer, PageContainerKind,
        PageIndex,
    };
    use tx_subsystems::process::step_chdir;
    use tx_subsystems::vfs::structure::{
        Credential, DEntry, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta,
        RNode, RNodeBacking, S_IFDIR, S_IFREG,
    };
    use tx_subsystems::vfs::FsOps;
    use tx_subsystems::vm::USER_PAGE_SIZE;

    // Minimal in-test FsOps fixture mirroring tx-scripts'
    // `ExecTestFs`. We re-implement here rather than depending on
    // tx-scripts' test-only types so the surface stays self-contained.

    struct ExecveTestFs {
        inner: SpinMutex<ExecveTestFsInner>,
    }

    struct ExecveTestFsInner {
        children: BTreeMap<FsObjectId, BTreeMap<Vec<u8>, FsObjectId>>,
        inodes: BTreeMap<FsObjectId, ExecveTestInode>,
        next_id: u64,
    }

    enum ExecveTestInode {
        Directory,
        Regular {
            container: Cap<PageContainer>,
            size: u64,
        },
    }

    impl ExecveTestFs {
        fn new(root_id: FsObjectId) -> Arc<Self> {
            let mut inner = ExecveTestFsInner {
                children: BTreeMap::new(),
                inodes: BTreeMap::new(),
                next_id: root_id.as_u64() + 1,
            };
            inner.children.insert(root_id, BTreeMap::new());
            inner.inodes.insert(root_id, ExecveTestInode::Directory);
            Arc::new(Self {
                inner: SpinMutex::new(inner),
            })
        }

        fn alloc_id(&self) -> FsObjectId {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            FsObjectId::new(id)
        }

        fn add_regular_with_bytes(
            &self,
            parent: FsObjectId,
            name: &[u8],
            bytes: &[u8],
        ) -> FsObjectId {
            let pages = ((bytes.len() as u64) + USER_PAGE_SIZE as u64 - 1) / USER_PAGE_SIZE as u64;
            let pages = core::cmp::max(pages, 1);
            let pc = PageContainer::new_cap(
                PageContainerKind::Anon {
                    swap_policy: AnonSwapPolicy::Reclaimable,
                },
                pages,
            )
            .expect("page container reservation");
            for (idx, chunk) in bytes.chunks(USER_PAGE_SIZE).enumerate() {
                let materialised = pc
                    .materialize_anon(PageIndex::new(idx as u64), MaterializeAccess::Write)
                    .expect("materialize anon page for fixture");
                let frame_base =
                    page_allocator::frame_kernel_addr(materialised.ppn).expect("direct-map view");
                // SAFETY: freshly materialised anon page; the pin is
                // held via `materialised.map_pin` until the iteration
                // ends.
                unsafe {
                    core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
                }
            }
            let size = bytes.len() as u64;
            let guard = tx_substrate::epoch::guard();
            match tx_subsystems::page_backed::step_truncate(&pc, size, &guard) {
                StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
                other => panic!("step_truncate(pc, {size}) failed: {other:?}"),
            }
            drop(guard);

            let id = self.alloc_id();
            let mut inner = self.inner.lock();
            inner
                .children
                .entry(parent)
                .or_default()
                .insert(name.to_vec(), id);
            inner.inodes.insert(
                id,
                ExecveTestInode::Regular {
                    container: pc,
                    size,
                },
            );
            id
        }
    }

    impl FsOps for ExecveTestFs {
        fn lookup(
            &self,
            parent: FsObjectId,
            name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<FsObjectId> {
            let inner = self.inner.lock();
            let Some(map) = inner.children.get(&parent) else {
                return StepOutcome::Err(Errno::ENOTDIR);
            };
            match map.get(name) {
                Some(id) => StepOutcome::Done(*id),
                None => StepOutcome::Err(Errno::ENOENT),
            }
        }

        fn load_inode_meta(
            &self,
            fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<InodeMeta> {
            let inner = self.inner.lock();
            let Some(inode) = inner.inodes.get(&fs_object_id) else {
                return StepOutcome::Err(Errno::ENOENT);
            };
            let meta = match inode {
                ExecveTestInode::Directory => InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
                ExecveTestInode::Regular { size, .. } => {
                    let mut meta = InodeMeta::new(InodeKind::Regular, S_IFREG | 0o755);
                    meta.size = *size;
                    meta
                }
            };
            StepOutcome::Done(meta)
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta)> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta)> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta)> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
            StepOutcome::Done(None)
        }

        fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn read_link(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Box<[u8]>> {
            StepOutcome::Err(Errno::EINVAL)
        }

        fn materialise_rnode(
            &self,
            fs_object_id: FsObjectId,
            meta: InodeMeta,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Cap<RNode>> {
            let inner = self.inner.lock();
            let Some(inode) = inner.inodes.get(&fs_object_id) else {
                return StepOutcome::Err(Errno::ENOENT);
            };
            match inode {
                ExecveTestInode::Regular { container, .. } => {
                    match RNode::new_cap(
                        fs_object_id,
                        meta,
                        RNodeBacking::PageBacked {
                            pc: container.clone(),
                        },
                    ) {
                        Ok(rnode) => StepOutcome::Done(rnode),
                        Err(_) => StepOutcome::Err(Errno::ENOMEM),
                    }
                }
                ExecveTestInode::Directory => StepOutcome::Err(Errno::EISDIR),
            }
        }
    }

    impl FsPageBacking for ExecveTestFs {
        fn fetch_page(
            &self,
            fs_object_id: FsObjectId,
            offset: u64,
            guard: &Guard<'_>,
        ) -> StepOutcome<Frame> {
            let inner = self.inner.lock();
            let container = match inner.inodes.get(&fs_object_id) {
                Some(ExecveTestInode::Regular { container, .. }) => container.clone(),
                Some(ExecveTestInode::Directory) => return StepOutcome::Err(Errno::EISDIR),
                None => return StepOutcome::Err(Errno::ENOENT),
            };
            drop(inner);

            let page_size = USER_PAGE_SIZE as u64;
            if offset % page_size != 0 {
                return StepOutcome::Err(Errno::EINVAL);
            }
            let page_index = PageIndex::new(offset / page_size);
            match container.materialize_page(page_index, MaterializeAccess::Read, guard) {
                StepOutcome::Done(materialised) => StepOutcome::Done(Frame::new(materialised.ppn)),
                StepOutcome::Advanced(materialised) => {
                    StepOutcome::Advanced(Frame::new(materialised.ppn))
                }
                StepOutcome::AdvancedThenBlocked(materialised, token) => {
                    StepOutcome::AdvancedThenBlocked(Frame::new(materialised.ppn), token)
                }
                StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
                StepOutcome::Err(errno) => StepOutcome::Err(errno),
            }
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }
    }

    // ----- Minimal RV64 ET_EXEC ELF fixture (mirrors
    //       tx-scripts' `minimal_elf_bytes`). -----

    const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const EV_CURRENT: u8 = 1;
    const ET_EXEC: u16 = 2;
    const EM_RISCV: u16 = 243;
    const PT_LOAD: u32 = 1;
    const PT_PHDR: u32 = 6;
    const PF_R: u32 = 4;
    const PF_X: u32 = 1;

    const FIX_PAGE: u64 = 4096;
    const BASE_LOAD_VADDR: u64 = 0x1_0000;
    const ENTRY_OFFSET: u64 = 0x80;

    fn minimal_elf_bytes() -> Vec<u8> {
        fn write_u16(b: &mut [u8], at: usize, v: u16) {
            b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        }
        fn write_u32(b: &mut [u8], at: usize, v: u32) {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn write_u64(b: &mut [u8], at: usize, v: u64) {
            b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        }
        fn write_phdr(
            b: &mut [u8],
            at: usize,
            p_type: u32,
            p_flags: u32,
            p_offset: u64,
            p_vaddr: u64,
            p_filesz: u64,
            p_memsz: u64,
            p_align: u64,
        ) {
            write_u32(b, at, p_type);
            write_u32(b, at + 4, p_flags);
            write_u64(b, at + 8, p_offset);
            write_u64(b, at + 16, p_vaddr);
            write_u64(b, at + 24, p_vaddr);
            write_u64(b, at + 32, p_filesz);
            write_u64(b, at + 40, p_memsz);
            write_u64(b, at + 48, p_align);
        }

        let phoff: u64 = 64;
        let n_phdrs: u16 = 2;
        let phent: u16 = 56;
        let total_phdrs = (n_phdrs as u64) * (phent as u64);
        let file_size: usize = (phoff + total_phdrs) as usize;
        let mut bytes = vec![0u8; file_size];

        bytes[0..4].copy_from_slice(&ELF_MAGIC);
        bytes[4] = ELFCLASS64;
        bytes[5] = ELFDATA2LSB;
        bytes[6] = EV_CURRENT;
        write_u16(&mut bytes, 16, ET_EXEC);
        write_u16(&mut bytes, 18, EM_RISCV);
        write_u32(&mut bytes, 20, 1);
        write_u64(&mut bytes, 24, BASE_LOAD_VADDR + ENTRY_OFFSET);
        write_u64(&mut bytes, 32, phoff);
        write_u64(&mut bytes, 40, 0);
        write_u32(&mut bytes, 48, 0);
        write_u16(&mut bytes, 52, 64);
        write_u16(&mut bytes, 54, phent);
        write_u16(&mut bytes, 56, n_phdrs);
        write_u16(&mut bytes, 58, 0);
        write_u16(&mut bytes, 60, 0);
        write_u16(&mut bytes, 62, 0);

        let pt_phdr_vaddr = BASE_LOAD_VADDR + phoff;
        write_phdr(
            &mut bytes,
            phoff as usize,
            PT_PHDR,
            PF_R,
            phoff,
            pt_phdr_vaddr,
            total_phdrs,
            total_phdrs,
            8,
        );

        write_phdr(
            &mut bytes,
            (phoff + 56) as usize,
            PT_LOAD,
            PF_R | PF_X,
            0,
            BASE_LOAD_VADDR,
            file_size as u64,
            file_size as u64,
            FIX_PAGE,
        );

        bytes
    }

    fn ensure_zero_frame_claimed() {
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for execve tests: {error:?}"),
        }
    }

    fn build_fs_root() -> (Cap<DEntry>, Arc<ExecveTestFs>) {
        let root_id = FsObjectId::new(2);
        let fs = ExecveTestFs::new(root_id);

        let payload = MountPayload::new_cap(
            fs.clone() as Arc<dyn FsOps>,
            fs.clone() as Arc<dyn FsPageBacking>,
            None,
            DevId::new(99),
            MountOptions::default(),
            "execve-test-fs",
            SourceLabel::Static("execve-test"),
        )
        .expect("mount payload");

        let root_rnode = {
            let raw = RNode::new(
                root_id,
                InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&payload);
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };

        let _mount = MountIdentity::new_cap(
            MountId::new(1),
            None,
            root_rnode.clone(),
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount identity");

        let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
        (root_dentry, fs)
    }

    fn bootstrap_with_file(
        name: &[u8],
        bytes: &[u8],
    ) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>, Arc<ExecveTestFs>) {
        let (root_dentry, fs) = build_fs_root();
        let _ = fs.add_regular_with_bytes(FsObjectId::new(2), name, bytes);

        let aspace = fresh_aspace();
        let process = bootstrap_init_process(aspace).expect("bootstrap init");
        let thread = process.nth_thread(0).expect("leader thread");

        match step_chdir(&process, root_dentry) {
            tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
            tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
                panic!("init bootstrap somehow zombified")
            }
        }

        (process, thread, fs)
    }

    fn execve_setup() -> TestSetup {
        let setup = setup();
        ensure_zero_frame_claimed();
        setup
    }

    /// `execve("/nope", NULL, NULL)` against an empty fs returns
    /// `-ENOENT` per the standard `ExecError::PathNotFound` mapping.
    /// Verifies the syscall arm walks the path through the VFS walker
    /// and surfaces the expected errno magnitude (positive 2 = ENOENT).
    #[test]
    fn dispatch_execve_path_not_found_returns_neg_enoent() {
        let _setup = execve_setup();

        let bytes = minimal_elf_bytes();
        let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
        let ctx = make_ctx(process, thread);

        // path = "/nope" — present in fs but registered as `init`.
        let path: &[u8] = b"/nope\0";
        let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(2));
    }

    /// `execve` of a non-ELF file returns `-ENOEXEC` (positive 8) via
    /// `ExecError::NotExecutable → -8`.
    #[test]
    fn dispatch_execve_invalid_elf_returns_neg_enoexec() {
        let _setup = execve_setup();

        let bytes = vec![0u8; 4096]; // 4 KiB of zeroes — fails magic check.
        let (process, thread, _fs) = bootstrap_with_file(b"bad", &bytes);
        let ctx = make_ctx(process, thread);

        let path: &[u8] = b"/bad\0";
        let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(8));
    }

    /// A path with no NUL terminator within `EXECVE_PATH_MAX = 4096`
    /// returns `-ENAMETOOLONG` (positive 36) — the bounded user-buffer
    /// copy short-circuits before any walker call.
    #[test]
    fn dispatch_execve_too_long_path_returns_neg_enametoolong() {
        let _setup = execve_setup();

        let bytes = minimal_elf_bytes();
        let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
        let ctx = make_ctx(process, thread);

        // Allocate a kernel-side buffer of 4097 'A' bytes, no NUL.
        // `read_user_cstr` walks 4096 bytes and gives up.
        let path: Vec<u8> = vec![b'A'; super::super::EXECVE_PATH_MAX + 1];
        let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(36));
        // Keep the buffer alive until after the call.
        drop(path);
    }

    /// argv whose total byte budget exceeds `EXECVE_ARG_MAX_INLINE =
    /// 8192` returns `-E2BIG` (positive 7) — the per-string read
    /// helper accumulates against a shared budget.
    #[test]
    fn dispatch_execve_argv_overflow_returns_neg_e2big() {
        let _setup = execve_setup();

        let bytes = minimal_elf_bytes();
        let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
        let ctx = make_ctx(process, thread);

        // Build two argv strings whose combined length (including
        // implicit NUL accounting) exceeds 8 KiB. Each is 5 KiB +
        // trailing NUL.
        let big_arg: Vec<u8> = {
            let mut v = vec![b'X'; 5 * 1024];
            v.push(0);
            v
        };
        // argv array: [&big_arg, &big_arg, NULL]
        let argv_array: [u64; 3] = [big_arg.as_ptr() as u64, big_arg.as_ptr() as u64, 0];

        let path: &[u8] = b"/init\0";
        let req = SyscallRequest::new(
            NR_EXECVE,
            [path.as_ptr() as u64, argv_array.as_ptr() as u64, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(7));
        drop(big_arg);
    }

    /// Successful execve of a minimal ELF returns
    /// `SyscallResult::ExecCommitted` AND swaps the process's
    /// `aspace_cap` AND seeds the thread's `saved_user_context` with
    /// the new image's `pc` / `sp`.
    ///
    /// Asserts the contract Phase 6 introduces: the dispatcher signals
    /// "do not write a syscall return; the new image's `_start` runs
    /// next".
    #[test]
    fn dispatch_execve_success_returns_exec_committed() {
        let _setup = execve_setup();

        let bytes = minimal_elf_bytes();
        let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

        let aspace_before_key = process.aspace_cap().expect("alive aspace").key();

        let ctx = make_ctx(process.clone(), thread.clone());

        let path: &[u8] = b"/init\0";
        let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

        assert_eq!(
            result,
            SyscallResult::ExecCommitted,
            "successful execve must surface ExecCommitted to the thread future"
        );

        // Phase 6 swap observable.
        let aspace_after = process.aspace_cap().expect("alive aspace post-exec");
        assert_ne!(
            aspace_before_key,
            aspace_after.key(),
            "aspace must have been atomically replaced"
        );

        // Saved user context populated.
        let payload = thread.payload_cap().expect("alive thread payload");
        let user_ctx = payload
            .saved_user_context()
            .expect("Phase 6 must seed saved_user_context");
        assert_eq!(
            user_ctx.pc as u64,
            BASE_LOAD_VADDR + ENTRY_OFFSET,
            "saved_user_context.pc must match the ELF entry"
        );
        // RV64 SP lives at register x2.
        assert_ne!(user_ctx.regs[2], 0, "initial sp must be populated");
        assert_eq!(
            user_ctx.regs[2] & 0xF,
            0,
            "initial sp must be 16-byte aligned"
        );
    }
}

// ===========================================================================
// Wave 2 of the fork/clone/wait4 slice — Part 2 (NR_CLONE) +
// Part 4 (5 + 1 introspection arms) + Part 5 (musl-startup stubs).
//
// NR_WAIT4 belongs to Wave 3 with the blocking-wait scaffolding and is
// **not** tested here.
// ===========================================================================

mod fork_clone_wait4_wave2 {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tx_hal::UserTrapContext;
    use tx_subsystems::process::{Pgid, Pid};
    use tx_subsystems::reactor_submit;

    // -----------------------------------------------------------------------
    // Reactor-submission test capture.
    //
    // The `submit_child_thread` seam is a process-static `AtomicPtr`-backed
    // function pointer. Tests install a no-op capture, run `sys_clone`,
    // then verify the seam was hit. We can't capture the exact (process,
    // thread) caps through a plain `fn` pointer (no captured state), so
    // we count invocations and snapshot the most-recent submission via
    // a process-static pair of AtomicUsize key holders.
    // -----------------------------------------------------------------------

    static SUBMIT_CHILD_THREAD_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SUBMIT_CHILD_PROC_KEY: AtomicUsize = AtomicUsize::new(0);
    static SUBMIT_CHILD_THREAD_KEY: AtomicUsize = AtomicUsize::new(0);

    fn capturing_submit(child_process: Cap<ProcessIdentity>, child_thread: Cap<ThreadIdentity>) {
        SUBMIT_CHILD_THREAD_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
        SUBMIT_CHILD_PROC_KEY.store(child_process.key().raw() as usize, AtomicOrdering::SeqCst);
        SUBMIT_CHILD_THREAD_KEY.store(child_thread.key().raw() as usize, AtomicOrdering::SeqCst);
    }

    fn install_capturing_seam_and_reset() {
        SUBMIT_CHILD_THREAD_CALLS.store(0, AtomicOrdering::SeqCst);
        SUBMIT_CHILD_PROC_KEY.store(0, AtomicOrdering::SeqCst);
        SUBMIT_CHILD_THREAD_KEY.store(0, AtomicOrdering::SeqCst);
        reactor_submit::install_submit_child_thread(capturing_submit);
    }

    /// Synthesise a parent `UserTrapContext` and stamp it onto the
    /// calling thread's payload so `sys_clone` finds something to copy.
    /// Returns the stamped context for assertion.
    fn seed_parent_trap_context(thread: &Cap<ThreadIdentity>) -> UserTrapContext {
        let mut regs = [0usize; 32];
        for (i, slot) in regs.iter_mut().enumerate() {
            *slot = 0x2000 + i;
        }
        // a0 (regs[10]) and a7 (regs[17]) carry the syscall number /
        // first arg before trap entry — they're whatever the parent
        // passed; the seed must zero a0 in the child.
        regs[10] = 0xdead_beef;
        let parent_ctx = UserTrapContext {
            regs,
            pc: 0x4000_2000,
            status: 0x123,
        };
        thread
            .payload_cap()
            .expect("alive thread payload")
            .store_saved_user_context(Some(parent_ctx));
        parent_ctx
    }

    // ----------------- Part 2: NR_CLONE ------------------------------

    /// `clone(SIGCHLD, 0, 0, 0, 0)` returns the child's pid (a positive
    /// number, distinct from the caller's pid).
    #[test]
    fn dispatch_clone_bare_sigchld_returns_child_pid() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _parent_ctx = seed_parent_trap_context(&thread);

        let parent_pid = proc_cap.pid;
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

        let child_pid = match result {
            SyscallResult::Return(v) => v,
            other => panic!("expected Return(child_pid), got {other:?}"),
        };
        assert!(child_pid > 0, "child pid must be positive, got {child_pid}");
        assert_ne!(
            child_pid as u32, parent_pid.0,
            "child must have a different pid from the parent"
        );
        // step_fork registers the child in the parent's children list.
        assert_eq!(
            proc_cap.children().len(),
            1,
            "step_fork must add the child to parent.children"
        );
    }

    /// flags = `SIGCHLD | CLONE_VM` (0x100) → -EINVAL. The slice
    /// rejects every flag combo other than bare `SIGCHLD`.
    #[test]
    fn dispatch_clone_with_clone_vm_flag_returns_neg_einval() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap, thread);

        const CLONE_VM: u64 = 0x100;
        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD | CLONE_VM, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(22));
    }

    /// flags = bare SIGCHLD, stack = `0x4000_0000` → -EINVAL. Non-zero
    /// stack is the posix_spawn / pthread_create path, deferred.
    #[test]
    fn dispatch_clone_with_nonzero_stack_returns_neg_einval() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0x4000_0000, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(22));
    }

    /// flags = 0 (no SIGCHLD, no CLONE_*) → -EINVAL. Bare-SIGCHLD only.
    #[test]
    fn dispatch_clone_with_zero_flags_returns_neg_einval() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_CLONE, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(22));
    }

    /// After a successful `clone(SIGCHLD, 0)`, the child's leader thread
    /// has `saved_user_context.regs[10] == 0` and `pc == parent.pc + 4`,
    /// matching the RV64 fork-clone ABI shape.
    #[test]
    fn dispatch_clone_seeds_child_a0_to_zero_and_pc_after_ecall() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let parent_ctx = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert!(matches!(result, SyscallResult::Return(v) if v > 0));

        // Inspect the child's leader thread saved context.
        let children = proc_cap.children();
        assert_eq!(children.len(), 1);
        let child = children[0].clone();
        let child_leader = child.nth_thread(0).expect("child has leader thread");
        let saved = child_leader
            .payload_cap()
            .expect("alive child leader payload")
            .saved_user_context()
            .expect("seed_child_leader_context installed Some");
        assert_eq!(
            saved.regs[10], 0,
            "RV64 a0 (regs[10]) must be 0 in the child — Linux fork-clone ABI"
        );
        assert_eq!(
            saved.pc,
            parent_ctx.pc + 4,
            "child's pc must skip past the trapping ecall (4 bytes on RV64)"
        );
        // Other GPRs preserved.
        for i in 0..32 {
            if i == 10 {
                continue;
            }
            assert_eq!(
                saved.regs[i], parent_ctx.regs[i],
                "regs[{i}] must match parent (only a0 is rewritten)"
            );
        }
        assert_eq!(saved.status, parent_ctx.status, "status preserved");
    }

    /// `sys_clone` calls through `reactor_submit::submit_child_thread`
    /// with the fresh (process, thread) caps. The test installs a
    /// capturing seam and verifies the call counter ticks AND the
    /// captured cap keys match what `step_fork`'s output would have
    /// been (the child cap exposed via `parent.children()` and its
    /// leader thread).
    #[test]
    fn dispatch_clone_submits_child_via_reactor_seam() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert!(matches!(result, SyscallResult::Return(v) if v > 0));

        // Seam was hit exactly once.
        assert_eq!(SUBMIT_CHILD_THREAD_CALLS.load(AtomicOrdering::SeqCst), 1);

        // Captured (process_key, thread_key) match the child + child's
        // leader thread visible from parent.children().
        let children = proc_cap.children();
        assert_eq!(children.len(), 1);
        let child = children[0].clone();
        let child_leader = child.nth_thread(0).expect("child has leader");
        assert_eq!(
            SUBMIT_CHILD_PROC_KEY.load(AtomicOrdering::SeqCst),
            child.key().raw() as usize,
            "captured process cap must be the freshly forked child"
        );
        assert_eq!(
            SUBMIT_CHILD_THREAD_KEY.load(AtomicOrdering::SeqCst),
            child_leader.key().raw() as usize,
            "captured thread cap must be the child's leader thread"
        );
    }

    // ----------------- Part 4: introspection arms --------------------

    /// `getppid()` from init returns `0` (`Pid::RESERVED`) — init has
    /// no parent. Real Linux returns init's pid for orphans; the
    /// trio's `parent_pid` accessor returns `Pid::RESERVED` (0) in this
    /// pre-init bootstrap edge.
    #[test]
    fn dispatch_getppid_returns_init_parent_pid_zero() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETPPID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// After a successful `clone(SIGCHLD, 0)`, the child's `getppid()`
    /// returns the parent's pid.
    #[test]
    fn dispatch_getppid_returns_real_parent_pid_after_clone() {
        let _setup = setup();
        install_capturing_seam_and_reset();

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let parent_pid = proc_cap.pid;
        let ctx_parent = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(req, &ctx_parent));

        let children = proc_cap.children();
        let child = children[0].clone();
        let child_leader = child.nth_thread(0).expect("child has leader");

        let ctx_child = make_ctx(child, child_leader);
        let req = SyscallRequest::new(NR_GETPPID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx_child));
        assert_eq!(result, SyscallResult::Return(parent_pid.0 as i64));
    }

    /// `setpgid(0, 0)` on self creates a fresh process group rooted at
    /// the caller's pid. Returns 0; the caller's pgid changes to its pid.
    #[test]
    fn dispatch_setpgid_self_zero_returns_zero() {
        let _setup = setup();
        // Bootstrap init starts in pgid == pid == 1 already, so we
        // need a non-init process to observe a state change. Fork once
        // first; the child inherits init's pgrp (pgid == 1), then
        // setpgid(0, 0) on the child creates a fresh pgrp at child's pid.
        install_capturing_seam_and_reset();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let children = proc_cap.children();
        let child = children[0].clone();
        let child_leader = child.nth_thread(0).expect("child has leader");
        // Before: child inherits parent's pgid (== 1).
        assert_eq!(child.pgrp_cap().pgid, Pgid(Pid::INIT.0));

        let ctx_child = make_ctx(child.clone(), child_leader);
        // setpgid(0, 0) means "self, use self.pid".
        let req = SyscallRequest::new(NR_SETPGID, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx_child));
        assert_eq!(result, SyscallResult::Return(0));
        assert_eq!(
            child.pgrp_cap().pgid,
            Pgid(child.pid.0),
            "setpgid(0,0) must create a fresh pgrp rooted at child's pid"
        );
    }

    /// Cross-process `setpgid(target, 0)` returns `-EPERM` — day-1 only
    /// supports self-pid.
    #[test]
    fn dispatch_setpgid_cross_process_returns_neg_eperm() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        // pid = 999 (non-self). EPERM.
        let req = SyscallRequest::new(NR_SETPGID, [999, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(1));
    }

    /// `getpgid(0)` returns the caller's pgid.
    #[test]
    fn dispatch_getpgid_self_returns_own_pgid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let expected_pgid = proc_cap.pgrp_cap().pgid.0 as i64;
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETPGID, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(expected_pgid));
    }

    /// `getpgrp()` returns `-ENOSYS`. musl uses `getpgid(0)` instead;
    /// the constant is defined for grep-stability only.
    #[test]
    fn dispatch_getpgrp_returns_neg_enosys() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETPGRP, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(38));
    }

    /// `getsid(0)` returns the caller's session id.
    #[test]
    fn dispatch_getsid_self_returns_own_sid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let expected_sid = proc_cap.pgrp_cap().session_cap().sid.0 as i64;
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETSID, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(expected_sid));
    }

    /// `setsid()` creates a fresh session rooted at the caller's pid.
    /// Returns the new sid (== caller's pid).
    #[test]
    fn dispatch_setsid_returns_new_sid() {
        let _setup = setup();
        // Bootstrap init starts as its own session leader (sid == pid == 1)
        // so we need to fork first to observe a real session change.
        install_capturing_seam_and_reset();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let _ = seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let children = proc_cap.children();
        let child = children[0].clone();
        let child_pid = child.pid.0;
        let child_leader = child.nth_thread(0).expect("child leader");
        let ctx_child = make_ctx(child.clone(), child_leader);

        let req = SyscallRequest::new(NR_SETSID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx_child));
        assert_eq!(
            result,
            SyscallResult::Return(child_pid as i64),
            "setsid returns the new session id, == caller's pid"
        );
        assert_eq!(
            child.pgrp_cap().session_cap().sid.0,
            child_pid,
            "child must now lead its own session"
        );
    }

    // ----------------- Part 5: musl-startup stubs --------------------

    /// `set_tid_address(_)` returns the calling thread's tid.
    #[test]
    fn dispatch_set_tid_address_returns_thread_tid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let expected_tid = thread.tid.0 as i64;
        let ctx = make_ctx(proc_cap, thread);

        // Pass an arbitrary non-zero pointer to verify it's ignored.
        let req = SyscallRequest::new(NR_SET_TID_ADDRESS, [0xdead_beef, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(expected_tid));
    }

    /// `set_robust_list(_, _)` returns 0 unconditionally (stub).
    #[test]
    fn dispatch_set_robust_list_returns_zero() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_SET_ROBUST_LIST, [0xdead_beef, 24, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }
}

// ===========================================================================
// Wave 3 of the fork/clone/wait4 slice — Part 3 (NR_WAIT4 syscall arm
// with blocking-wait via the per-process `exit_port` carrier).
// ===========================================================================

mod fork_clone_wait4_wave3 {
    use super::*;

    use tx_hal::UserTrapContext;
    use tx_subsystems::process::{step_exit_group, ExitStatus};
    use tx_subsystems::reactor_submit;

    /// No-op reactor-submit seam for tests that fork via `sys_clone`.
    /// The blocking-wait tests just need the seam to not panic; they
    /// don't poll the child, so a trivial seam is enough.
    fn install_noop_submit_seam() {
        fn noop(_p: Cap<ProcessIdentity>, _t: Cap<ThreadIdentity>) {}
        reactor_submit::install_submit_child_thread(noop);
    }

    /// Stamp a parent trap context so `sys_clone` finds the saved
    /// context. Mirrors the Wave 2 helper.
    fn seed_parent_trap_context(thread: &Cap<ThreadIdentity>) {
        let regs = [0usize; 32];
        let parent_ctx = UserTrapContext {
            regs,
            pc: 0x4000_2000,
            status: 0x123,
        };
        thread
            .payload_cap()
            .expect("alive thread payload")
            .store_saved_user_context(Some(parent_ctx));
    }

    /// `wait4(-1, NULL, 0, NULL)` from a process with no children
    /// returns `-ECHILD`.
    #[test]
    fn dispatch_wait4_no_children_returns_neg_echild() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        // pid = -1 (Any), wstatus = NULL, options = 0, rusage = NULL.
        let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(10), "expected -ECHILD");
    }

    /// `wait4(-1, NULL, WNOHANG, NULL)` with a live (non-zombie) child
    /// returns `0` (no zombie ready). The child is still alive after
    /// the call.
    #[test]
    fn dispatch_wait4_wnohang_no_zombies_returns_zero() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Fork once.
        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
        assert_eq!(proc_cap.children().len(), 1);
        let child = proc_cap.children()[0].clone();
        assert!(!child.is_zombie());

        // wait4(-1, NULL, WNOHANG, NULL) → Return(0).
        let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, WNOHANG as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));

        // Child is still alive (not reaped).
        assert!(!child.is_zombie());
        assert_eq!(proc_cap.children().len(), 1);
    }

    /// `wait4(-1, NULL, WNOHANG, NULL)` after the child zombifies
    /// returns the child's pid and reaps the zombie (parent.children
    /// shrinks).
    #[test]
    fn dispatch_wait4_wnohang_zombie_ready_reaps_and_returns_pid() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
        let child = proc_cap.children()[0].clone();
        let child_pid = child.pid.0 as i64;

        // Zombify the child via step_exit_group.
        step_exit_group(&child, ExitStatus::Exited(0));
        assert!(child.is_zombie());

        let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, WNOHANG as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(child_pid));

        // Child has been reaped: parent.children no longer contains it.
        assert_eq!(
            proc_cap.children().len(),
            0,
            "wait4 must reap the zombie out of parent.children"
        );
    }

    /// `wait4(-1, &mut ws, WNOHANG, NULL)` with `ExitStatus::Exited(42)`
    /// writes `0x2a00` to the user wstatus address per the POSIX
    /// `<sys/wait.h>` encoding (`(code & 0xff) << 8`).
    #[test]
    fn dispatch_wait4_wnohang_writes_status_word_for_exited() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
        let child = proc_cap.children()[0].clone();

        step_exit_group(&child, ExitStatus::Exited(42));

        let mut wstatus: i32 = -1;
        let wstatus_addr = &mut wstatus as *mut i32 as u64;
        let req = SyscallRequest::new(
            NR_WAIT4,
            [(-1i64) as u64, wstatus_addr, WNOHANG as u64, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert!(matches!(result, SyscallResult::Return(_)));

        // POSIX: Exited(42) → (42 & 0xff) << 8 = 0x2a00.
        assert_eq!(
            wstatus, 0x2a00,
            "wstatus must encode Exited(42) as (42 << 8) per <sys/wait.h>"
        );
    }

    /// `wait4(child_a.pid, NULL, WNOHANG, NULL)` skips zombies that
    /// are not the requested pid. Two-fork shape: A is alive, B is
    /// zombie; selector targets A → Return(0). Then zombify A and call
    /// again → Return(A.pid).
    #[test]
    fn dispatch_wait4_specific_pid_skips_other_zombies() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Fork A.
        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
        let child_a = proc_cap.children()[0].clone();
        let child_a_pid = child_a.pid.0 as i64;

        // Fork B (re-seed the parent ctx because step_fork doesn't
        // touch saved_user_context).
        seed_parent_trap_context(&first_thread(&proc_cap));
        let _ = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]),
            &ctx,
        ));
        let children = proc_cap.children();
        assert_eq!(children.len(), 2);
        let child_b = children
            .iter()
            .find(|c| c.pid.0 != child_a.pid.0)
            .expect("two distinct children")
            .clone();

        // Zombify B; A still alive.
        step_exit_group(&child_b, ExitStatus::Exited(0));
        assert!(child_b.is_zombie());
        assert!(!child_a.is_zombie());

        // wait4(A.pid, ...) WNOHANG: A is alive, so no zombie matches
        // the selector → Return(0). B's zombie state must NOT satisfy
        // a Pid(A.pid) selector.
        let req = SyscallRequest::new(NR_WAIT4, [child_a_pid as u64, 0, WNOHANG as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(
            result,
            SyscallResult::Return(0),
            "wait4(A.pid) must not reap zombie B"
        );

        // Now zombify A; call wait4(A.pid, ...) again → Return(A.pid).
        step_exit_group(&child_a, ExitStatus::Exited(0));
        let req = SyscallRequest::new(NR_WAIT4, [child_a_pid as u64, 0, WNOHANG as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(child_a_pid));
    }

    /// Blocking wait4 — load-bearing test. Fork a child; spawn a host
    /// driver future that polls `sys_wait4(-1, NULL, 0, NULL)`. First
    /// poll → Pending (no zombie). Manually call `step_exit_group` on
    /// the child (which routes through `post_sigchld_to_parent` and
    /// fires the parent's `exit_port` channel). Subsequent polls →
    /// Ready with the child's pid.
    #[test]
    fn dispatch_wait4_blocking_resolves_when_child_zombifies() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
        let child = proc_cap.children()[0].clone();
        let child_pid_i64 = child.pid.0 as i64;
        assert!(!child.is_zombie());

        // Drive the wait4 future manually so we can interleave the
        // child's exit between polls. Same shape as
        // `dispatch_read_blocks_until_tty_input_then_returns_byte`.
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);

        // Blocking variant — options = 0 (no WNOHANG).
        let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, 0, 0, 0, 0]);
        let fut = dispatch::<ShimsTestPmap>(req, &ctx);
        let mut pinned = Box::pin(fut);

        // First poll: no zombie ready, parks on exit_port via
        // wait_carrier::wait_on_token.
        let first = pinned.as_mut().poll(&mut cx);
        assert!(
            matches!(first, Poll::Pending),
            "blocking wait4 with no ready zombie should park; got {first:?}"
        );

        // Now zombify the child. step_exit_group →
        // post_sigchld_to_parent → parent.fire_exit_port(...) — fires
        // the EXIT_PORT_CHILD_ZOMBIFIED bit on the parent's channel,
        // which wakes the WaitFuture.
        step_exit_group(&child, ExitStatus::Exited(0));
        assert!(child.is_zombie());

        // Spin-poll a bounded number of times so a stuck future fails
        // fast rather than hanging the test.
        let mut last = Poll::Pending;
        for _ in 0..256 {
            last = pinned.as_mut().poll(&mut cx);
            if let Poll::Ready(value) = last {
                assert_eq!(
                    value,
                    SyscallResult::Return(child_pid_i64),
                    "wait4 should observe the now-zombie child"
                );
                // The reap retired the child from parent.children.
                assert_eq!(proc_cap.children().len(), 0);
                return;
            }
        }
        panic!("dispatch_wait4 did not resolve after child zombified; last poll = {last:?}");
    }

    /// Non-NULL `rusage` argument is rejected with `-EINVAL`. Wave 3
    /// doesn't track rusage; future LTP tests that pass an rusage
    /// pointer will need zero-fill or real population (deferred).
    #[test]
    fn dispatch_wait4_rusage_nonzero_returns_neg_einval() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, 0, 0xDEAD, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(22), "expected -EINVAL");
    }

    /// `WUNTRACED` (0x2) and `WCONTINUED` (0x8) are accepted but
    /// silently ignored — Linux ignores unknown options bits for
    /// `wait4`. Verifies the arm doesn't fail with `-EINVAL` on
    /// these bits; with no zombie ready and no `WNOHANG`, the call
    /// would block, so we add `WNOHANG` to keep the test bounded.
    #[test]
    fn dispatch_wait4_unknown_options_bits_ignored() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Need a child so the call returns Return(0) (no ready zombie)
        // rather than -ECHILD (no children).
        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));

        const WUNTRACED: i32 = 0x2;
        const WCONTINUED: i32 = 0x8;
        let options = WNOHANG | WUNTRACED | WCONTINUED;
        let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, options as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        // WNOHANG + no zombie ready → Return(0). The crucial
        // assertion is "not -EINVAL" — the unknown bits were tolerated.
        assert_eq!(
            result,
            SyscallResult::Return(0),
            "WUNTRACED/WCONTINUED must be silently ignored, not rejected"
        );
    }

    /// `wait4(i32::MIN, ...)` overflows on negate (the `WaitTarget`
    /// translation would compute `Pgrp(-i32::MIN as u32)` which is
    /// undefined behaviour); we reject upfront with `-EINVAL` per
    /// LTP `wait403`.
    #[test]
    fn dispatch_wait4_intmin_pid_returns_neg_einval() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_WAIT4, [(i32::MIN as i64) as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(22), "expected -EINVAL");
    }

    /// `wait4(-pgid, NULL, WNOHANG, NULL)` matches a zombie child
    /// whose pgid equals `pgid`. By default a fork inherits the
    /// parent's pgrp, so the child's pgid == init's pgid == 1; we
    /// pass `pid = -1` (which would catch any child) plus a separate
    /// run with `pid = -(child.pgid)` to verify the pgrp selector
    /// path.
    #[test]
    fn dispatch_wait4_pgid_selector_picks_grouped_zombie() {
        let _setup = setup();
        install_noop_submit_seam();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        seed_parent_trap_context(&thread);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
        let child = proc_cap.children()[0].clone();
        let child_pid_i64 = child.pid.0 as i64;
        let child_pgid = child.pgrp_cap().pgid.0 as i64;

        step_exit_group(&child, ExitStatus::Exited(0));
        assert!(child.is_zombie());

        // pid = -(pgid). The selector becomes Pgrp(child_pgid). Since
        // the child's pgid == child_pgid (inherited from parent), this
        // matches.
        let neg_pgid = -child_pgid;
        let req = SyscallRequest::new(NR_WAIT4, [neg_pgid as u64, 0, WNOHANG as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(
            result,
            SyscallResult::Return(child_pid_i64),
            "wait4(-pgid, ...) must reap a zombie in the target pgrp"
        );
    }
}
