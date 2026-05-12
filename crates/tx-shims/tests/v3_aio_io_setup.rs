//! PR-11 phase 1 — `sys_io_setup` AIO fd-shape scaffold tests.
//!
//! Pins the wiring for:
//!
//! 1. `sys_io_setup(nr_events, _ctx_idp)` mints a `Cap<AioContext>`
//!    (W-Z phase 1 zone), wraps in an `OpenFile` with
//!    `OpenFileBacking::AioContext`, installs at the lowest free fd,
//!    and returns the fd.
//! 2. The returned `OpenFile`'s `aio_context()` accessor resolves the
//!    inner `Cap<AioContext>`; the `nr_events` field round-trips
//!    through the cap.
//! 3. The legacy `OpenFile::rnode()` accessor panics on the AIO
//!    backing — W-Q's choice for the Ufd variant is mirrored.
//! 4. The scaffold round-trip is **end-to-end via `dispatch`** so the
//!    dispatch table entry (NR_IO_SETUP) is exercised the same way
//!    userspace would reach it.
//!
//! Linux divergence (per D8 §4.1): `io_setup` returns a real fd, not
//! a pointer-shaped `aio_context_t`. The `_ctx_idp` argument is
//! ignored — these tests pass `0` for it.
//!
//! Phases 2–4 (D8 plan) add `io_submit` + worker + `with_on_behalf_of`
//! integration, `io_getevents` + completion routing, and `io_destroy`
//! with cleanup. This file's tests only exercise the open + fd-install
//! primitives.

extern crate alloc;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Asid, EntropyIf, PhysAddr, PmapError, PmapIf, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, TimeIf, VirtAddr,
};
use tx_shims::adapter::reactor_entry::SyscallRequest;
use tx_shims::adapter::step_engine::Cap;
use tx_subsystems::aio::reset_context_id_counter_for_test;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::userfaultfd::UserfaultFd;
use tx_subsystems::vfs::structure::{OpenFileBacking, OpenFileFlags};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::numbers::NR_IO_SETUP;
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP (mirrors v3_userfaultfd_syscall_scaffold.rs) ----

struct StubPmap;

#[derive(Default)]
struct StubPmapState {
    next_root: usize,
}

static STUB_PMAP_STATE: LazyLock<Mutex<StubPmapState>> =
    LazyLock::new(|| Mutex::new(StubPmapState { next_root: 1 }));

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = STUB_PMAP_STATE.lock().expect("stub pmap lock");
        let id = state.next_root;
        state.next_root += 1;
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * USER_PAGE_SIZE)),
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
}

impl EntropyIf for StubPmap {}

impl TimeIf for StubPmap {
    fn read_ns() -> u64 {
        0
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

// -------- Setup -----------------------------------------------------

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_substrate::testing::init_host_for_test_once();
    let _ = zones::register_all();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    reset_context_id_counter_for_test();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
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

fn block_on<F: Future>(mut fut: F) -> F::Output {
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("block_on: future did not resolve in 1024 polls");
}

fn dispatch_io_setup(ctx: &SyscallCtx<'_>, nr_events: u32, ctx_idp: u64) -> SyscallResult {
    let req = SyscallRequest::new(NR_IO_SETUP, [nr_events as u64, ctx_idp, 0, 0, 0, 0]);
    block_on(dispatch::<StubPmap>(req, ctx))
}

// -------- Tests -----------------------------------------------------

/// `sys_io_setup(nr_events=0, _ctx_idp=0)` returns a fresh fd >= 0
/// whose backing is the AioContext shape.
#[test]
fn sys_io_setup_returns_an_aio_context_backed_fd() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let result = dispatch_io_setup(&ctx, 0, 0);
    let fd = match result {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let open_file = proc_cap.fd(fd).expect("fd installed");
    assert!(
        matches!(open_file.backing(), OpenFileBacking::AioContext { .. }),
        "OpenFile must carry OpenFileBacking::AioContext",
    );
    let aio = open_file.aio_context().expect("aio_context accessor");
    assert!(
        aio.context_id() >= 1,
        "context_id must be positive, got {}",
        aio.context_id()
    );
    assert_eq!(aio.nr_events(), 0, "nr_events round-trips the syscall arg");
}

/// `sys_io_setup(nr_events=128, _)` stashes the capacity on the
/// `AioContext` cap so phase 3's submission-queue sizing can read it.
#[test]
fn sys_io_setup_round_trips_nr_events_through_the_cap() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_setup(&ctx, 128, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let open_file = proc_cap.fd(fd).expect("fd installed");
    let aio = open_file.aio_context().expect("aio_context accessor");
    assert_eq!(aio.nr_events(), 128);
}

/// Two back-to-back `sys_io_setup` calls produce two distinct
/// `context_id`s and two distinct fds. Pins the per-call freshness
/// guarantee phase 2's iocb-routing key depends on.
#[test]
fn sys_io_setup_mints_fresh_context_ids() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd_a = match dispatch_io_setup(&ctx, 8, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let fd_b = match dispatch_io_setup(&ctx, 8, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    assert_ne!(
        fd_a, fd_b,
        "each io_setup call installs at the next free fd"
    );

    let aio_a = proc_cap
        .fd(fd_a)
        .expect("fd_a installed")
        .aio_context()
        .expect("aio_context accessor for fd_a")
        .context_id();
    let aio_b = proc_cap
        .fd(fd_b)
        .expect("fd_b installed")
        .aio_context()
        .expect("aio_context accessor for fd_b")
        .context_id();
    assert_ne!(aio_a, aio_b, "each io_setup call mints a fresh context_id");
}

/// On a non-AIO OpenFile, [`tx_subsystems::vfs::OpenFile::aio_context`]
/// must return `None`. Pins the discriminator contract every phase-2+
/// AIO syscall arm relies on. Constructs a ufd-backed OpenFile
/// directly (W-Q's PR-10 phase 0 substrate) so the test does not
/// depend on the bootstrap process pre-populating any specific fd
/// shape.
#[test]
fn aio_context_accessor_returns_none_on_non_aio_open_file() {
    let _g = setup();

    let ufd_cap = UserfaultFd::new_cap().expect("ufd cap");
    let ufd_file = OpenFile::new_userfaultfd(ufd_cap, OpenFileFlags::default());
    assert!(
        ufd_file.aio_context().is_none(),
        "ufd-backed OpenFile must report aio_context() = None",
    );
    // And the inverse: a ufd's `ufd()` accessor still returns `Some`,
    // pinning that the two non-VFS variants do not cross-resolve.
    assert!(
        ufd_file.ufd().is_some(),
        "ufd-backed OpenFile must report ufd() = Some",
    );
}

/// The legacy `OpenFile::rnode()` accessor panics on the AIO backing,
/// mirroring W-Q's choice for the Ufd variant. Pins the assertion so
/// a future refactor that turns `rnode()` into a `Result` returns
/// here for a deliberate decision rather than silently sliding into
/// surprise behaviour.
#[test]
#[should_panic(expected = "AIO-context-backed OpenFile")]
fn open_file_rnode_panics_on_aio_backing() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_setup(&ctx, 0, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let open_file = proc_cap.fd(fd).expect("fd installed");
    // Intentional panic — pins the rnode() shape.
    let _rnode = open_file.rnode();
}
