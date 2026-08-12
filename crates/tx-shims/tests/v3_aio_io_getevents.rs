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

use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Arch, Asid, DeadlineTimerIf, EntropyIf, MonotonicCounterIf, PhysAddr, PlatformConfig,
    PmapError, PmapIf, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot,
    PmapUnmapResult, PtNode, UserPtr, VirtAddr,
};
use tx_shims::adapter::reactor_entry::SyscallRequest;
use tx_shims::adapter::step_engine::{self as zone, Cap};
use tx_substrate::step::InterestMask;
use tx_substrate::wake::{MailboxEvent, TaskMailbox};
use tx_subsystems::aio::{
    reset_context_id_counter_for_test, AioWorkerFuture, EVENTS_AVAILABLE_MASK, IOCB_CMD_PREAD,
};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
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
impl tx_hal::AuxvIf for StubPmap {}
impl tx_hal::ConsoleIf for StubPmap {
    fn write_bytes(_bytes: &[u8]) {}
}
impl tx_hal::SmpIf for StubPmap {}

impl MonotonicCounterIf for StubPmap {
    fn read_ns() -> u64 {
        0
    }

    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl DeadlineTimerIf for StubPmap {
    fn set_deadline_ns(_deadline: u64) {}

    fn cancel_deadline() {}
}

impl tx_hal::PersistentClockIf for StubPmap {}

// -------- Setup ----------------------------------------------------

static TEST_LOCK: Mutex<()> = Mutex::new(());
static AIO_REF_POST_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

fn counting_aio_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    AIO_REF_POST_COUNT.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    mailbox.post(event)
}

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

const USER_AIO_IOCBPP: usize = 0x5300_0000;
const USER_AIO_EVENTS: usize = 0x5300_4000;

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn map_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, len: usize) {
    let len = align_up(len.max(1), USER_PAGE_SIZE);
    let range = UserRange::new_aligned(UserVirtAddr(uaddr), len).expect("aligned user range");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace
        .try_mmap(request)
        .expect("mmap anon for aio io_getevents test");
}

fn copy_to_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) {
    let guard = zone::guard();
    let copied = ctx
        .aspace
        .copy_to_user(UserPtr::<u8>::new(uaddr), bytes, &guard);
    drop(guard);
    assert_eq!(copied, zone::StepOutcome::Done(bytes.len()));
}

fn copy_from_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, out: &mut [u8]) {
    let guard = zone::guard();
    let copied = ctx
        .aspace
        .copy_from_user(out, UserPtr::<u8>::new(uaddr), &guard);
    drop(guard);
    assert_eq!(copied, zone::StepOutcome::Done(out.len()));
}

fn stage_iocb_array(ctx: &SyscallCtx<'_>, base: usize, iocbs: &[[u8; 64]]) -> u64 {
    let pointer_bytes_len = iocbs.len() * core::mem::size_of::<u64>();
    let iocb_base = base + USER_PAGE_SIZE;
    map_user_bytes(ctx, base, pointer_bytes_len);
    map_user_bytes(ctx, iocb_base, iocbs.len() * 64);

    let mut pointer_bytes = alloc::vec![0u8; pointer_bytes_len];
    for (i, iocb) in iocbs.iter().enumerate() {
        let iocb_addr = iocb_base + i * 64;
        pointer_bytes[i * 8..(i + 1) * 8].copy_from_slice(&(iocb_addr as u64).to_le_bytes());
        copy_to_user_bytes(ctx, iocb_addr, iocb);
    }
    copy_to_user_bytes(ctx, base, &pointer_bytes);
    base as u64
}

fn stage_events_buffer(ctx: &SyscallCtx<'_>, uaddr: usize, n: usize) -> u64 {
    map_user_bytes(ctx, uaddr, n * 32);
    uaddr as u64
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
    AIO_REF_POST_COUNT.store(0, core::sync::atomic::Ordering::Release);
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread).with_mailbox_ref_post(counting_aio_ref_post);

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
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let _sub = aio
        .events_available_source()
        .prepare(
            Arc::downgrade(&mailbox),
            generation,
            InterestMask::new(EVENTS_AVAILABLE_MASK),
        )
        .install();
    let events_source = aio.events_available_id();

    let worker = take_worker_future_for_test(aio.context_id()).expect("worker stashed by io_setup");

    // Submit one PREAD iocb. aio_fildes=99 → dispatcher will get
    // None from ctx.process.fd(99) → returns -EBADF (-9).
    let iocb = encode_iocb(0xCAFE_BABE, IOCB_CMD_PREAD, 99, 0, 16, 0);
    let iocbpp = stage_iocb_array(&ctx, USER_AIO_IOCBPP, &[iocb]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 1, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));

    // Pump worker until one completion lands.
    _ = pump_worker_until(worker, |_| aio.completion_len() >= 1, 128);
    assert_eq!(aio.completion_len(), 1);
    assert_eq!(
        AIO_REF_POST_COUNT.load(core::sync::atomic::Ordering::Acquire),
        1,
        "AIO worker completion should use the SyscallCtx mailbox-ref post"
    );
    match mailbox.poll().expect("events_available source fired") {
        MailboxEvent::SourceFired {
            generation: seen_generation,
            source,
            interests,
        } => {
            assert_eq!(seen_generation, generation);
            assert_eq!(source.raw(), events_source);
            assert_eq!(interests.raw(), EVENTS_AVAILABLE_MASK);
        }
        other => panic!("expected AIO SourceFired, got {other:?}"),
    }

    // Drain via sys_io_getevents.
    let events_ptr = stage_events_buffer(&ctx, USER_AIO_EVENTS, 2);
    // timeout = 1 (any non-zero pointer) → non-blocking variant per
    // our canary semantic.
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 0, 2, events_ptr, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Return(1));

    // Verify the event was serialised into user memory at events_ptr.
    let mut event_bytes = [0u8; 32];
    copy_from_user_bytes(&ctx, events_ptr as usize, &mut event_bytes);
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

    let events_ptr = stage_events_buffer(&ctx, USER_AIO_EVENTS, 1);
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
    let iocbpp = stage_iocb_array(&ctx, USER_AIO_IOCBPP, &[a, b]);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [fd as u64, 2, iocbpp, 0, 0, 0]),
    );
    assert_eq!(r, SyscallResult::Return(2));

    // Pump until both completions land.
    _ = pump_worker_until(worker, |_| aio.completion_len() >= 2, 128);
    assert_eq!(aio.completion_len(), 2);

    // Drain via sys_io_getevents with min_nr = 2.
    let events_ptr = stage_events_buffer(&ctx, USER_AIO_EVENTS, 4);
    let r = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [fd as u64, 2, 4, events_ptr, 1, 0]),
    );
    assert_eq!(r, SyscallResult::Return(2));

    // Both events should land at offsets 0 and 32.
    let mut first = [0u8; 32];
    let mut second = [0u8; 32];
    copy_from_user_bytes(&ctx, events_ptr as usize, &mut first);
    copy_from_user_bytes(&ctx, events_ptr as usize + 32, &mut second);
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

    let events_ptr = stage_events_buffer(&ctx, USER_AIO_EVENTS, 1);
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
