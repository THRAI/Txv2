use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, ObserverIf, PercpuIf, PhysRange,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapIf, PowerIf, SignalFrameIf, SmpIf, TimeIf,
    TrapIf, VirtAddr,
};
use tx_substrate::{epoch, zone};

static AP_INIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static ONLINE_CPUS: AtomicUsize = AtomicUsize::new(0b1);

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
impl IrqIf for TestPlatform {}
impl EntropyIf for TestPlatform {}

impl TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }

    fn set_deadline_ns(_deadline: u64) {}

    fn cancel_deadline() {}

    fn frequency_hz() -> u64 {
        PLATFORM_INFO.timebase_frequency_hz
    }
}

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
        CpuMask::first(PLATFORM_INFO.possible_cpu_count)
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(ONLINE_CPUS.load(Ordering::Acquire) as u64)
    }
}

impl PowerIf for TestPlatform {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}

fn reset_runtime() {
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    CURRENT_CPU.store(0, Ordering::Release);
    ONLINE_CPUS.store(0b1, Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("epoch bsp init");
    zone::init_on_bsp::<TestPlatform>().expect("zone bsp init");
}

#[test]
fn substrate_ap_init_makes_epoch_guard_valid_on_secondary_cpu() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime();

    CURRENT_CPU.store(1, Ordering::Release);
    ONLINE_CPUS.store(0b11, Ordering::Release);
    tx_substrate::init_on_ap(CpuId(1)).expect("ap substrate init");

    let guard = epoch::guard();

    assert_eq!(guard.cpu_id(), CpuId(1));
}

#[test]
fn substrate_ap_init_rejects_cpu_outside_possible_mask() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime();

    let err = tx_substrate::init_on_ap(CpuId(2)).expect_err("cpu 2 is not possible");

    assert_eq!(
        err,
        tx_substrate::ApInitError::Epoch(epoch::EpochError::InvalidCpu)
    );
}
