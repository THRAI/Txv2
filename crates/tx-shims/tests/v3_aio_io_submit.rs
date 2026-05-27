//! PR-11 phase 2 — `sys_io_submit` + worker dispatch tests.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §7
//!   (phase plan row P-11.4)
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution
//!   scope — worker enters `with_on_behalf_of` once at setup)
//!
//! Pinned invariants:
//!
//! 1. **Worker spawn at `io_setup`.** Each `io_setup` call installs a
//!    `AioWorkerFuture` keyed by the AIO context's `context_id`. The
//!    worker future is the one that holds the long-lived
//!    `with_on_behalf_of` borrow body. Phase 2 stashes it in a
//!    test-visible registry under the deferred-pump model documented
//!    in `crates/tx-shims/src/linux_syscall/aio.rs`.
//!
//! 2. **`sys_io_submit` admits one iocb per slot.** A single PREAD
//!    iocb, copied into user memory and submitted via the syscall,
//!    is pushed onto the AIO context's submit queue and notified
//!    through the `iocb_arrived` wait source. Return value is the
//!    count admitted.
//!
//! 3. **Worker body observes the pushed iocb.** Pumping the worker
//!    future for a bounded number of polls after submission increments
//!    the AIO context's `dispatched` counter — pinning the
//!    submit-queue → worker-body wake path.
//!
//! 4. **Abort signal terminates the body.** Tripping the AIO
//!    context's worker abort signal (the structural equivalent of
//!    `io_destroy` / principal exit; phase 4 wires the real exit
//!    source) drives the worker future to a `Ready` resolution with
//!    `Err(OnBehalfOfAbort::*)` cleanly.
//!
//! 5. **Batch overflow short-circuits.** With `nr_events = 2`,
//!    submitting 3 iocbs admits 2 and returns 2 — Linux-style
//!    partial-batch behavior. Submitting 0 iocbs returns 0.

extern crate alloc;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Arch, Asid, EntropyIf, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, TimeIf, VirtAddr,
};
use tx_shims::adapter::reactor_entry::SyscallRequest;
use tx_shims::adapter::step_engine::{Cap, OnBehalfOfAbort};
use tx_subsystems::aio::{reset_context_id_counter_for_test, AioWorkerFuture, IOCB_CMD_PREAD};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::aio::{
    reset_worker_registry_for_test, take_worker_future_for_test, worker_install_count_for_test,
};
use tx_shims::linux_syscall::numbers::{NR_IO_SETUP, NR_IO_SUBMIT};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP (mirrors v3_aio_io_setup.rs) --------------------

struct StubPmap;

impl PlatformConfig for StubPmap {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "shims-v3-test";
}

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
impl tx_hal::AuxvIf for StubPmap {}
impl tx_hal::SmpIf for StubPmap {}

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
    reset_context_id_counter_for_test();
    reset_worker_registry_for_test();
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

/// Encode a Linux `struct iocb` in the layout `sys_io_submit` parses.
///
/// Layout (selected fields only — the rest is zero-padded):
///   offset 0 : `aio_data`   (u64 LE)
///   offset 16: `aio_lio_opcode` (u16 LE)
///   offset 20: `aio_fildes` (u32 LE)
///   offset 24: `aio_buf`    (u64 LE)
///   offset 32: `aio_nbytes` (u64 LE)
///   offset 40: `aio_offset` (i64 LE)
fn encode_iocb(
    aio_data: u64,
    aio_lio_opcode: u16,
    aio_fildes: u32,
    aio_buf: u64,
    aio_nbytes: u64,
    aio_offset: i64,
) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0..8].copy_from_slice(&aio_data.to_le_bytes());
    buf[16..18].copy_from_slice(&aio_lio_opcode.to_le_bytes());
    buf[20..24].copy_from_slice(&aio_fildes.to_le_bytes());
    buf[24..32].copy_from_slice(&aio_buf.to_le_bytes());
    buf[32..40].copy_from_slice(&aio_nbytes.to_le_bytes());
    buf[40..48].copy_from_slice(&aio_offset.to_le_bytes());
    buf
}

/// Allocate space in the calling process's address space for a single
/// 64-byte iocb + an 8-byte pointer-slot, write a single iocb into
/// the iocb buffer + its pointer into the slot, and return
/// `(iocbpp_ptr, iocb_ptr)` — the syscall arm reads `iocbpp_ptr` as
/// the user-pointer-to-`*iocb` array head.
///
/// In our test stub, user-VA copies fall back to a raw memcpy (see
/// `bootstrap_copy_from_user`). So we can stash bytes anywhere
/// addressable in the test process's memory and pass kernel pointers
/// as the "user VA" — the fallback path picks them up.
#[allow(clippy::vec_box)] // stable per-element heap addresses; Vec growth must not invalidate
fn stage_iocb_array(iocbs: &[[u8; 64]]) -> (u64, alloc::vec::Vec<alloc::boxed::Box<[u8; 64]>>) {
    // Heap-allocate every iocb so the pointers are stable across the
    // syscall arm's read window.
    let mut heap_iocbs: alloc::vec::Vec<alloc::boxed::Box<[u8; 64]>> =
        iocbs.iter().map(|b| alloc::boxed::Box::new(*b)).collect();
    // The iocbpp array stores u64 pointer values.
    let mut pointers: alloc::vec::Vec<u64> = heap_iocbs
        .iter_mut()
        .map(|b| b.as_mut_ptr() as u64)
        .collect();
    let iocbpp_ptr = pointers.as_mut_ptr() as u64;
    // The pointer array must outlive the syscall arm's reads. We
    // leak it for the test's duration (small constant memory) so the
    // address stays valid for the syscall arm and any subsequent
    // worker polls. Tests are independent of each other (TEST_LOCK
    // serialises them); the leak is bounded by the test count.
    let _leak = alloc::boxed::Box::leak(pointers.into_boxed_slice());
    (iocbpp_ptr, heap_iocbs)
}

// -------- Tests -----------------------------------------------------

/// `sys_io_setup` spawns a per-context worker future and stashes it
/// in the test registry. The install count goes up by exactly one
/// per setup call.
#[test]
fn io_setup_spawns_one_worker_per_context() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    assert_eq!(worker_install_count_for_test(), 0);
    let fd_a = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [8, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    assert_eq!(worker_install_count_for_test(), 1);

    let aio_a = proc_cap
        .fd(fd_a)
        .expect("fd_a installed")
        .aio_context()
        .expect("aio_context accessor")
        .context_id();
    _ = take_worker_future_for_test(aio_a).expect("worker future stashed");

    // Second setup → second install.
    let fd_b = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [8, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    assert_eq!(worker_install_count_for_test(), 2);
    let aio_b = proc_cap
        .fd(fd_b)
        .expect("fd_b installed")
        .aio_context()
        .expect("aio_context accessor")
        .context_id();
    assert_ne!(aio_a, aio_b);
    _ = take_worker_future_for_test(aio_b).expect("worker future for fd_b stashed");
}

/// `sys_io_submit` with `nr = 0` returns 0 without touching the queue.
#[test]
fn io_submit_with_zero_nr_returns_zero() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));

    let aio = proc_cap
        .fd(fd)
        .expect("fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();
    assert_eq!(aio.queue_len(), 0);
}

/// `sys_io_submit` admits a single iocb onto the AIO context's
/// submit queue; return value is the count admitted; the queue
/// reflects the push.
#[test]
fn io_submit_admits_one_iocb_onto_the_queue() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    // Stage one PREAD iocb. Buffer / offset are placeholders — phase
    // 2's dispatch doesn't resolve them (stub).
    let iocb = encode_iocb(0xCAFE_BABE, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let (iocbpp, _keepalive) = stage_iocb_array(&[iocb]);

    // Drain the worker future first so the test's pump-loop below
    // observes the dispatch from a clean state.
    let aio = proc_cap
        .fd(fd)
        .expect("fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();
    let worker =
        take_worker_future_for_test(aio.context_id()).expect("worker future stashed by io_setup");

    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));
    // Queue contains the pushed iocb until the worker drains it.
    assert!(aio.queue_len() <= 1, "queue admitted ≤ nr submitted");

    // Pump the worker future. The body's drain loop must observe
    // the pushed iocb within a bounded number of polls; phase 2's
    // stub increments `dispatched` per iocb.
    pump_worker_until(worker, |_| aio.dispatched() >= 1, 64);
    assert_eq!(aio.dispatched(), 1, "worker body observed one iocb");
    assert_eq!(aio.queue_len(), 0, "worker drained the queue");
}

/// Aborting the worker (the structural equivalent of `io_destroy` /
/// principal exit) drives the worker future to `Ready(Err)` with the
/// `OnBehalfOfAbort` reason. Phase 2 trips the abort via
/// `AioContext::abort_worker`; phase 4 wires it to the real
/// `exit_source` fire path.
#[test]
fn worker_terminates_cleanly_when_abort_signal_trips() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let aio = proc_cap
        .fd(fd)
        .expect("fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();
    let mut worker =
        take_worker_future_for_test(aio.context_id()).expect("worker future stashed by io_setup");

    // Pump once — the worker enters the borrow and parks on the
    // empty queue (Pending).
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    match pinned.as_mut().poll(&mut cx) {
        Poll::Pending => {}
        other => panic!("expected Pending on first poll, got {other:?}"),
    }
    // Trip the abort.
    aio.abort_worker();
    // Subsequent polls must terminate cleanly with the abort reason.
    for _ in 0..64 {
        if let Poll::Ready(out) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(
                out,
                Err(OnBehalfOfAbort::PrincipalExited),
                "abort_worker trips PrincipalExited per phase-2 surrogate"
            );
            return;
        }
    }
    panic!("worker future did not terminate after abort within 64 polls");
}

/// Submitting more iocbs than `nr_events` admits exactly `nr_events`
/// and returns the count admitted. Mirrors Linux's "io_submit returns
/// the count accepted" behavior.
#[test]
fn io_submit_overflow_short_circuits_with_partial_admit() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [2, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let aio = proc_cap
        .fd(fd)
        .expect("fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();
    // Drop the worker so it doesn't drain the queue before our
    // assertion reads `queue_len`.
    drop(take_worker_future_for_test(aio.context_id()));

    // Submit 3 iocbs to a context of capacity 2.
    let iocb_a = encode_iocb(0xAAAA, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let iocb_b = encode_iocb(0xBBBB, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let iocb_c = encode_iocb(0xCCCC, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let (iocbpp, _keepalive) = stage_iocb_array(&[iocb_a, iocb_b, iocb_c]);

    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 3, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(2), "partial admit returns count");
    assert_eq!(aio.queue_len(), 2);
}

/// `sys_io_submit` against a non-AIO fd returns `-EINVAL`.
#[test]
fn io_submit_against_non_aio_fd_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // fd 0 is whatever the bootstrap process opened; it's certainly
    // not an AIO context.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [0, 1, 0xdead_beef, 0, 0, 0]),
    );
    // The bootstrap process may or may not have fd 0 — accept
    // either -EBADF (fd missing) or -EINVAL (fd present but
    // non-AIO).
    match r {
        SyscallResult::Error(e) => {
            assert!(e == 9 || e == 22, "expected -EBADF or -EINVAL, got {e}")
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// `sys_io_submit` admitting a single PREAD also notifies the AIO
/// context's `iocb_arrived` wait source. Tests pin the notify path
/// by reading the subscriber count before/after a notify; the
/// invariant is "push fires the wait source" so we observe via the
/// worker future's `dispatched` counter (the source's wake is what
/// causes the worker to re-poll the queue).
#[test]
fn io_submit_iocb_arrived_notify_drives_worker_drain() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let aio = proc_cap
        .fd(fd)
        .expect("fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();
    let worker = take_worker_future_for_test(aio.context_id()).expect("worker future stashed");

    // Submit two iocbs.
    let iocb_a = encode_iocb(0x1111, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let iocb_b = encode_iocb(0x2222, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let (iocbpp, _keepalive) = stage_iocb_array(&[iocb_a, iocb_b]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 2, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(2));

    // Pump until both iocbs are dispatched.
    pump_worker_until(worker, |_| aio.dispatched() >= 2, 64);
    assert_eq!(aio.dispatched(), 2);
}

// -------- Helpers ---------------------------------------------------

fn pump_worker_until<F>(mut worker: AioWorkerFuture, mut done: F, budget: u32)
where
    F: FnMut(&AioWorkerFuture) -> bool,
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
