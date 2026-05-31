//! PR-10 phase 2 — `sys_userfaultfd` + `UFFDIO_API` scaffold tests.
//!
//! Pins the wiring for:
//!
//! 1. `sys_userfaultfd(flags)` mints a `Cap<UserfaultFd>` (W-Q phase
//!    0 zone), wraps in an `OpenFile` with `OpenFileBacking::Ufd`,
//!    installs at the lowest free fd, returns the fd, and sets
//!    `O_CLOEXEC` on the per-process cloexec bitmap when requested.
//! 2. `ioctl(ufd, UFFDIO_API, &api)` returns 0 on the first call,
//!    writes back `features = 0, ioctls = 0`, and flips the ufd's
//!    handshake bit. A second call returns `-EPERM` (Linux's
//!    "API already set" rule). Mismatching api/features return
//!    `-EINVAL`.
//! 3. The scaffold round-trip is **end-to-end via `dispatch`** so
//!    the dispatch table entries (NR_USERFAULTFD, NR_IOCTL) are
//!    exercised the same way userspace would reach them.
//!
//! Per D7 §6 phase plan P-10.2: phases 3–5 add `UFFDIO_REGISTER`,
//! fault-path interception, and reply ioctls. This file's tests
//! only exercise the open + handshake + close primitives.

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
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::userfaultfd::reset_ufd_id_counter_for_test;
use tx_subsystems::vfs::structure::OpenFileBacking;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use tx_shims::linux_syscall::numbers::{NR_IOCTL, NR_USERFAULTFD, O_CLOEXEC, UFFDIO_API, UFFD_API};
use tx_shims::linux_syscall::{dispatch, SyscallCtx, SyscallResult};

// -------- Stub PMAP (mirrors v3_subject_population.rs) --------------

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

// -------- Helpers ---------------------------------------------------

/// Userland layout of `struct uffdio_api`; mirrors the kernel-side
/// shape used by `step_uffdio_api`. Three `u64` fields, 24 bytes.
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

// -------- Tests -----------------------------------------------------

/// `sys_userfaultfd(0)` returns a fresh fd >= 0 whose backing is the
/// userfaultfd shape.
#[test]
fn sys_userfaultfd_returns_a_userfaultfd_backed_fd() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let result = dispatch_userfaultfd(&ctx, 0);
    let fd = match result {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let open_file = proc_cap.fd(fd).expect("fd installed");
    // The resolved OpenFile is the ufd shape, not the VFS shape.
    assert!(
        matches!(open_file.backing(), OpenFileBacking::Ufd { .. }),
        "OpenFile must carry OpenFileBacking::Ufd",
    );
    let ufd = open_file.ufd().expect("ufd accessor");
    // ufd_id is monotonic from 1.
    assert!(
        ufd.ufd_id() >= 1,
        "ufd_id must be positive, got {}",
        ufd.ufd_id()
    );
    // Open-time flags are stashed (zero here).
    assert_eq!(ufd.open_flags(), 0);
    // Handshake bit starts cleared.
    assert!(!ufd.api_handshake_done());
    // No close-on-exec bit (flags == 0).
    assert!(!proc_cap.fd_cloexec(fd));
}

/// `sys_userfaultfd(O_CLOEXEC)` sets the per-process cloexec bit on
/// the returned fd.
#[test]
fn sys_userfaultfd_cloexec_sets_per_process_bit() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_userfaultfd(&ctx, O_CLOEXEC) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };
    assert!(
        proc_cap.fd_cloexec(fd),
        "O_CLOEXEC must set the per-process cloexec bit"
    );
}

/// `sys_userfaultfd(bogus_bits)` rejects with `-EINVAL` (22).
#[test]
fn sys_userfaultfd_unrecognised_flags_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // 0x40 is not in the recognised set (we only accept O_CLOEXEC).
    let result = dispatch_userfaultfd(&ctx, 0x40);
    assert_eq!(result, SyscallResult::Error(22), "EINVAL = 22");
}

/// `ioctl(ufd, UFFDIO_API, &api)` performs a no-op handshake on the
/// first call, returns 0, writes back zero feature/ioctl bitmaps,
/// and flips the ufd's handshake bit.
#[test]
fn uffdio_api_first_call_succeeds_and_marks_handshake_done() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_userfaultfd(&ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let mut api = UffdioApi {
        api: UFFD_API,
        features: 0,
        ioctls: 0xDEAD_BEEF, // any garbage; arm should overwrite
    };
    let argp = (&mut api as *mut UffdioApi) as u64;
    let result = dispatch_uffdio_api(&ctx, fd, argp);
    assert_eq!(
        result,
        SyscallResult::Return(0),
        "UFFDIO_API returns 0 on success"
    );

    // Writeback: features and ioctls cleared.
    assert_eq!(api.api, UFFD_API);
    assert_eq!(api.features, 0);
    assert_eq!(api.ioctls, 0);

    // Handshake bit flipped on the substrate side.
    let open_file = proc_cap.fd(fd).expect("fd still installed");
    let ufd = open_file.ufd().expect("ufd accessor");
    assert!(
        ufd.api_handshake_done(),
        "UFFDIO_API must set the handshake bit"
    );
}

/// A second `UFFDIO_API` against the same ufd returns `-EPERM` (1)
/// per Linux's "API already set" rule.
#[test]
fn uffdio_api_second_call_returns_eperm() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_userfaultfd(&ctx, 0) {
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
        dispatch_uffdio_api(&ctx, fd, argp),
        SyscallResult::Return(0)
    );

    // Second call: already handshaken → EPERM (1).
    let result2 = dispatch_uffdio_api(&ctx, fd, argp);
    assert_eq!(
        result2,
        SyscallResult::Error(1),
        "second UFFDIO_API → EPERM"
    );
}

/// `UFFDIO_API` with the wrong api version returns `-EINVAL`.
#[test]
fn uffdio_api_rejects_bad_api_version() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_userfaultfd(&ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let mut api = UffdioApi {
        api: 0x99, // not UFFD_API == 0xAA
        features: 0,
        ioctls: 0,
    };
    let argp = (&mut api as *mut UffdioApi) as u64;
    assert_eq!(
        dispatch_uffdio_api(&ctx, fd, argp),
        SyscallResult::Error(22),
        "bad api version → EINVAL (22)"
    );

    // Handshake bit was NOT set on the rejection path.
    let open_file = proc_cap.fd(fd).expect("fd still installed");
    let ufd = open_file.ufd().expect("ufd accessor");
    assert!(!ufd.api_handshake_done());
}

/// `UFFDIO_API` with non-zero features returns `-EINVAL` (no
/// supported feature bits today).
#[test]
fn uffdio_api_rejects_non_zero_features() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_userfaultfd(&ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    let mut api = UffdioApi {
        api: UFFD_API,
        features: 0x1, // any non-zero bit
        ioctls: 0,
    };
    let argp = (&mut api as *mut UffdioApi) as u64;
    assert_eq!(
        dispatch_uffdio_api(&ctx, fd, argp),
        SyscallResult::Error(22),
        "non-zero features → EINVAL"
    );
}

/// `ioctl(ufd, <not UFFDIO_API>, _)` against a ufd fd returns
/// `-EINVAL`. Phase 2's scaffold has no other recognised ioctls
/// yet.
#[test]
fn ufd_unknown_ioctl_returns_einval() {
    let _g = setup();
    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match dispatch_userfaultfd(&ctx, 0) {
        SyscallResult::Return(n) => n as u32,
        other => panic!("expected Return, got {other:?}"),
    };

    // 0xDEAD_BEEF is not UFFDIO_API; phase 2 has no UFFDIO_REGISTER /
    // UFFDIO_COPY yet, so any other request → EINVAL.
    let req = SyscallRequest::new(NR_IOCTL, [fd as u64, 0xDEAD_BEEF, 0, 0, 0, 0]);
    let result = block_on(dispatch::<StubPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(22));
}
