//! Raw Linux AIO ABI tests.
//!
//! These supersede the older fd-shaped AIO scaffold expectations:
//! `io_setup` writes an `aio_context_t` user handle and later syscalls
//! consume that handle directly.

extern crate alloc;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Arch, Asid, EntropyIf, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, TimeIf, UserPtr, VirtAddr,
};
use tx_shims::adapter::reactor_entry::SyscallRequest;
use tx_shims::adapter::step_engine::{self as zone, Cap};
use tx_shims::linux_syscall::aio::take_worker_future_for_test;
use tx_shims::linux_syscall::numbers::{
    NR_EVENTFD2, NR_IO_CANCEL, NR_IO_DESTROY, NR_IO_GETEVENTS, NR_IO_PGETEVENTS, NR_IO_SETUP,
    NR_IO_SUBMIT, NR_READ,
};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};
use tx_subsystems::aio::reset_context_id_counter_for_test;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::signal::{SignalMask, Signum};
use tx_subsystems::thread_runtime::execution::{step_sigprocmask, SigmaskHow};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{
    project, AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::zones;

struct StubPmap;

impl PlatformConfig for StubPmap {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "shims-aio-raw-test";
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
    for _ in 0..2048 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("block_on: future did not resolve");
}

fn dispatch_call(ctx: &SyscallCtx<'_>, req: SyscallRequest) -> SyscallResult {
    block_on(dispatch::<StubPmap>(req, ctx))
}

fn map_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, len: usize) {
    let len = (len.max(1) + USER_PAGE_SIZE - 1) & !(USER_PAGE_SIZE - 1);
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
        .expect("mmap anon for aio raw test");
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

fn read_user_u64(ctx: &SyscallCtx<'_>, uaddr: usize) -> u64 {
    let mut bytes = [0u8; 8];
    copy_from_user_bytes(ctx, uaddr, &mut bytes);
    u64::from_le_bytes(bytes)
}

fn read_user_u32(ctx: &SyscallCtx<'_>, uaddr: usize) -> u32 {
    let mut bytes = [0u8; 4];
    copy_from_user_bytes(ctx, uaddr, &mut bytes);
    u32::from_le_bytes(bytes)
}

const USER_CTXP: usize = 0x5200_0000;
const USER_IOCBPP: usize = 0x5201_0000;
const USER_IOCB: usize = 0x5202_0000;
const USER_EVENTS: usize = 0x5203_0000;
const USER_EVENTFD_READ: usize = 0x5204_0000;
const USER_SIGSET: usize = 0x5205_0000;
const USER_SIGMASK: usize = 0x5206_0000;
const USER_CANCEL_RESULT: usize = 0x5207_0000;
const EFD_NONBLOCK: u64 = 0x800;

fn setup_ctx() -> (Cap<ProcessIdentity>, SyscallCtx<'static>) {
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    (proc_cap, ctx)
}

fn raw_io_setup(ctx: &SyscallCtx<'_>, nr_events: u32) -> u64 {
    map_user_bytes(ctx, USER_CTXP, 8);
    copy_to_user_bytes(ctx, USER_CTXP, &0u64.to_le_bytes());
    let ret = dispatch_call(
        ctx,
        SyscallRequest::new(
            NR_IO_SETUP,
            [nr_events as u64, USER_CTXP as u64, 0, 0, 0, 0],
        ),
    );
    assert_eq!(ret, SyscallResult::Return(0));
    read_user_u64(ctx, USER_CTXP)
}

fn raw_context_inner_id(ctx: &SyscallCtx<'_>, ctx_id: u64) -> u64 {
    read_user_u32(ctx, ctx_id as usize) as u64
}

fn pump_raw_aio_worker_once(ctx: &SyscallCtx<'_>, ctx_id: u64) {
    let inner_id = raw_context_inner_id(ctx, ctx_id);
    let mut worker = take_worker_future_for_test(inner_id).expect("worker future stashed");
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut worker) };
    for _ in 0..16 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(_) | Poll::Pending => {
                // The worker is long-lived and normally returns Pending after
                // draining the current queue. Re-poll a bounded number of
                // times so the drain loop has a chance to publish completions.
            }
        }
    }
}

#[test]
fn io_setup_writes_raw_context_and_linux_ring_header() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();

    let ctx_id = raw_io_setup(&ctx, 4);

    assert_ne!(ctx_id, 0, "io_setup must write a nonzero aio_context_t");
    assert_eq!(read_user_u32(&ctx, ctx_id as usize + 4), 124);
    assert_eq!(read_user_u32(&ctx, ctx_id as usize + 16), 0xa10a10a1);
    assert_eq!(read_user_u32(&ctx, ctx_id as usize + 20), 1);
    assert_eq!(read_user_u32(&ctx, ctx_id as usize + 24), 0);
    assert_eq!(read_user_u32(&ctx, ctx_id as usize + 28), 128);
}

#[test]
fn io_setup_maps_raw_ring_as_shared_page_backed_memory() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();

    let ctx_id = raw_io_setup(&ctx, 4);
    let projection = project::project_address_space(&ctx.aspace);
    let ring = projection
        .mappings
        .iter()
        .find(|mapping| mapping.range.start().0 == ctx_id as usize)
        .expect("io_setup ring mapping is visible in address-space projection");

    assert_eq!(ring.prot, Prot::READ_WRITE);
    assert_eq!(ring.flags, VmEntryFlags::SHARED);
    assert_eq!(
        ring.backing,
        project::VmBackingProjection::Page { offset: 0 }
    );
}

#[test]
fn io_setup_rejects_nonzero_out_param_and_zero_events() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();

    map_user_bytes(&ctx, USER_CTXP, 8);
    copy_to_user_bytes(&ctx, USER_CTXP, &1234u64.to_le_bytes());
    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SETUP, [4, USER_CTXP as u64, 0, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Error(22));

    copy_to_user_bytes(&ctx, USER_CTXP, &0u64.to_le_bytes());
    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SETUP, [0, USER_CTXP as u64, 0, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Error(22));

    let ret = dispatch_call(&ctx, SyscallRequest::new(NR_IO_SETUP, [4, 0, 0, 0, 0, 0]));
    assert_eq!(ret, SyscallResult::Error(14));
}

#[test]
fn raw_context_submit_destroy_and_getevents_use_context_handle() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();
    let ctx_id = raw_io_setup(&ctx, 2);

    map_user_bytes(&ctx, USER_IOCBPP, 8);
    map_user_bytes(&ctx, USER_IOCB, 64);
    map_user_bytes(&ctx, USER_EVENTS, 32);
    copy_to_user_bytes(&ctx, USER_IOCBPP, &(USER_IOCB as u64).to_le_bytes());

    let mut iocb = [0u8; 64];
    iocb[0..8].copy_from_slice(&0xABCDu64.to_le_bytes());
    iocb[16..18].copy_from_slice(&6u16.to_le_bytes()); // IOCB_CMD_NOOP
    copy_to_user_bytes(&ctx, USER_IOCB, &iocb);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [ctx_id, 1, USER_IOCBPP as u64, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(1));

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [ctx_id, 0, 1, USER_EVENTS as u64, 1, 0]),
    );
    assert_eq!(
        ret,
        SyscallResult::Return(0),
        "io_submit admits only; completion is published by the AIO worker"
    );

    pump_raw_aio_worker_once(&ctx, ctx_id);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [ctx_id, 1, 1, USER_EVENTS as u64, 1, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(1));

    let mut event = [0u8; 32];
    copy_from_user_bytes(&ctx, USER_EVENTS, &mut event);
    assert_eq!(u64::from_le_bytes(event[0..8].try_into().unwrap()), 0xABCD);
    assert_eq!(
        u64::from_le_bytes(event[8..16].try_into().unwrap()),
        USER_IOCB as u64
    );
    assert_eq!(i64::from_le_bytes(event[16..24].try_into().unwrap()), 0);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_DESTROY, [ctx_id, 0, 0, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(0));

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [ctx_id, 0, 1, USER_EVENTS as u64, 1, 0]),
    );
    assert_eq!(ret, SyscallResult::Error(22));
}

#[test]
fn io_cancel_rejects_completed_or_unknown_requests() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();
    let ctx_id = raw_io_setup(&ctx, 2);

    map_user_bytes(&ctx, USER_IOCB, 64);
    let mut iocb = [0u8; 64];
    iocb[8..12].copy_from_slice(&0u32.to_le_bytes());
    copy_to_user_bytes(&ctx, USER_IOCB, &iocb);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_CANCEL, [ctx_id, USER_IOCB as u64, 0, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Error(11));

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_CANCEL, [0xDEAD_BEEF, USER_IOCB as u64, 0, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Error(22));
}

#[test]
fn io_cancel_queued_request_publishes_canceled_completion() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();
    let ctx_id = raw_io_setup(&ctx, 2);

    map_user_bytes(&ctx, USER_IOCBPP, 8);
    map_user_bytes(&ctx, USER_IOCB, 64);
    map_user_bytes(&ctx, USER_EVENTS, 32);
    map_user_bytes(&ctx, USER_CANCEL_RESULT, 32);
    copy_to_user_bytes(&ctx, USER_IOCBPP, &(USER_IOCB as u64).to_le_bytes());

    let mut iocb = [0u8; 64];
    iocb[0..8].copy_from_slice(&0xCA11u64.to_le_bytes());
    iocb[16..18].copy_from_slice(&6u16.to_le_bytes()); // IOCB_CMD_NOOP
    copy_to_user_bytes(&ctx, USER_IOCB, &iocb);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [ctx_id, 1, USER_IOCBPP as u64, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(1));

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(
            NR_IO_CANCEL,
            [ctx_id, USER_IOCB as u64, USER_CANCEL_RESULT as u64, 0, 0, 0],
        ),
    );
    assert_eq!(ret, SyscallResult::Return(0));

    let mut canceled = [0u8; 32];
    copy_from_user_bytes(&ctx, USER_CANCEL_RESULT, &mut canceled);
    assert_eq!(
        u64::from_le_bytes(canceled[0..8].try_into().unwrap()),
        0xCA11
    );
    assert_eq!(
        u64::from_le_bytes(canceled[8..16].try_into().unwrap()),
        USER_IOCB as u64
    );
    assert_eq!(
        i64::from_le_bytes(canceled[16..24].try_into().unwrap()),
        -125
    );

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_GETEVENTS, [ctx_id, 1, 1, USER_EVENTS as u64, 1, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(1));

    let mut event = [0u8; 32];
    copy_from_user_bytes(&ctx, USER_EVENTS, &mut event);
    assert_eq!(event, canceled);
}

#[test]
fn aio_resfd_completion_signals_eventfd() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();
    let ctx_id = raw_io_setup(&ctx, 2);
    let eventfd = match dispatch_call(
        &ctx,
        SyscallRequest::new(NR_EVENTFD2, [0, EFD_NONBLOCK, 0, 0, 0, 0]),
    ) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("eventfd2: {other:?}"),
    };

    map_user_bytes(&ctx, USER_IOCBPP, 8);
    map_user_bytes(&ctx, USER_IOCB, 64);
    map_user_bytes(&ctx, USER_EVENTS, 32);
    map_user_bytes(&ctx, USER_EVENTFD_READ, 8);
    copy_to_user_bytes(&ctx, USER_IOCBPP, &(USER_IOCB as u64).to_le_bytes());

    let mut iocb = [0u8; 64];
    iocb[0..8].copy_from_slice(&0xEEFDu64.to_le_bytes());
    iocb[16..18].copy_from_slice(&6u16.to_le_bytes()); // IOCB_CMD_NOOP
    iocb[56..60].copy_from_slice(&1u32.to_le_bytes()); // IOCB_FLAG_RESFD
    iocb[60..64].copy_from_slice(&(eventfd as u32).to_le_bytes());
    copy_to_user_bytes(&ctx, USER_IOCB, &iocb);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_IO_SUBMIT, [ctx_id, 1, USER_IOCBPP as u64, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(1));

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_READ, [eventfd, USER_EVENTFD_READ as u64, 8, 0, 0, 0]),
    );
    assert_eq!(
        ret,
        SyscallResult::Error(11),
        "eventfd is signaled by worker completion, not io_submit admission"
    );

    pump_raw_aio_worker_once(&ctx, ctx_id);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(NR_READ, [eventfd, USER_EVENTFD_READ as u64, 8, 0, 0, 0]),
    );
    assert_eq!(ret, SyscallResult::Return(8));
    assert_eq!(read_user_u64(&ctx, USER_EVENTFD_READ), 1);
}

#[test]
fn io_pgetevents_temporarily_applies_and_restores_sigmask() {
    let _g = setup();
    let (_proc_cap, ctx) = setup_ctx();
    let ctx_id = raw_io_setup(&ctx, 2);
    let sigterm = Signum::new(15).expect("SIGTERM");
    let sigint = Signum::new(2).expect("SIGINT");
    let mut original = SignalMask::EMPTY;
    original.block(sigterm);
    let _ = step_sigprocmask(&ctx.thread, SigmaskHow::SetMask, original);

    let mut temporary = SignalMask::EMPTY;
    temporary.block(sigint);
    map_user_bytes(&ctx, USER_SIGMASK, 8);
    map_user_bytes(&ctx, USER_SIGSET, 16);
    map_user_bytes(&ctx, USER_EVENTS, 32);
    copy_to_user_bytes(&ctx, USER_SIGMASK, &temporary.raw_bits().to_le_bytes());
    let mut sigset = [0u8; 16];
    sigset[0..8].copy_from_slice(&(USER_SIGMASK as u64).to_le_bytes());
    sigset[8..16].copy_from_slice(&8u64.to_le_bytes());
    copy_to_user_bytes(&ctx, USER_SIGSET, &sigset);

    let ret = dispatch_call(
        &ctx,
        SyscallRequest::new(
            NR_IO_PGETEVENTS,
            [ctx_id, 0, 1, USER_EVENTS as u64, 1, USER_SIGSET as u64],
        ),
    );
    assert_eq!(ret, SyscallResult::Return(0));
    assert_eq!(
        ctx.thread
            .payload_cap()
            .expect("payload")
            .signal_mask()
            .raw_bits(),
        original.raw_bits()
    );
}
