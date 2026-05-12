//! PR-10 phase 3 — `UFFDIO_REGISTER` ioctl + `VmEntry::ufd_registration`
//! field tests.
//!
//! Pins the wiring for:
//!
//! 1. `UFFDIO_REGISTER` rejects before `UFFDIO_API` is performed
//!    (`-EINVAL`).
//! 2. Page-alignment + range validation: bogus start, bogus length,
//!    out-of-range, zero length all map to `-EINVAL`.
//! 3. Mode validation: only `UFFDIO_REGISTER_MODE_MISSING` is
//!    accepted; `WP` / `MINOR` / unknown bits all return `-EINVAL`.
//! 4. A registered range:
//!    - Tags every covering VMA's `VmEntry::ufd_registration` with
//!      the ufd's id + MISSING mode.
//!    - Writes back
//!      `UFFDIO_COPY | UFFDIO_ZEROPAGE` in the `ioctls` field.
//!    - Appends one entry to the ufd's `registrations_snapshot()`.
//! 5. Missing-mapping registration (no VMA in the range) returns
//!    `-EINVAL` (substrate's `MissingMapping`).
//! 6. Re-registering an already-tagged whole-VMA range is
//!    **idempotent**: the call succeeds, the tag is re-stamped with
//!    the same id+mode, and a second entry is appended to the
//!    ufd's registrations list (history preserved).
//!
//! Per D7 §6 phase plan P-10.3: phases 4–5 add fault interception
//! and reply ioctls. This file's tests only exercise the registration
//! handshake and the VmEntry tag landing.

extern crate alloc;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use std::sync::{LazyLock, Mutex};

use tx_hal::{
    Asid, EntropyIf, PhysAddr, PmapError, PmapIf, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, TimeIf, VirtAddr,
};
use tx_reactor::userspace::SyscallRequest;
use tx_substrate::zone::Cap;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::userfaultfd::reset_ufd_id_counter_for_test;
use tx_subsystems::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::zones;

use tx_shims::linux_syscall::numbers::{
    NR_IOCTL, NR_USERFAULTFD, UFFDIO_API, UFFDIO_REGISTER, UFFDIO_REGISTER_MODE_MINOR,
    UFFDIO_REGISTER_MODE_MISSING, UFFDIO_REGISTER_MODE_WP, UFFDIO_REGISTER_REPLY_IOCTLS, UFFD_API,
};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP -------------------------------------------------

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

// -------- Helpers ---------------------------------------------------

/// Userland layout of `struct uffdio_range`; mirrors the kernel-side
/// shape used by `step_uffdio_register`. Two `u64` fields, 16 bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

/// Userland layout of `struct uffdio_register`; mirrors the kernel-side
/// shape. 32 bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

/// Mirror of `UffdioApi` from the shim's test scaffold.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

fn dispatch_userfaultfd(ctx: &SyscallCtx<'_>, flags: u32) -> SyscallResult {
    let req = SyscallRequest::new(NR_USERFAULTFD, [flags as u64, 0, 0, 0, 0, 0]);
    block_on(dispatch::<StubPmap>(req, ctx))
}

fn dispatch_uffdio_api(ctx: &SyscallCtx<'_>, ufd_fd: u32, argp: u64) -> SyscallResult {
    let req = SyscallRequest::new(NR_IOCTL, [ufd_fd as u64, UFFDIO_API as u64, argp, 0, 0, 0]);
    block_on(dispatch::<StubPmap>(req, ctx))
}

fn dispatch_uffdio_register(ctx: &SyscallCtx<'_>, ufd_fd: u32, argp: u64) -> SyscallResult {
    let req = SyscallRequest::new(
        NR_IOCTL,
        [ufd_fd as u64, UFFDIO_REGISTER as u64, argp, 0, 0, 0],
    );
    block_on(dispatch::<StubPmap>(req, ctx))
}

/// Mint a ufd fd and perform the api handshake. Returns the fd.
fn open_ufd_with_handshake(ctx: &SyscallCtx<'_>) -> u32 {
    let fd = match dispatch_userfaultfd(ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let mut api = UffdioApi {
        api: UFFD_API,
        features: 0,
        ioctls: 0,
    };
    let argp = (&mut api as *mut UffdioApi) as u64;
    assert_eq!(
        dispatch_uffdio_api(ctx, fd, argp),
        SyscallResult::Return(0),
        "UFFDIO_API handshake must succeed"
    );
    fd
}

/// Stamp a private-anon VMA covering `[base, base+len)` so a later
/// `UFFDIO_REGISTER` has something to tag.
fn install_anon_vma(aspace: &Cap<AddressSpace>, base: usize, page_count: usize) -> UserRange {
    let range = UserRange::new_aligned(UserVirtAddr(base), page_count * USER_PAGE_SIZE)
        .expect("aligned range");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    aspace.try_mmap(request).expect("mmap anon vma");
    range
}

// -------- Tests -----------------------------------------------------

/// `UFFDIO_REGISTER` before `UFFDIO_API` returns `-EINVAL`.
#[test]
fn uffdio_register_without_handshake_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Open ufd but do NOT drive UFFDIO_API.
    let fd = match dispatch_userfaultfd(&ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    let base = 0x10_0000usize;
    install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22),
        "register without handshake → EINVAL"
    );
}

/// `UFFDIO_REGISTER` with `mode == 0` returns `-EINVAL`.
#[test]
fn uffdio_register_zero_mode_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: 0,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_REGISTER` with `MODE_WP` returns `-EINVAL` (deferred per
/// D7).
#[test]
fn uffdio_register_wp_mode_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_WP,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22),
        "WP mode → EINVAL (deferred)"
    );
}

/// `UFFDIO_REGISTER` with `MODE_MINOR` returns `-EINVAL`.
#[test]
fn uffdio_register_minor_mode_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MINOR,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_REGISTER` with an unaligned start returns `-EINVAL`.
#[test]
fn uffdio_register_unaligned_start_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: (base + 1) as u64, // off-by-one, unaligned
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22),
        "unaligned start → EINVAL"
    );
}

/// `UFFDIO_REGISTER` with `len == 0` returns `-EINVAL`.
#[test]
fn uffdio_register_zero_length_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: 0,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22)
    );
}

/// `UFFDIO_REGISTER` against an unmapped range returns `-EINVAL`
/// (substrate's `MissingMapping`).
#[test]
fn uffdio_register_unmapped_range_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);

    // No VMA installed at this base.
    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: 0x20_0000,
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Error(22),
        "unmapped range → EINVAL"
    );
}

/// `UFFDIO_REGISTER` against a valid whole-VMA range succeeds:
/// returns 0, writes back the `UFFDIO_COPY | UFFDIO_ZEROPAGE`
/// supported-ioctls bitmap, tags the covering VMA's
/// `ufd_registration`, and appends one entry to the ufd's
/// `registrations_snapshot`.
#[test]
fn uffdio_register_valid_range_tags_vma_and_writes_ioctls() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    let page_count = 2;
    let vma_range = install_anon_vma(&ctx.aspace, base, page_count);

    // Sanity: pre-registration, the recipe has no ufd tag.
    let entry_before = ctx.aspace.lookup(vma_range.start()).expect("vma present");
    assert!(
        entry_before.ufd_registration.is_none(),
        "pre-registration vma should be untagged"
    );

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: (page_count * USER_PAGE_SIZE) as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0xDEAD_BEEF, // any garbage; arm should overwrite
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Return(0),
        "valid register → 0"
    );

    // Writeback: supported reply ioctls bitmap, unchanged range/mode.
    assert_eq!(reg.ioctls, UFFDIO_REGISTER_REPLY_IOCTLS);
    assert_eq!(reg.range.start, base as u64);
    assert_eq!(reg.range.len, (page_count * USER_PAGE_SIZE) as u64);
    assert_eq!(reg.mode, UFFDIO_REGISTER_MODE_MISSING);

    // VMA tagged with ufd id + MISSING mode.
    let entry_after = ctx
        .aspace
        .lookup(vma_range.start())
        .expect("vma still present");
    let tag = entry_after
        .ufd_registration
        .expect("vma must carry ufd_registration after register");

    let open_file = proc_cap.fd(fd).expect("fd still installed");
    let ufd = open_file.ufd().expect("ufd backing");
    assert_eq!(tag.ufd_id, ufd.ufd_id(), "tag id matches ufd_id()");
    assert_eq!(tag.mode, UFFDIO_REGISTER_MODE_MISSING);

    // Per-ufd registration bookkeeping appended.
    let regs = ufd.registrations_snapshot();
    assert_eq!(regs.len(), 1, "exactly one registration recorded");
    assert_eq!(regs[0].start, base as u64);
    assert_eq!(regs[0].len, (page_count * USER_PAGE_SIZE) as u64);
    assert_eq!(regs[0].mode, UFFDIO_REGISTER_MODE_MISSING);
}

/// Re-registering an already-registered whole-VMA range succeeds:
/// the VMA tag is re-stamped (same id + mode), and a second entry
/// is appended to the ufd's `registrations_snapshot` (history is
/// preserved per the phase 3 docs).
#[test]
fn uffdio_register_duplicate_is_idempotent_and_appends_history() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    let base = 0x10_0000usize;
    let vma_range = install_anon_vma(&ctx.aspace, base, 1);

    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: base as u64,
            len: USER_PAGE_SIZE as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    let argp = (&mut reg as *mut UffdioRegister) as u64;

    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Return(0)
    );
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, argp),
        SyscallResult::Return(0),
        "duplicate register must succeed (idempotent at VMA tag)"
    );

    let entry = ctx.aspace.lookup(vma_range.start()).expect("vma present");
    let tag = entry.ufd_registration.expect("vma tagged");
    let open_file = proc_cap.fd(fd).expect("fd still installed");
    let ufd = open_file.ufd().expect("ufd backing");
    assert_eq!(tag.ufd_id, ufd.ufd_id());
    assert_eq!(tag.mode, UFFDIO_REGISTER_MODE_MISSING);

    // History: two appended entries.
    assert_eq!(
        ufd.registration_count(),
        2,
        "each successful register appends one entry"
    );
}

/// `UFFDIO_REGISTER` with `argp == 0` returns `-EFAULT` (14).
#[test]
fn uffdio_register_null_argp_returns_efault() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = open_ufd_with_handshake(&ctx);
    assert_eq!(
        dispatch_uffdio_register(&ctx, fd, 0),
        SyscallResult::Error(14),
        "null argp → EFAULT"
    );
}
