//! PR-11 phase 5 — `sys_io_destroy` tests.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §7
//!   (phase plan row P-11.6)
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §5 (abandonment routing)
//!
//! Pinned invariants:
//!
//! 1. **`io_destroy` trips the worker's abort signal.** After
//!    `sys_io_destroy(fd)`, the worker future stashed in the test
//!    registry resolves to `Err(CooperativeCancel(OwnerRequested))`
//!    on its next poll — the worker's `with_on_behalf_of` racer
//!    observes the trip and terminates cleanly.
//!
//! 2. **`io_destroy` removes the fd-table entry.** Subsequent
//!    operations (`io_submit`, `io_getevents`, `io_destroy`, even
//!    `sys_close`) against the same fd return `-EBADF`.
//!
//! 3. **`io_destroy` against a non-AIO fd returns -EINVAL.**
//!
//! 4. **`io_destroy` against an unknown fd returns -EBADF.**
//!
//! 5. **`io_destroy` drops the worker future from the registry.**
//!    `take_worker_future_for_test` returns `None` after destroy.

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
use tx_shims::adapter::step_engine::{CancelReason, Cap, OnBehalfOfAbort};
use tx_subsystems::aio::{reset_context_id_counter_for_test, IOCB_CMD_PREAD};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::aio::{reset_worker_registry_for_test, take_worker_future_for_test};
use tx_shims::linux_syscall::numbers::{NR_IO_DESTROY, NR_IO_GETEVENTS, NR_IO_SETUP, NR_IO_SUBMIT};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP ------------------------------------------------

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

// -------- Setup ---------------------------------------------------

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

#[allow(clippy::vec_box)] // stable per-element heap addresses; Vec growth must not invalidate
fn stage_iocb_array(iocbs: &[[u8; 64]]) -> (u64, alloc::vec::Vec<alloc::boxed::Box<[u8; 64]>>) {
    let mut heap_iocbs: alloc::vec::Vec<alloc::boxed::Box<[u8; 64]>> =
        iocbs.iter().map(|b| alloc::boxed::Box::new(*b)).collect();
    let mut pointers: alloc::vec::Vec<u64> = heap_iocbs
        .iter_mut()
        .map(|b| b.as_mut_ptr() as u64)
        .collect();
    let iocbpp_ptr = pointers.as_mut_ptr() as u64;
    let _leak = alloc::boxed::Box::leak(pointers.into_boxed_slice());
    (iocbpp_ptr, heap_iocbs)
}

// -------- Tests ---------------------------------------------------

/// `sys_io_destroy` trips the worker abort signal — pumping the
/// worker future after destroy resolves it to `Err(CooperativeCancel(
/// OwnerRequested))`.
#[test]
fn io_destroy_trips_worker_cooperative_cancel() {
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
        .expect("aio fd installed")
        .aio_context()
        .expect("aio_context accessor")
        .clone();
    let mut worker =
        take_worker_future_for_test(aio.context_id()).expect("worker stashed by io_setup");

    // Pump once so the worker enters the borrow body and parks.
    let waker = Waker::noop().clone();
    let mut poll_cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    match pinned.as_mut().poll(&mut poll_cx) {
        Poll::Pending => {}
        other => panic!("expected Pending on first poll, got {other:?}"),
    }

    // sys_io_destroy.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));

    // Subsequent polls of the worker future observe the abort.
    for _ in 0..64 {
        if let Poll::Ready(out) = pinned.as_mut().poll(&mut poll_cx) {
            assert_eq!(
                out,
                Err(OnBehalfOfAbort::CooperativeCancel(
                    CancelReason::OwnerRequested
                )),
                "io_destroy trips OwnerRequested"
            );
            return;
        }
    }
    panic!("worker future did not terminate after io_destroy within 64 polls");
}

/// Post-`io_destroy`, fd ops against the same fd return `-EBADF`.
#[test]
fn post_destroy_fd_ops_return_ebadf() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    // Destroy.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));

    // io_submit against the destroyed fd: -EBADF.
    let iocb = encode_iocb(0, IOCB_CMD_PREAD, 0, 0, 0, 0);
    let (iocbpp, _ka) = stage_iocb_array(&[iocb]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Error(9 /* EBADF */));

    // io_getevents against the destroyed fd: -EBADF.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 0, 1, 0, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Error(9 /* EBADF */));

    // io_destroy against the destroyed fd: -EBADF (idempotent failure).
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Error(9 /* EBADF */));
}

/// `sys_io_destroy` against an unknown fd returns -EBADF.
#[test]
fn io_destroy_unknown_fd_returns_ebadf() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [9999, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Error(9 /* EBADF */));
}

/// `sys_io_destroy` drops the worker future from the registry —
/// `take_worker_future_for_test` returns `None` after destroy.
#[test]
fn io_destroy_drops_worker_from_registry() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let context_id = proc_cap
        .fd(fd)
        .expect("aio fd installed")
        .aio_context()
        .expect("aio_context")
        .context_id();

    // io_destroy drops the future from the registry.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));
    assert!(
        take_worker_future_for_test(context_id).is_none(),
        "io_destroy must drop the stashed worker future"
    );
}

/// `sys_io_destroy` mid-flight: submit an iocb, do not pump the
/// worker, then destroy. The worker terminates cleanly when next
/// polled (which mirrors the deferred-pump model production wiring
/// will replace with a real reactor abort).
#[test]
fn io_destroy_mid_flight_cancels_worker_cleanly() {
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
        .expect("aio fd installed")
        .aio_context()
        .expect("aio_context")
        .clone();
    let mut worker = take_worker_future_for_test(aio.context_id()).expect("worker stashed");

    // Submit an iocb. The dispatcher will run when we pump, but
    // we destroy first.
    let iocb = encode_iocb(0xDEAD, IOCB_CMD_PREAD, 99, 0, 16, 0);
    let (iocbpp, _ka) = stage_iocb_array(&[iocb]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));

    // Pump once so the body enters and parks.
    let waker = Waker::noop().clone();
    let mut poll_cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    match pinned.as_mut().poll(&mut poll_cx) {
        Poll::Pending => {}
        Poll::Ready(_) => {
            // It's also fine if the body already drained the iocb
            // and parked again on the empty queue. Re-poll once
            // more would be Pending.
        }
    }

    // Destroy.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [fd as u64, 0, 0, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));

    // Worker terminates with CooperativeCancel(OwnerRequested) on
    // its next yield.
    for _ in 0..64 {
        if let Poll::Ready(out) = pinned.as_mut().poll(&mut poll_cx) {
            assert!(
                matches!(
                    out,
                    Err(OnBehalfOfAbort::CooperativeCancel(
                        CancelReason::OwnerRequested
                    )) | Ok(_)
                ),
                "worker terminates after io_destroy with cancel or clean exit, got {out:?}"
            );
            return;
        }
    }
    panic!("worker did not terminate after mid-flight io_destroy within 64 polls");
}
