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

use super::{dispatch, SyscallCtx, SyscallResult, NR_EXIT, NR_EXIT_GROUP, NR_GETPID, NR_WRITE};

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

/// Any nr not in the Phase 2a table returns `-ENOSYS` (positive
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
