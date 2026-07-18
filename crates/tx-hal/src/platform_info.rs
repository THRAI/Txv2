//! Boot handoff and platform-description types: the firmware boot protocol,
//! the `BootInfo` / `MemoryRegion` memory map, MMIO regions, and the discovered
//! `DeviceInfo` / `PlatformInfo` the arch-neutral kernel consumes.

use crate::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootProtocol {
    RiscvSbi,
    RiscvDirect,
    LoongArchFirmware,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootHandoff {
    pub cpu_id: CpuId,
    pub firmware_arg: BootArg,
    pub protocol: BootProtocol,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootInfo {
    pub memory_regions: &'static [MemoryRegion],
    pub kernel_image: PhysRange,
    pub initrd: Option<PhysRange>,
    pub cmdline: Option<&'static str>,
}

impl BootInfo {
    pub const fn empty() -> Self {
        Self {
            memory_regions: &[],
            kernel_image: PhysRange::empty(),
            initrd: None,
            cmdline: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    pub base: PhysAddr,
    pub size: usize,
    pub kind: MemoryRegionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRegionKind {
    Usable,
    Reserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpiSdInfo {
    pub controller: &'static str,
    pub chip_select: u8,
    pub mode: u8,
    pub max_hz: u32,
    pub qemu_backing: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioFlags(pub u32);

impl MmioFlags {
    pub const DEVICE_NGNRNE: Self = Self(1 << 0);
    pub const DEVICE_NGNRE: Self = Self(1 << 1);
    pub const READ: Self = Self(1 << 2);
    pub const WRITE: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioRegion {
    pub name: &'static str,
    pub phys: PhysRange,
    pub virt: VirtRange,
    pub flags: MmioFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformInfo {
    pub board: &'static str,
    pub spi_sd: Option<SpiSdInfo>,
    pub mmio_regions: &'static [MmioRegion],
    pub timebase_frequency_hz: u64,
    pub possible_cpu_count: usize,
}

/// Kind of platform device a board publishes for generic device
/// registration. Boards derive entries from their firmware-provided
/// device tree (or static knowledge); the FDT itself never crosses
/// the HAL boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceKind {
    /// 16550-family UART (ns16550a, snps,dw-apb-uart, ...).
    Uart,
    /// Platform interrupt controller (RISC-V PLIC / LoongArch extioi).
    IntController,
    /// virtio-mmio transport slot.
    VirtioMmio,
    /// PCI host bridge ECAM window.
    PciEcam,
    /// SD/MMC host controller (DesignWare MSHC on VisionFive 2).
    SdController,
}

/// One discovered platform device, published through
/// [`PlatformInfoIf::devices`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub kind: DeviceKind,
    /// Register window (physical).
    pub mmio: PhysRange,
    /// Platform interrupt number wired to the parent interrupt
    /// controller, when the node declares one.
    pub irq: Option<u32>,
    /// 16550-style register stride from `reg-shift` (log2 bytes);
    /// 0 for byte-adjacent registers.
    pub reg_shift: u8,
    /// 16550-style register access width in bytes from
    /// `reg-io-width`; 1 for byte registers (QEMU), 4 on dw-apb.
    pub reg_io_width: u8,
}
