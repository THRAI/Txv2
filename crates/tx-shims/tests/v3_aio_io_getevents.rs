//! PR-11 phase 4 — `sys_io_getevents` + completion-queue tests.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §7
//!   (phase plan row P-11.5) + §4.2 (completion queue shape)
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution
//!   scope — worker enters `with_on_behalf_of` once at setup, pushes
//!   completions under the borrow)
//!
//! Pinned invariants:
//!
//! 1. **submit → dispatch → completion arrival round-trip.** A single
//!    `IOCB_CMD_PREAD` iocb is submitted; the worker body invokes the
//!    dispatcher (returns `-EBADF` since the test process has no fd 0
//!    in its bootstrap fd table); the resulting `IoEvent` lands on the
//!    completion queue; `sys_io_getevents` drains it and writes the
//!    32-byte `struct io_event` into user memory at the events array.
//!
//! 2. **`io_getevents` with `min_nr = 0` returns immediately.** Even
//!    if no completions are pending, the syscall returns `0` without
//!    blocking when the caller doesn't insist on any.
//!
//! 3. **`io_getevents` with `min_nr ≥ 2` blocks until enough events
//!    arrive.** Submits one iocb, pumps the worker once to produce
//!    one completion, then the syscall must wait (or return what it
//!    has after a non-blocking exit). For the canary's
//!    deferred-pump model we don't attempt a true block — we pump the
//!    worker to produce ≥ min_nr events first, then call getevents.
//!
//! 4. **`io_getevents` against a non-AIO fd returns -EINVAL.**
//!
//! 5. **The user-events buffer is written in `IoEvent`'s wire layout
//!    (`{data: u64@0, obj: u64@8, res: i64@16, res2: i64@24}`).**

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
use tx_shims::adapter::step_engine::Cap;
use tx_subsystems::aio::{reset_context_id_counter_for_test, AioWorkerFuture, IOCB_CMD_PREAD};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::aio::{reset_worker_registry_for_test, take_worker_future_for_test};
use tx_shims::linux_syscall::numbers::{NR_IO_GETEVENTS, NR_IO_SETUP, NR_IO_SUBMIT};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP (mirrors v3_aio_io_submit.rs) -------------------

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

// -------- Setup ----------------------------------------------------

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

/// Heap-allocate a buffer large enough for `n` `struct io_event`
/// records (32 bytes each) and return the raw pointer; leak it for
/// the test's lifetime so the user-VA write stays valid across
/// `sys_io_getevents` and the post-syscall assertion.
fn stage_events_buffer(n: usize) -> u64 {
    let buf: alloc::boxed::Box<[u8]> = alloc::vec![0u8; n * 32].into_boxed_slice();
    let leak = alloc::boxed::Box::leak(buf);
    leak.as_mut_ptr() as u64
}

fn pump_worker_until<F>(mut worker: AioWorkerFuture, mut done: F, budget: u32) -> AioWorkerFuture
where
    F: FnMut(&AioWorkerFuture) -> bool,
{
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    for _ in 0..budget {
        let _ = pinned.as_mut().poll(&mut cx);
        if done(&pinned) {
            return worker;
        }
    }
    panic!("pump_worker_until: condition not met within {budget} polls");
}

// -------- Tests ----------------------------------------------------

/// Round-trip: submit one PREAD → worker dispatches (returns -EBADF
/// since fd 0 is absent in the bootstrap process) → completion lands
/// on the queue → `sys_io_getevents` drains it and writes the
/// `struct io_event` into user memory.
#[test]
fn submit_dispatch_completion_round_trip() {
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

    let worker = take_worker_future_for_test(aio.context_id()).expect("worker stashed by io_setup");

    // Submit one PREAD iocb. aio_fildes=99 → dispatcher will get
    // None from ctx.process.fd(99) → returns -EBADF (-9).
    let iocb = encode_iocb(0xCAFE_BABE, IOCB_CMD_PREAD, 99, 0, 16, 0);
    let (iocbpp, _ka) = stage_iocb_array(&[iocb]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));

    // Pump worker until one completion lands.
    _ = pump_worker_until(worker, |_| aio.completion_len() >= 1, 128);
    assert_eq!(aio.completion_len(), 1);

    // Drain via sys_io_getevents.
    let events_ptr = stage_events_buffer(2);
    // timeout = 1 (any non-zero pointer) → non-blocking variant per
    // our canary semantic.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 0, 2, events_ptr, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));

    // Verify the event was serialised into user memory at events_ptr.
    let event_bytes = unsafe { core::slice::from_raw_parts(events_ptr as *const u8, 32) };
    let data = u64::from_le_bytes(event_bytes[0..8].try_into().unwrap());
    let obj = u64::from_le_bytes(event_bytes[8..16].try_into().unwrap());
    let res = i64::from_le_bytes(event_bytes[16..24].try_into().unwrap());
    let res2 = i64::from_le_bytes(event_bytes[24..32].try_into().unwrap());
    assert_eq!(data, 0xCAFE_BABE, "data echoes user cookie");
    assert_eq!(obj, 0xCAFE_BABE, "obj placeholder echoes user cookie");
    assert_eq!(res, -9, "PREAD against absent fd yields -EBADF (-9)");
    assert_eq!(res2, 0);
    assert_eq!(aio.completion_len(), 0, "queue drained by getevents");
}

/// `sys_io_getevents` with `min_nr = 0` and no completions pending
/// returns 0 immediately.
#[test]
fn io_getevents_with_min_nr_zero_and_empty_queue_returns_zero() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0])) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let events_ptr = stage_events_buffer(1);
    // timeout = 1 (non-blocking variant) so we don't park on the
    // wait carrier even with min_nr=0.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 0, 1, events_ptr, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));
}

/// `sys_io_getevents` with `min_nr = 2` drains both completions once
/// the worker has produced them. Pins the multi-event drain shape.
#[test]
fn io_getevents_min_nr_two_drains_two_completions() {
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
    let worker = take_worker_future_for_test(aio.context_id()).expect("worker stashed");

    // Submit two iocbs (both surface -EBADF since fd 99 absent).
    let a = encode_iocb(0xAAAA, IOCB_CMD_PREAD, 99, 0, 16, 0);
    let b = encode_iocb(0xBBBB, IOCB_CMD_PREAD, 99, 0, 16, 0);
    let (iocbpp, _ka) = stage_iocb_array(&[a, b]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 2, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(2));

    // Pump until both completions land.
    _ = pump_worker_until(worker, |_| aio.completion_len() >= 2, 128);
    assert_eq!(aio.completion_len(), 2);

    // Drain via sys_io_getevents with min_nr = 2.
    let events_ptr = stage_events_buffer(4);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 2, 4, events_ptr, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Return(2));

    // Both events should land at offsets 0 and 32.
    let first = unsafe { core::slice::from_raw_parts(events_ptr as *const u8, 32) };
    let second = unsafe { core::slice::from_raw_parts((events_ptr + 32) as *const u8, 32) };
    let data0 = u64::from_le_bytes(first[0..8].try_into().unwrap());
    let data1 = u64::from_le_bytes(second[0..8].try_into().unwrap());
    assert_eq!(data0, 0xAAAA);
    assert_eq!(data1, 0xBBBB);
}

/// `sys_io_getevents` against a non-AIO fd returns -EINVAL (or -EBADF
/// if the fd was never installed).
#[test]
fn io_getevents_against_non_aio_fd_returns_einval_or_ebadf() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let events_ptr = stage_events_buffer(1);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [0, 0, 1, events_ptr, 1, 0]),
    );
    match r {
        SyscallResult::Error(e) => assert!(
            e == 9 || e == 22,
            "expected -EBADF (9) or -EINVAL (22), got {e}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// `sys_io_getevents` with `nr = 0` returns 0 without touching the
/// queue or user memory.
#[test]
fn io_getevents_with_nr_zero_returns_zero() {
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
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 0, 0, 0, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Return(0));
}
