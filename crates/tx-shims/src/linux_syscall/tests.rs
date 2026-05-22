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
use std::sync::Mutex;

use crate::adapter::reactor_entry::userspace::SyscallRequest;
use crate::adapter::step_engine::{self as step_engine, guard, Cap, StepOutcome};
use crate::linux_syscall::reset_uts_nodename_for_test;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_reactor_affinity_seam, reset_tid_counter,
};
use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT};
use tx_subsystems::execution::Guard;
use tx_subsystems::process::{bootstrap_init_process, ExitStatus, Pid, ProcessIdentity};
use tx_subsystems::signal::Signum;
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::tty::execution::{register_console_alias, register_hardware};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

use super::{
    dispatch, SyscallCtx, SyscallResult, CLONE_CHILD_CLEARTID, CLONE_CHILD_SETTID,
    CLONE_PARENT_SETTID, EINVAL_VALUE, ENOSYS_VALUE, FD_CLOEXEC, F_GETFD, F_SETFD, NR_BRK,
    NR_CLONE, NR_EXECVE, NR_EXIT, NR_EXIT_GROUP, NR_FCNTL, NR_GETPGID, NR_GETPGRP, NR_GETPID,
    NR_GETPPID, NR_GETSID, NR_GET_ROBUST_LIST, NR_MEMBARRIER, NR_PIPE2, NR_PPOLL, NR_READ,
    NR_RT_SIGACTION, NR_RT_SIGPROCMASK, NR_RT_SIGTIMEDWAIT, NR_SCHED_GETAFFINITY,
    NR_SCHED_SETAFFINITY, NR_SETPGID, NR_SETSID, NR_SET_ROBUST_LIST, NR_SET_TID_ADDRESS,
    NR_TIMERFD_CREATE, NR_WAIT4, NR_WRITE, NR_WRITEV, O_DIRECTORY, SIGCHLD, WNOHANG,
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
    Arch, Asid, EntropyIf, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, SmpIf, VirtAddr,
};
use tx_subsystems::vm::USER_PAGE_SIZE;

struct ShimsTestPmap;

impl PlatformConfig for ShimsTestPmap {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "shims-test";
}

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

impl tx_hal::AuxvIf for ShimsTestPmap {}

impl tx_hal::ConsoleIf for ShimsTestPmap {
    fn write_bytes(_bytes: &[u8]) {}
}

impl SmpIf for ShimsTestPmap {}

impl tx_hal::TrapIf for ShimsTestPmap {}
impl tx_hal::SignalFrameIf for ShimsTestPmap {}

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
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    reset_reactor_affinity_seam();
    reset_uts_nodename_for_test();
    tx_subsystems::net::reset_initial_net_namespace_for_test();
    tx_subsystems::net::initial_loopback_iface().clear_for_test_or_bootstrap();
    tx_subsystems::net::device::reset_net_registry_for_test();
    tx_subsystems::net::reset_netfilter_for_test();
    super::reset_itimer_registry_for_test();
    super::reset_sigaction_restorers_for_test();
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
    fn read(
        &self,
        _out: &mut [u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, step_engine::ByteProgress> {
        StepOutcome::Done(0)
    }

    fn write(
        &self,
        bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, step_engine::ByteProgress> {
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
    let guard = guard();
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

fn block_on<F: Future>(mut fut: F) -> F::Output {
    let waker = Waker::noop().clone();
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

/// `writev(1, iov, 2)` uses the stdout/stderr TTY fast path. The
/// combined buffer is kernel-owned after iovec gather, so this verifies
/// that the fast path writes those bytes directly instead of feeding a
/// kernel pointer back through the user-buffer `write(2)` lane.
#[test]
fn dispatch_writev_stdout_tty_fast_path_writes_combined_buffer() {
    let _setup = setup();
    let ops = install_capturing_console();

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(1, Some(tx_fs::devfs::open_console_for_init()));

    let ctx = make_ctx(proc_cap, thread);
    let part1: &[u8] = b"netperf ";
    let part2: &[u8] = b"row\n";
    let iov = [
        part1.as_ptr() as u64,
        part1.len() as u64,
        part2.as_ptr() as u64,
        part2.len() as u64,
    ];
    let req = SyscallRequest::new(NR_WRITEV, [1, iov.as_ptr() as u64, 2, 0, 0, 0]);

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Return(12));
    assert_eq!(ops.snapshot(), b"netperf row\r\n");
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
/// the registered `wait_source` channel, then returns the byte.
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
    let waker = Waker::noop().clone();
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
        use step_engine::StepOutcome as V3Out;
        let guard = guard();
        let outcome = tx_subsystems::tty::execution::step_ingest(&console_tty, b"X\n", &guard);
        assert!(
            matches!(outcome, V3Out::Done(_)),
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[test]
fn dispatch_ppoll_reports_pipe_polout_only_while_space_remains() {
    const POLLOUT: i16 = 0x0004;

    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut pipefd = [-1i32; 2];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let timeout = [0u64; 2];
    let mut pfd = TestPollFd {
        fd: pipefd[1],
        events: POLLOUT,
        revents: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_PPOLL,
                [
                    &mut pfd as *mut TestPollFd as u64,
                    1,
                    timeout.as_ptr() as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(pfd.revents, POLLOUT);

    let buf = [0x41u8; tx_subsystems::pipe::PIPE_BUF];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [
                    pipefd[1] as u64,
                    buf.as_ptr() as u64,
                    buf.len() as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(buf.len() as i64)
    );

    pfd.revents = 0;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_PPOLL,
                [
                    &mut pfd as *mut TestPollFd as u64,
                    1,
                    timeout.as_ptr() as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(pfd.revents, 0);
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

/// `rt_sigtimedwait(set, info, {0,0}, 8)` is a true poll: if no
/// matching pending signal exists, it returns `-EAGAIN`.
#[test]
fn dispatch_rt_sigtimedwait_zero_timeout_returns_neg_eagain() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut set = Signum::SIGCHLD.bit();
    let mut timeout = [0i64, 0i64];
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_RT_SIGTIMEDWAIT,
            [
                &mut set as *mut u64 as u64,
                0,
                timeout.as_mut_ptr() as u64,
                8,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(11));
}

/// `rt_sigtimedwait` consumes a matching pending signal and writes the
/// leading `siginfo_t.si_signo` field. This pins the ABI shape used by
/// the OSComp libctest runtest harness to wait for child `SIGCHLD`.
#[test]
fn dispatch_rt_sigtimedwait_consumes_pending_sigchld() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread.clone());

    let payload = thread
        .payload_cap()
        .expect("bootstrap leader thread must be live");
    payload.pending().post(Signum::SIGCHLD);

    let mut set = Signum::SIGCHLD.bit();
    let mut info = [0xa5u8; 128];
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_RT_SIGTIMEDWAIT,
            [
                &mut set as *mut u64 as u64,
                info.as_mut_ptr() as u64,
                0,
                8,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(SIGCHLD as i64));
    assert!(
        !payload.pending().is_pending(Signum::SIGCHLD),
        "sigtimedwait must dequeue the consumed signal"
    );
    assert_eq!(
        u32::from_le_bytes(info[0..4].try_into().unwrap()),
        SIGCHLD as u32
    );
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
    let act: [u64; 4] = [HANDLER_ADDR, 0, 0, 0]; // handler/flags/mask/unused
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

/// musl's pthread cancellation handler is installed with
/// `SA_SIGINFO | SA_RESTART | SA_ONSTACK` and a full mask. The kernel
/// rt_sigaction ABI must preserve those words when queried back; losing
/// them means AST delivery cannot distinguish the 3-argument handler
/// shape or compute the handler-entry mask.
#[test]
fn dispatch_rt_sigaction_round_trips_musl_rv64_flags_and_mask() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    const HANDLER_ADDR: u64 = 0xCAFE_F00D_DEAD_BEEFu64;
    const SA_SIGINFO: u64 = 4;
    const SA_ONSTACK: u64 = 0x0800_0000;
    const SA_RESTART: u64 = 0x1000_0000;
    const FLAGS: u64 = SA_SIGINFO | SA_ONSTACK | SA_RESTART;
    const MASK: u64 = u64::MAX;
    const UNUSED: u64 = 0x4444_5555_6666_7777;

    let act: [u64; 4] = [HANDLER_ADDR, FLAGS, MASK, UNUSED];
    let mut oldact: [u64; 4] = [0xDEADu64; 4];

    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_RT_SIGACTION,
            [
                33, // musl-internal SIGCANCEL
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
    assert_eq!(oldact, [0, 0, 0, 0]);

    let mut observed: [u64; 4] = [0xDEADu64; 4];
    let r2 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_RT_SIGACTION,
            [33, 0, observed.as_mut_ptr() as u64, 8, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(0));
    assert_eq!(observed[0], HANDLER_ADDR);
    assert_eq!(observed[1], FLAGS);
    assert_eq!(
        observed[2] & tx_subsystems::signal::Signum::SIGKILL.bit(),
        0,
        "kernel SignalMask must strip uncatchable bits"
    );
    assert_eq!(
        observed[2] & tx_subsystems::signal::Signum::SIGSTOP.bit(),
        0,
        "kernel SignalMask must strip uncatchable bits"
    );
    assert_ne!(
        observed[2] & tx_subsystems::signal::Signum::SIGTERM.bit(),
        0
    );
    assert_eq!(
        observed[3], UNUSED,
        "RV64 musl has no SA_RESTORER, so the last word is ABI-unused"
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

/// The pinned musl riscv64 syscall header defines `__NR_membarrier`
/// as 283. This is observed by pthread/TLS initialization, so the
/// exported syscall number must match the submodule header.
#[test]
fn rv64_membarrier_number_matches_pinned_musl_header() {
    assert_eq!(NR_MEMBARRIER, 283);
}

/// The pinned musl riscv64 syscall header defines
/// `__NR_timerfd_create` as 85. This must not collide with
/// `membarrier(283)`, or dispatch will route timerfd calls to the
/// wrong syscall arm.
#[test]
fn rv64_timerfd_create_number_matches_pinned_musl_header() {
    assert_eq!(NR_TIMERFD_CREATE, 85);
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

/// Unknown `cmd` values return `-ENOSYS`.
///
/// Unknown fcntl commands return `-EINVAL` per Linux.
#[test]
fn dispatch_fcntl_unknown_cmd_returns_neg_einval() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    // fd-ops Wave 1: install at fd 3 so the EBADF gate doesn't
    // fire before the unknown-cmd path is reached.
    proc_cap.set_fd(3, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FCNTL, [3, 9999, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Error(22));
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

fn fake_set_thread_affinity(
    _tid: u32,
    affinity: u64,
) -> Result<(), tx_subsystems::reactor_affinity::ReactorAffinityError> {
    if affinity == 0b101 {
        Ok(())
    } else if affinity == 0 {
        Err(tx_subsystems::reactor_affinity::ReactorAffinityError::InvalidMask)
    } else {
        Err(tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)
    }
}

fn fake_get_thread_affinity(
    tid: u32,
) -> Result<u64, tx_subsystems::reactor_affinity::ReactorAffinityError> {
    if tid != 0 {
        Ok(0b101)
    } else {
        Err(tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)
    }
}

#[test]
fn dispatch_sched_affinity_round_trips_through_reactor_seam() {
    let _setup = setup();
    tx_subsystems::reactor_affinity::install_thread_affinity(
        fake_set_thread_affinity,
        fake_get_thread_affinity,
    );
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let in_mask = 0b101u64;
    let mut out_mask = 0u64;

    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SCHED_SETAFFINITY,
            [
                0,
                core::mem::size_of::<u64>() as u64,
                &in_mask as *const u64 as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));

    let get = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SCHED_GETAFFINITY,
            [
                0,
                core::mem::size_of::<u64>() as u64,
                &mut out_mask as *mut u64 as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(
        get,
        SyscallResult::Return(core::mem::size_of::<u64>() as i64)
    );
    assert_eq!(out_mask, 0b101);
}

#[test]
fn dispatch_sched_setaffinity_rejects_empty_mask() {
    let _setup = setup();
    tx_subsystems::reactor_affinity::install_thread_affinity(
        fake_set_thread_affinity,
        fake_get_thread_affinity,
    );
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let in_mask = 0u64;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SCHED_SETAFFINITY,
            [
                0,
                core::mem::size_of::<u64>() as u64,
                &in_mask as *const u64 as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(EINVAL_VALUE));
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

mod execve;

// ===========================================================================
// Wave 2 of the fork/clone/wait4 slice — Part 2 (NR_CLONE) +
// Part 4 (5 + 1 introspection arms) + Part 5 (musl-startup stubs).
//
// NR_WAIT4 belongs to Wave 3 with the blocking-wait scaffolding and is
// **not** tested here.
// ===========================================================================

mod fork_clone_wait4_wave2;

// ===========================================================================
// Wave 3 of the fork/clone/wait4 slice — Part 3 (NR_WAIT4 syscall arm
// with blocking-wait via the per-process `exit_source` carrier).
// ===========================================================================

mod fork_clone_wait4_wave3;

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
mod dac_setuid_wave2;

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

mod dac_setuid_wave4;

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

mod fd_ops_wave2;

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
mod fd_ops_wave3;

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
mod fd_ops_wave4;

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
mod vm_syscalls;

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
mod futex_dispatch;

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
mod time_syscalls;

// ===========================================================================
// timerfd syscall ABI.
//
// Coverage:
//   - `timerfd_settime` copies the caller's LP64 `struct itimerspec`
//     into the kernel and reports the previous value in the same layout.
//   - `timerfd_gettime(fd, curr_value)` uses the second syscall argument
//     as the writeback pointer and returns remaining time, not the absolute
//     internal deadline.
//   - Unknown `timerfd_settime` flags and invalid `tv_nsec` fields return
//     `-EINVAL`.
// ===========================================================================
mod timerfd_dispatch;

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
mod ioctl_dispatch;

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

mod stat_family;

// =====================================================================
// Slice 7 of the shell-prompt roadmap — fcntl extension + day-1 misc
// syscalls (`F_DUPFD` / `F_DUPFD_CLOEXEC` / `F_GETFL` / `F_SETFL` /
// `getpgrp` / `kill` / `tkill` / `tgkill` / `getrandom` / `uname` /
// `prlimit64` / `rt_sigreturn`).
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 7.
// =====================================================================

mod fcntl_misc;

// =====================================================================
// Slice 8 of the shell-prompt roadmap — file-mutation syscalls.
//
// Coverage matches the slice plan §"Tests": one or more dispatch tests
// per arm exercising the success path and the canonical error shapes
// (EEXIST / ENOENT / ENOTDIR / EISDIR / EINVAL / ENOSYS as relevant).
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 8.
// =====================================================================

mod file_mutation;

// =====================================================================
// `sys_rt_sigtimedwait` — verify the bit-encoding and post→read
// round-trip used by libctest's `runtest.c`.
// =====================================================================

mod sigtimedwait_dispatch;

// ===========================================================================
// `sigaltstack` — LP64 stack_t copy-in/copy-out contract used by musl.
// ===========================================================================
mod sigaltstack_dispatch;

// ===========================================================================
// SysV IPC dispatch copy paths and musl userspace layouts.
// ===========================================================================

mod ipc_dispatch;

// ===========================================================================
// `epoll_create1` / `epoll_ctl` / `epoll_pwait` generic musl syscall numbers
// and LP64 `struct epoll_event` copy paths.
// ===========================================================================
mod epoll_dispatch;

// ===========================================================================
// POSIX message queues — musl treats `mqd_t` as an fd and uses the LP64
// `struct mq_attr` layout from `<mqueue.h>`.
// ===========================================================================
mod mq_dispatch;

// ===========================================================================
// Kernel-to-user layout marker registry used by the musl ABI detector.
// ===========================================================================
mod kernel_user_layouts;

// =====================================================================
// Network N39 — socket fdtable/syscall bridge.
//
// Coverage:
// - `socket(2)` installs a struct-backed socket `OpenFile`.
// - `bind(2)` / `listen(2)` update the socket state and
//   `getsockname(2)` reports the bound endpoint.
// - `setsockopt(2)` / `getsockopt(2)` round-trip day-1 socket options.
// - `close(2)` tears down the socket binding when the final fd closes.
// =====================================================================

mod socket_fdtable;

// =====================================================================
// Network N71M3 — namespace user ABI.
//
// Coverage:
// - `unshare(CLONE_NEWNET)` publishes a fresh process net namespace.
// - `setns(fd, CLONE_NEWNET)` joins a namespace fd payload.
// - `/proc/<pid>/ns/net` materialises as a struct-backed namespace fd.
// =====================================================================

mod netns_syscalls;
