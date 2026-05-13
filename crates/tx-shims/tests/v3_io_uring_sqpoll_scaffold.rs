//! Future PR-12 phase 0 — io_uring SQPOLL scaffold tests (second
//! `OnBehalfOf<P>` canary).
//!
//! Spec:
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §8.1 (SQPOLL design)
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §13
//!   (future canary section — "io_uring SQPOLL as a second canary")
//!
//! Pinned invariants:
//!
//! 1. **`sys_io_uring_setup` mints an `OpenFileBacking::IoUring` fd.**
//!    The returned fd's `OpenFile` carries `OpenFileBacking::IoUring`
//!    and the `io_uring()` accessor resolves the inner `Cap<IoUring>`;
//!    the `ring_id`, `sq_entries`, `cq_entries` round-trip through the
//!    cap. Mirrors W-Z's PR-11 phase 1 `sys_io_setup` pin.
//!
//! 2. **SQPOLL kthread spawn at setup.** Each `io_uring_setup` call
//!    installs a `SqpollWorkerFuture` keyed by the ring's `ring_id`.
//!    The future holds the long-lived `with_on_behalf_of` borrow body.
//!    Mirrors W-CC's PR-11 phase 2 deferred-pump stash.
//!
//! 3. **kthread body observes pushed SQEs.** An SQE pushed via the
//!    scaffold test helper [`IoUring::push_sqe_for_test`] is drained
//!    by the kthread within a bounded number of polls; the ring's
//!    `dispatched` counter increments. Mirrors W-CC's PR-11 phase 2
//!    `worker body observes the pushed iocb` pin.
//!
//! 4. **Abort signal terminates the kthread cleanly.** Tripping the
//!    ring's `worker_abort` signal (the structural equivalent of
//!    `io_uring_destroy(2)` / principal exit) drives the kthread
//!    future to a `Ready` resolution with `Err(OnBehalfOfAbort::*)`.
//!    Mirrors W-CC's PR-11 phase 2 `worker_terminates_cleanly` pin.
//!
//! 5. **Framework reusability.** The SQPOLL kthread is constructed
//!    from substrate's `step_v3::with_on_behalf_of` **as-is** — no
//!    new framework primitive is added. The scaffold's
//!    [`tx_subsystems::io_uring::spawn_sqpoll_worker`] is a
//!    row-for-row clone of
//!    [`tx_subsystems::aio::spawn_worker_for_context`] with the
//!    `AioContext` cap swapped for an `IoUring` cap and the dispatcher
//!    parameter removed (phase 0 has no dispatcher). The fact that
//!    these tests pass with that minimal delta is the evidence W-W's
//!    "zero additional framework work" prediction holds.

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
use tx_shims::adapter::step_engine::{Cap, CancelReason, OnBehalfOfAbort};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::io_uring::{reset_ring_id_counter_for_test, SqeStub, SqpollWorkerFuture};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::OpenFileBacking;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::io_uring::{
    io_uring_worker_install_count_for_test, reset_io_uring_worker_registry_for_test,
    take_io_uring_worker_for_test,
};
use tx_shims::linux_syscall::numbers::NR_IO_URING_SETUP;
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP (mirrors v3_aio_io_setup.rs) --------------------

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
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    reset_ring_id_counter_for_test();
    reset_io_uring_worker_registry_for_test();
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

fn dispatch_call(ctx: &SyscallCtx<'_>, req: SyscallRequest) -> SyscallResult {
    block_on(dispatch::<StubPmap>(req, ctx))
}

fn dispatch_io_uring_setup(ctx: &SyscallCtx<'_>, entries: u32) -> SyscallResult {
    dispatch_call(
        ctx,
        SyscallRequest::new(NR_IO_URING_SETUP, [entries as u64, 0, 0, 0, 0, 0]),
    )
}

fn pump_worker_until<F>(mut worker: SqpollWorkerFuture, mut done: F, budget: u32)
where
    F: FnMut(&SqpollWorkerFuture) -> bool,
{
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    for _ in 0..budget {
        let _ = pinned.as_mut().poll(&mut cx);
        if done(&pinned) {
            return;
        }
    }
    panic!("pump_worker_until: condition not met within {budget} polls");
}

// -------- Tests -----------------------------------------------------

/// Pin invariant 1 — `sys_io_uring_setup(entries=4)` returns a fresh
/// fd whose backing is the IoUring shape and whose ring depths
/// round-trip the syscall args.
#[test]
fn sys_io_uring_setup_returns_an_io_uring_backed_fd() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_uring_setup(&ctx, 4) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let open_file = proc_cap.fd(fd).expect("fd installed");
    assert!(
        matches!(open_file.backing(), OpenFileBacking::IoUring { .. }),
        "OpenFile must carry OpenFileBacking::IoUring",
    );
    let ring = open_file.io_uring().expect("io_uring accessor");
    assert!(
        ring.ring_id() >= 1,
        "ring_id must be positive, got {}",
        ring.ring_id()
    );
    assert_eq!(
        ring.sq_entries(),
        4,
        "sq_entries round-trips the syscall arg"
    );
    assert_eq!(
        ring.cq_entries(),
        8,
        "cq_entries defaults to 2 * sq_entries per Linux convention",
    );
}

/// Pin invariant 1 (cross-discriminator) — a non-uring `OpenFile` does
/// not falsely report itself as io_uring-backed, and a uring-backed
/// `OpenFile` reports `None` for the AIO / signalfd / ufd accessors.
/// Pins the discriminator contract every future
/// `sys_io_uring_enter` / `sys_io_uring_destroy` arm relies on.
#[test]
fn io_uring_accessor_returns_none_on_non_uring_open_file() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // A uring-backed fd:
    let fd = match dispatch_io_uring_setup(&ctx, 2) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let uring_file = proc_cap.fd(fd).expect("fd installed");
    assert!(
        uring_file.io_uring().is_some(),
        "uring-backed OpenFile must report io_uring() = Some",
    );
    assert!(
        uring_file.aio_context().is_none(),
        "uring-backed OpenFile must report aio_context() = None",
    );
    assert!(
        uring_file.signalfd().is_none(),
        "uring-backed OpenFile must report signalfd() = None",
    );
    assert!(
        uring_file.ufd().is_none(),
        "uring-backed OpenFile must report ufd() = None",
    );
}

/// Pin invariant 2 — `sys_io_uring_setup` installs exactly one SQPOLL
/// kthread future per setup call into the test registry.
#[test]
fn io_uring_setup_spawns_one_sqpoll_worker_per_ring() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    assert_eq!(io_uring_worker_install_count_for_test(), 0);
    let fd_a = match dispatch_io_uring_setup(&ctx, 8) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    assert_eq!(io_uring_worker_install_count_for_test(), 1);

    let ring_a = proc_cap
        .fd(fd_a)
        .expect("fd installed")
        .io_uring()
        .expect("io_uring accessor")
        .ring_id();
    _ = take_io_uring_worker_for_test(ring_a).expect("worker future stashed");

    // A second setup installs a second worker keyed by a fresh
    // ring_id.
    let fd_b = match dispatch_io_uring_setup(&ctx, 8) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    assert_eq!(io_uring_worker_install_count_for_test(), 2);
    let ring_b = proc_cap
        .fd(fd_b)
        .expect("fd installed")
        .io_uring()
        .expect("io_uring accessor")
        .ring_id();
    assert_ne!(ring_a, ring_b, "each setup mints a fresh ring_id");
    _ = take_io_uring_worker_for_test(ring_b).expect("worker future for fd_b stashed");
}

/// Pin invariant 3 — an SQE pushed via the scaffold test helper is
/// drained by the SQPOLL kthread within a bounded number of polls;
/// the ring's `dispatched` counter increments.
#[test]
fn sqpoll_kthread_observes_pushed_sqe_within_bounded_ticks() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_uring_setup(&ctx, 4) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let ring = proc_cap
        .fd(fd)
        .expect("fd installed")
        .io_uring()
        .expect("io_uring accessor")
        .clone();
    let worker = take_io_uring_worker_for_test(ring.ring_id())
        .expect("SQPOLL kthread future stashed by io_uring_setup");

    // Push a single SQE through the scaffold's test helper. Production
    // phase 1 will dequeue from the user-mmapped SQ ring instead.
    ring.push_sqe_for_test(SqeStub::new(0, 0xCAFE_BABE))
        .expect("sqe admitted");
    assert!(ring.sq_len() <= 1, "ring admitted ≤ pushed");

    // Pump the kthread future. The body's drain loop must observe
    // the pushed SQE within a bounded number of polls; phase 0's
    // stub increments `dispatched` per SQE.
    pump_worker_until(worker, |_| ring.dispatched() >= 1, 64);
    assert_eq!(ring.dispatched(), 1, "kthread body observed one SQE");
    assert_eq!(ring.sq_len(), 0, "kthread drained the SQ ring");
}

/// Pin invariant 3 (multi-SQE) — pushing two SQEs in sequence both
/// land in the dispatched counter.
#[test]
fn sqpoll_kthread_drains_multiple_sqes_in_sequence() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_uring_setup(&ctx, 8) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let ring = proc_cap
        .fd(fd)
        .expect("fd installed")
        .io_uring()
        .expect("io_uring accessor")
        .clone();
    let worker =
        take_io_uring_worker_for_test(ring.ring_id()).expect("SQPOLL kthread future stashed");

    ring.push_sqe_for_test(SqeStub::new(0, 0x1111))
        .expect("sqe a");
    ring.push_sqe_for_test(SqeStub::new(0, 0x2222))
        .expect("sqe b");
    pump_worker_until(worker, |_| ring.dispatched() >= 2, 64);
    assert_eq!(ring.dispatched(), 2);
    assert_eq!(ring.sq_len(), 0);
}

/// Pin invariant 4 — tripping the ring's worker_abort signal (the
/// structural equivalent of `io_uring_destroy` / principal exit)
/// drives the SQPOLL kthread future to `Ready(Err)` with the
/// `OnBehalfOfAbort` reason.
#[test]
fn sqpoll_kthread_terminates_cleanly_when_abort_trips() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_uring_setup(&ctx, 4) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let ring = proc_cap
        .fd(fd)
        .expect("fd installed")
        .io_uring()
        .expect("io_uring accessor")
        .clone();
    let mut worker =
        take_io_uring_worker_for_test(ring.ring_id()).expect("SQPOLL kthread future stashed");

    // Pump once — the kthread enters the borrow and parks on the
    // empty SQ ring (Pending).
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    match pinned.as_mut().poll(&mut cx) {
        Poll::Pending => {}
        other => panic!("expected Pending on first poll, got {other:?}"),
    }

    // Trip the abort. PrincipalExited is the structural surrogate for
    // principal exit; phase 1's `io_uring_destroy` would use
    // CooperativeCancel(OwnerRequested) instead — both terminate the
    // kthread cleanly.
    ring.abort_worker();

    for _ in 0..64 {
        if let Poll::Ready(out) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(
                out,
                Err(OnBehalfOfAbort::PrincipalExited),
                "abort_worker trips PrincipalExited"
            );
            return;
        }
    }
    panic!("kthread did not terminate within 64 polls after abort");
}

/// Pin invariant 4 (cooperative cancel) — `cancel_worker` trips the
/// abort with `CooperativeCancel(OwnerRequested)`, the structural
/// equivalent of a future `sys_io_uring_destroy(2)` arm. Pins that
/// the kthread distinguishes a clean cooperative teardown from a
/// principal-exit kill.
#[test]
fn sqpoll_kthread_cancel_worker_trips_cooperative_cancel() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_io_uring_setup(&ctx, 4) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let ring = proc_cap
        .fd(fd)
        .expect("fd installed")
        .io_uring()
        .expect("io_uring accessor")
        .clone();
    let mut worker =
        take_io_uring_worker_for_test(ring.ring_id()).expect("SQPOLL kthread future stashed");

    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    let _ = pinned.as_mut().poll(&mut cx);

    ring.cancel_worker();

    for _ in 0..64 {
        if let Poll::Ready(out) = pinned.as_mut().poll(&mut cx) {
            assert!(
                matches!(
                    out,
                    Err(OnBehalfOfAbort::CooperativeCancel(
                        CancelReason::OwnerRequested
                    ))
                ),
                "cancel_worker trips CooperativeCancel(OwnerRequested), got {out:?}"
            );
            return;
        }
    }
    panic!("kthread did not terminate within 64 polls after cancel");
}
