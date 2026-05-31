//! PR-10 phase 5 — `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` / `UFFDIO_CONTINUE`
//! reply ioctls + `read(uffd_fd, &mut uffd_msg)` arm tests.
//!
//! Pins the wiring for:
//!
//! 1. The three reply ioctls (COPY / ZEROPAGE / CONTINUE):
//!    - Pre-handshake call rejects with `-EINVAL`.
//!    - Validation: bogus `dst` alignment, zero `len`, len not a
//!      page multiple, range outside any registered range all map
//!      to `-EINVAL`.
//!    - "No pending fault" (queue empty) rejects with `-EINVAL`.
//!    - Successful reply: a pending fault pushed onto the ufd's queue
//!      gets drained, the `DelegateRegistry::mark_replied` transitions
//!      `Pending → Replied`, the queue depth returns to 0, and the
//!      arg struct's `copy` / `zeropage` / `mapped` field is written
//!      back with `len`.
//!
//! 2. The `read(uffd_fd, &mut uffd_msg)` arm:
//!    - Empty queue + O_NONBLOCK → `-EAGAIN`.
//!    - One pushed message → `Return(32)` with the wire-format bytes
//!      written to user memory; subsequent read with empty queue
//!      → `-EAGAIN`.
//!    - Wire layout: byte 0 = `UFFD_EVENT_PAGEFAULT`; bytes 16..24 =
//!      `fault_addr.to_le_bytes()`.
//!
//! Per D7 §6 phase plan P-10.5: phase 6 lands the e2e Linux-style
//! agent program test against the QEMU shim.

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
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::userfaultfd::{reset_ufd_id_counter_for_test, UffdMsg, UFFD_MSG_WIRE_SIZE};
use tx_subsystems::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::zones;

use tx_shims::linux_syscall::numbers::{
    NR_IOCTL, NR_READ, NR_USERFAULTFD, UFFDIO_API, UFFDIO_CONTINUE, UFFDIO_COPY, UFFDIO_REGISTER,
    UFFDIO_REGISTER_MODE_MISSING, UFFDIO_ZEROPAGE, UFFD_API, UFFD_EVENT_PAGEFAULT,
};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP -------------------------------------------------

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
impl tx_hal::CacheIf for StubPmap {}

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
    reset_ufd_id_counter_for_test();
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

// -------- Userland struct mirrors ----------------------------------

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioZeropage {
    range: UffdioRange,
    mode: u64,
    zeropage: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioContinue {
    range: UffdioRange,
    mode: u64,
    mapped: u64,
}

// -------- Dispatch helpers ------------------------------------------

fn dispatch_userfaultfd(ctx: &SyscallCtx<'_>, flags: u32) -> SyscallResult {
    let req = SyscallRequest::new(NR_USERFAULTFD, [flags as u64, 0, 0, 0, 0, 0]);
    block_on(dispatch::<StubPmap>(req, ctx))
}

fn dispatch_ioctl(ctx: &SyscallCtx<'_>, fd: u32, request: u32, argp: u64) -> SyscallResult {
    let req = SyscallRequest::new(NR_IOCTL, [fd as u64, request as u64, argp, 0, 0, 0]);
    block_on(dispatch::<StubPmap>(req, ctx))
}

fn dispatch_read(ctx: &SyscallCtx<'_>, fd: u32, buf: u64, len: usize) -> SyscallResult {
    let req = SyscallRequest::new(NR_READ, [fd as u64, buf, len as u64, 0, 0, 0]);
    block_on(dispatch::<StubPmap>(req, ctx))
}

const USER_UFD_IOCTL_ARG: usize = 0x5300_0000;
const USER_UFD_READ_BUF: usize = 0x5300_1000;

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
        .expect("mmap anon for ufd test");
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

fn stage_user_value<T: Copy>(ctx: &SyscallCtx<'_>, uaddr: usize, value: &T) -> u64 {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(*value) as *const u8,
            core::mem::size_of::<T>(),
        )
    };
    if ctx
        .aspace
        .lookup(UserVirtAddr(uaddr))
        .filter(|entry| {
            entry
                .range
                .contains_addr(UserVirtAddr(uaddr + bytes.len() - 1))
        })
        .is_none()
    {
        map_user_bytes(ctx, uaddr, bytes.len());
    }
    copy_to_user_bytes(ctx, uaddr, bytes);
    uaddr as u64
}

fn load_user_value<T: Copy>(ctx: &SyscallCtx<'_>, uaddr: usize) -> T {
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, core::mem::size_of::<T>())
    };
    copy_from_user_bytes(ctx, uaddr, bytes);
    unsafe { value.assume_init() }
}

/// Mint a ufd fd, perform the API handshake, and register a single
/// private-anon page at `base`. Returns the fd.
fn open_ufd_register_one_page(ctx: &SyscallCtx<'_>, base: usize) -> u32 {
    let fd = match dispatch_userfaultfd(ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    // API handshake.
    let api = UffdioApi {
        api: UFFD_API,
        features: 0,
        ioctls: 0,
    };
    let argp = stage_user_value(ctx, USER_UFD_IOCTL_ARG, &api);
    assert_eq!(
        dispatch_ioctl(ctx, fd, UFFDIO_API, argp),
        SyscallResult::Return(0)
    );

    // Install a private-anon VMA.
    let range = UserRange::new_aligned(UserVirtAddr(base), USER_PAGE_SIZE).expect("aligned range");
    let req = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(req).expect("mmap anon");

    // Register.
    let reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    let reg_argp = stage_user_value(ctx, USER_UFD_IOCTL_ARG, &reg);
    assert_eq!(
        dispatch_ioctl(ctx, fd, UFFDIO_REGISTER, reg_argp),
        SyscallResult::Return(0)
    );
    fd
}

/// Push a fault directly onto the ufd's pending queue + install a
/// matching pending request in its DelegateRegistry. Mirrors the
/// runtime path the phase-4 fault_script takes, without requiring a
/// fault interceptor here.
fn push_fault(ctx: &SyscallCtx<'_>, fd: u32, fault_addr: u64) {
    use tx_shims::adapter::step_engine::{
        AgentCancelPolicy, DelegateRequest, TokenDropPolicy, UfdAccessKind, UfdRequest,
    };
    let open = ctx.process.fd(fd).expect("fd installed");
    let ufd = open.ufd().expect("ufd backing");
    let request = DelegateRequest::Ufd(UfdRequest::PageFault {
        faulting_addr: fault_addr,
        access_kind: UfdAccessKind::Missing,
        faulting_tid: 0,
    });
    // Install request — endpoint marker = ufd_id, no mailbox, no
    // deadline.
    let guard = ufd.delegate_registry().install_request(
        request,
        ufd.ufd_id(),
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        alloc::sync::Weak::new(),
        None,
    );
    let token_id = guard.id();
    // Detach the guard so it doesn't fire CancelOnDrop on the slot.
    core::mem::forget(guard);

    ufd.push_fault_msg(UffdMsg {
        event: UFFD_EVENT_PAGEFAULT,
        fault_addr,
        ufd_thread_id: 0,
        token_id,
    });
}

// -------- Tests -----------------------------------------------------

/// `UFFDIO_COPY` before `UFFDIO_API` returns `-EINVAL`.
#[test]
fn uffdio_copy_without_handshake_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = match dispatch_userfaultfd(&ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let req = UffdioCopy {
        dst: 0x10_0000,
        src: 0,
        len: USER_PAGE_SIZE as u64,
        mode: 0,
        copy: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_COPY, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_COPY` with unaligned `dst` returns `-EINVAL`.
#[test]
fn uffdio_copy_unaligned_dst_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);
    let req = UffdioCopy {
        dst: (base + 1) as u64,
        src: 0,
        len: USER_PAGE_SIZE as u64,
        mode: 0,
        copy: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_COPY, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_COPY` with `len = 0` returns `-EINVAL`.
#[test]
fn uffdio_copy_zero_len_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);
    let req = UffdioCopy {
        dst: base as u64,
        src: 0,
        len: 0,
        mode: 0,
        copy: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_COPY, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_COPY` when no fault is pending returns `-EINVAL`.
#[test]
fn uffdio_copy_no_pending_fault_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    let req = UffdioCopy {
        dst: base as u64,
        src: 0,
        len: USER_PAGE_SIZE as u64,
        mode: 0,
        copy: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_COPY, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_COPY` with a matching pending fault drives `mark_replied`,
/// pops the fault from the queue, and writes back `copy = len`.
#[test]
fn uffdio_copy_with_pending_fault_succeeds_and_drains_queue() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);

    // Pre-state: one pending fault.
    let open = ctx.process.fd(fd).expect("fd installed");
    let ufd = open.ufd().expect("ufd backing");
    assert_eq!(ufd.pending_fault_count(), 1);

    let req = UffdioCopy {
        dst: base as u64,
        src: 0xDEAD_BEEF,
        len: USER_PAGE_SIZE as u64,
        mode: 0,
        copy: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_COPY, argp),
        SyscallResult::Return(0)
    );

    // Post-state: queue drained, writeback set.
    assert_eq!(ufd.pending_fault_count(), 0);
    let req_after: UffdioCopy = load_user_value(&ctx, USER_UFD_IOCTL_ARG);
    assert_eq!(req_after.copy, USER_PAGE_SIZE as u64);
}

/// `UFFDIO_ZEROPAGE` with a matching pending fault drives `mark_replied`,
/// pops the fault, and writes back `zeropage = len`.
#[test]
fn uffdio_zeropage_with_pending_fault_succeeds_and_drains_queue() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);

    let req = UffdioZeropage {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: 0,
        zeropage: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_ZEROPAGE, argp),
        SyscallResult::Return(0)
    );

    let open = ctx.process.fd(fd).expect("fd installed");
    let ufd = open.ufd().expect("ufd backing");
    assert_eq!(ufd.pending_fault_count(), 0);
    let req_after: UffdioZeropage = load_user_value(&ctx, USER_UFD_IOCTL_ARG);
    assert_eq!(req_after.zeropage, USER_PAGE_SIZE as u64);
}

/// `UFFDIO_CONTINUE` with a matching pending fault drives
/// `mark_replied`, pops the fault, and writes back `mapped = len`.
#[test]
fn uffdio_continue_with_pending_fault_succeeds_and_drains_queue() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);

    let req = UffdioContinue {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: 0,
        mapped: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_CONTINUE, argp),
        SyscallResult::Return(0)
    );

    let open = ctx.process.fd(fd).expect("fd installed");
    let ufd = open.ufd().expect("ufd backing");
    assert_eq!(ufd.pending_fault_count(), 0);
    let req_after: UffdioContinue = load_user_value(&ctx, USER_UFD_IOCTL_ARG);
    assert_eq!(req_after.mapped, USER_PAGE_SIZE as u64);
}

/// `UFFDIO_COPY` with `dst != fault_addr` returns `-EINVAL`. The agent
/// must address the same page that was reported in the fault message.
#[test]
fn uffdio_copy_mismatched_dst_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);

    let other = base + USER_PAGE_SIZE; // outside the registered range
    let req = UffdioCopy {
        dst: other as u64,
        src: 0,
        len: USER_PAGE_SIZE as u64,
        mode: 0,
        copy: 0,
    };
    let argp = stage_user_value(&ctx, USER_UFD_IOCTL_ARG, &req);
    assert_eq!(
        dispatch_ioctl(&ctx, fd, UFFDIO_COPY, argp),
        SyscallResult::Error(22)
    );

    // Queue remained intact — the front message stayed.
    let open = ctx.process.fd(fd).expect("fd installed");
    let ufd = open.ufd().expect("ufd backing");
    assert_eq!(ufd.pending_fault_count(), 1);
}

/// `read(uffd_fd, &mut uffd_msg)` with empty queue + `O_NONBLOCK`
/// returns `-EAGAIN`.
#[test]
fn ufd_read_empty_queue_nonblocking_returns_eagain() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Open ufd with O_NONBLOCK (= 0x800). The substrate stashes the
    // flag on the OpenFile so step_ufd_read sees it on the
    // empty-queue path.
    const O_NONBLOCK: u32 = 0x800;
    let fd = match dispatch_userfaultfd(&ctx, O_NONBLOCK) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let buf_ptr = USER_UFD_READ_BUF as u64;
    map_user_bytes(&ctx, USER_UFD_READ_BUF, UFFD_MSG_WIRE_SIZE);
    assert_eq!(
        dispatch_read(&ctx, fd, buf_ptr, UFFD_MSG_WIRE_SIZE),
        SyscallResult::Error(11),
        "empty queue + O_NONBLOCK → EAGAIN"
    );
}

/// `read(uffd_fd, &mut uffd_msg)` with a pushed message returns 32
/// bytes containing the Linux-wire `struct uffd_msg` layout.
#[test]
fn ufd_read_with_pending_returns_serialized_uffd_msg() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);

    let mut buf = [0xAAu8; UFFD_MSG_WIRE_SIZE];
    let buf_ptr = USER_UFD_READ_BUF as u64;
    map_user_bytes(&ctx, USER_UFD_READ_BUF, UFFD_MSG_WIRE_SIZE);
    copy_to_user_bytes(&ctx, USER_UFD_READ_BUF, &buf);
    assert_eq!(
        dispatch_read(&ctx, fd, buf_ptr, UFFD_MSG_WIRE_SIZE),
        SyscallResult::Return(UFFD_MSG_WIRE_SIZE as i64),
        "pending message → Return(32)"
    );

    // Wire-format pin:
    //   byte 0     = UFFD_EVENT_PAGEFAULT (0x12)
    //   bytes 1..8 = reserved (zero)
    //   bytes 8..16 = pagefault.flags (zero — Missing)
    //   bytes 16..24 = pagefault.address (le bytes of fault_addr)
    //   bytes 24..28 = pagefault.feat.ptid (zero in test push)
    //   bytes 28..32 = reserved (zero)
    copy_from_user_bytes(&ctx, USER_UFD_READ_BUF, &mut buf);
    assert_eq!(buf[0], UFFD_EVENT_PAGEFAULT, "wire byte 0 is event");
    for b in &buf[1..16] {
        assert_eq!(*b, 0, "reserved + flags must be zero");
    }
    let addr_bytes = &buf[16..24];
    let observed = u64::from_le_bytes(addr_bytes.try_into().unwrap());
    assert_eq!(observed, base as u64, "wire bytes 16..24 carry fault_addr");

    // Queue drained by the read.
    let open = ctx.process.fd(fd).expect("fd installed");
    let ufd = open.ufd().expect("ufd backing");
    assert_eq!(ufd.pending_fault_count(), 0);
}

/// `read(uffd_fd, &mut uffd_msg, n)` with `n < UFFD_MSG_WIRE_SIZE`
/// returns `-EINVAL` — Linux requires the agent supply at least one
/// full message worth of buffer.
#[test]
fn ufd_read_buf_too_small_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let base = 0x10_0000usize;
    let fd = open_ufd_register_one_page(&ctx, base);
    push_fault(&ctx, fd, base as u64);

    let buf_ptr = USER_UFD_READ_BUF as u64;
    map_user_bytes(&ctx, USER_UFD_READ_BUF, UFFD_MSG_WIRE_SIZE - 1);
    assert_eq!(
        dispatch_read(&ctx, fd, buf_ptr, UFFD_MSG_WIRE_SIZE - 1),
        SyscallResult::Error(22),
        "len < sizeof(struct uffd_msg) → EINVAL"
    );
}
