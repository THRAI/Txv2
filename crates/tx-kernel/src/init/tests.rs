//! Phase 3b boot-wiring tests.
//!
//! Exercises the new ordered steps `register_console_hardware` →
//! `mount_rootfs_tmpfs` → `mount_devfs_at_dev` →
//! `register_devfs_console_alias` → `bind_init_cwd_and_root`. The
//! tests bypass `init_substrate_if_ready` (which depends on the full
//! HAL `BootHandoff` shape) and call each method on a stub
//! `TxPlatform` directly.
//!
//! End-to-end mount-walker assertions (`step_lookup("/dev/console")`)
//! are downgraded per the trio plan §"Phase 3b tests": the VFS walker
//! / `step_open` infrastructure is deferred. The tests instead assert
//! the substrate-level facts the trio actually pins down — global
//! mount slots populate, tmpfs's `/dev` mkdir succeeds, devfs's
//! console alias resolves, and init's cwd + fds 0/1/2 are bound.

use core::sync::atomic::{AtomicUsize, Ordering};

use std::sync::Mutex;

use tx_hal::{
    AllocError, Arch, Asid, BootHandoff, BootInfo, BootPlatformIf, BootProtocol, ConsoleIf, InitIf,
    PhysAddr, PlatformConfig, PlatformInfo, PmapError, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PtNode,
};

/// Page size used to fabricate distinct test pmap roots. tx-hal
/// surfaces `PAGE_SIZE` through `Arch` rather than as a free constant;
/// hardcoding `4096` keeps this test isolated to the boot-wiring
/// surface.
const TEST_PAGE_SIZE: usize = 4096;

use crate::init::{console_tty, dev_mount, root_mount, CoreInit};

/// Serialise every test in this module: they all touch the global
/// `INIT_PROCESS` / mount / TTY slots plus the per-CPU epoch domain
/// (which forbids guard nesting on the same CPU). One lock keeps the
/// test set deterministic; same shape `tx-fs` uses.
static INIT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Test-only platform satisfying every `TxPlatform` super-trait. The
/// pmap surface uses a host-side `Mutex`-guarded `BTreeMap` mirroring
/// `tx_subsystems::vm::TestPmap`'s shape; the rest are no-ops because
/// the boot-wiring tests don't touch reactor / SMP / time paths.
struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-kernel-init-test";
}

impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for TestPlatform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

static EMPTY_BOOT_INFO: BootInfo = BootInfo::empty();
static TEST_PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: TestPlatform::BOARD,
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

impl tx_hal::BootInfoIf for TestPlatform {
    fn boot_info() -> &'static BootInfo {
        &EMPTY_BOOT_INFO
    }
}

impl tx_hal::PlatformInfoIf for TestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &TEST_PLATFORM_INFO
    }
}

impl tx_hal::AuxvIf for TestPlatform {}

/// Capture every byte the kernel writes to the platform console.
/// Lets tests assert that writes through the preopened fds 1/2 reach
/// the underlying transport — no need for a fake `CharDeviceBinding`,
/// because `register_console_hardware` already wires the
/// `ConsoleCharOps::<TestPlatform>` instance to forward into
/// `ConsoleIf::write_bytes`.
static CONSOLE_CAPTURED_LEN: AtomicUsize = AtomicUsize::new(0);

impl ConsoleIf for TestPlatform {
    fn write_bytes(bytes: &[u8]) {
        CONSOLE_CAPTURED_LEN.fetch_add(bytes.len(), Ordering::AcqRel);
    }
}

impl tx_hal::TrapIf for TestPlatform {}
impl tx_hal::UserAccessIf for TestPlatform {}
impl tx_hal::SignalFrameIf for TestPlatform {}
impl tx_hal::IrqIf for TestPlatform {}

impl tx_hal::TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl tx_hal::PercpuIf for TestPlatform {}
impl tx_hal::CacheIf for TestPlatform {}
impl tx_hal::DmaIf for TestPlatform {}
impl tx_hal::SmpIf for TestPlatform {}

impl tx_hal::PowerIf for TestPlatform {
    fn system_off() -> ! {
        loop {}
    }
}

// --- minimal pmap test impl ---------------------------------------

static TEST_PMAP_NEXT_ROOT: AtomicUsize = AtomicUsize::new(1);

impl tx_hal::PmapIf for TestPlatform {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let root_id = TEST_PMAP_NEXT_ROOT.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(root_id * TEST_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

    fn reserve_mapping(
        _root: &PmapRoot,
        virt: tx_hal::VirtAddr,
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

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        // Boot-wiring tests don't traverse intermediate page-table
        // nodes; AddressSpace creation and root-pmap allocation are
        // sufficient. A real allocator returns `Exhausted` here
        // rather than fabricating a bogus PtNode (which would risk
        // silent acceptance of unintended mapping work).
        Err(AllocError::Exhausted)
    }
}

// --- test setup ---------------------------------------------------

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = INIT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_substrate::testing::init_host_for_test_once();
    let _ = tx_subsystems::zones::register_all();
    // Reset every global slot the boot wiring touches. The
    // `cross_crate_test_support::reset_*` helpers are gated on the
    // `test-support` feature (enabled by tx-kernel's
    // `[dev-dependencies]`).
    tx_subsystems::cross_crate_test_support::reset_init_process();
    tx_subsystems::cross_crate_test_support::reset_pid_counter();
    tx_subsystems::cross_crate_test_support::reset_tid_counter();
    crate::init::reset_boot_state_for_test();
    CONSOLE_CAPTURED_LEN.store(0, Ordering::Release);
    guard
}

fn bootstrap_init() {
    // Mirror what `init_process_subsystem` does, without going
    // through the full `init_substrate_if_ready` shell (which
    // requires a real `BootHandoff` + reactor smoke flow).
    let aspace = tx_subsystems::vm::AddressSpace::new_cap_for_platform::<TestPlatform>()
        .expect("test aspace");
    let _init = tx_subsystems::process::bootstrap_init_process(aspace).expect("bootstrap init");
}

fn drive_boot_wiring() {
    bootstrap_init();
    CoreInit::<TestPlatform>::register_console_hardware();
    CoreInit::<TestPlatform>::mount_rootfs_tmpfs();
    CoreInit::<TestPlatform>::mount_devfs_at_dev();
    CoreInit::<TestPlatform>::register_devfs_console_alias();
    CoreInit::<TestPlatform>::bind_init_cwd_and_root();
}

// --- tests --------------------------------------------------------

/// **Downgrade note (per trio plan §"Phase 3b tests"):** end-to-end
/// `step_lookup("/dev/console")` cannot run yet — VFS's walker
/// (`step_open` etc.) is deferred. The downgraded shape asserts:
/// - tmpfs root mount cap is in `ROOT_MOUNT`,
/// - devfs mount cap is in `DEV_MOUNT` and points at the rootfs's
///   `/dev` DEntry as its mountpoint,
/// - the registered TTY's devfs alias resolves through the live
///   registry (`tty::project::resolve_devfs_alias(b"console")`).
#[test]
fn boot_smoke_mounts_root_and_dev_and_resolves_console() {
    let _serial = setup();
    drive_boot_wiring();

    // Root mount populates and points at a tmpfs payload over the
    // root rnode.
    let root = root_mount().expect("ROOT_MOUNT must be populated");
    assert_eq!(root.payload().fstype, "tmpfs");
    assert_eq!(
        root.root().fs_object_id(),
        tx_fs::tmpfs::TMPFS_ROOT_OBJECT_ID
    );
    // Rootfs is the namespace root: no parent, no mountpoint
    // dentry.
    assert!(root.parent().is_none());
    assert!(root.mountpoint().is_none());

    // Devfs mount populates and is parented at the rootfs.
    let dev = dev_mount().expect("DEV_MOUNT must be populated");
    assert_eq!(dev.payload().fstype, "devfs");
    assert_eq!(
        dev.root().fs_object_id(),
        tx_fs::devfs::DEVFS_ROOT_OBJECT_ID
    );
    let dev_parent = dev.parent().expect("dev mount must have rootfs parent");
    assert_eq!(dev_parent.id(), root.id());
    let dev_mountpoint = dev
        .mountpoint()
        .expect("dev mount must have mountpoint dentry");
    // Mountpoint dentry's name is `dev`.
    assert_eq!(dev_mountpoint.name().as_bytes(), b"dev");

    // The console alias resolves: `register_devfs_console_alias`
    // republished `console` against the boot console TTY.
    let resolved = tx_subsystems::tty::project::resolve_devfs_alias(b"console")
        .expect("console alias must resolve");
    let registered = console_tty().expect("CONSOLE_TTY must be populated");
    assert_eq!(resolved, registered);
}

#[test]
fn boot_smoke_init_fds_preopened_to_console() {
    let _serial = setup();
    drive_boot_wiring();

    let init =
        tx_subsystems::process::execution::init_process().expect("INIT_PROCESS must be populated");

    // Each of fds 0/1/2 is bound to a console-shaped OpenFile.
    for fd in 0..3 {
        let file = init
            .fd(fd)
            .unwrap_or_else(|| panic!("init.fd({fd}) must be Some after bind_init_cwd_and_root"));
        // Sanity: the OpenFile is read+write (per
        // `open_console_for_init`).
        assert!(file.flags().read);
        assert!(file.flags().write);
    }

    // Drive a write through fd 1 — the bytes must reach the
    // platform console (`TestPlatform::write_bytes` increments
    // `CONSOLE_CAPTURED_LEN`).
    let baseline = CONSOLE_CAPTURED_LEN.load(Ordering::Acquire);
    let stdout = init.fd(1).expect("fd 1");
    let guard = tx_substrate::epoch::guard();
    match stdout.step_write(b"hi\n", &guard) {
        tx_subsystems::execution::StepOutcome::Done(written) => {
            assert_eq!(written, 3, "step_write reports the requested byte count");
        }
        other => panic!("fd 1 step_write failed: {other:?}"),
    }
    drop(guard);

    // The TTY ldisc rewrites LF to CR LF (ONLCR) before the bytes
    // hit the platform console; we only assert that *some* bytes
    // arrived, not the exact post-cooking count, because tx-fs's
    // devfs tests already pin the ONLCR shape.
    let after = CONSOLE_CAPTURED_LEN.load(Ordering::Acquire);
    assert!(
        after > baseline,
        "TestPlatform::write_bytes must observe the fd-1 write \
         (baseline={baseline}, after={after})"
    );

    // init's cwd is now Some — `bind_init_cwd_and_root` ran
    // `step_chdir`. We can't observe the dentry directly without
    // the ChdirOutcome's `prev` (which `bind_init_cwd_and_root`
    // discards), but `step_getcwd` returns `Some` byte-vec when
    // the cwd is set.
    let cwd_bytes =
        tx_subsystems::process::execution::step_getcwd(&init).expect("init cwd must be set");
    assert_eq!(
        cwd_bytes, b"/",
        "init's rendered cwd path is the namespace root"
    );
}
