use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DeadlineTimerIf, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, MonotonicCounterIf,
    ObserverIf, PercpuIf, PersistentClockIf, PhysRange, PlatformConfig, PlatformInfo,
    PlatformInfoIf, PmapIf, PowerIf, SignalFrameIf, SmpIf, TrapIf, VirtAddr,
};
use tx_substrate::{epoch, zone};

static AP_INIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static POSSIBLE_CPUS: AtomicU64 = AtomicU64::new(0b11);
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0b1);
static RECLAIM_COUNT: AtomicUsize = AtomicUsize::new(0);

static BOOT_MEMORY: [MemoryRegion; 0] = [];
static BOOT_INFO: BootInfo = BootInfo {
    memory_regions: &BOOT_MEMORY,
    kernel_image: PhysRange {
        start: tx_hal::PhysAddr(0),
        size: 0,
    },
    initrd: None,
    cmdline: None,
};
static PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: "ap-init-test",
    spi_sd: None,
    mmio_regions: &[],
    device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: 1,
    possible_cpu_count: 2,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "ap-init-test";
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0xffff_ffc0_0000_0000);
}

impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for TestPlatform {
    fn init_early(_handoff: tx_hal::BootHandoff) {}

    fn init_later(_handoff: tx_hal::BootHandoff) {}
}

impl BootInfoIf for TestPlatform {
    fn boot_info() -> &'static BootInfo {
        &BOOT_INFO
    }
}

impl PlatformInfoIf for TestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO
    }
}

impl AuxvIf for TestPlatform {}

impl ConsoleIf for TestPlatform {
    fn write_bytes(_bytes: &[u8]) {}
}

impl PmapIf for TestPlatform {}
impl TrapIf for TestPlatform {}
impl SignalFrameIf for TestPlatform {}
unsafe fn restore_test_local_execution(_: usize) {}
impl IrqIf for TestPlatform {
    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        unsafe { tx_hal::LocalExecutionGuard::new(0, restore_test_local_execution) }
    }
}
impl EntropyIf for TestPlatform {}

impl MonotonicCounterIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }

    fn frequency_hz() -> u64 {
        PLATFORM_INFO.timebase_frequency_hz
    }
}

impl DeadlineTimerIf for TestPlatform {
    fn set_deadline_ns(_deadline: u64) {}

    fn cancel_deadline() {}
}

impl PersistentClockIf for TestPlatform {}

impl PercpuIf for TestPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(CURRENT_CPU.load(Ordering::Acquire))
    }
}

impl ObserverIf for TestPlatform {}
impl CacheIf for TestPlatform {}
impl DmaIf for TestPlatform {}

impl SmpIf for TestPlatform {
    fn possible_cpus() -> CpuMask {
        CpuMask::from_bits(POSSIBLE_CPUS.load(Ordering::Acquire))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(ONLINE_CPUS.load(Ordering::Acquire))
    }
}

impl PowerIf for TestPlatform {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}

fn reset_runtime(possible_cpus: CpuMask, current_cpu: CpuId, online_cpus: CpuMask) {
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    POSSIBLE_CPUS.store(possible_cpus.bits(), Ordering::Release);
    CURRENT_CPU.store(current_cpu.0, Ordering::Release);
    ONLINE_CPUS.store(online_cpus.bits(), Ordering::Release);
    RECLAIM_COUNT.store(0, Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("epoch bsp init");
    zone::init_on_bsp::<TestPlatform>().expect("zone bsp init");
}

unsafe fn count_reclaim(_ptr: *mut u8) {
    RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn retired_ptr() -> *mut u8 {
    NonNull::<u8>::dangling().as_ptr()
}

#[test]
fn substrate_ap_init_makes_epoch_guard_valid_on_secondary_cpu() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );

    CURRENT_CPU.store(1, Ordering::Release);
    ONLINE_CPUS.store(0b11, Ordering::Release);
    tx_substrate::init_on_ap(CpuId(1)).expect("ap substrate init");

    let guard = epoch::guard();

    assert_eq!(guard.cpu_id(), CpuId(1));
}

#[test]
fn substrate_ap_init_rejects_cpu_outside_possible_mask() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );

    let err = tx_substrate::init_on_ap(CpuId(2)).expect_err("cpu 2 is not possible");

    assert_eq!(
        err,
        tx_substrate::ApInitError::Epoch(epoch::EpochError::InvalidCpu)
    );
}

#[test]
fn sparse_nonzero_bsp_cpu_initializes_epoch_and_zone() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::single(CpuId(1)),
        CpuId(1),
        CpuMask::single(CpuId(1)),
    );

    let guard = epoch::guard();
    assert_eq!(guard.cpu_id(), CpuId(1));
    assert_eq!(epoch::summary().possible_cpus, 1);
    assert!(epoch::cpu_summary(CpuId(0)).is_none());
    assert!(epoch::cpu_summary(CpuId(1)).is_some());
}

#[test]
fn epoch_rejects_missing_bsp_without_poisoning_initialization() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
    }
    CURRENT_CPU.store(1, Ordering::Release);
    ONLINE_CPUS.store(CpuMask::single(CpuId(1)).bits(), Ordering::Release);
    POSSIBLE_CPUS.store(CpuMask::single(CpuId(4)).bits(), Ordering::Release);

    assert_eq!(
        epoch::init_on_bsp::<TestPlatform>(),
        Err(epoch::EpochError::InvalidCpu)
    );

    POSSIBLE_CPUS.store(CpuMask::single(CpuId(1)).bits(), Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("retry after rejected BSP mask");
    assert_eq!(epoch::guard().cpu_id(), CpuId(1));
}

#[test]
fn sparse_ap_guard_blocks_reclaim_until_that_cpu_quiesces() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    let sparse_mask = CpuMask::from_bits((1 << 1) | (1 << 4));
    reset_runtime(sparse_mask, CpuId(1), sparse_mask);
    tx_substrate::init_on_ap(CpuId(4)).expect("sparse AP substrate init");

    CURRENT_CPU.store(4, Ordering::Release);
    let ap_guard = epoch::guard();
    assert_eq!(ap_guard.cpu_id(), CpuId(4));

    CURRENT_CPU.store(1, Ordering::Release);
    unsafe {
        epoch::testing::retire_raw_for_test(retired_ptr(), count_reclaim).expect("retire");
    }
    assert_eq!(epoch::try_drain(usize::MAX).reclaimed, 0);
    assert_eq!(epoch::try_drain(usize::MAX).reclaimed, 0);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(ap_guard);
    let reclaimed: usize = (0..3).map(|_| epoch::try_drain(usize::MAX).reclaimed).sum();
    assert_eq!(reclaimed, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn sparse_cpu_holes_and_out_of_range_ids_are_rejected() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    let sparse_mask = CpuMask::from_bits((1 << 1) | (1 << 4));
    reset_runtime(sparse_mask, CpuId(1), sparse_mask);

    assert_eq!(
        epoch::init_on_ap(CpuId(2)),
        Err(epoch::EpochError::InvalidCpu)
    );
    assert_eq!(
        epoch::init_on_ap(CpuId(64)),
        Err(epoch::EpochError::InvalidCpu)
    );
    assert_eq!(
        zone::init_on_ap(CpuId(2)),
        Err(zone::ZoneError::InvalidState)
    );
    assert_eq!(
        zone::init_on_ap(CpuId(64)),
        Err(zone::ZoneError::InvalidState)
    );
}
