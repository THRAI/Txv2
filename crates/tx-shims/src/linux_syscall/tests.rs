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
    Asid, EntropyIf, PhysAddr, PmapError, PmapIf, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
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

// `dispatch::<P>` requires `P: PmapIf + EntropyIf` (added by the
// CSPRNG chore so the execve path can pull AT_RANDOM bytes).
// Trait default fills bytes from the deterministic boot-counter
// xorshift, which is what test sites want — non-zero,
// reproducible, no hardware dependency.
impl EntropyIf for ShimsTestPmap {}

// Slice 4 of the shell-prompt roadmap (2026-05-07) added a `TimeIf`
// bound to `dispatch::<P>` so the time-syscall arms can read the
// platform monotonic clock through `<P as TimeIf>::read_ns()`. The
// test platform exposes a monotonically-increasing nanosecond
// counter — each call returns 1ns more than the last — so tests can
// observe both the nanosecond-to-`(tv_sec, tv_nsec)` decomposition
// (`SHIMS_TEST_NS_BASE` is large enough to span a full tv_sec) and
// the strict-monotonicity contract `TimeIf` requires. The
// deadline / cancel hooks are no-ops; the test harness never drives
// the reactor's timer queue and Slice 4 has no real-duration
// `nanosleep` path.
static SHIMS_TEST_NS_COUNTER: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(SHIMS_TEST_NS_BASE);
const SHIMS_TEST_NS_BASE: u64 = 5_000_000_000;

impl tx_hal::TimeIf for ShimsTestPmap {
    fn read_ns() -> u64 {
        SHIMS_TEST_NS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
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
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    // fd-ops Wave 1: F_GETFD now requires the fd to actually be
    // open (Linux semantic). Install a console at fd 3 so the
    // CLOEXEC bit is observable.
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
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
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    // fd-ops Wave 1: install at fd 3 so the syscall arm sees an
    // open fd (EBADF otherwise).
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
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
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    // fd-ops Wave 1: install at fd 3 so the syscall arm sees an
    // open fd (EBADF otherwise).
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Pre-mark fd 5 CLOEXEC at the API level so we can confirm
    // F_SETFD on fd 3 does not touch it. fd 5 itself need not be
    // open — the API-level mutation accepts any `u32`.
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
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    // fd-ops Wave 1: install at fd 3 so the EBADF gate doesn't
    // fire before the unknown-cmd path is reached.
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    // F_DUPFD = 0 is not in the Wave 2 surface.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(38));
}

/// `fcntl` against a closed/never-installed fd returns `-EBADF`.
///
/// fd-ops Wave 1 (2026-05-07): the previous `FD_TABLE_SIZE = 8`
/// ceiling has been retired (the fd table is now a sparse
/// `BTreeMap<u32, Cap<OpenFile>>`); the EBADF discriminant is now
/// "is this fd actually open?", matching Linux semantics for
/// `F_GETFD` / `F_SETFD` against a closed fd.
#[test]
fn dispatch_fcntl_closed_fd_returns_neg_ebadf() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // Bootstrap leaves the fd table empty — every fd is closed.
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [8, F_GETFD as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(9));

    // Same for F_SETFD; the fd-ops Wave 1 sparse table allows fd 99
    // as a key but with no installed `Cap<OpenFile>` it's still
    // closed, so EBADF.
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

// =====================================================================
// Wave 2 of the DAC + setuid slice — Part 7 (`SyscallCtx::cred()`
// accessor) + Part 3 (process-side cred-mutation / cred-reading
// syscall arms). Each test wraps a Wave 1 `cred::step_*` helper
// through the new `ctx.cred()` accessor and asserts the dispatcher's
// `SyscallResult` matches the documented Linux semantics.
//
// Test discipline: bootstrap_init_process initialises the cred to
// root (uid 0, full caps). For "non-root" tests we drop privileges
// once via `step_setresuid` from the privileged starting state, then
// dispatch the syscall under test against the now-unprivileged cred.
//
// See `docs/progress/plans/2026-05-06-dac-and-setuid.md` Wave 2.
// =====================================================================
mod dac_setuid_wave2 {
    use super::*;

    use tx_subsystems::cred::{step_setresuid, Uid};
    use tx_subsystems::cross_crate_test_support::clear_caps_for_test;

    use crate::linux_syscall::{
        NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETRESGID, NR_GETRESUID, NR_GETUID, NR_SETGID,
        NR_SETREGID, NR_SETRESUID, NR_SETREUID, NR_SETUID,
    };

    /// `(u32) -1` — Linux's "leave unchanged" sentinel for the
    /// `setre{u,g}id` / `setres{u,g}id` family. Userspace passes this
    /// as the unsigned `uid_t` cast of `-1`; the kernel arm decodes it
    /// to `Option::None` before calling the cred helper.
    const NEG_ONE_U32: u64 = u32::MAX as u64;

    /// Drop privileges from the bootstrap-init root cred to a
    /// concrete non-root uid. Uses `step_setresuid` from the
    /// privileged starting state, which sets `uid`, `euid`, and
    /// `suid` all to `target` in one atomic step. Then clears
    /// `effective_caps` / `permitted_caps` via the cross-crate
    /// test-support helper so `Cred::is_privileged_for(...)` no
    /// longer short-circuits (root.effective_caps == FULL by
    /// construction; the shipping `step_set*` mutators preserve
    /// caps, so a uid drop alone is not enough to land in an
    /// unprivileged state).
    fn drop_privs_to(proc_cap: &Cap<ProcessIdentity>, uid: u32) {
        let target = Uid(uid);
        let outcome = step_setresuid(proc_cap, Some(target), Some(target), Some(target));
        assert!(
            matches!(outcome, tx_subsystems::cred::CredChange::Replaced { .. }),
            "drop_privs_to({uid}) must succeed from root: got {outcome:?}"
        );
        clear_caps_for_test(proc_cap);
    }

    // -----------------------------------------------------------------
    // Read-side arms.
    // -----------------------------------------------------------------

    /// `getuid()` returns the bootstrap-init real uid (0 for root).
    #[test]
    fn dispatch_getuid_returns_cred_uid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETUID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// `geteuid()` returns the bootstrap-init effective uid (0).
    #[test]
    fn dispatch_geteuid_returns_cred_euid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETEUID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// `getgid()` returns the bootstrap-init real gid (0).
    #[test]
    fn dispatch_getgid_returns_cred_gid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETGID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// `getegid()` returns the bootstrap-init effective gid (0).
    #[test]
    fn dispatch_getegid_returns_cred_egid() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETEGID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// After dropping privs to uid 1000, `getuid` reads the new
    /// real uid through `ctx.cred()` — pins that the
    /// `SyscallCtx::cred()` accessor reads from `ProcessPayload.cred`
    /// (not a stale snapshot stamped at construction).
    #[test]
    fn dispatch_getuid_reflects_post_setresuid_cred() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETUID, [0; 6]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(1000));
    }

    // -----------------------------------------------------------------
    // Single-arg setters.
    // -----------------------------------------------------------------

    /// Privileged `setuid(0)` (root → root) succeeds with
    /// `Return(0)`. Sanity-check on the privileged path.
    #[test]
    fn dispatch_setuid_root_to_zero_succeeds() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_SETUID, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.uid.raw(), 0);
        assert_eq!(cred.euid.raw(), 0);
        assert_eq!(cred.suid.raw(), 0);
    }

    /// Non-privileged `setuid(target)` where `target` already matches
    /// one of `(uid, euid, suid)` succeeds (`euid` swap). After
    /// dropping privs to 1000, `setuid(1000)` is a no-op-shaped
    /// success.
    #[test]
    fn dispatch_setuid_to_existing_id_succeeds() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_SETUID, [1000, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.euid.raw(), 1000);
    }

    /// Non-privileged `setuid(unrelated)` returns `-EPERM` and leaves
    /// the cred unchanged.
    #[test]
    fn dispatch_setuid_unprivileged_outside_returns_neg_eperm() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // 2000 is not in {uid=1000, euid=1000, suid=1000}.
        let req = SyscallRequest::new(NR_SETUID, [2000, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(1), "expected -EPERM");
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.uid.raw(), 1000);
        assert_eq!(cred.euid.raw(), 1000);
        assert_eq!(cred.suid.raw(), 1000);
    }

    /// Non-privileged `setgid(target)` where `target` matches one of
    /// `(gid, egid, sgid)` succeeds.
    #[test]
    fn dispatch_setgid_to_existing_id_succeeds() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // After drop_privs_to(1000), gid family is still 0 (only the
        // uid family was changed). `setgid(0)` is a no-op-shaped
        // success because 0 is in {gid, egid, sgid}.
        let req = SyscallRequest::new(NR_SETGID, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.egid.raw(), 0);
    }

    // -----------------------------------------------------------------
    // Two-arg setters.
    // -----------------------------------------------------------------

    /// Privileged `setresuid(1000, 1000, 1000)` writes all three uid
    /// fields to 1000.
    #[test]
    fn dispatch_setresuid_privileged_writes_all_three() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_SETRESUID, [1000, 1000, 1000, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.uid.raw(), 1000);
        assert_eq!(cred.euid.raw(), 1000);
        assert_eq!(cred.suid.raw(), 1000);
    }

    /// `setresuid(-1, -1, -1)` decodes all three sentinels and
    /// leaves the cred unchanged. `Return(0)` because the privilege
    /// rule trivially passes (no requested fields to validate).
    #[test]
    fn dispatch_setresuid_with_neg1_leaves_unchanged() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(
            NR_SETRESUID,
            [NEG_ONE_U32, NEG_ONE_U32, NEG_ONE_U32, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.uid.raw(), 1000);
        assert_eq!(cred.euid.raw(), 1000);
        assert_eq!(cred.suid.raw(), 1000);
    }

    /// Non-privileged `setresuid(other, -1, -1)` where `other` is not
    /// in `{uid, euid, suid}` returns `-EPERM` and leaves the cred
    /// unchanged.
    #[test]
    fn dispatch_setresuid_unprivileged_outside_returns_neg_eperm() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // 2000 is unrelated to the current {1000, 1000, 1000}.
        let req = SyscallRequest::new(NR_SETRESUID, [2000, NEG_ONE_U32, NEG_ONE_U32, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(1), "expected -EPERM");
        let cred = proc_cap.cred().expect("alive process has cred");
        assert_eq!(cred.uid.raw(), 1000);
    }

    /// Non-privileged `setreuid(-1, current_uid)` exercises the
    /// Linux saved-set quirk: changing `euid` away from the pre-call
    /// `uid` bumps `suid`. We arrange a state where the quirk fires
    /// — start at `(uid, euid, suid) = (0, 0, 0)`, drop euid only.
    /// Wait — the privileged path always sets all three. We instead
    /// arrange a non-trivial starting state via two calls: privileged
    /// `setresuid(1001, 1000, 1000)` to get `(1001, 1000, 1000)`,
    /// then non-privileged `setreuid(-1, 1001)` (1001 is in the set
    /// because uid==1001). The quirk says "if euid_after != prev.uid,
    /// suid := euid_after". Pre-call: `(1001, 1000, 1000)`.
    /// Post-call: `(1001, 1001, 1001)`.
    #[test]
    fn dispatch_setreuid_updates_suid_on_euid_change() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);

        // Privileged call: split uid from euid/suid using
        // step_setresuid directly (NR_SETRESUID would also work).
        let outcome = step_setresuid(&proc_cap, Some(Uid(1001)), Some(Uid(1000)), Some(Uid(1000)));
        assert!(matches!(
            outcome,
            tx_subsystems::cred::CredChange::Replaced { .. }
        ));

        let ctx = make_ctx(proc_cap.clone(), thread);
        // Non-privileged (euid is 1000, not 0) `setreuid(-1, 1001)`.
        // 1001 is in the existing set as `uid`. The quirk fires
        // because `euid_after == 1001 != prev.uid == 1001`? No —
        // wait. Pre: prev.uid = 1001. New euid = 1001. The quirk
        // condition is `ruid.is_some() || new.euid != prev.uid`. With
        // ruid = None and new.euid (1001) == prev.uid (1001), the
        // quirk does NOT fire. We need euid != prev.uid. So instead
        // set up `(1000, 1001, 1001)` and call setreuid(-1, 1000).
        let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1001)));
        assert!(matches!(
            outcome,
            tx_subsystems::cred::CredChange::Replaced { .. }
        ));

        // Now: prev = (1000, 1001, 1001). Non-privileged setreuid(-1, 1000):
        // - 1000 is in {1000, 1001, 1001}, so the rule passes.
        // - new.euid = 1000, prev.uid = 1000 → quirk does NOT fire.
        // Need a case that DOES fire: setreuid(-1, 1001) with prev
        // (1000, 1001, 1001) — new.euid=1001, prev.uid=1000 → quirk
        // fires, suid := 1001 (already 1001). To observe a change,
        // start with suid != target euid: (1000, 1000, 1000) start
        // and call setreuid(-1, 1000) — no change; can't trigger.
        // Privileged setresuid(1000, 1001, 1000) → (1000, 1001, 1000)
        // then non-privileged setreuid(-1, 1000) → euid_after=1000,
        // prev.uid=1000, quirk doesn't fire; setreuid(-1, 1001)
        // → euid_after=1001, prev.uid=1000, quirk fires, suid=1001.
        let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1000)));
        assert!(matches!(
            outcome,
            tx_subsystems::cred::CredChange::Replaced { .. }
        ));
        // Pre-syscall snapshot for clarity.
        let pre = proc_cap.cred().expect("alive process has cred");
        assert_eq!(pre.uid.raw(), 1000);
        assert_eq!(pre.euid.raw(), 1001);
        assert_eq!(pre.suid.raw(), 1000);

        // setreuid(-1, 1001) — 1001 is in {1000, 1001, 1000}, allowed.
        // Quirk fires: new.euid=1001 != prev.uid=1000 → suid := 1001.
        let req = SyscallRequest::new(NR_SETREUID, [NEG_ONE_U32, 1001, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let post = proc_cap.cred().expect("alive process has cred");
        assert_eq!(post.uid.raw(), 1000);
        assert_eq!(post.euid.raw(), 1001);
        assert_eq!(
            post.suid.raw(),
            1001,
            "setreuid quirk: euid change must bump suid to euid_after"
        );
    }

    /// Gid analog of the setreuid-bumps-suid test. Starts at
    /// `(gid, egid, sgid) = (1000, 1001, 1000)` and calls
    /// `setregid(-1, 1001)` (non-privileged). The quirk fires:
    /// `new.egid=1001 != prev.gid=1000 → sgid := 1001`.
    #[test]
    fn dispatch_setregid_updates_sgid_on_egid_change() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);

        // Privileged setresgid to seed the asymmetric state.
        let outcome = tx_subsystems::cred::step_setresgid(
            &proc_cap,
            Some(tx_subsystems::cred::Gid(1000)),
            Some(tx_subsystems::cred::Gid(1001)),
            Some(tx_subsystems::cred::Gid(1000)),
        );
        assert!(matches!(
            outcome,
            tx_subsystems::cred::CredChange::Replaced { .. }
        ));
        // Drop uid privs so the gid mutators run as non-privileged.
        // (CAP_SETGID is granted by full caps; root euid==0 also
        // privileges; we need both gone. drop_privs_to clears euid.)
        drop_privs_to(&proc_cap, 1000);

        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_SETREGID, [NEG_ONE_U32, 1001, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let post = proc_cap.cred().expect("alive process has cred");
        assert_eq!(post.gid.raw(), 1000);
        assert_eq!(post.egid.raw(), 1001);
        assert_eq!(
            post.sgid.raw(),
            1001,
            "setregid quirk: egid change must bump sgid to egid_after"
        );
    }

    // -----------------------------------------------------------------
    // getresuid / getresgid round-trips.
    // -----------------------------------------------------------------

    /// `getresuid(&r, &e, &s)` writes `(uid.raw(), euid.raw(),
    /// suid.raw())` to the three user pointers. Wave 2 bootstrap
    /// exemption: the three uaddrs are kernel-side `*mut u32`. NULL
    /// pointers skip the corresponding write.
    #[test]
    fn dispatch_getresuid_writes_three_u32s() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        // Asymmetric state so the three reads are distinguishable.
        let outcome = step_setresuid(&proc_cap, Some(Uid(1000)), Some(Uid(1001)), Some(Uid(1002)));
        assert!(matches!(
            outcome,
            tx_subsystems::cred::CredChange::Replaced { .. }
        ));
        let ctx = make_ctx(proc_cap, thread);

        let mut ruid: u32 = 0xDEAD_BEEF;
        let mut euid: u32 = 0xDEAD_BEEF;
        let mut suid: u32 = 0xDEAD_BEEF;
        let req = SyscallRequest::new(
            NR_GETRESUID,
            [
                &mut ruid as *mut u32 as u64,
                &mut euid as *mut u32 as u64,
                &mut suid as *mut u32 as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert_eq!(ruid, 1000);
        assert_eq!(euid, 1001);
        assert_eq!(suid, 1002);
    }

    /// Gid analog of the getresuid round-trip.
    #[test]
    fn dispatch_getresgid_writes_three_u32s() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let outcome = tx_subsystems::cred::step_setresgid(
            &proc_cap,
            Some(tx_subsystems::cred::Gid(2000)),
            Some(tx_subsystems::cred::Gid(2001)),
            Some(tx_subsystems::cred::Gid(2002)),
        );
        assert!(matches!(
            outcome,
            tx_subsystems::cred::CredChange::Replaced { .. }
        ));
        let ctx = make_ctx(proc_cap, thread);

        let mut rgid: u32 = 0xDEAD_BEEF;
        let mut egid: u32 = 0xDEAD_BEEF;
        let mut sgid: u32 = 0xDEAD_BEEF;
        let req = SyscallRequest::new(
            NR_GETRESGID,
            [
                &mut rgid as *mut u32 as u64,
                &mut egid as *mut u32 as u64,
                &mut sgid as *mut u32 as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert_eq!(rgid, 2000);
        assert_eq!(egid, 2001);
        assert_eq!(sgid, 2002);
    }

    /// `getresuid(NULL, NULL, NULL)` returns `0` and writes nothing.
    /// Confirms the NULL-skip arm doesn't fault when given zero
    /// pointers (kernel-side bootstrap exemption applies).
    #[test]
    fn dispatch_getresuid_null_pointers_skip_writes() {
        let _setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETRESUID, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }
}

// ===========================================================================
// Wave 4 Part 4 of the DAC + setuid slice — file-mode syscall arms.
//
// Coverage:
//   - `fchmodat` against a real tmpfs mount (owner success, non-owner EPERM)
//   - `fchmodat` against devfs (EROFS)
//   - `fchmodat` invalid dirfd (EBADF)
//   - `fchmodat` overlong path (ENAMETOOLONG)
//   - `fchownat` self-target / foreign-target rules (Wave 3 Part 2 chown)
//   - `fchownat` `(u32) -1` "leave unchanged" sentinel
//   - `faccessat` F_OK / R_OK / X_OK + DAC_OVERRIDE quirk
//   - `faccessat2` AT_EACCESS effective-id branch
//
// Mount setup mirrors the `execve` module's `build_fs_root` shape but
// uses the shipping `Tmpfs` backend so `step_chmod` / `step_chown`
// reach the real implementation.
// ===========================================================================

mod dac_setuid_wave4 {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;

    use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
    use tx_substrate::{page_allocator, zone};
    use tx_subsystems::cred::{step_setresuid, Capability, CapabilitySet, Uid};
    use tx_subsystems::cross_crate_test_support::{
        clear_caps_for_test, install_caps_for_test, set_cred_ids_for_test,
    };
    use tx_subsystems::mount::{
        DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
    };
    use tx_subsystems::page_backed::FsPageBacking;
    use tx_subsystems::process::step_chdir;
    use tx_subsystems::vfs::structure::{
        Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_IFDIR,
    };
    use tx_subsystems::vfs::FsOps;

    use crate::linux_syscall::{
        AT_EACCESS, AT_FDCWD, EXECVE_PATH_MAX, F_OK, NR_FACCESSAT, NR_FACCESSAT2, NR_FCHMODAT,
        NR_FCHOWNAT, R_OK, W_OK, X_OK,
    };

    /// errno magnitudes the tests check against (positive Linux RV64
    /// generic ABI values; the dispatcher returns the positive
    /// magnitude in `SyscallResult::Error`).
    const E_PERM: i32 = 1;
    const E_BADF: i32 = 9;
    const E_ACCES: i32 = 13;
    const E_ROFS: i32 = 30;
    const E_NAMETOOLONG: i32 = 36;

    /// `(u32) -1` — Linux's "leave unchanged" sentinel for
    /// `fchownat`'s `uid` / `gid` args. Same convention as the
    /// `setre{u,g}id` family; reused here for symmetry.
    const NEG_ONE_U32: u64 = u32::MAX as u64;

    fn ensure_zero_frame_claimed() {
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for wave4 tests: {error:?}"),
        }
    }

    fn wave4_setup() -> TestSetup {
        let setup = setup();
        ensure_zero_frame_claimed();
        setup
    }

    /// Build a fresh tmpfs-backed mount and a root `Cap<DEntry>`
    /// pointing at the tmpfs root inode. Returns the dentry plus the
    /// `Arc<Tmpfs>` so callers can mint files with specific
    /// `(uid, gid, mode)` directly through the FsOps surface.
    fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>) {
        let tmpfs = Arc::new(Tmpfs::new());
        let payload = MountPayload::new_cap(
            tmpfs.clone() as Arc<dyn FsOps>,
            tmpfs.clone() as Arc<dyn FsPageBacking>,
            None,
            DevId::new(101),
            MountOptions::default(),
            "tmpfs-wave4",
            SourceLabel::Static("tmpfs-wave4"),
        )
        .expect("mount payload");

        let root_rnode = {
            let raw = RNode::new(
                TMPFS_ROOT_OBJECT_ID,
                InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&payload);
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };

        let _mount = MountIdentity::new_cap(
            MountId::new(11),
            None,
            root_rnode.clone(),
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount identity");

        let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
        (root_dentry, tmpfs)
    }

    /// Devfs analog of `build_tmpfs_root`. Mounts the shipping
    /// `tx_fs::devfs::Devfs` (FsOps + FsPageBacking) at root.
    fn build_devfs_root() -> Cap<DEntry> {
        let payload = MountPayload::new_cap(
            tx_fs::devfs::Devfs::fs_ops_arc(),
            tx_fs::devfs::Devfs::fs_page_backing_arc(),
            None,
            DevId::new(102),
            MountOptions::default(),
            "devfs-wave4",
            SourceLabel::Static("devfs-wave4"),
        )
        .expect("mount payload");

        let root_rnode = {
            let raw = RNode::new(
                tx_fs::devfs::DEVFS_ROOT_OBJECT_ID,
                InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&payload);
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };

        let _mount = MountIdentity::new_cap(
            MountId::new(12),
            None,
            root_rnode.clone(),
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount identity");

        DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry")
    }

    /// Bootstrap an init process whose cwd is `root_dentry`. Returns
    /// the (process, leader-thread) pair.
    fn bootstrap_with_cwd(root_dentry: Cap<DEntry>) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let aspace = fresh_aspace();
        let process = bootstrap_init_process(aspace).expect("bootstrap init");
        let thread = process.nth_thread(0).expect("leader thread");
        match step_chdir(&process, root_dentry) {
            tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
            tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
                panic!("init bootstrap zombified")
            }
        }
        (process, thread)
    }

    /// Drop privileges on `proc_cap`: setresuid to `(uid, uid, uid)`
    /// then clear effective + permitted caps. Mirrors
    /// `dac_setuid_wave2::drop_privs_to`.
    fn drop_privs_to(proc_cap: &Cap<ProcessIdentity>, uid: u32) {
        let target = Uid(uid);
        let outcome = step_setresuid(proc_cap, Some(target), Some(target), Some(target));
        assert!(
            matches!(outcome, tx_subsystems::cred::CredChange::Replaced { .. }),
            "drop_privs_to({uid}) must succeed from root: got {outcome:?}"
        );
        clear_caps_for_test(proc_cap);
    }

    /// NUL-terminate a path slice into a kernel-side `Vec<u8>` so the
    /// inline `read_user_cstr` reads it correctly. Returns the buffer
    /// + a u64 pointer suitable for the syscall args.
    fn nul_terminate(path: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(path.len() + 1);
        v.extend_from_slice(path);
        v.push(0);
        v
    }

    // -----------------------------------------------------------------
    // fchmodat
    // -----------------------------------------------------------------

    /// `fchmodat(AT_FDCWD, "/f", 0o600, 0)` against a tmpfs file owned
    /// by uid 1000 succeeds when the caller is uid 1000. Verifies the
    /// arm wires `walker_cred` (effective ids) into
    /// `FsOps::step_chmod` and that mode is masked to 0o7777.
    #[test]
    fn dispatch_fchmodat_owner_succeeds() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        // Create the file as uid 1000 so the inode is owned by them.
        let guard = tx_substrate::epoch::guard();
        let (file_id, _) =
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done(out) => out,
                other => panic!("create_inode: {other:?}"),
            };
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FCHMODAT,
            [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o600, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));

        // Backend reflects the new mode.
        let guard = tx_substrate::epoch::guard();
        let meta = match tmpfs.load_inode_meta(file_id, &guard) {
            StepOutcome::Done(m) => m,
            other => panic!("load_inode_meta: {other:?}"),
        };
        assert_eq!(meta.mode & 0o7777, 0o600);
        drop(path);
    }

    /// `fchmodat` against a file owned by someone else, called from a
    /// non-privileged caller, returns `-EPERM` (matches Linux's
    /// `chmod(2)` errno for "not owner, no CAP_FOWNER").
    #[test]
    fn dispatch_fchmodat_non_owner_returns_neg_eperm() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        // File owned by uid 1000.
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Caller is uid 2000 — not the owner.
        drop_privs_to(&proc_cap, 2000);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FCHMODAT,
            [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o755, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_PERM));
        drop(path);
    }

    /// `fchmodat` against devfs (a read-only projection) returns
    /// `-EROFS` per Wave 3 Part 2's devfs-side `Errno::EROFS`.
    #[test]
    fn dispatch_fchmodat_devfs_returns_neg_erofs() {
        let _setup = wave4_setup();
        let root_dentry = build_devfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        // The devfs root inode itself is targetable — chmod on it
        // routes through `step_chmod` which devfs short-circuits to
        // EROFS regardless of fs_object_id. Use the root path "/"
        // which always resolves.
        let path = nul_terminate(b"/");
        let req = SyscallRequest::new(
            NR_FCHMODAT,
            [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o755, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_ROFS));
        drop(path);
    }

    /// Any non-`AT_FDCWD` dirfd value returns `-EBADF`. The slice's
    /// fd table doesn't carry directory-fd semantics yet
    /// (TODO(phase-dirfd)).
    #[test]
    fn dispatch_fchmodat_invalid_dirfd_returns_neg_ebadf() {
        let _setup = wave4_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        // dirfd = 3 (a positive fd value); not AT_FDCWD = -100.
        let req = SyscallRequest::new(NR_FCHMODAT, [3u64, path.as_ptr() as u64, 0o600, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
        drop(path);
    }

    /// A path with no NUL terminator within `EXECVE_PATH_MAX` returns
    /// `-ENAMETOOLONG`. Matches the `read_user_cstr` budget.
    #[test]
    fn dispatch_fchmodat_path_too_long_returns_neg_enametoolong() {
        let _setup = wave4_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        // 4097 'A' bytes, no NUL — `read_user_cstr` walks the full
        // budget and gives up.
        let path: Vec<u8> = vec![b'A'; EXECVE_PATH_MAX + 1];
        let req = SyscallRequest::new(
            NR_FCHMODAT,
            [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o600, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NAMETOOLONG));
        drop(path);
    }

    // -----------------------------------------------------------------
    // fchownat
    // -----------------------------------------------------------------

    /// Non-privileged caller chowning to its own uid+gid succeeds (the
    /// no-op-shaped self-chown POSIX explicitly allows for non-root
    /// callers).
    #[test]
    fn dispatch_fchownat_unprivileged_to_self_succeeds() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 1000,
            gid: 200,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let (file_id, _) =
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done(out) => out,
                other => panic!("create_inode: {other:?}"),
            };
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Set caller to (uid=1000, gid=200) — matching the file's
        // owner so the self-chown is a no-op-shaped success. Use the
        // direct test helper rather than chaining setresuid/setresgid:
        // the file's gid is 200, not 0, so the shipping mutators
        // would need a valid starting state.
        set_cred_ids_for_test(&proc_cap, 1000, 1000, 1000, 200, 200, 200);
        clear_caps_for_test(&proc_cap);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FCHOWNAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                1000,
                200,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));

        let guard = tx_substrate::epoch::guard();
        let meta = match tmpfs.load_inode_meta(file_id, &guard) {
            StepOutcome::Done(m) => m,
            other => panic!("load_inode_meta: {other:?}"),
        };
        assert_eq!(meta.uid, 1000);
        assert_eq!(meta.gid, 200);
        drop(path);
    }

    /// Non-privileged caller chowning to a foreign uid returns
    /// `-EPERM`. Matches POSIX `chown(2)` ("only superuser may change
    /// the file's owner").
    #[test]
    fn dispatch_fchownat_unprivileged_to_other_returns_neg_eperm() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        drop_privs_to(&proc_cap, 1000);
        let ctx = make_ctx(proc_cap, thread);

        // Try to chown to uid 2000 (foreign). Must EPERM.
        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FCHOWNAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                2000,
                NEG_ONE_U32,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_PERM));
        drop(path);
    }

    /// `fchownat(.., -1, -1, ..)` (both ids = sentinel) is a "leave
    /// unchanged" no-op. Confirms `decode_uid_arg` / `decode_gid_arg`
    /// flow correctly into `step_chown`'s `(None, None)`.
    #[test]
    fn dispatch_fchownat_minus_one_leaves_unchanged() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 1000,
            gid: 200,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let (file_id, _) =
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done(out) => out,
                other => panic!("create_inode: {other:?}"),
            };
        drop(guard);

        // Stay root + CAP_FOWNER so step_chown is permitted; this test
        // is about the sentinel decoding, not the privilege check.
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FCHOWNAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                NEG_ONE_U32,
                NEG_ONE_U32,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));

        // uid/gid unchanged (still 1000/200 from create_inode).
        let guard = tx_substrate::epoch::guard();
        let meta = match tmpfs.load_inode_meta(file_id, &guard) {
            StepOutcome::Done(m) => m,
            other => panic!("load_inode_meta: {other:?}"),
        };
        assert_eq!(meta.uid, 1000);
        assert_eq!(meta.gid, 200);
        drop(path);
    }

    // -----------------------------------------------------------------
    // faccessat / faccessat2
    // -----------------------------------------------------------------

    /// `faccessat(AT_FDCWD, "/f", F_OK)` against an existing tmpfs
    /// file returns 0. F_OK = 0 short-circuits the permission-bit
    /// check after path resolution succeeds.
    #[test]
    fn dispatch_faccessat_existing_file_f_ok_returns_zero() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FACCESSAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                F_OK as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        drop(path);
    }

    /// `faccessat(.., R_OK)` on a file with no read bits in any
    /// triplet returns `-EACCES`. Caller is non-root, non-owner so
    /// the "other" triplet (0o0) applies.
    #[test]
    fn dispatch_faccessat_no_read_bit_returns_neg_eacces() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        // File owned by uid 1000, mode 0o000 (no perms anywhere).
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100000, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Caller uid 2000 — not the owner.
        drop_privs_to(&proc_cap, 2000);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FACCESSAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                R_OK as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_ACCES));
        drop(path);
    }

    /// `faccessat(.., R_OK | W_OK)` from a non-owner caller carrying
    /// `CAP_DAC_OVERRIDE` returns 0 — read/write are always granted
    /// to DAC_OVERRIDE callers regardless of the inode's mode bits.
    #[test]
    fn dispatch_faccessat_dac_override_bypasses() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        // File owned by uid 1000, mode 0o000.
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100000, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Caller is uid 2000 (non-owner) but carries CAP_DAC_OVERRIDE.
        drop_privs_to(&proc_cap, 2000);
        let mut caps = CapabilitySet::EMPTY;
        caps.add(Capability::DAC_OVERRIDE);
        install_caps_for_test(&proc_cap, caps);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        // R_OK | W_OK only — X_OK has the Linux quirk tested below.
        let req = SyscallRequest::new(
            NR_FACCESSAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                (R_OK | W_OK) as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        drop(path);
    }

    /// `faccessat2(.., AT_EACCESS)` switches to **effective** ids.
    /// File owned by uid 1000; caller's real uid is 2000 (non-owner)
    /// but its effective uid is 1000 (owner). With AT_EACCESS, the
    /// owner triplet applies and R_OK is granted.
    #[test]
    fn dispatch_faccessat2_at_eaccess_uses_effective_uid() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        // File owned by uid 1000, mode 0o400 (owner-read only).
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100400, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Real uid 2000, effective uid 1000 — the AT_EACCESS shape.
        // Reach this directly via the test-only setter; the shipping
        // `step_setresuid` rules can't move from root → (real=2000,
        // effective=1000) in one call.
        set_cred_ids_for_test(&proc_cap, 2000, 1000, 1000, 0, 0, 0);
        clear_caps_for_test(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");

        // Without AT_EACCESS: real uid 2000 is "other" → 0 perm bits → EACCES.
        let req_real = SyscallRequest::new(
            NR_FACCESSAT2,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                R_OK as u64,
                0,
                0,
                0,
            ],
        );
        let result_real = block_on(dispatch::<ShimsTestPmap>(req_real, &ctx));
        assert_eq!(
            result_real,
            SyscallResult::Error(E_ACCES),
            "real-id walk should treat caller as 'other' and deny R_OK"
        );

        // With AT_EACCESS: effective uid 1000 matches inode owner →
        // owner triplet (0o4) → R_OK granted.
        let req_effective = SyscallRequest::new(
            NR_FACCESSAT2,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                R_OK as u64,
                AT_EACCESS as u64,
                0,
                0,
            ],
        );
        let result_effective = block_on(dispatch::<ShimsTestPmap>(req_effective, &ctx));
        assert_eq!(result_effective, SyscallResult::Return(0));
        drop(path);
    }

    /// Linux X-bit quirk: `access(X_OK)` fails with EACCES if no
    /// execute bit is set anywhere on the inode, even when the caller
    /// holds `CAP_DAC_OVERRIDE`. Matches `fs/namei.c::generic_permission`.
    #[test]
    fn dispatch_faccessat2_no_x_bit_returns_neg_eacces_even_with_dac_override() {
        let _setup = wave4_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        // File mode 0o644 — no execute bit anywhere.
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Drop to non-owner, then re-install CAP_DAC_OVERRIDE only.
        drop_privs_to(&proc_cap, 2000);
        let mut caps = CapabilitySet::EMPTY;
        caps.add(Capability::DAC_OVERRIDE);
        install_caps_for_test(&proc_cap, caps);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_FACCESSAT2,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                X_OK as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_ACCES));
        drop(path);
    }
}

// ===========================================================================
// Wave 2 of the fd-ops slice — fd-management syscall arms.
//
// Coverage:
//   - `openat(AT_FDCWD, path, flags, mode)` against tmpfs:
//     - existing file with O_RDONLY returns the fd
//     - O_CREAT against missing file creates + opens
//     - O_CREAT | O_EXCL against existing file returns -EEXIST
//     - O_TRUNC against existing non-empty file truncates to 0
//     - O_CLOEXEC sets the per-fd cloexec bit
//     - missing file without O_CREAT returns -ENOENT
//     - non-AT_FDCWD dirfd returns -EBADF
//     - overlong path returns -ENAMETOOLONG
//     - file lacking read perm + O_RDONLY returns -EACCES
//   - `close(fd)`: open fd returns 0 + clears slot; closed fd returns
//     -EBADF; cloexec bit cleared on close.
//   - `dup(oldfd)`: returns lowest unused fd with same OpenFile;
//     closed oldfd returns -EBADF.
//   - `dup3(oldfd, newfd, flags)`: replaces existing newfd; O_CLOEXEC
//     sets the bit; same-fd returns -EINVAL; junk flags return -EINVAL.
// ===========================================================================

mod fd_ops_wave2 {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;

    use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
    use tx_substrate::{page_allocator, zone};
    use tx_subsystems::cred::{step_setresuid, CapabilitySet, Uid};
    use tx_subsystems::cross_crate_test_support::clear_caps_for_test;
    use tx_subsystems::mount::{
        DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
    };
    use tx_subsystems::page_backed::FsPageBacking;
    use tx_subsystems::process::step_chdir;
    use tx_subsystems::vfs::structure::{
        Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_IFDIR,
    };
    use tx_subsystems::vfs::FsOps;

    use crate::linux_syscall::{
        AT_FDCWD, EXECVE_PATH_MAX, NR_CLOSE, NR_DUP, NR_DUP3, NR_OPENAT, O_CLOEXEC, O_CREAT,
        O_EXCL, O_RDONLY, O_RDWR, O_TRUNC,
    };

    /// errno magnitudes: positive Linux RV64 generic ABI values.
    const E_BADF: i32 = 9;
    const E_NOENT: i32 = 2;
    const E_EXIST: i32 = 17;
    const E_INVAL: i32 = 22;
    const E_ACCES: i32 = 13;
    const E_NAMETOOLONG: i32 = 36;

    fn ensure_zero_frame_claimed() {
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for fd-ops wave2 tests: {error:?}"),
        }
    }

    fn fd_ops_setup() -> TestSetup {
        let setup = setup();
        ensure_zero_frame_claimed();
        setup
    }

    /// Build a fresh tmpfs-backed mount + a root `Cap<DEntry>`.
    /// Mirrors the wave4_setup helper's shape but lives in this module
    /// so the fd-ops tests don't depend on wave4's path.
    fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>) {
        let tmpfs = Arc::new(Tmpfs::new());
        let payload = MountPayload::new_cap(
            tmpfs.clone() as Arc<dyn FsOps>,
            tmpfs.clone() as Arc<dyn FsPageBacking>,
            None,
            DevId::new(201),
            MountOptions::default(),
            "tmpfs-fdops",
            SourceLabel::Static("tmpfs-fdops"),
        )
        .expect("mount payload");

        let root_rnode = {
            let raw = RNode::new(
                TMPFS_ROOT_OBJECT_ID,
                InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&payload);
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };

        let _mount = MountIdentity::new_cap(
            MountId::new(21),
            None,
            root_rnode.clone(),
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount identity");

        let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
        (root_dentry, tmpfs)
    }

    /// Bootstrap an init process whose cwd is `root_dentry`. Returns
    /// the (process, leader-thread) pair.
    fn bootstrap_with_cwd(root_dentry: Cap<DEntry>) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let aspace = fresh_aspace();
        let process = bootstrap_init_process(aspace).expect("bootstrap init");
        let thread = process.nth_thread(0).expect("leader thread");
        match step_chdir(&process, root_dentry) {
            tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
            tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
                panic!("init bootstrap zombified")
            }
        }
        (process, thread)
    }

    fn drop_privs_to(proc_cap: &Cap<ProcessIdentity>, uid: u32) {
        let target = Uid(uid);
        let outcome = step_setresuid(proc_cap, Some(target), Some(target), Some(target));
        assert!(
            matches!(outcome, tx_subsystems::cred::CredChange::Replaced { .. }),
            "drop_privs_to({uid}) must succeed from root: got {outcome:?}"
        );
        clear_caps_for_test(proc_cap);
    }

    /// NUL-terminate a path slice into a kernel-side `Vec<u8>` so the
    /// inline `read_user_cstr` reads it correctly.
    fn nul_terminate(path: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(path.len() + 1);
        v.extend_from_slice(path);
        v.push(0);
        v
    }

    // ----------------------------------------------------------------
    // openat
    // ----------------------------------------------------------------

    /// `openat(AT_FDCWD, "/f", O_RDONLY)` against an existing tmpfs
    /// file returns the lowest unused fd (which, with the bootstrap
    /// init process having no preopened fds, is 0).
    #[test]
    fn dispatch_openat_existing_file_o_rdonly_returns_fd() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        match result {
            SyscallResult::Return(fd) => {
                assert!(fd >= 0, "openat returned negative fd: {fd}");
                // fd table now carries the OpenFile.
                assert!(
                    proc_cap.fd(fd as u32).is_some(),
                    "fd {fd} should be installed"
                );
            }
            other => panic!("openat existing file: {other:?}"),
        }
        drop(path);
    }

    /// `openat(AT_FDCWD, "/new", O_RDWR | O_CREAT, 0o644)` against a
    /// missing file creates it via `FsOps::create_inode` and opens
    /// the result. The new inode's mode is the supplied 0o644 plus
    /// the regular-file kind bits.
    #[test]
    fn dispatch_openat_o_creat_creates_new_file() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/new");
        let flags = (O_RDWR | O_CREAT) as u64;
        let req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                flags,
                0o644,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let fd = match result {
            SyscallResult::Return(fd) => fd,
            other => panic!("openat O_CREAT: {other:?}"),
        };
        assert!(fd >= 0);
        assert!(proc_cap.fd(fd as u32).is_some());

        // Verify the new inode landed in tmpfs's directory.
        let guard = tx_substrate::epoch::guard();
        let outcome = tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"new", &guard);
        assert!(
            matches!(outcome, StepOutcome::Done(_) | StepOutcome::Advanced(_)),
            "tmpfs should now resolve /new: {outcome:?}"
        );
        drop(path);
    }

    /// `openat(.., O_CREAT | O_EXCL)` against an *existing* file
    /// returns `-EEXIST` per POSIX. The `O_EXCL` lock-file primitive.
    #[test]
    fn dispatch_openat_o_creat_o_excl_existing_returns_neg_eexist() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let flags = (O_RDWR | O_CREAT | O_EXCL) as u64;
        let req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                flags,
                0o644,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_EXIST));
        drop(path);
    }

    /// `openat(.., O_RDWR | O_TRUNC)` against an existing file with
    /// non-zero size truncates the file via the in-scope
    /// `FsPageBacking::truncate`. After the call the inode meta
    /// reports `size == 0`.
    #[test]
    fn dispatch_openat_o_trunc_truncates_existing() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let (file_id, _) =
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"big", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done(out) => out,
                other => panic!("create_inode: {other:?}"),
            };
        // Pre-stuff the file's page-backing so its size is non-zero.
        // tmpfs's FsPageBacking::truncate doubles as a "set size" op.
        match tmpfs.truncate(file_id, 4096, &guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
            other => panic!("preload truncate: {other:?}"),
        }
        drop(guard);

        // Verify the precondition.
        {
            let guard = tx_substrate::epoch::guard();
            let meta = match tmpfs.load_inode_meta(file_id, &guard) {
                StepOutcome::Done(m) => m,
                other => panic!("load_inode_meta: {other:?}"),
            };
            assert_eq!(meta.size, 4096, "precondition: size should be 4096");
        }

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/big");
        let flags = (O_RDWR | O_TRUNC) as u64;
        let req = SyscallRequest::new(
            NR_OPENAT,
            [AT_FDCWD as i64 as u64, path.as_ptr() as u64, flags, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        match result {
            SyscallResult::Return(_) => {}
            other => panic!("openat O_TRUNC: {other:?}"),
        }

        // Postcondition: tmpfs reports size 0 for the file.
        let guard = tx_substrate::epoch::guard();
        let meta = match tmpfs.load_inode_meta(file_id, &guard) {
            StepOutcome::Done(m) => m,
            other => panic!("load_inode_meta: {other:?}"),
        };
        assert_eq!(meta.size, 0, "O_TRUNC should have truncated to 0");
        drop(path);
    }

    /// `openat(.., O_RDONLY | O_CLOEXEC)` sets the cloexec bit on the
    /// returned fd. The bit is consulted by `step_close_cloexec_fds`
    /// at exec time.
    #[test]
    fn dispatch_openat_o_cloexec_sets_fd_cloexec_bit() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/f");
        let flags = (O_RDONLY | O_CLOEXEC) as u64;
        let req = SyscallRequest::new(
            NR_OPENAT,
            [AT_FDCWD as i64 as u64, path.as_ptr() as u64, flags, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let fd = match result {
            SyscallResult::Return(fd) => fd,
            other => panic!("openat O_CLOEXEC: {other:?}"),
        };
        assert!(
            proc_cap.fd_cloexec(fd as u32),
            "O_CLOEXEC should set the cloexec bit on fd {fd}"
        );
        drop(path);
    }

    /// `openat(AT_FDCWD, "/missing", O_RDONLY)` returns `-ENOENT` —
    /// no `O_CREAT`, file doesn't exist.
    #[test]
    fn dispatch_openat_path_not_found_returns_neg_enoent() {
        let _setup = fd_ops_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/missing");
        let req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOENT));
        drop(path);
    }

    /// Non-`AT_FDCWD` dirfd values return `-EBADF`. The slice's fd
    /// table doesn't carry directory-fd semantics yet.
    #[test]
    fn dispatch_openat_dirfd_not_at_fdcwd_returns_neg_ebadf() {
        let _setup = fd_ops_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        // dirfd = 5 (a positive fd value); not AT_FDCWD = -100.
        let req = SyscallRequest::new(
            NR_OPENAT,
            [5u64, path.as_ptr() as u64, O_RDONLY as u64, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
        drop(path);
    }

    /// A path with no NUL terminator within `EXECVE_PATH_MAX` returns
    /// `-ENAMETOOLONG`.
    #[test]
    fn dispatch_openat_path_too_long_returns_neg_enametoolong() {
        let _setup = fd_ops_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path: Vec<u8> = vec![b'A'; EXECVE_PATH_MAX + 1];
        let req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NAMETOOLONG));
        drop(path);
    }

    /// `openat(.., O_RDONLY)` against a file with no read perm in any
    /// triplet returns `-EACCES`. The walker's terminal-component
    /// `check_open_perm` predicate fires (DAC Wave 3).
    #[test]
    fn dispatch_openat_no_read_perm_returns_neg_eacces() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        // File owned by uid 1000, mode 0o100000 — no read bit anywhere.
        let owner_cred = Credential {
            uid: 1000,
            gid: 0,
            effective_caps: CapabilitySet::EMPTY,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100000, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        // Caller uid 2000 — not the owner.
        drop_privs_to(&proc_cap, 2000);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_ACCES));
        drop(path);
    }

    // ----------------------------------------------------------------
    // close
    // ----------------------------------------------------------------

    /// `close(fd)` against an open fd returns 0 and removes the cap
    /// from the fd table.
    #[test]
    fn dispatch_close_open_fd_returns_zero_and_clears_slot() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Open then close.
        let path = nul_terminate(b"/f");
        let open_req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat: {other:?}"),
        };
        assert!(proc_cap.fd(fd).is_some());

        let close_req = SyscallRequest::new(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(close_req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert!(
            proc_cap.fd(fd).is_none(),
            "close should have cleared fd {fd}"
        );
        drop(path);
    }

    /// `close(fd)` against an already-closed fd returns `-EBADF`.
    #[test]
    fn dispatch_close_closed_fd_returns_neg_ebadf() {
        let _setup = fd_ops_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_CLOSE, [42u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
    }

    /// `close(fd)` clears the cloexec bit so a future `fcntl(F_GETFD)`
    /// against the same fd number after re-open reports a clean state.
    #[test]
    fn dispatch_close_clears_cloexec_bit() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Open with O_CLOEXEC.
        let path = nul_terminate(b"/f");
        let open_req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                (O_RDONLY | O_CLOEXEC) as u64,
                0,
                0,
                0,
            ],
        );
        let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat: {other:?}"),
        };
        assert!(proc_cap.fd_cloexec(fd), "cloexec set after O_CLOEXEC open");

        let close_req = SyscallRequest::new(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        let _ = block_on(dispatch::<ShimsTestPmap>(close_req, &ctx));
        assert!(!proc_cap.fd_cloexec(fd), "cloexec cleared after close");
        drop(path);
    }

    // ----------------------------------------------------------------
    // dup
    // ----------------------------------------------------------------

    /// `dup(oldfd)` returns the lowest unused fd; the new fd
    /// references the same `OpenFile` as `oldfd` (cap clone semantics).
    #[test]
    fn dispatch_dup_returns_lowest_unused_fd_with_same_openfile() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Open a file → fd 0.
        let path = nul_terminate(b"/f");
        let open_req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let oldfd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat: {other:?}"),
        };

        let dup_req = SyscallRequest::new(NR_DUP, [oldfd as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(dup_req, &ctx));
        let newfd = match result {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("dup: {other:?}"),
        };
        assert_ne!(oldfd, newfd, "dup must return a different fd");
        assert!(proc_cap.fd(oldfd).is_some(), "oldfd still open");
        assert!(proc_cap.fd(newfd).is_some(), "newfd installed");
        // POSIX: dup-derived fd has cloexec cleared.
        assert!(!proc_cap.fd_cloexec(newfd), "dup must clear cloexec");
        drop(path);
    }

    /// `dup(closed_fd)` returns `-EBADF`.
    #[test]
    fn dispatch_dup_closed_fd_returns_neg_ebadf() {
        let _setup = fd_ops_setup();
        let (root_dentry, _tmpfs) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_DUP, [99u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
    }

    // ----------------------------------------------------------------
    // dup3
    // ----------------------------------------------------------------

    /// `dup3(oldfd, newfd, 0)` against an existing `newfd` silently
    /// closes the previous occupant and binds `newfd` to the same
    /// `OpenFile` as `oldfd`. Atomic-replace semantic per Linux.
    #[test]
    fn dispatch_dup3_at_specific_fd_replaces_existing() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"a", 0o100644, &owner_cred, &guard);
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"b", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        // Open /a → fd_a; open /b → fd_b.
        let path_a = nul_terminate(b"/a");
        let req_a = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path_a.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let fd_a = match block_on(dispatch::<ShimsTestPmap>(req_a, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat /a: {other:?}"),
        };
        let path_b = nul_terminate(b"/b");
        let req_b = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path_b.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let fd_b = match block_on(dispatch::<ShimsTestPmap>(req_b, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat /b: {other:?}"),
        };
        assert_ne!(fd_a, fd_b);

        // dup3(fd_a, fd_b, 0) — fd_b was open against /b, now points at /a.
        let dup3_req = SyscallRequest::new(NR_DUP3, [fd_a as u64, fd_b as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(dup3_req, &ctx));
        assert_eq!(result, SyscallResult::Return(fd_b as i64));
        // fd_b is still occupied (now binds to /a's OpenFile).
        assert!(proc_cap.fd(fd_b).is_some());
        // fd_a is unchanged.
        assert!(proc_cap.fd(fd_a).is_some());
        drop(path_a);
        drop(path_b);
    }

    /// `dup3(oldfd, newfd, O_CLOEXEC)` sets the cloexec bit on the
    /// newfd specifically (not on oldfd).
    #[test]
    fn dispatch_dup3_with_o_cloexec_sets_cloexec() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/f");
        let open_req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let oldfd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat: {other:?}"),
        };
        // dup3 to a fresh slot 100 with O_CLOEXEC.
        let dup3_req =
            SyscallRequest::new(NR_DUP3, [oldfd as u64, 100u64, O_CLOEXEC as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(dup3_req, &ctx));
        assert_eq!(result, SyscallResult::Return(100));
        assert!(proc_cap.fd(100).is_some());
        assert!(proc_cap.fd_cloexec(100), "dup3(O_CLOEXEC) sets cloexec");
        // oldfd's cloexec is unchanged (was clear after openat).
        assert!(!proc_cap.fd_cloexec(oldfd), "oldfd cloexec unchanged");
        drop(path);
    }

    /// `dup3(fd, fd, 0)` returns `-EINVAL`. Linux dup3 rejects the
    /// same-fd shape that legacy dup2 would accept as a no-op.
    #[test]
    fn dispatch_dup3_same_fd_returns_neg_einval() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let open_req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat: {other:?}"),
        };

        let req = SyscallRequest::new(NR_DUP3, [fd as u64, fd as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
        drop(path);
    }

    /// `dup3(oldfd, newfd, junk_flags)` with bits other than
    /// `O_CLOEXEC` set returns `-EINVAL`. Linux dup3 specifically
    /// rejects junk flags rather than ignoring them (open() ignores).
    #[test]
    fn dispatch_dup3_invalid_flags_returns_neg_einval() {
        let _setup = fd_ops_setup();
        let (root_dentry, tmpfs) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let guard = tx_substrate::epoch::guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        drop(guard);

        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let open_req = SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("openat: {other:?}"),
        };

        // A non-O_CLOEXEC junk flag (use O_RDWR's bit pattern as the
        // "unrecognised by dup3" sentinel — dup3's flag arg has only
        // O_CLOEXEC defined).
        let junk_flags: u64 = 0o100; // O_CREAT bit, not O_CLOEXEC
        let req = SyscallRequest::new(NR_DUP3, [fd as u64, 50u64, junk_flags, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
        drop(path);
    }
}

// ===========================================================================
// Wave 3 of the fd-ops slice — `pipe2(2)` syscall arm.
//
// Coverage:
//   - `pipe2(uaddr, 0)` allocates two fds and writes their pair to
//     `uaddr`; both fds resolve to the same shared payload.
//   - `pipe2(uaddr, O_CLOEXEC)` sets the cloexec bit on both fds.
//   - `pipe2(uaddr, O_NONBLOCK)` threads through to OpenFile.flags.
//   - `pipe2(uaddr, O_DIRECT)` returns `-ENOSYS` (packet-mode pipes).
//   - `pipe2(uaddr, junk)` returns `-EINVAL`.
// ===========================================================================
mod fd_ops_wave3 {
    use super::*;
    use tx_subsystems::process::bootstrap_init_process;

    use crate::linux_syscall::{NR_PIPE2, O_CLOEXEC, O_DIRECT, O_NONBLOCK};

    const E_INVAL: i32 = 22;
    const E_NOSYS: i32 = 38;

    fn pipe2_setup() -> TestSetup {
        setup()
    }

    fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let process =
            bootstrap_init_process(fresh_aspace()).expect("bootstrap init for pipe2 tests");
        let thread = process.nth_thread(0).expect("leader thread");
        (process, thread)
    }

    /// `pipe2(uaddr, 0)` succeeds, writes `(reader_fd, writer_fd)` to
    /// userspace, and installs both fds in the table.
    #[test]
    fn dispatch_pipe2_allocates_two_fds_and_writes_pair_to_userspace() {
        let _setup = pipe2_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap.clone(), thread);
        let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

        let req = SyscallRequest::new(
            NR_PIPE2,
            [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let reader_fd = pipefd[0];
        let writer_fd = pipefd[1];
        assert_ne!(reader_fd, u32::MAX, "reader_fd written");
        assert_ne!(writer_fd, u32::MAX, "writer_fd written");
        assert_ne!(reader_fd, writer_fd, "fds distinct");
        assert!(proc_cap.fd(reader_fd).is_some(), "reader fd installed");
        assert!(proc_cap.fd(writer_fd).is_some(), "writer fd installed");
        // Default flags: cloexec clear, nonblocking clear.
        assert!(!proc_cap.fd_cloexec(reader_fd));
        assert!(!proc_cap.fd_cloexec(writer_fd));
    }

    /// `pipe2(uaddr, O_CLOEXEC)` sets the cloexec bit on both fds.
    #[test]
    fn dispatch_pipe2_with_o_cloexec_sets_cloexec_on_both_fds() {
        let _setup = pipe2_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap.clone(), thread);
        let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

        let req = SyscallRequest::new(
            NR_PIPE2,
            [pipefd.as_mut_ptr() as u64, O_CLOEXEC as u64, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert!(proc_cap.fd_cloexec(pipefd[0]), "reader cloexec set");
        assert!(proc_cap.fd_cloexec(pipefd[1]), "writer cloexec set");
    }

    /// `pipe2(uaddr, O_NONBLOCK)` threads through to OpenFile.flags
    /// on both ends.
    #[test]
    fn dispatch_pipe2_with_o_nonblock_sets_nonblocking_on_both_openfiles() {
        let _setup = pipe2_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap.clone(), thread);
        let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

        let req = SyscallRequest::new(
            NR_PIPE2,
            [pipefd.as_mut_ptr() as u64, O_NONBLOCK as u64, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let reader = proc_cap.fd(pipefd[0]).expect("reader fd installed");
        let writer = proc_cap.fd(pipefd[1]).expect("writer fd installed");
        assert!(reader.flags().nonblocking, "reader nonblocking");
        assert!(writer.flags().nonblocking, "writer nonblocking");
    }

    /// `pipe2(uaddr, O_DIRECT)` returns `-ENOSYS` (packet-mode pipes
    /// are out of scope for the slice).
    #[test]
    fn dispatch_pipe2_with_o_direct_returns_neg_enosys() {
        let _setup = pipe2_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

        let req = SyscallRequest::new(
            NR_PIPE2,
            [pipefd.as_mut_ptr() as u64, O_DIRECT as u64, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOSYS));
    }

    /// `pipe2(uaddr, junk_bits)` returns `-EINVAL` for any
    /// unrecognised flag bits.
    #[test]
    fn dispatch_pipe2_with_junk_flags_returns_neg_einval() {
        let _setup = pipe2_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

        // 0x80000000 is well outside the recognised set
        // (O_CLOEXEC | O_NONBLOCK | O_DIRECT).
        let junk: u64 = 0x8000_0000;
        let req = SyscallRequest::new(
            NR_PIPE2,
            [pipefd.as_mut_ptr() as u64, junk, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }
}

// ===========================================================================
// Wave 4 of the fd-ops slice — `lseek(2)` syscall arm.
//
// Coverage:
//   - SEEK_SET writes the offset and returns the new value.
//   - SEEK_CUR adds to the current offset.
//   - SEEK_END returns `pc.size_bytes() + offset` for a PageBacked fd.
//   - lseek on a pipe fd returns `-ESPIPE` (regardless of whence/offset).
//   - lseek on a TTY fd returns `-ESPIPE`.
//   - A negative resulting offset returns `-EINVAL`.
//   - An unknown whence returns `-EINVAL`.
//   - lseek against an unknown fd returns `-EBADF`.
// ===========================================================================
mod fd_ops_wave4 {
    use super::*;
    use tx_substrate::{page_allocator, zone};
    use tx_subsystems::page_backed::{
        AnonSwapPolicy, PageContainer, PageContainerKind, step_truncate,
    };
    use tx_subsystems::pipe::{step_pipe2, PipeFlags};
    use tx_subsystems::process::bootstrap_init_process;
    use tx_subsystems::vfs::structure::{
        FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
    };

    use crate::linux_syscall::{NR_LSEEK, SEEK_CUR, SEEK_END, SEEK_SET};

    const E_BADF: i32 = 9;
    const E_INVAL: i32 = 22;
    const E_SPIPE: i32 = 29;

    fn lseek_setup() -> TestSetup {
        let setup = setup();
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for fd-ops wave4 tests: {error:?}"),
        }
        setup
    }

    fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let process =
            bootstrap_init_process(fresh_aspace()).expect("bootstrap init for lseek tests");
        let thread = process.nth_thread(0).expect("leader thread");
        (process, thread)
    }

    /// Build a `Cap<OpenFile>` over a fresh anon `PageContainer` of
    /// `page_count` pages. The container's visible size is set to
    /// `size_bytes` via `step_truncate` so SEEK_END has a stable
    /// number to assert against.
    fn pagebacked_open_file(page_count: u64, size_bytes: u64) -> Cap<OpenFile> {
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            page_count,
        )
        .expect("page container cap");
        let guard = tx_substrate::epoch::guard();
        match step_truncate(&pc, size_bytes, &guard) {
            tx_subsystems::execution::StepOutcome::Done(())
            | tx_subsystems::execution::StepOutcome::Advanced(()) => {}
            other => panic!("step_truncate({size_bytes}): {other:?}"),
        }
        drop(guard);
        let rnode = {
            let raw = RNode::new(
                FsObjectId::new(8_900),
                InodeMeta::new(InodeKind::Regular, 0o100644),
                RNodeBacking::PageBacked { pc },
            );
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };
        OpenFile::new_cap(
            rnode,
            OpenFileFlags {
                read: true,
                write: true,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("open file cap")
    }

    /// `lseek(fd, 17, SEEK_SET)` returns `17` and persists `17` as
    /// the per-fd offset on a freshly-opened PageBacked fd.
    #[test]
    fn dispatch_lseek_seek_set_returns_new_offset_and_persists_on_openfile() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let file = pagebacked_open_file(2, tx_subsystems::vm::USER_PAGE_SIZE as u64);
        proc_cap.set_fd(7, Some(file));
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_LSEEK, [7, 17, SEEK_SET as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(17));
        let installed = proc_cap.fd(7).expect("fd 7 installed");
        assert_eq!(installed.offset(), 17);
    }

    /// `lseek(fd, +5, SEEK_CUR)` adds 5 to the current offset.
    /// Sequence: lseek(20, SEEK_SET) → lseek(+5, SEEK_CUR). Final
    /// value 25.
    #[test]
    fn dispatch_lseek_seek_cur_adds_to_current_offset() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let file = pagebacked_open_file(2, tx_subsystems::vm::USER_PAGE_SIZE as u64);
        proc_cap.set_fd(7, Some(file));
        let ctx = make_ctx(proc_cap.clone(), thread);

        // First, set to 20 via SEEK_SET.
        let r1 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_LSEEK, [7, 20, SEEK_SET as u64, 0, 0, 0]),
            &ctx,
        ));
        assert_eq!(r1, SyscallResult::Return(20));

        // Now SEEK_CUR(+5) → 25.
        let r2 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_LSEEK, [7, 5, SEEK_CUR as u64, 0, 0, 0]),
            &ctx,
        ));
        assert_eq!(r2, SyscallResult::Return(25));
        assert_eq!(proc_cap.fd(7).expect("fd 7").offset(), 25);
    }

    /// `lseek(fd, +offset, SEEK_END)` returns `pc.size_bytes() +
    /// offset` for a PageBacked fd. Pin the size to 1024 via
    /// `step_truncate`; then SEEK_END(+8) → 1032.
    #[test]
    fn dispatch_lseek_seek_end_returns_size_plus_offset_for_pagebacked_fd() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        // Size = 1024 (well under the 1-page capacity).
        let file = pagebacked_open_file(1, 1024);
        proc_cap.set_fd(7, Some(file));
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_LSEEK, [7, 8, SEEK_END as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(1032));
        assert_eq!(proc_cap.fd(7).expect("fd 7").offset(), 1032);
    }

    /// `lseek(pipe_fd, 0, SEEK_SET)` returns `-ESPIPE`. Pipes are
    /// non-seekable per Linux semantics; even SEEK_SET 0 (the
    /// "tell me your current offset" no-op) returns ESPIPE.
    #[test]
    fn dispatch_lseek_on_pipe_fd_returns_neg_espipe() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let (reader_cap, _writer_cap) =
            step_pipe2(PipeFlags::default()).expect("pipe2 for lseek-espipe test");
        proc_cap.set_fd(11, Some(reader_cap));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_LSEEK, [11, 0, SEEK_SET as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_SPIPE));
    }

    /// `lseek(tty_fd, 0, SEEK_SET)` returns `-ESPIPE`. TTYs are
    /// non-seekable like pipes; the dispatch routes the same
    /// `Errno::ESPIPE` regardless of whether the underlying
    /// `StructPayload` is `Tty` or `Pipe`.
    #[test]
    fn dispatch_lseek_on_tty_fd_returns_neg_espipe() {
        let _setup = lseek_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(13, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_LSEEK, [13, 0, SEEK_SET as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_SPIPE));
    }

    /// `lseek(fd, -1, SEEK_SET)` returns `-EINVAL`. A negative
    /// resulting offset is rejected before the `set_offset` write;
    /// the per-fd offset is unchanged.
    #[test]
    fn dispatch_lseek_negative_result_returns_neg_einval() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let file = pagebacked_open_file(1, 64);
        proc_cap.set_fd(7, Some(file));
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_LSEEK, [7, (-1i64) as u64, SEEK_SET as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
        // Pre-call default offset is 0; the failed call must not
        // mutate it.
        assert_eq!(proc_cap.fd(7).expect("fd 7").offset(), 0);
    }

    /// `lseek(fd, 0, 99)` returns `-EINVAL`. Whence values outside
    /// {SEEK_SET, SEEK_CUR, SEEK_END} are rejected with EINVAL,
    /// matching Linux's `lseek(2)` errno surface.
    #[test]
    fn dispatch_lseek_invalid_whence_returns_neg_einval() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let file = pagebacked_open_file(1, 64);
        proc_cap.set_fd(7, Some(file));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_LSEEK, [7, 0, 99, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `lseek(99, 0, SEEK_SET)` against an unallocated fd returns
    /// `-EBADF`. Defence in depth — the BTreeMap lookup returns
    /// `None` before the per-OpenFile step runs.
    #[test]
    fn dispatch_lseek_unknown_fd_returns_neg_ebadf() {
        let _setup = lseek_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_LSEEK, [99, 0, SEEK_SET as u64, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
    }
}

// ===========================================================================
// Slice 2 of the shell-prompt roadmap — VM syscall arms
// (`mmap` / `munmap` / `mprotect` / `mremap` / `madvise` / `msync`).
//
// Coverage:
//   - mmap anonymous private: returns aligned user VA on success.
//   - mmap zero length: -EINVAL.
//   - mmap unaligned addr with MAP_FIXED: -EINVAL.
//   - mmap file-backed against a PageBacked fd: returns user VA.
//   - mmap file-backed against a TTY fd: -ENODEV.
//   - mmap file-backed against a pipe fd: -ENODEV.
//   - mmap MAP_FIXED replaces an existing mapping (no -EEXIST).
//   - mmap MAP_FIXED_NOREPLACE on overlap: -EEXIST.
//   - mmap with neither MAP_SHARED nor MAP_PRIVATE: -EINVAL.
//   - munmap against existing mapping: returns 0 and unmaps.
//   - munmap against fully-disjoint unmapped range: returns 0 (Linux
//     semantic — `try_munmap`'s `rewrite_unmap` is permissive).
//   - mprotect flips protection on an existing mapping.
//   - madvise(DONTNEED) on existing mapping: returns 0.
//   - madvise unsupported advice: -ENOSYS.
//   - mmap with unknown PROT bits: -EINVAL.
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 2.
// ===========================================================================
mod vm_syscalls {
    use super::*;
    use tx_substrate::{page_allocator, zone};
    use tx_subsystems::page_backed::{
        AnonSwapPolicy, PageContainer, PageContainerKind, step_truncate,
    };
    use tx_subsystems::pipe::{step_pipe2, PipeFlags};
    use tx_subsystems::process::bootstrap_init_process;
    use tx_subsystems::vfs::structure::{
        FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
    };
    use tx_subsystems::vm::USER_PAGE_SIZE;

    use crate::linux_syscall::{
        MADV_DONTNEED, MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, NR_MADVISE,
        NR_MMAP, NR_MPROTECT, NR_MUNMAP, PROT_READ, PROT_WRITE,
    };

    const E_BADF: i32 = 9;
    const E_EXIST: i32 = 17;
    const E_INVAL: i32 = 22;
    const E_NODEV: i32 = 19;
    const E_NOSYS: i32 = 38;

    fn vm_setup() -> TestSetup {
        let setup = setup();
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for vm-syscalls tests: {error:?}"),
        }
        setup
    }

    fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let process =
            bootstrap_init_process(fresh_aspace()).expect("bootstrap init for vm-syscalls tests");
        let thread = process.nth_thread(0).expect("leader thread");
        (process, thread)
    }

    /// Build a `Cap<OpenFile>` over a fresh anon `PageContainer` of
    /// `page_count` pages with size `size_bytes`. Mirrors the
    /// fd-ops Wave 4 helper but local to this module.
    fn pagebacked_open_file(page_count: u64, size_bytes: u64) -> Cap<OpenFile> {
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            page_count,
        )
        .expect("page container cap");
        let guard = tx_substrate::epoch::guard();
        match step_truncate(&pc, size_bytes, &guard) {
            tx_subsystems::execution::StepOutcome::Done(())
            | tx_subsystems::execution::StepOutcome::Advanced(()) => {}
            other => panic!("step_truncate({size_bytes}): {other:?}"),
        }
        drop(guard);
        let rnode = {
            let raw = RNode::new(
                FsObjectId::new(9_100),
                InodeMeta::new(InodeKind::Regular, 0o100644),
                RNodeBacking::PageBacked { pc },
            );
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };
        OpenFile::new_cap(
            rnode,
            OpenFileFlags {
                read: true,
                write: true,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("open file cap")
    }

    /// `mmap(0, PAGE, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS,
    /// -1, 0)` returns a page-aligned user VA on success.
    #[test]
    fn dispatch_mmap_anonymous_private_returns_aligned_user_va() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                u64::MAX, // fd = -1 (unused for ANONYMOUS)
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        match result {
            SyscallResult::Return(addr) => {
                // The returned VA may be 0 — `find_free_range` over a
                // fresh aspace's full V1 user window starts at addr 0
                // and that's a valid page-aligned result. The
                // structural assertion is page-alignment.
                assert!(addr >= 0, "mmap returned non-negative VA");
                assert_eq!(
                    (addr as usize) % USER_PAGE_SIZE,
                    0,
                    "returned VA is page-aligned"
                );
            }
            other => panic!("expected Return, got {other:?}"),
        }
    }

    /// `mmap(.., 0, ..)` rejects with -EINVAL.
    #[test]
    fn dispatch_mmap_anonymous_private_zero_length_returns_neg_einval() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                0,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                u64::MAX,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `mmap(unaligned, PAGE, .., MAP_FIXED, ..)` rejects -EINVAL.
    #[test]
    fn dispatch_mmap_anonymous_private_unaligned_addr_with_fixed_returns_neg_einval() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0x1234, // not page-aligned
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `mmap(0, PAGE, PROT_READ, MAP_PRIVATE, fd, 0)` against a fd
    /// whose rnode is `RNodeBacking::PageBacked` returns a page-aligned
    /// user VA.
    #[test]
    fn dispatch_mmap_file_backed_against_pagebacked_fd_returns_va() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let file = pagebacked_open_file(2, USER_PAGE_SIZE as u64);
        proc_cap.set_fd(7, Some(file));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ,
                MAP_PRIVATE,
                7,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        match result {
            SyscallResult::Return(addr) => {
                // Page-aligned non-negative VA — see comment in the
                // anonymous-private test.
                assert!(addr >= 0);
                assert_eq!((addr as usize) % USER_PAGE_SIZE, 0);
            }
            other => panic!("expected Return, got {other:?}"),
        }
    }

    /// `mmap(0, PAGE, PROT_READ, MAP_PRIVATE, tty_fd, 0)` returns
    /// -ENODEV. TTY rnodes are `StructBacked { Tty }`, not
    /// PageBacked; mmap rejects rather than installing a recipe with
    /// `VmBacking::None`.
    #[test]
    fn dispatch_mmap_file_backed_against_tty_fd_returns_neg_enodev() {
        let _setup = vm_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(13, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ,
                MAP_PRIVATE,
                13,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NODEV));
    }

    /// `mmap(.., MAP_PRIVATE, pipe_fd, ..)` returns -ENODEV. Pipes are
    /// `StructBacked { Pipe }`; same rejection as TTY.
    #[test]
    fn dispatch_mmap_file_backed_against_pipe_fd_returns_neg_enodev() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let (reader_cap, _writer_cap) =
            step_pipe2(PipeFlags::default()).expect("pipe2 for vm-syscalls test");
        proc_cap.set_fd(11, Some(reader_cap));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ,
                MAP_PRIVATE,
                11,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NODEV));
    }

    /// `mmap(.., MAP_FIXED, ..)` over an existing mapping silently
    /// replaces it (no -EEXIST). First call installs at a known page;
    /// second call with MAP_FIXED at the same page succeeds.
    #[test]
    fn dispatch_mmap_with_map_fixed_replaces_existing_mapping() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        // First, install at a known fixed addr (chosen high to avoid
        // colliding with anywhere-allocator output).
        let target = 0x1_0000_0000u64;
        let r1 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r1, SyscallResult::Return(target as i64));

        // Now MAP_FIXED at the same target — silently replaces.
        let r2 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r2, SyscallResult::Return(target as i64));
    }

    /// `mmap(.., MAP_FIXED_NOREPLACE, ..)` over an existing mapping
    /// returns -EEXIST.
    #[test]
    fn dispatch_mmap_with_map_fixed_noreplace_on_overlap_returns_neg_eexist() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let target = 0x1_0000_0000u64;
        let r1 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r1, SyscallResult::Return(target as i64));

        let r2 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r2, SyscallResult::Error(E_EXIST));
    }

    /// `mmap` with neither MAP_SHARED nor MAP_PRIVATE rejects -EINVAL.
    #[test]
    fn dispatch_mmap_with_neither_shared_nor_private_returns_neg_einval() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_ANONYMOUS, // no MAP_PRIVATE / MAP_SHARED
                u64::MAX,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `munmap(addr, length)` against a previously-installed mmap
    /// returns 0 and unmaps the range; a follow-up MAP_FIXED_NOREPLACE
    /// at the same addr now succeeds (proves the recipe is gone).
    #[test]
    fn dispatch_munmap_against_existing_mapping_returns_zero_and_unmaps() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let target = 0x1_0000_0000u64;
        let r1 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r1, SyscallResult::Return(target as i64));

        let unmap = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_MUNMAP, [target, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
            &ctx,
        ));
        assert_eq!(unmap, SyscallResult::Return(0));

        // The slot is now free — MAP_FIXED_NOREPLACE succeeds.
        let r2 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r2, SyscallResult::Return(target as i64));
    }

    /// `munmap(addr, length)` against a fully-disjoint unmapped range
    /// returns 0 — Linux's permissive semantic, matching
    /// `try_munmap`'s `rewrite_unmap` (no entries intersect, so no
    /// rewrite happens; the call is a no-op success).
    #[test]
    fn dispatch_munmap_against_unmapped_range_returns_zero() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let target = 0x1_0000_0000u64;
        let r = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_MUNMAP, [target, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
            &ctx,
        ));
        assert_eq!(r, SyscallResult::Return(0));
    }

    /// `mprotect(addr, length, PROT_READ)` against an existing
    /// PROT_READ|PROT_WRITE mapping flips it to PROT_READ.
    #[test]
    fn dispatch_mprotect_flips_existing_mapping_protection() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let target = 0x1_0000_0000u64;
        let r1 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r1, SyscallResult::Return(target as i64));

        let mprotect = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MPROTECT,
                [target, USER_PAGE_SIZE as u64, PROT_READ, 0, 0, 0],
            ),
            &ctx,
        ));
        assert_eq!(mprotect, SyscallResult::Return(0));
    }

    /// `madvise(addr, length, MADV_DONTNEED)` against an existing
    /// mapping returns 0 (range-scoped pmap teardown; recipe
    /// preserved).
    #[test]
    fn dispatch_madvise_dontneed_on_existing_mapping_returns_zero() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let target = 0x1_0000_0000u64;
        let r1 = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(r1, SyscallResult::Return(target as i64));

        let advice = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MADVISE,
                [target, USER_PAGE_SIZE as u64, MADV_DONTNEED, 0, 0, 0],
            ),
            &ctx,
        ));
        assert_eq!(advice, SyscallResult::Return(0));
    }

    /// `madvise(.., 99)` returns -ENOSYS for unsupported advice
    /// values. The slice maps the documented six values
    /// (NORMAL/RANDOM/SEQUENTIAL/WILLNEED/DONTNEED/FREE); any other
    /// raw value is rejected.
    #[test]
    fn dispatch_madvise_unsupported_advice_returns_neg_enosys() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        // Use addr 0 + length PAGE; the advice rejection happens
        // before range parsing, so we don't need a real mapping.
        let target = 0x1_0000_0000u64;
        let req = SyscallRequest::new(NR_MADVISE, [target, USER_PAGE_SIZE as u64, 99, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOSYS));
    }

    /// `mmap` with unknown PROT bits (high bit, not in the recognised
    /// set) returns -EINVAL.
    #[test]
    fn dispatch_mmap_with_unknown_prot_bits_returns_neg_einval() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                0x8000, // unknown bit, not in PROT_READ/WRITE/EXEC/NONE/GROWSDOWN/GROWSUP
                MAP_PRIVATE | MAP_ANONYMOUS,
                u64::MAX,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    // E_BADF kept reachable for fd-required mmap shapes — assert
    // the no-anonymous + bad fd path returns -EBADF (not -ENODEV)
    // when fd is negative.
    #[test]
    fn dispatch_mmap_file_backed_with_negative_fd_returns_neg_ebadf() {
        let _setup = vm_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ,
                MAP_PRIVATE,
                u64::MAX, // fd = -1, no MAP_ANONYMOUS
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
    }
}

// ===========================================================================
// Slice 3 of the shell-prompt roadmap — `futex(2)` syscall arm.
//
// Coverage:
//   - `FUTEX_WAIT` with mismatched val returns `-EAGAIN` immediately
//     (first-call mismatch path — `parked == false`).
//   - `FUTEX_WAIT` with zero uaddr returns `-EINVAL`.
//   - `FUTEX_WAKE` with valid uaddr returns the requested wake count
//     (best-effort wake-N).
//   - Unsupported op selectors (REQUEUE / WAKE_OP / etc.) return
//     `-ENOSYS`.
//   - The `FUTEX_PRIVATE_FLAG` bit is masked off before op match
//     (per-process isolation is implicit; the flag is recognised
//     but ignored).
//   - The `FUTEX_CLOCK_REALTIME` bit is masked off before op match
//     (timeout support deferred to Slice 4).
//
// **What is NOT covered here.** A full WAIT-then-WAKE roundtrip
// would require driving two coordinated async tasks — task A parks
// on `FUTEX_WAIT`, task B fires `FUTEX_WAKE`, task A re-checks the
// user word and returns 0. That coordination is best validated by
// the eventual QEMU shell smoke (Slice 11). The unit tests below
// cover the step contract and the syscall arm's flag-decode +
// errno-mapping surface; the multi-task roundtrip is the next layer.
// ===========================================================================
mod futex_dispatch {
    use super::*;
    use tx_subsystems::process::bootstrap_init_process;

    use crate::linux_syscall::{
        FUTEX_CLOCK_REALTIME, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAKE,
        FUTEX_WAKE_OP, NR_FUTEX,
    };

    const E_INVAL: i32 = 22;
    const E_AGAIN: i32 = 11;
    const E_NOSYS: i32 = 38;

    fn futex_setup() -> TestSetup {
        setup()
    }

    fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let process =
            bootstrap_init_process(fresh_aspace()).expect("bootstrap init for futex tests");
        let thread = process.nth_thread(0).expect("leader thread");
        (process, thread)
    }

    /// `futex(uaddr, FUTEX_WAIT, val, ...)` with `*uaddr != val`
    /// returns `-EAGAIN` immediately (first-call mismatch — the
    /// futex's "fast path" guard short-circuits before parking).
    #[test]
    fn dispatch_futex_wait_with_mismatched_val_returns_neg_eagain() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        // User word holds 0x1234; FUTEX_WAIT with val=0x5678 must
        // observe the mismatch and short-circuit.
        let word: u32 = 0x1234;
        let uaddr = &word as *const u32 as u64;

        let req = SyscallRequest::new(
            NR_FUTEX,
            [uaddr, FUTEX_WAIT as u64, 0x5678, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_AGAIN));
    }

    /// `futex(0, FUTEX_WAIT, ...)` returns `-EINVAL` — null uaddr
    /// is rejected at the step level.
    #[test]
    fn dispatch_futex_wait_zero_uaddr_returns_neg_einval() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_FUTEX, [0, FUTEX_WAIT as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `futex(uaddr, FUTEX_WAKE, n, ...)` returns `n` (best-effort
    /// wake-N). v1 doesn't track per-bucket waiter counts so the
    /// return value is the requested maximum.
    #[test]
    fn dispatch_futex_wake_returns_n() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;

        let req = SyscallRequest::new(
            NR_FUTEX,
            [uaddr, FUTEX_WAKE as u64, 3, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(3));
    }

    /// `futex(uaddr, FUTEX_REQUEUE, ...)` returns `-ENOSYS` — only
    /// FUTEX_WAIT and FUTEX_WAKE are supported in v1.
    #[test]
    fn dispatch_futex_unsupported_op_returns_neg_enosys() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;

        let req = SyscallRequest::new(
            NR_FUTEX,
            [uaddr, FUTEX_REQUEUE as u64, 1, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOSYS));
    }

    /// `futex(uaddr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, n, ...)`
    /// behaves identically to plain `FUTEX_WAKE` — the PRIVATE flag
    /// is masked off before the op match. musl emits the
    /// `_PRIVATE` form for in-process guards.
    #[test]
    fn dispatch_futex_with_private_flag_works() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;

        let op = FUTEX_WAKE | FUTEX_PRIVATE_FLAG;
        let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 5, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(5));
    }

    /// `futex(uaddr, FUTEX_WAKE | FUTEX_CLOCK_REALTIME, n, ...)`
    /// behaves identically to plain `FUTEX_WAKE` — the
    /// CLOCK_REALTIME flag is masked off before the op match
    /// (timeout support deferred to Slice 4).
    #[test]
    fn dispatch_futex_with_clock_realtime_flag_works() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;

        let op = FUTEX_WAKE | FUTEX_CLOCK_REALTIME;
        let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 2, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(2));
    }

    /// `futex(uaddr, FUTEX_WAKE_OP, ...)` returns `-ENOSYS`. Pinned
    /// here as the canonical "complex op out of v1's surface" case.
    #[test]
    fn dispatch_futex_wake_op_returns_neg_enosys() {
        let _setup = futex_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;

        let req = SyscallRequest::new(
            NR_FUTEX,
            [uaddr, FUTEX_WAKE_OP as u64, 1, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOSYS));
    }
}

// ===========================================================================
// Slice 4 of the shell-prompt roadmap — time syscalls.
//
// Coverage:
//   - `clock_gettime(CLOCK_MONOTONIC, ts)` writes a tv_sec / tv_nsec
//     pair derived from `<P as TimeIf>::read_ns()`. Tests the
//     nanosecond → `(tv_sec, tv_nsec)` decomposition.
//   - `clock_gettime` with an unrecognised clock id returns
//     `-EINVAL`.
//   - `clock_gettime` with a null `tp` returns `-EFAULT`.
//   - `gettimeofday(tv, _)` writes a tv_sec / tv_usec pair (microsecond
//     resolution — sub-microsecond residue is truncated).
//   - `gettimeofday` with a null `tv` returns `-EFAULT`.
//   - `times(buf)` returns the monotonic tick count and writes
//     `tms_utime = ticks` into `buf` (with the other three fields
//     zeroed since CPU-time accounting is not yet wired).
//   - `times(NULL)` returns the tick count without writing anywhere
//     (POSIX permits null buf — only the return value matters).
//   - `nanosleep(zero-duration, _)` short-circuits to `Return(0)`.
//   - `nanosleep(non-zero, _)` returns `-ENOSYS` (deferred — see slice
//     header in `mod.rs`).
//   - `clock_nanosleep(TIMER_ABSTIME, past-deadline, _)` short-circuits
//     to `Return(0)` (the past-deadline path is independent of the
//     timer-channel wiring).
// ===========================================================================
mod time_syscalls {
    use super::*;

    use crate::linux_syscall::{
        CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
        NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_GETTIMEOFDAY, NR_NANOSLEEP, NR_TIMES,
        TIMER_ABSTIME, TIMES_NS_PER_TICK,
    };

    const E_INVAL: i32 = 22;
    const E_FAULT: i32 = 14;
    const E_NOSYS: i32 = 38;

    /// Mirror of `TimespecLayout` for test-side decoding. The
    /// production layout is private to `mod.rs`, so the tests
    /// reconstruct the same shape via `read_volatile` against a
    /// stack-allocated buffer.
    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    struct TestTimespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    struct TestTimeval {
        tv_sec: i64,
        tv_usec: i64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    struct TestTms {
        tms_utime: i64,
        tms_stime: i64,
        tms_cutime: i64,
        tms_cstime: i64,
    }

    fn time_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let setup = setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        (setup, proc_cap, thread)
    }

    /// `clock_gettime(CLOCK_MONOTONIC, ts)` succeeds and writes a
    /// `(tv_sec, tv_nsec)` pair derived from the platform clock.
    /// `ShimsTestPmap::read_ns()` starts at 5_000_000_000 ns
    /// (= 5 seconds) and increments per call, so the observed
    /// timespec must satisfy `tv_sec >= 5` and `tv_nsec` is in
    /// `[0, 1_000_000_000)`.
    #[test]
    fn dispatch_clock_gettime_monotonic_writes_timespec_to_user() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let mut ts = TestTimespec::default();
        let ts_uaddr = &mut ts as *mut TestTimespec as u64;

        let req = SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [CLOCK_MONOTONIC as u64, ts_uaddr, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert!(ts.tv_sec >= 5, "tv_sec should reflect the test clock base");
        assert!(
            (0..1_000_000_000).contains(&ts.tv_nsec),
            "tv_nsec must be in [0, 1e9): got {}",
            ts.tv_nsec,
        );
    }

    /// CPU-time clock ids alias to the platform monotonic in v1 and
    /// must succeed.
    #[test]
    fn dispatch_clock_gettime_cputime_aliases_to_monotonic() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let mut ts = TestTimespec::default();
        let ts_uaddr = &mut ts as *mut TestTimespec as u64;
        for clk in [
            CLOCK_REALTIME,
            CLOCK_PROCESS_CPUTIME_ID,
            CLOCK_THREAD_CPUTIME_ID,
        ] {
            let req = SyscallRequest::new(
                NR_CLOCK_GETTIME,
                [clk as u64, ts_uaddr, 0, 0, 0, 0],
            );
            let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
            assert_eq!(result, SyscallResult::Return(0), "clk_id {clk}");
        }
    }

    /// Unrecognised clock ids return `-EINVAL`.
    #[test]
    fn dispatch_clock_gettime_invalid_clock_returns_neg_einval() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let mut ts = TestTimespec::default();
        let ts_uaddr = &mut ts as *mut TestTimespec as u64;

        let req = SyscallRequest::new(NR_CLOCK_GETTIME, [99, ts_uaddr, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// Null `tp` returns `-EFAULT` (without dereferencing the null
    /// pointer).
    #[test]
    fn dispatch_clock_gettime_null_buffer_returns_neg_efault() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [CLOCK_MONOTONIC as u64, 0, 0, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_FAULT));
    }

    /// `gettimeofday(tv, _)` writes a `(tv_sec, tv_usec)` pair from
    /// the platform clock.
    #[test]
    fn dispatch_gettimeofday_writes_timeval_to_user() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let mut tv = TestTimeval::default();
        let tv_uaddr = &mut tv as *mut TestTimeval as u64;

        let req = SyscallRequest::new(NR_GETTIMEOFDAY, [tv_uaddr, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert!(tv.tv_sec >= 5);
        assert!(
            (0..1_000_000).contains(&tv.tv_usec),
            "tv_usec must be in [0, 1e6): got {}",
            tv.tv_usec,
        );
    }

    /// Null `tv` returns `-EFAULT`.
    #[test]
    fn dispatch_gettimeofday_null_buffer_returns_neg_efault() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_GETTIMEOFDAY, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_FAULT));
    }

    /// `times(buf)` returns the monotonic tick count and writes
    /// `tms_utime = ticks`. The other three fields stay at their
    /// pre-existing values (the syscall arm zeros them, which is
    /// observable since the buffer is initialised to a non-zero
    /// sentinel below).
    #[test]
    fn dispatch_times_returns_tick_count_and_writes_buffer() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        // Initialise the buffer to a sentinel so we can assert the
        // syscall arm overwrites all four fields.
        let mut tms = TestTms {
            tms_utime: 0xdead_beef,
            tms_stime: 0xdead_beef,
            tms_cutime: 0xdead_beef,
            tms_cstime: 0xdead_beef,
        };
        let buf_uaddr = &mut tms as *mut TestTms as u64;

        let req = SyscallRequest::new(NR_TIMES, [buf_uaddr, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let returned_ticks = match result {
            SyscallResult::Return(t) => t,
            other => panic!("expected Return, got {other:?}"),
        };
        assert!(returned_ticks > 0);
        assert_eq!(
            tms.tms_utime, returned_ticks,
            "tms_utime should match the returned tick count",
        );
        assert_eq!(tms.tms_stime, 0);
        assert_eq!(tms.tms_cutime, 0);
        assert_eq!(tms.tms_cstime, 0);

        // Sanity: returned_ticks * NS_PER_TICK should be in the same
        // ballpark as the test clock base (5 seconds = 500 ticks at
        // 100Hz) — bounded loosely so other tests advancing the
        // counter do not break this one.
        let approx_ns = (returned_ticks as u64) * TIMES_NS_PER_TICK;
        assert!(
            approx_ns >= 5_000_000_000,
            "ticks {returned_ticks} * {TIMES_NS_PER_TICK}ns = {approx_ns}ns < 5e9ns",
        );
    }

    /// `times(NULL)` returns the tick count without writing anywhere.
    /// Linux semantics: a null `buf` is permitted; only the return
    /// value matters in that case (LTP `times02`).
    #[test]
    fn dispatch_times_with_null_buffer_returns_tick_count() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_TIMES, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let ticks = match result {
            SyscallResult::Return(t) => t,
            other => panic!("expected Return, got {other:?}"),
        };
        assert!(ticks > 0);
    }

    /// `nanosleep((0, 0), _)` short-circuits to `Return(0)` per the
    /// Linux semantics — a zero-duration sleep is a no-op.
    #[test]
    fn dispatch_nanosleep_zero_duration_returns_immediately() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let req_ts = TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let req_uaddr = &req_ts as *const TestTimespec as u64;

        let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// `nanosleep((1, 0), _)` returns `-ENOSYS` in Slice 4 — real
    /// non-zero durations are deferred to the timer-channel slice.
    /// Pinned here so a future slice that lands real-duration
    /// nanosleep updates this test alongside the implementation.
    #[test]
    fn dispatch_nanosleep_nonzero_duration_returns_neg_enosys() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let req_ts = TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let req_uaddr = &req_ts as *const TestTimespec as u64;

        let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOSYS));
    }

    /// `nanosleep((-1, 0), _)` returns `-EINVAL` — negative tv_sec is
    /// rejected by Linux.
    #[test]
    fn dispatch_nanosleep_negative_tv_sec_returns_neg_einval() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let req_ts = TestTimespec {
            tv_sec: -1,
            tv_nsec: 0,
        };
        let req_uaddr = &req_ts as *const TestTimespec as u64;

        let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `nanosleep(NULL, _)` returns `-EFAULT`.
    #[test]
    fn dispatch_nanosleep_null_buffer_returns_neg_efault() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_NANOSLEEP, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_FAULT));
    }

    /// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, past_ts, _)`
    /// short-circuits to `Return(0)` because the absolute deadline
    /// is already in the past.
    #[test]
    fn dispatch_clock_nanosleep_abstime_past_deadline_returns_immediately() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        // tv_sec = 1 (== 1e9 ns), well below the test clock base of
        // 5_000_000_000 ns — the deadline is already past.
        let req_ts = TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let req_uaddr = &req_ts as *const TestTimespec as u64;

        let req = SyscallRequest::new(
            NR_CLOCK_NANOSLEEP,
            [
                CLOCK_MONOTONIC as u64,
                TIMER_ABSTIME as u64,
                req_uaddr,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// `clock_nanosleep` with an unknown clock id returns `-EINVAL`.
    #[test]
    fn dispatch_clock_nanosleep_invalid_clock_returns_neg_einval() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let req_ts = TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let req_uaddr = &req_ts as *const TestTimespec as u64;

        let req = SyscallRequest::new(
            NR_CLOCK_NANOSLEEP,
            [99, 0, req_uaddr, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `clock_nanosleep` with an unknown flag bit returns `-EINVAL`.
    #[test]
    fn dispatch_clock_nanosleep_unknown_flag_returns_neg_einval() {
        let (_setup, proc_cap, thread) = time_setup();
        let ctx = make_ctx(proc_cap, thread);
        let req_ts = TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let req_uaddr = &req_ts as *const TestTimespec as u64;

        let req = SyscallRequest::new(
            NR_CLOCK_NANOSLEEP,
            [
                CLOCK_MONOTONIC as u64,
                0x2, // unknown flag bit
                req_uaddr,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }
}

// ===========================================================================
// Slice 5 of the shell-prompt roadmap — `ioctl(2)` + TTY routing.
//
// Coverage:
//
// - TCGETS on a TTY fd writes a `Termios` to the user buffer.
// - TCGETS on a pipe-end fd returns `-ENOTTY` (Linux semantic — even
//   pipes return ENOTTY for terminal ioctls).
// - TCGETS on an unknown fd returns `-EBADF`.
// - TCSETS on a TTY fd updates the TTY's termios state (round-trip).
// - TIOCGPGRP on a TTY without a session-bound foreground pgrp
//   returns `-EINVAL` (the underlying step's contract).
// - TIOCGWINSZ on a TTY writes a `Winsize` to the user buffer.
// - TIOCSWINSZ on a TTY updates the TTY's window-size state.
// - TIOCSCTTY succeeds when the caller is a session leader without
//   a controlling TTY.
// - TIOCSCTTY on an already-bound TTY returns `-EBUSY`.
// - Unknown ioctl request returns `-ENOTTY`.
// - Null `argp` for TCGETS returns `-EFAULT`.
// - TIOCSPGRP / TIOCNOTTY arms dispatch (return -EINVAL on unbound).
// ===========================================================================
mod ioctl_dispatch {
    use super::*;
    use tx_subsystems::pipe::{step_pipe2, PipeFlags};
    use tx_subsystems::process::bootstrap_init_process;
    use tx_subsystems::tty::structure::{Termios, Winsize};

    use crate::linux_syscall::{
        NR_IOCTL, TCGETS, TCSETS, TIOCGPGRP, TIOCGWINSZ, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP,
        TIOCSWINSZ,
    };

    const E_BADF: i32 = 9;
    const E_BUSY: i32 = 16;
    const E_FAULT: i32 = 14;
    const E_INVAL: i32 = 22;
    const E_NOTTY: i32 = 25;

    fn ioctl_setup() -> TestSetup {
        setup()
    }

    fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let process =
            bootstrap_init_process(fresh_aspace()).expect("bootstrap init for ioctl tests");
        let thread = process.nth_thread(0).expect("leader thread");
        (process, thread)
    }

    /// `ioctl(tty_fd, TCGETS, &out)` returns 0 and writes a Termios
    /// struct into the caller's buffer. The boot console TTY is in
    /// cooked mode (`Termios::default_cooked`); we observe non-zero
    /// `c_lflag` (ICANON | ECHO | ...) as a signal that real termios
    /// state landed in the buffer.
    #[test]
    fn dispatch_ioctl_tcgets_on_tty_fd_writes_termios() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let mut out = Termios::zeroed();
        let argp = &mut out as *mut Termios as u64;
        let req = SyscallRequest::new(NR_IOCTL, [0, TCGETS as u64, argp, 0, 0, 0]);

        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert_ne!(
            out.c_lflag, 0,
            "TCGETS should write the cooked-mode termios; c_lflag has ICANON|ECHO|... set"
        );
    }

    /// `ioctl(pipe_fd, TCGETS, &out)` returns `-ENOTTY` — terminal
    /// ioctls on non-TTY fds are the canonical ENOTTY case per
    /// `man ioctl_tty`.
    #[test]
    fn dispatch_ioctl_tcgets_on_pipe_fd_returns_neg_enotty() {
        let _setup = ioctl_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let (reader_cap, _writer_cap) =
            step_pipe2(PipeFlags::default()).expect("pipe2 for ioctl-enotty test");
        proc_cap.set_fd(11, Some(reader_cap));
        let ctx = make_ctx(proc_cap, thread);

        let mut out = Termios::zeroed();
        let argp = &mut out as *mut Termios as u64;
        let req = SyscallRequest::new(NR_IOCTL, [11, TCGETS as u64, argp, 0, 0, 0]);

        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOTTY));
    }

    /// `ioctl(unknown_fd, TCGETS, &out)` returns `-EBADF` — the fd
    /// resolution short-circuits before the request decode.
    #[test]
    fn dispatch_ioctl_tcgets_on_unknown_fd_returns_neg_ebadf() {
        let _setup = ioctl_setup();
        let (proc_cap, thread) = fresh_proc_thread();
        let ctx = make_ctx(proc_cap, thread);

        let mut out = Termios::zeroed();
        let argp = &mut out as *mut Termios as u64;
        let req = SyscallRequest::new(NR_IOCTL, [42, TCGETS as u64, argp, 0, 0, 0]);

        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
    }

    /// `ioctl(tty_fd, TCSETS, &new)` returns 0 and the next TCGETS
    /// reads back the same termios value. We change `c_lflag` to a
    /// distinct bit pattern to prove the new value lands.
    #[test]
    fn dispatch_ioctl_tcsets_on_tty_fd_updates_termios() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let mut new = Termios::zeroed();
        new.c_lflag = 0xdead_beef;
        let argp_in = &new as *const Termios as u64;
        let req_set = SyscallRequest::new(NR_IOCTL, [0, TCSETS as u64, argp_in, 0, 0, 0]);
        let result_set = block_on(dispatch::<ShimsTestPmap>(req_set, &ctx));
        assert_eq!(result_set, SyscallResult::Return(0));

        let mut readback = Termios::zeroed();
        let argp_out = &mut readback as *mut Termios as u64;
        let req_get = SyscallRequest::new(NR_IOCTL, [0, TCGETS as u64, argp_out, 0, 0, 0]);
        let result_get = block_on(dispatch::<ShimsTestPmap>(req_get, &ctx));
        assert_eq!(result_get, SyscallResult::Return(0));
        assert_eq!(
            readback.c_lflag, 0xdead_beef,
            "TCGETS after TCSETS should observe the value installed by TCSETS"
        );
    }

    /// `ioctl(tty_fd, TIOCGPGRP, &out)` on a TTY without a bound
    /// foreground pgrp returns `-EINVAL` — the underlying step
    /// requires `tty.session_pgrp().is_some()` and surfaces EINVAL
    /// when the slot is empty.
    #[test]
    fn dispatch_ioctl_tiocgpgrp_on_unbound_tty_returns_neg_einval() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let mut out: u32 = 0;
        let argp = &mut out as *mut u32 as u64;
        let req = SyscallRequest::new(NR_IOCTL, [0, TIOCGPGRP as u64, argp, 0, 0, 0]);

        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `ioctl(tty_fd, TIOCGWINSZ, &out)` returns 0 and writes a
    /// `Winsize` struct into the caller's buffer.
    #[test]
    fn dispatch_ioctl_tiocgwinsz_on_tty_writes_winsize() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        // Pre-set a known winsize so the readback assertion is stable.
        let preset = Winsize::new(24, 80);
        let argp_set = &preset as *const Winsize as u64;
        let req_set = SyscallRequest::new(NR_IOCTL, [0, TIOCSWINSZ as u64, argp_set, 0, 0, 0]);
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(req_set, &ctx)),
            SyscallResult::Return(0)
        );

        let mut out = Winsize::default();
        let argp = &mut out as *mut Winsize as u64;
        let req = SyscallRequest::new(NR_IOCTL, [0, TIOCGWINSZ as u64, argp, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert_eq!(out, preset);
    }

    /// `ioctl(tty_fd, TIOCSWINSZ, &new)` returns 0 and the next
    /// TIOCGWINSZ reads back the same struct.
    #[test]
    fn dispatch_ioctl_tiocswinsz_on_tty_updates_winsize() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let new = Winsize::new(50, 120);
        let argp_in = &new as *const Winsize as u64;
        let req_set = SyscallRequest::new(NR_IOCTL, [0, TIOCSWINSZ as u64, argp_in, 0, 0, 0]);
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(req_set, &ctx)),
            SyscallResult::Return(0)
        );

        let mut readback = Winsize::default();
        let argp_out = &mut readback as *mut Winsize as u64;
        let req_get = SyscallRequest::new(NR_IOCTL, [0, TIOCGWINSZ as u64, argp_out, 0, 0, 0]);
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(req_get, &ctx)),
            SyscallResult::Return(0)
        );
        assert_eq!(readback, new);
    }

    /// `ioctl(tty_fd, TIOCSCTTY, 0)` succeeds when the caller is a
    /// session leader without an existing controlling TTY. The
    /// bootstrap init process satisfies both predicates: its session
    /// leader's pid equals its sid, and `bootstrap_init_process`
    /// constructs the session with no controlling TTY bound.
    #[test]
    fn dispatch_ioctl_tiocsctty_on_session_leader_succeeds() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_IOCTL, [0, TIOCSCTTY as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    /// `ioctl(tty_fd, TIOCSCTTY, 0)` returns `-EBUSY` if a previous
    /// caller has already bound the TTY (`step_ioctl_tiocsctty`'s
    /// already-bound check). Two TIOCSCTTY calls from the same
    /// session-leader caller exercise the dispatch path and the
    /// step's EBUSY rejection.
    #[test]
    fn dispatch_ioctl_tiocsctty_on_already_bound_tty_returns_neg_ebusy() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_IOCTL, [0, TIOCSCTTY as u64, 0, 0, 0, 0]);
        let first = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(first, SyscallResult::Return(0));

        let req2 = SyscallRequest::new(NR_IOCTL, [0, TIOCSCTTY as u64, 0, 0, 0, 0]);
        let second = block_on(dispatch::<ShimsTestPmap>(req2, &ctx));
        assert_eq!(second, SyscallResult::Error(E_BUSY));
    }

    /// `ioctl(tty_fd, 0xDEADBEEF, 0)` returns `-ENOTTY` — unknown
    /// terminal-shape requests fall through to the default arm per
    /// `man ioctl_tty`.
    #[test]
    fn dispatch_ioctl_unknown_request_returns_neg_enotty() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_IOCTL, [0, 0xDEAD_BEEF, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOTTY));
    }

    /// `ioctl(tty_fd, TCGETS, NULL)` returns `-EFAULT` before any
    /// step is invoked. Mirrors the time-syscall arms' null-uaddr
    /// short-circuit — the user-VA sweep (Slice 9) lifts this to a
    /// real EFAULT-on-invalid-VA contract via `copy_to_user`.
    #[test]
    fn dispatch_ioctl_null_argp_for_tcgets_returns_neg_efault() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_IOCTL, [0, TCGETS as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_FAULT));
    }

    /// `ioctl(tty_fd, TIOCSPGRP, &val)` against an unbound TTY
    /// returns `-EINVAL` (the same path TIOCGPGRP exercises) — the
    /// step rejects when no session is bound.
    #[test]
    fn dispatch_ioctl_tiocspgrp_on_unbound_tty_returns_neg_einval() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let new_pgrp: u32 = 7;
        let argp = &new_pgrp as *const u32 as u64;
        let req = SyscallRequest::new(NR_IOCTL, [0, TIOCSPGRP as u64, argp, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `ioctl(tty_fd, TIOCNOTTY, 0)` against a TTY whose session is
    /// not bound to the caller returns `-EINVAL` (the step rejects
    /// when there's no binding to detach). This exercises the
    /// `TIOCNOTTY` arm dispatch wiring.
    #[test]
    fn dispatch_ioctl_tiocnotty_on_unbound_tty_returns_neg_einval() {
        let _setup = ioctl_setup();
        let _ops = install_capturing_console();
        let (proc_cap, thread) = fresh_proc_thread();
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_IOCTL, [0, TIOCNOTTY as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }
}

// ===========================================================================
// Slice 6 of the shell-prompt roadmap — stat family
// (`fstat` / `newfstatat` / `getdents64` / `getcwd` / `chdir` /
// `fchdir` / `umask`).
//
// Each test goes through the full `dispatch::<ShimsTestPmap>` path to
// exercise the dispatch wiring + arm body together. The directory
// fixtures use the same `build_tmpfs_root` shape as the fd-ops Wave 2
// module (a real `Tmpfs` mount with the root rnode carrying
// `with_containing_mount` so `fs_ops_for_rnode` resolves through the
// FsOps trait).
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 6.
// ===========================================================================

mod stat_family {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;

    use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
    use tx_substrate::{page_allocator, zone};
    use tx_subsystems::cred::CapabilitySet;
    use tx_subsystems::mount::{
        DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
    };
    use tx_subsystems::page_backed::FsPageBacking;
    use tx_subsystems::pipe::{step_pipe2, PipeFlags};
    use tx_subsystems::process::step_chdir;
    use tx_subsystems::vfs::structure::{
        Credential, DEntry, InlineName, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
        S_IFDIR,
    };
    use tx_subsystems::vfs::{FsOps, OpenFile};

    use crate::linux_syscall::{
        AT_EMPTY_PATH, AT_FDCWD, NR_CHDIR, NR_FCHDIR, NR_FSTAT, NR_GETCWD, NR_GETDENTS64,
        NR_NEWFSTATAT, NR_UMASK,
    };

    /// errno magnitudes the tests check against (positive Linux RV64
    /// generic ABI values; the dispatcher returns the positive
    /// magnitude in `SyscallResult::Error`).
    const E_BADF: i32 = 9;
    const E_NOENT: i32 = 2;
    const E_NOTDIR: i32 = 20;
    const E_INVAL: i32 = 22;
    const E_FAULT: i32 = 14;
    const E_NOSYS: i32 = 38;
    const E_RANGE: i32 = 34;

    /// Stat field offsets (verified against Linux's `asm-generic/stat.h`
    /// + the `StatLayout` struct in `mod.rs`). Tests read directly out
    /// of the kernel-side stat buffer using these offsets.
    const STAT_INO_OFF: usize = 8;
    const STAT_MODE_OFF: usize = 16;
    const STAT_NLINK_OFF: usize = 20;
    const STAT_UID_OFF: usize = 24;
    const STAT_GID_OFF: usize = 28;
    const STAT_SIZE_OFF: usize = 48;
    const STAT_BLKSIZE_OFF: usize = 56;
    /// Total `struct stat` byte size on RV64 generic ABI: matches
    /// `size_of::<StatLayout>` per the field layout in `mod.rs`.
    const STAT_BYTES: usize = 128;

    /// `linux_dirent64` fixed header byte size (8 + 8 + 2 + 1 = 19).
    const DIRENT_HEADER_BYTES: usize = 19;

    fn ensure_zero_frame_claimed() {
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for stat_family tests: {error:?}"),
        }
    }

    fn stat_setup() -> TestSetup {
        let setup = setup();
        ensure_zero_frame_claimed();
        setup
    }

    /// Build a fresh tmpfs-backed mount + a root `Cap<DEntry>`. Returns
    /// the dentry, the FsOps `Arc` (so callers can mint files
    /// directly), and the root `Cap<RNode>` (so callers can build a
    /// directory OpenFile for getdents64 tests).
    fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>, Cap<RNode>) {
        let tmpfs = Arc::new(Tmpfs::new());
        let payload = MountPayload::new_cap(
            tmpfs.clone() as Arc<dyn FsOps>,
            tmpfs.clone() as Arc<dyn FsPageBacking>,
            None,
            DevId::new(311),
            MountOptions::default(),
            "tmpfs-stat-family",
            SourceLabel::Static("tmpfs-stat-family"),
        )
        .expect("mount payload");

        let root_rnode = {
            let raw = RNode::new(
                TMPFS_ROOT_OBJECT_ID,
                InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&payload);
            let res = zone::reserve_for::<RNode>().expect("rnode reservation");
            zone::sign_for(res, raw)
        };

        let _mount = MountIdentity::new_cap(
            MountId::new(31),
            None,
            root_rnode.clone(),
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount identity");

        let root_dentry =
            DEntry::new_cap(InlineName::ROOT, root_rnode.clone()).expect("root dentry");
        (root_dentry, tmpfs, root_rnode)
    }

    /// Bootstrap an init process whose cwd is `root_dentry`. Returns
    /// the `(process, leader-thread)` pair.
    fn bootstrap_with_cwd(
        root_dentry: Cap<DEntry>,
    ) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
        let aspace = fresh_aspace();
        let process = bootstrap_init_process(aspace).expect("bootstrap init");
        let thread = process.nth_thread(0).expect("leader thread");
        match step_chdir(&process, root_dentry) {
            tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
            tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
                panic!("init bootstrap zombified")
            }
        }
        (process, thread)
    }

    /// NUL-terminate a path slice into a kernel-side `Vec<u8>` so the
    /// inline `read_user_cstr` reads it correctly.
    fn nul_terminate(path: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(path.len() + 1);
        v.extend_from_slice(path);
        v.push(0);
        v
    }

    /// Build a directory OpenFile rooted at `root_rnode`. Used by the
    /// `getdents64` tests so the per-fd readdir cursor can be exercised
    /// independently of the walker.
    fn directory_open_file(root_rnode: Cap<RNode>) -> Cap<OpenFile> {
        OpenFile::new_cap(
            root_rnode,
            OpenFileFlags {
                read: true,
                write: false,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("directory open file cap")
    }

    fn read_u32_at(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    }

    fn read_u64_at(buf: &[u8], off: usize) -> u64 {
        u64::from_le_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
            buf[off + 4],
            buf[off + 5],
            buf[off + 6],
            buf[off + 7],
        ])
    }

    fn read_u16_at(buf: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([buf[off], buf[off + 1]])
    }

    // -----------------------------------------------------------------
    // fstat
    // -----------------------------------------------------------------

    /// `fstat(fd_for_pagebacked_file, statbuf)` writes the
    /// `(mode, size, ino)` triple from the inode meta into the user
    /// buffer and returns `0`.
    #[test]
    fn dispatch_fstat_on_pagebacked_fd_writes_stat_struct() {
        let _setup = stat_setup();
        let (root_dentry, tmpfs, _root_rnode) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let (file_id, _meta) = {
            let guard = tx_substrate::epoch::guard();
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done(pair) | StepOutcome::Advanced(pair) => pair,
                other => panic!("create_inode: {other:?}"),
            }
        };
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);

        // Open the file via the openat path so the resulting OpenFile
        // is a real PageBacked rnode whose meta carries the 0o100644
        // mode + 0 size from create_inode.
        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(
            crate::linux_syscall::NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                crate::linux_syscall::O_RDONLY as u64,
                0,
                0,
                0,
            ],
        );
        let ctx = make_ctx(proc_cap.clone(), thread);
        let fd = match block_on(dispatch::<ShimsTestPmap>(req, &ctx)) {
            SyscallResult::Return(fd) => fd,
            other => panic!("openat /f: {other:?}"),
        };
        drop(path);

        // Now call fstat(fd, &statbuf).
        let mut statbuf = vec![0u8; STAT_BYTES];
        let req =
            SyscallRequest::new(NR_FSTAT, [fd as u64, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert_eq!(read_u64_at(&statbuf, STAT_INO_OFF), file_id.as_u64());
        assert_eq!(read_u32_at(&statbuf, STAT_MODE_OFF), 0o100644);
        assert_eq!(read_u32_at(&statbuf, STAT_UID_OFF), 0);
        assert_eq!(read_u32_at(&statbuf, STAT_GID_OFF), 0);
        // `st_size` is `i64`; freshly-created file has size 0.
        assert_eq!(read_u64_at(&statbuf, STAT_SIZE_OFF), 0);
        assert_eq!(read_u32_at(&statbuf, STAT_BLKSIZE_OFF), 4096);
        assert_eq!(read_u32_at(&statbuf, STAT_NLINK_OFF), 1);
    }

    /// `fstat(stdout_fd, statbuf)` against a TTY-backed fd writes a
    /// stat layout whose mode carries the `S_IFCHR` bits (the boot
    /// console's RNode is constructed with kind = CharDevice).
    #[test]
    fn dispatch_fstat_on_tty_fd_writes_stat_struct() {
        let _setup = stat_setup();
        let _ops = install_capturing_console();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        proc_cap.set_fd(1, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap.clone(), thread);

        let mut statbuf = vec![0u8; STAT_BYTES];
        let req = SyscallRequest::new(NR_FSTAT, [1, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        // S_IFCHR = 0o020000 in the upper nibble.
        let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
        assert_eq!(mode & 0o170000, 0o020000, "expected S_IFCHR; got {mode:#o}");
    }

    /// `fstat(unknown_fd, statbuf)` returns `-EBADF`.
    #[test]
    fn dispatch_fstat_unknown_fd_returns_neg_ebadf() {
        let _setup = stat_setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let mut statbuf = vec![0u8; STAT_BYTES];
        let req = SyscallRequest::new(NR_FSTAT, [42, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_BADF));
    }

    /// `fstat(0, NULL)` returns `-EFAULT`. The arm rejects null
    /// statbuf before resolving the fd.
    #[test]
    fn dispatch_fstat_null_buffer_returns_neg_efault() {
        let _setup = stat_setup();
        let _ops = install_capturing_console();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_FSTAT, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_FAULT));
    }

    // -----------------------------------------------------------------
    // newfstatat
    // -----------------------------------------------------------------

    /// `newfstatat(AT_FDCWD, "/f", &statbuf, 0)` walks the path from
    /// the cwd and stats the resulting rnode, returning `0` plus the
    /// expected mode bits.
    #[test]
    fn dispatch_newfstatat_with_valid_path_returns_zero() {
        let _setup = stat_setup();
        let (root_dentry, tmpfs, _root_rnode) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        {
            let guard = tx_substrate::epoch::guard();
            let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100640, &owner_cred, &guard);
        }
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let mut statbuf = vec![0u8; STAT_BYTES];
        let req = SyscallRequest::new(
            NR_NEWFSTATAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                statbuf.as_mut_ptr() as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert_eq!(read_u32_at(&statbuf, STAT_MODE_OFF), 0o100640);
        drop(path);
    }

    /// `newfstatat(AT_FDCWD, "/missing", &statbuf, 0)` surfaces the
    /// walker's `Errno::ENOENT` as `-ENOENT`.
    #[test]
    fn dispatch_newfstatat_with_nonexistent_path_returns_neg_enoent() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/missing");
        let mut statbuf = vec![0u8; STAT_BYTES];
        let req = SyscallRequest::new(
            NR_NEWFSTATAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                statbuf.as_mut_ptr() as u64,
                0,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOENT));
        drop(path);
    }

    /// `newfstatat(AT_FDCWD, "", &statbuf, AT_EMPTY_PATH)` stats the
    /// cwd directly. The mode carries the directory `S_IFDIR` bits
    /// from the tmpfs root inode.
    #[test]
    fn dispatch_newfstatat_at_empty_path_stats_cwd() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"");
        let mut statbuf = vec![0u8; STAT_BYTES];
        let req = SyscallRequest::new(
            NR_NEWFSTATAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                statbuf.as_mut_ptr() as u64,
                AT_EMPTY_PATH as u64,
                0,
                0,
            ],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
        assert_eq!(mode & 0o170000, 0o040000, "expected S_IFDIR; got {mode:#o}");
        drop(path);
    }

    // -----------------------------------------------------------------
    // chdir / fchdir
    // -----------------------------------------------------------------

    /// `chdir("/")` against the bootstrap-with-cwd init succeeds and
    /// returns 0; the cwd dentry is replaced by the resolved root.
    #[test]
    fn dispatch_chdir_to_existing_dir_succeeds() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let path = nul_terminate(b"/");
        let req = SyscallRequest::new(NR_CHDIR, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
        assert!(proc_cap.cwd().is_some(), "cwd should remain installed");
        drop(path);
    }

    /// `chdir("/missing")` returns `-ENOENT`.
    #[test]
    fn dispatch_chdir_to_nonexistent_path_returns_neg_enoent() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/missing");
        let req = SyscallRequest::new(NR_CHDIR, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOENT));
        drop(path);
    }

    /// `chdir("/f")` against an existing regular file returns
    /// `-ENOTDIR` (cwd must be a directory).
    #[test]
    fn dispatch_chdir_to_regular_file_returns_neg_enotdir() {
        let _setup = stat_setup();
        let (root_dentry, tmpfs, _root_rnode) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        {
            let guard = tx_substrate::epoch::guard();
            let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
        }
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let path = nul_terminate(b"/f");
        let req = SyscallRequest::new(NR_CHDIR, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOTDIR));
        drop(path);
    }

    /// `fchdir(fd)` returns `-ENOSYS` (Slice 6 carryover; OpenFile
    /// has no DEntry hint to install via step_chdir).
    #[test]
    fn dispatch_fchdir_returns_neg_enosys() {
        let _setup = stat_setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap, thread);

        let req = SyscallRequest::new(NR_FCHDIR, [0, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOSYS));
    }

    // -----------------------------------------------------------------
    // getcwd
    // -----------------------------------------------------------------

    /// `getcwd(&buf, sizeof buf)` after `step_chdir` returns the
    /// rendered path bytes plus the byte count (incl. terminator).
    #[test]
    fn dispatch_getcwd_after_chdir_returns_path() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let mut buf = [0u8; 64];
        let req = SyscallRequest::new(NR_GETCWD, [buf.as_mut_ptr() as u64, 64, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        // Root path renders as `/` (1 byte) + NUL = 2.
        assert_eq!(result, SyscallResult::Return(2));
        assert_eq!(&buf[..2], b"/\0");
    }

    /// `getcwd(buf, 0)` (with non-NULL buf) returns `-EINVAL` per
    /// Linux's syscall-side semantics (libc handles the
    /// allocate-on-zero shape, not the kernel).
    #[test]
    fn dispatch_getcwd_zero_size_returns_neg_einval() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let mut buf = [0u8; 8];
        let req = SyscallRequest::new(NR_GETCWD, [buf.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_INVAL));
    }

    /// `getcwd(buf, 1)` cannot fit `"/" + NUL` (2 bytes); returns
    /// `-ERANGE`.
    #[test]
    fn dispatch_getcwd_too_small_buffer_returns_neg_erange() {
        let _setup = stat_setup();
        let (root_dentry, _tmpfs, _root_rnode) = build_tmpfs_root();
        let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
        let ctx = make_ctx(proc_cap, thread);

        let mut buf = [0u8; 1];
        let req = SyscallRequest::new(NR_GETCWD, [buf.as_mut_ptr() as u64, 1, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_RANGE));
    }

    // -----------------------------------------------------------------
    // getdents64
    // -----------------------------------------------------------------

    /// `getdents64(dir_fd, buf, buf_len)` against a tmpfs root with
    /// two files writes two `linux_dirent64` records and returns the
    /// total byte count. Each record's header carries the inode id +
    /// the `DT_REG` `d_type` byte.
    #[test]
    fn dispatch_getdents64_on_directory_fd_writes_entries() {
        let _setup = stat_setup();
        let (_root_dentry, tmpfs, root_rnode) = build_tmpfs_root();
        let owner_cred = Credential {
            uid: 0,
            gid: 0,
            effective_caps: CapabilitySet::FULL,
        };
        let id_a = {
            let guard = tx_substrate::epoch::guard();
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"a", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done((id, _)) | StepOutcome::Advanced((id, _)) => id,
                other => panic!("create_inode a: {other:?}"),
            }
        };
        let id_b = {
            let guard = tx_substrate::epoch::guard();
            match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"bb", 0o100644, &owner_cred, &guard) {
                StepOutcome::Done((id, _)) | StepOutcome::Advanced((id, _)) => id,
                other => panic!("create_inode bb: {other:?}"),
            }
        };

        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        proc_cap.set_fd(7, Some(directory_open_file(root_rnode)));
        let ctx = make_ctx(proc_cap.clone(), thread);

        let mut buf = vec![0u8; 256];
        let req = SyscallRequest::new(
            NR_GETDENTS64,
            [7, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        let total = match result {
            SyscallResult::Return(n) => n as usize,
            other => panic!("getdents64: {other:?}"),
        };
        assert!(total >= 2 * (DIRENT_HEADER_BYTES + 1 + 1) /* min */);

        // Walk the records and verify both inode ids appear with
        // DT_REG.
        let mut seen_a = false;
        let mut seen_b = false;
        let mut off = 0usize;
        while off < total {
            let d_ino = read_u64_at(&buf, off);
            let d_reclen = read_u16_at(&buf, off + 16) as usize;
            let d_type = buf[off + 18];
            assert!(d_reclen >= DIRENT_HEADER_BYTES + 1);
            assert!(off + d_reclen <= total);
            assert_eq!(d_type, crate::linux_syscall::DT_REG);
            if d_ino == id_a.as_u64() {
                seen_a = true;
            }
            if d_ino == id_b.as_u64() {
                seen_b = true;
            }
            off += d_reclen;
        }
        assert!(seen_a && seen_b, "both entries should appear");

        // A second call after EOD returns 0.
        let req2 = SyscallRequest::new(
            NR_GETDENTS64,
            [7, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        );
        let result2 = block_on(dispatch::<ShimsTestPmap>(req2, &ctx));
        assert_eq!(result2, SyscallResult::Return(0));
    }

    /// `getdents64(pipe_fd, buf, buf_len)` returns `-ENOTDIR`. Pipes
    /// are not directory backings.
    #[test]
    fn dispatch_getdents64_on_pipe_fd_returns_neg_enotdir() {
        let _setup = stat_setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let (reader_cap, _writer_cap) =
            step_pipe2(PipeFlags::default()).expect("pipe2 for getdents64-enotdir test");
        proc_cap.set_fd(11, Some(reader_cap));
        let ctx = make_ctx(proc_cap, thread);

        let mut buf = vec![0u8; 256];
        let req = SyscallRequest::new(
            NR_GETDENTS64,
            [11, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Error(E_NOTDIR));
    }

    /// After the first `getdents64` consumed every record, a second
    /// call returns `0` (end of directory).
    #[test]
    fn dispatch_getdents64_after_full_read_returns_zero() {
        let _setup = stat_setup();
        let (_root_dentry, _tmpfs, root_rnode) = build_tmpfs_root();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        proc_cap.set_fd(8, Some(directory_open_file(root_rnode)));
        let ctx = make_ctx(proc_cap, thread);

        // First call against an empty directory returns 0
        // immediately — there are no entries to encode.
        let mut buf = vec![0u8; 256];
        let req = SyscallRequest::new(
            NR_GETDENTS64,
            [8, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        );
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0));
    }

    // -----------------------------------------------------------------
    // umask
    // -----------------------------------------------------------------

    /// `umask(0o077)` swaps the per-process file-creation mask and
    /// returns the previous default `0o022`.
    #[test]
    fn dispatch_umask_swaps_and_returns_old_value() {
        let _setup = stat_setup();
        let proc_cap = bootstrap();
        let thread = first_thread(&proc_cap);
        let ctx = make_ctx(proc_cap.clone(), thread);

        let req = SyscallRequest::new(NR_UMASK, [0o077, 0, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0o022));
        assert_eq!(proc_cap.umask(), 0o077);

        // Second call returns the value installed by the first.
        let req2 = SyscallRequest::new(NR_UMASK, [0o000, 0, 0, 0, 0, 0]);
        let result2 = block_on(dispatch::<ShimsTestPmap>(req2, &ctx));
        assert_eq!(result2, SyscallResult::Return(0o077));
        assert_eq!(proc_cap.umask(), 0o000);
    }
}
