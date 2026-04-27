#![no_std]

use tx_hal::{
    Arch, AuxvIf, BootHandoff, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf,
    ConsoleIf, DmaIf, InitIf, IrqIf, PercpuIf, PlatformConfig, PlatformInfo, PlatformInfoIf,
    PmapIf, PowerIf, SignalFrameIf, SmpIf, SpiSdInfo, TimeIf, TrapIf, UserAccessIf,
};

pub struct Platform;

pub const SPI0_CS0_SD: SpiSdInfo = SpiSdInfo {
    controller: "spi0",
    chip_select: 0,
    mode: 0,
    max_hz: 25_000_000,
    qemu_backing: "target/images/m1dock-sd.img",
};

static BOOT_INFO: BootInfo = BootInfo::empty();

static PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: Platform::BOARD,
    spi_sd: Some(SPI0_CS0_SD),
};

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "sipeed-m1-dock-mock";
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for Platform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

impl BootInfoIf for Platform {
    fn boot_info() -> &'static BootInfo {
        &BOOT_INFO
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO
    }
}

impl AuxvIf for Platform {}
impl ConsoleIf for Platform {
    fn write_bytes(_bytes: &[u8]) {}
}
impl PmapIf for Platform {}
impl TrapIf for Platform {}
impl UserAccessIf for Platform {}
impl SignalFrameIf for Platform {}
impl IrqIf for Platform {}
impl TimeIf for Platform {}
impl PercpuIf for Platform {}
impl CacheIf for Platform {}
impl DmaIf for Platform {}
impl SmpIf for Platform {}

impl PowerIf for Platform {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}
