use core::cell::UnsafeCell;
use core::marker::PhantomData;

use tx_hal::{
    BootInfo, BootstrapPmapInfo, DeviceId, DeviceInfo, DeviceKind, DeviceLocalId, DeviceMatchId,
    DeviceResource, DeviceResourceGraph, DeviceStatus, DmaCoherency, DmaConstraints, DmaDomain,
    DmaDomainId, DmaDomainRef, DmaTranslation, IrqPolarity, IrqResource, IrqSharing, IrqTrigger,
    MemoryRegion, MemoryRegionKind, MmioFlags, MmioRegion, MmioResource, PhysAddr, PhysRange,
    PlatformConfig, PlatformDevice, PlatformInfo, ResourceOrigin, ResourceOriginKind,
    ResourceProviderId, ResourceRole, VirtAddr, VirtRange,
};

use crate::pmap::topology::{
    DIRECT_MAP_BASE, KERNEL_ALIAS_L0_TABLES, KERNEL_BOOTSTRAP_ALIAS_SIZE, KERNEL_VIRT_BASE,
    PAGE_SIZE, PT_NODE_POOL_ENTRIES, QEMU_KERNEL_PHYS_BASE,
};
use crate::time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ;
use crate::Platform;

// ===== 容量上限:no_std 早期无堆,所有缓冲区都是定长静态数组,先把上限定死 =====
// 可用内存节点 + /reserved-memory 子节点 + memreserve 项 + 固件加载保留区,共用此数组
pub(crate) const MAX_MEMORY_REGIONS: usize = 16;
pub(crate) const CMDLINE_CAPACITY: usize = 16384;
pub(crate) const BOOTSTRAP_PMAP_RESERVED_RANGES: usize = 4;
// QEMU virt 有 12 个可识别节点(8×virtio-mmio + uart + plic + rtc + pci ecam);VF2 更少,留余量
pub(crate) const MAX_PLATFORM_DEVICES: usize = 24;
// 每个设备一条 MmioRegion,外加固定的 clint 项(clint 无 DeviceKind 但其 MMIO 必须保持映射)
const GENERATED_MMIO_REGIONS: usize = MAX_PLATFORM_DEVICES + 1;
const MAX_TYPED_PLATFORM_MMIO: usize = MAX_PLATFORM_DEVICES;
const MAX_TYPED_DEVICE_MATCHES: usize = MAX_PLATFORM_DEVICES * 4;
const MAX_TYPED_DEVICE_RESOURCES: usize = MAX_PLATFORM_DEVICES * 3;
const DEVICE_RESOURCE_STRING_CAPACITY: usize = MAX_PLATFORM_DEVICES * 256;

// 生成的 MMIO 区名字表,按各类型在设备树里出现的顺序取用
const VIRTIO_REGION_NAMES: [&str; 12] = [
    "virtio0", "virtio1", "virtio2", "virtio3", "virtio4", "virtio5", "virtio6", "virtio7",
    "virtio8", "virtio9", "virtio10", "virtio11",
];
const UART_REGION_NAMES: [&str; 6] = ["uart0", "uart1", "uart2", "uart3", "uart4", "uart5"];
const SDIO_REGION_NAMES: [&str; 4] = ["sdio0", "sdio1", "sdio2", "sdio3"];
const DWMAC_REGION_NAMES: [&str; 4] = ["dwmac0", "dwmac1", "dwmac2", "dwmac3"];
const CLOCK_REGION_NAMES: [&str; 5] = ["clock0", "clock1", "clock2", "clock3", "clock4"];
const CACHE_REGION_NAMES: [&str; 2] = ["cache0", "cache1"];

const EMPTY_DEVICE: DeviceInfo = DeviceInfo {
    kind: DeviceKind::Uart,
    mmio: PhysRange::empty(),
    irq: None,
    reg_shift: 0,
    reg_io_width: 1,
};

const FDT_RESOURCE_PROVIDER: ResourceProviderId = ResourceProviderId("riscv-fdt");
const DEFAULT_DMA_DOMAIN_ID: DmaDomainId = DmaDomainId {
    provider: FDT_RESOURCE_PROVIDER,
    local: 0,
};
const DEFAULT_DMA_ORIGIN: ResourceOrigin = ResourceOrigin {
    provider: FDT_RESOURCE_PROVIDER,
    record: "platform-default-dma",
    kind: ResourceOriginKind::PlatformStatic,
};
static DEFAULT_DMA_DOMAINS: [DmaDomain; 1] = [DmaDomain {
    id: DEFAULT_DMA_DOMAIN_ID,
    translation: DmaTranslation::Direct { offset: 0 },
    constraints: DmaConstraints {
        dma_address_bits: usize::BITS as u8,
        min_alignment: 1,
        segment_boundary: None,
        max_segment_len: usize::MAX,
        max_segments: u16::MAX,
    },
    coherency: if <Platform as PlatformConfig>::DMA_COHERENT {
        DmaCoherency::Coherent
    } else {
        DmaCoherency::NonCoherent
    },
    origin: DEFAULT_DMA_ORIGIN,
}];
const EMPTY_RESOURCE_ORIGIN: ResourceOrigin = ResourceOrigin {
    provider: FDT_RESOURCE_PROVIDER,
    record: "",
    kind: ResourceOriginKind::Firmware,
};
const EMPTY_MMIO_RESOURCE: MmioResource = MmioResource {
    role: ResourceRole::Index(0),
    phys: PhysRange::empty(),
    virt: VirtRange::empty(),
    flags: MmioFlags::empty(),
    origin: EMPTY_RESOURCE_ORIGIN,
};
const EMPTY_DEVICE_MATCH: DeviceMatchId = DeviceMatchId::FirmwareCompatible("");
const EMPTY_DEVICE_RESOURCE: DeviceResource = DeviceResource::Mmio(EMPTY_MMIO_RESOURCE);
const EMPTY_PLATFORM_DEVICE: PlatformDevice = PlatformDevice {
    id: DeviceId {
        provider: FDT_RESOURCE_PROVIDER,
        local: DeviceLocalId::FirmwarePath(""),
    },
    status: DeviceStatus::Enabled,
    matches: &[],
    resources: &[],
    origin: EMPTY_RESOURCE_ORIGIN,
};

pub(crate) struct IdentityLive;
pub(crate) struct IdentityDropped;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FirmwareDtb {
    phys: PhysAddr,
}

impl FirmwareDtb {
    pub(crate) const fn from_addr(addr: usize) -> Self {
        Self {
            phys: PhysAddr(addr),
        }
    }

    pub(crate) const fn addr(self) -> usize {
        self.phys.0
    }

    /// Address used while parsing the firmware-owned blob.
    ///
    /// The SBI register carries a physical address. Real RV64 execution has
    /// already left the low-linked image by the time Rust parses it, so use
    /// the board's named direct-map conversion. Host tests keep passing real
    /// host pointers and therefore intentionally use the raw value.
    pub(crate) const fn parse_addr(self) -> usize {
        #[cfg(target_arch = "riscv64")]
        {
            DIRECT_MAP_BASE + self.phys.0
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            self.phys.0
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootLinkedAddr(usize);

impl BootLinkedAddr {
    #[cfg(any(test, not(target_arch = "riscv64")))]
    pub(crate) const fn from_linked(addr: usize) -> Self {
        Self(addr)
    }

    pub(crate) const fn from_runtime_addr(addr: usize) -> Self {
        let Some(offset) = addr.checked_sub(KERNEL_VIRT_BASE) else {
            return Self(addr);
        };
        if offset >= KERNEL_BOOTSTRAP_ALIAS_SIZE {
            return Self(addr);
        }
        Self(QEMU_KERNEL_PHYS_BASE + offset)
    }

    pub(crate) const fn raw(self) -> usize {
        self.0
    }

    pub(crate) const fn phys(self) -> PhysAddr {
        PhysAddr(self.0)
    }

    #[cfg(test)]
    pub(crate) const fn identity_va(self) -> VirtAddr {
        VirtAddr(self.0)
    }

    #[cfg(test)]
    pub(crate) const fn direct_va(self) -> VirtAddr {
        VirtAddr(DIRECT_MAP_BASE + self.0)
    }

    pub(crate) const fn kernel_alias_va(self) -> Option<VirtAddr> {
        let Some(offset) = self.0.checked_sub(QEMU_KERNEL_PHYS_BASE) else {
            return None;
        };
        if offset >= KERNEL_BOOTSTRAP_ALIAS_SIZE {
            return None;
        }
        Some(VirtAddr(KERNEL_VIRT_BASE + offset))
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HighBootTransition {
    pub(crate) stack_top: VirtAddr,
    pub(crate) global_pointer: VirtAddr,
    pub(crate) rust_entry: VirtAddr,
}

#[cfg(test)]
impl HighBootTransition {
    pub(crate) fn from_linked(
        stack_top: BootLinkedAddr,
        global_pointer: BootLinkedAddr,
        rust_entry: BootLinkedAddr,
    ) -> Option<Self> {
        Some(Self {
            stack_top: stack_top.kernel_alias_va()?,
            global_pointer: global_pointer.kernel_alias_va()?,
            rust_entry: rust_entry.kernel_alias_va()?,
        })
    }
}

#[derive(Clone, Copy)]
#[repr(C, align(4096))]
pub(crate) struct PageTable(pub(crate) [u64; 512]);

// ===== 全局存储三件套 =====
// no_std 可变全局:每种缓冲区都要 ①Cell 包装(UnsafeCell 才能写)②unsafe impl Sync
// (手动担保可共享,依据是启动单线程)③static 声明(变量真正住的地方)。以下重复约 18 遍。
struct BootInfoCell(UnsafeCell<BootInfo>);
struct BootstrapPmapInfoCell(UnsafeCell<Option<BootstrapPmapInfo>>);
struct CmdlineCell(UnsafeCell<[u8; CMDLINE_CAPACITY]>);
struct MemoryRegionsCell(UnsafeCell<[MemoryRegion; MAX_MEMORY_REGIONS]>);
struct PlatformInfoCell(UnsafeCell<PlatformInfo>);
struct PlatformMmioRegionsCell(UnsafeCell<[MmioRegion; GENERATED_MMIO_REGIONS]>);
struct TypedPlatformMmioCell(UnsafeCell<[MmioResource; MAX_TYPED_PLATFORM_MMIO]>);
struct TypedPlatformDevicesCell(UnsafeCell<[PlatformDevice; MAX_PLATFORM_DEVICES]>);
struct TypedDeviceMatchesCell(UnsafeCell<[DeviceMatchId; MAX_TYPED_DEVICE_MATCHES]>);
struct TypedDeviceResourcesCell(UnsafeCell<[DeviceResource; MAX_TYPED_DEVICE_RESOURCES]>);
struct DeviceResourceStringsCell(UnsafeCell<[u8; DEVICE_RESOURCE_STRING_CAPACITY]>);
struct DeviceResourceGraphCell(UnsafeCell<DeviceResourceGraph>);
struct TimebaseFrequencyCell(UnsafeCell<u64>);
struct PlatformDevicesCell(UnsafeCell<[DeviceInfo; MAX_PLATFORM_DEVICES]>);
struct PlatformDeviceCountCell(UnsafeCell<usize>);
struct PlicScontextsCell(UnsafeCell<[Option<u32>; crate::dtb::MAX_PLIC_HARTS]>);
struct PlicPhysBaseCell(UnsafeCell<usize>);
struct StartableHartsCell(UnsafeCell<u64>);
struct PossibleCpuCountCell(UnsafeCell<usize>);
struct ReservedPageTablesCell(UnsafeCell<[PhysRange; BOOTSTRAP_PMAP_RESERVED_RANGES]>);
struct PageTableCell(UnsafeCell<PageTable>);
struct KernelAliasL0TablesCell(UnsafeCell<[PageTable; KERNEL_ALIAS_L0_TABLES]>);
struct PtNodePoolCell(UnsafeCell<[PageTable; PT_NODE_POOL_ENTRIES]>);
struct StoredBootStaticBagCell(UnsafeCell<StoredBootStaticBag>);

unsafe impl Sync for BootInfoCell {}
unsafe impl Sync for BootstrapPmapInfoCell {}
unsafe impl Sync for CmdlineCell {}
unsafe impl Sync for MemoryRegionsCell {}
unsafe impl Sync for PlatformInfoCell {}
unsafe impl Sync for PlatformMmioRegionsCell {}
unsafe impl Sync for TypedPlatformMmioCell {}
unsafe impl Sync for TypedPlatformDevicesCell {}
unsafe impl Sync for TypedDeviceMatchesCell {}
unsafe impl Sync for TypedDeviceResourcesCell {}
unsafe impl Sync for DeviceResourceStringsCell {}
unsafe impl Sync for DeviceResourceGraphCell {}
unsafe impl Sync for TimebaseFrequencyCell {}
unsafe impl Sync for PlatformDevicesCell {}
unsafe impl Sync for PlatformDeviceCountCell {}
unsafe impl Sync for PlicScontextsCell {}
unsafe impl Sync for PlicPhysBaseCell {}
unsafe impl Sync for StartableHartsCell {}
unsafe impl Sync for PossibleCpuCountCell {}
unsafe impl Sync for ReservedPageTablesCell {}
unsafe impl Sync for PageTableCell {}
unsafe impl Sync for KernelAliasL0TablesCell {}
unsafe impl Sync for PtNodePoolCell {}
unsafe impl Sync for StoredBootStaticBagCell {}

static BOOT_INFO: BootInfoCell = BootInfoCell(UnsafeCell::new(BootInfo::empty()));
static BOOTSTRAP_PMAP_INFO: BootstrapPmapInfoCell = BootstrapPmapInfoCell(UnsafeCell::new(None));
static CMDLINE: CmdlineCell = CmdlineCell(UnsafeCell::new([0; CMDLINE_CAPACITY]));
static MEMORY_REGIONS: MemoryRegionsCell =
    MemoryRegionsCell(UnsafeCell::new([reserved_region(); MAX_MEMORY_REGIONS]));
static PLATFORM_INFO: PlatformInfoCell = PlatformInfoCell(UnsafeCell::new(PlatformInfo {
    board: "",
    spi_sd: None,
    mmio_regions: &[],
    device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: QEMU_VIRT_FALLBACK_TIMEBASE_HZ,
    possible_cpu_count: 1,
}));
static PLATFORM_MMIO_REGIONS: PlatformMmioRegionsCell = PlatformMmioRegionsCell(UnsafeCell::new(
    [empty_mmio_region(); GENERATED_MMIO_REGIONS],
));
static TYPED_PLATFORM_MMIO: TypedPlatformMmioCell = TypedPlatformMmioCell(UnsafeCell::new(
    [EMPTY_MMIO_RESOURCE; MAX_TYPED_PLATFORM_MMIO],
));
static TYPED_PLATFORM_DEVICES: TypedPlatformDevicesCell = TypedPlatformDevicesCell(
    UnsafeCell::new([EMPTY_PLATFORM_DEVICE; MAX_PLATFORM_DEVICES]),
);
static TYPED_DEVICE_MATCHES: TypedDeviceMatchesCell = TypedDeviceMatchesCell(UnsafeCell::new(
    [EMPTY_DEVICE_MATCH; MAX_TYPED_DEVICE_MATCHES],
));
static TYPED_DEVICE_RESOURCES: TypedDeviceResourcesCell = TypedDeviceResourcesCell(
    UnsafeCell::new([EMPTY_DEVICE_RESOURCE; MAX_TYPED_DEVICE_RESOURCES]),
);
static DEVICE_RESOURCE_STRINGS: DeviceResourceStringsCell =
    DeviceResourceStringsCell(UnsafeCell::new([0; DEVICE_RESOURCE_STRING_CAPACITY]));
static DEVICE_RESOURCE_GRAPH: DeviceResourceGraphCell =
    DeviceResourceGraphCell(UnsafeCell::new(DeviceResourceGraph {
        platform_mmio: &[],
        devices: &[],
        dma_domains: &[],
    }));
static TIMEBASE_FREQUENCY_HZ: TimebaseFrequencyCell =
    TimebaseFrequencyCell(UnsafeCell::new(QEMU_VIRT_FALLBACK_TIMEBASE_HZ));
static PLATFORM_DEVICES: PlatformDevicesCell =
    PlatformDevicesCell(UnsafeCell::new([EMPTY_DEVICE; MAX_PLATFORM_DEVICES]));
static PLATFORM_DEVICE_COUNT: PlatformDeviceCountCell = PlatformDeviceCountCell(UnsafeCell::new(0));
static PLIC_SCONTEXTS: PlicScontextsCell =
    PlicScontextsCell(UnsafeCell::new([None; crate::dtb::MAX_PLIC_HARTS]));
static PLIC_PHYS_BASE_PUBLISHED: PlicPhysBaseCell =
    PlicPhysBaseCell(UnsafeCell::new(crate::PLIC_PHYS_BASE));
static STARTABLE_HARTS: StartableHartsCell = StartableHartsCell(UnsafeCell::new(0));

/// 从 DTB 导出的"可启动 hart"位图(0 = 无数据,调用方回退到 possible_cpu_count 前缀掩码)
pub(crate) fn startable_harts() -> u64 {
    unsafe { *STARTABLE_HARTS.0.get() }
}

/// hart 对应的 PLIC S 模式 context(启动时从设备树导出);None 让调用方回退到 QEMU 公式
pub(crate) fn plic_scontext_for_hart(hart: usize) -> Option<u32> {
    unsafe {
        let table: &[Option<u32>; crate::dtb::MAX_PLIC_HARTS] = &*PLIC_SCONTEXTS.0.get();
        table.get(hart).copied().flatten()
    }
}

/// PLIC 寄存器窗口物理基址:启动发布后用设备树值,发布前(及兜底)用 QEMU-virt 常量
#[cfg_attr(not(target_arch = "riscv64"), allow(dead_code))]
pub(crate) fn plic_phys_base() -> usize {
    unsafe { *PLIC_PHYS_BASE_PUBLISHED.0.get() }
}
static POSSIBLE_CPU_COUNT: PossibleCpuCountCell = PossibleCpuCountCell(UnsafeCell::new(1));
static RESERVED_PAGE_TABLES: ReservedPageTablesCell = ReservedPageTablesCell(UnsafeCell::new(
    [PhysRange::empty(); BOOTSTRAP_PMAP_RESERVED_RANGES],
));
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.bootstrap_root"
)]
static BOOTSTRAP_ROOT: PageTableCell = PageTableCell(UnsafeCell::new(PageTable([0; 512])));
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.kernel_alias_l1"
)]
static KERNEL_ALIAS_L1: PageTableCell = PageTableCell(UnsafeCell::new(PageTable([0; 512])));
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.kernel_alias_l0_tables"
)]
static KERNEL_ALIAS_L0_TABLES_STORAGE: KernelAliasL0TablesCell = KernelAliasL0TablesCell(
    UnsafeCell::new([PageTable([0; 512]); KERNEL_ALIAS_L0_TABLES]),
);
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.pt_node_pool"
)]
static PT_NODE_POOL: PtNodePoolCell =
    PtNodePoolCell(UnsafeCell::new([PageTable([0; 512]); PT_NODE_POOL_ENTRIES]));
static STORED_BOOT_STATIC_BAG: StoredBootStaticBagCell =
    StoredBootStaticBagCell(UnsafeCell::new(StoredBootStaticBag::Uninit));

#[cfg(target_arch = "riscv64")]
unsafe extern "C" {
    fn tx_rv64_qemu_secondary_start();
}

#[cfg(target_arch = "riscv64")]
#[used]
static SECONDARY_START_ENTRY: unsafe extern "C" fn() = tx_rv64_qemu_secondary_start;

struct TypedSeedWriter {
    platform_mmio_count: usize,
    device_count: usize,
    match_count: usize,
    resource_count: usize,
    string_cursor: usize,
}

impl TypedSeedWriter {
    unsafe fn new() -> Self {
        unsafe {
            (*TYPED_PLATFORM_MMIO.0.get()).fill(EMPTY_MMIO_RESOURCE);
            (*TYPED_PLATFORM_DEVICES.0.get()).fill(EMPTY_PLATFORM_DEVICE);
            (*TYPED_DEVICE_MATCHES.0.get()).fill(EMPTY_DEVICE_MATCH);
            (*TYPED_DEVICE_RESOURCES.0.get()).fill(EMPTY_DEVICE_RESOURCE);
            (*DEVICE_RESOURCE_STRINGS.0.get()).fill(0);
        }
        Self {
            platform_mmio_count: 0,
            device_count: 0,
            match_count: 0,
            resource_count: 0,
            string_cursor: 0,
        }
    }

    fn push(
        &mut self,
        fact: crate::dtb::DtbDeviceFact<'_>,
    ) -> Result<(), crate::dtb::DtbDeviceError> {
        let path = self.copy_path(fact.node_name, fact.unit_address)?;
        let origin = ResourceOrigin {
            provider: FDT_RESOURCE_PROVIDER,
            record: path,
            kind: ResourceOriginKind::Firmware,
        };
        let mmio = mapped_mmio_resource(fact.mmio, origin);

        match fact.class {
            crate::dtb::DtbDeviceClass::PlatformMmio(_) => self.push_platform_mmio(mmio),
            crate::dtb::DtbDeviceClass::PlatformDevice(_) => {
                self.push_platform_device(fact, path, origin, mmio)
            }
        }
    }

    fn push_platform_mmio(&mut self, mmio: MmioResource) -> Result<(), crate::dtb::DtbDeviceError> {
        if self.platform_mmio_count == MAX_TYPED_PLATFORM_MMIO {
            return Err(crate::dtb::DtbDeviceError::PlatformMmioCapacityExceeded {
                capacity: MAX_TYPED_PLATFORM_MMIO,
                required: self.platform_mmio_count + 1,
            });
        }
        unsafe {
            (*TYPED_PLATFORM_MMIO.0.get())[self.platform_mmio_count] = mmio;
        }
        self.platform_mmio_count += 1;
        Ok(())
    }

    fn push_platform_device(
        &mut self,
        fact: crate::dtb::DtbDeviceFact<'_>,
        path: &'static str,
        origin: ResourceOrigin,
        mmio: MmioResource,
    ) -> Result<(), crate::dtb::DtbDeviceError> {
        if self.device_count == MAX_PLATFORM_DEVICES {
            return Err(crate::dtb::DtbDeviceError::PlatformDeviceCapacityExceeded {
                capacity: MAX_PLATFORM_DEVICES,
                required: self.device_count + 1,
            });
        }

        let match_start = self.match_count;
        for compatible in fact
            .compatible
            .split(|&byte| byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let compatible = core::str::from_utf8(compatible)
                .map_err(|_| crate::dtb::DtbDeviceError::MalformedCompatible)?;
            let compatible = self.copy_string_parts(&[compatible])?;
            self.push_match(DeviceMatchId::FirmwareCompatible(compatible))?;
        }
        if self.match_count == match_start {
            return Err(crate::dtb::DtbDeviceError::MalformedCompatible);
        }

        let resource_start = self.resource_count;
        self.push_resource(DeviceResource::Mmio(mmio))?;
        if let Some(line) = fact.irq {
            if line == 0 {
                return Err(crate::dtb::DtbDeviceError::InvalidIrq(line));
            }
            self.push_resource(DeviceResource::Irq(IrqResource {
                role: ResourceRole::Index(0),
                line,
                // The RISC-V PLIC binding encodes only a source number. The
                // platform route supplies its level/high, exclusive defaults.
                trigger: IrqTrigger::Level,
                polarity: IrqPolarity::High,
                sharing: IrqSharing::Exclusive,
                origin,
            }))?;
        }
        if matches!(
            fact.class,
            crate::dtb::DtbDeviceClass::PlatformDevice(DeviceKind::VirtioMmio | DeviceKind::Dwmac)
        ) {
            self.push_resource(DeviceResource::DmaDomain(DmaDomainRef {
                role: ResourceRole::Index(0),
                domain: DEFAULT_DMA_DOMAIN_ID,
            }))?;
        }

        let matches: &'static [DeviceMatchId] = unsafe {
            let all: &'static [DeviceMatchId; MAX_TYPED_DEVICE_MATCHES] =
                &*TYPED_DEVICE_MATCHES.0.get();
            &all[match_start..self.match_count]
        };
        let resources: &'static [DeviceResource] = unsafe {
            let all: &'static [DeviceResource; MAX_TYPED_DEVICE_RESOURCES] =
                &*TYPED_DEVICE_RESOURCES.0.get();
            &all[resource_start..self.resource_count]
        };
        unsafe {
            (*TYPED_PLATFORM_DEVICES.0.get())[self.device_count] = PlatformDevice {
                id: DeviceId {
                    provider: FDT_RESOURCE_PROVIDER,
                    local: DeviceLocalId::FirmwarePath(path),
                },
                status: DeviceStatus::Enabled,
                matches,
                resources,
                origin,
            };
        }
        self.device_count += 1;
        Ok(())
    }

    fn push_match(&mut self, value: DeviceMatchId) -> Result<(), crate::dtb::DtbDeviceError> {
        if self.match_count == MAX_TYPED_DEVICE_MATCHES {
            return Err(crate::dtb::DtbDeviceError::DeviceMatchCapacityExceeded {
                capacity: MAX_TYPED_DEVICE_MATCHES,
                required: self.match_count + 1,
            });
        }
        unsafe {
            (*TYPED_DEVICE_MATCHES.0.get())[self.match_count] = value;
        }
        self.match_count += 1;
        Ok(())
    }

    fn push_resource(&mut self, value: DeviceResource) -> Result<(), crate::dtb::DtbDeviceError> {
        if self.resource_count == MAX_TYPED_DEVICE_RESOURCES {
            return Err(crate::dtb::DtbDeviceError::DeviceResourceCapacityExceeded {
                capacity: MAX_TYPED_DEVICE_RESOURCES,
                required: self.resource_count + 1,
            });
        }
        unsafe {
            (*TYPED_DEVICE_RESOURCES.0.get())[self.resource_count] = value;
        }
        self.resource_count += 1;
        Ok(())
    }

    fn copy_path(
        &mut self,
        node_name: &str,
        unit_address: Option<&str>,
    ) -> Result<&'static str, crate::dtb::DtbDeviceError> {
        match unit_address {
            Some(unit_address) => self.copy_string_parts(&["/soc/", node_name, "@", unit_address]),
            None => self.copy_string_parts(&["/soc/", node_name]),
        }
    }

    fn copy_string_parts(
        &mut self,
        parts: &[&str],
    ) -> Result<&'static str, crate::dtb::DtbDeviceError> {
        let byte_len = parts
            .iter()
            .try_fold(0usize, |len, part| len.checked_add(part.len()))
            .ok_or(crate::dtb::DtbDeviceError::StringArenaCapacityExceeded {
                capacity: DEVICE_RESOURCE_STRING_CAPACITY,
                required: usize::MAX,
            })?;
        let end = self.string_cursor.checked_add(byte_len).ok_or(
            crate::dtb::DtbDeviceError::StringArenaCapacityExceeded {
                capacity: DEVICE_RESOURCE_STRING_CAPACITY,
                required: usize::MAX,
            },
        )?;
        if end > DEVICE_RESOURCE_STRING_CAPACITY {
            return Err(crate::dtb::DtbDeviceError::StringArenaCapacityExceeded {
                capacity: DEVICE_RESOURCE_STRING_CAPACITY,
                required: end,
            });
        }

        let start = self.string_cursor;
        let mut cursor = start;
        unsafe {
            let storage = core::ptr::addr_of_mut!((*DEVICE_RESOURCE_STRINGS.0.get())[0]);
            for part in parts {
                core::ptr::copy_nonoverlapping(part.as_ptr(), storage.add(cursor), part.len());
                cursor += part.len();
            }
            self.string_cursor = end;
            let bytes: &'static [u8] = core::slice::from_raw_parts(storage.add(start), byte_len);
            Ok(core::str::from_utf8_unchecked(bytes))
        }
    }

    unsafe fn finish(self) -> &'static DeviceResourceGraph {
        let platform_mmio: &'static [MmioResource] = unsafe {
            let all: &'static [MmioResource; MAX_TYPED_PLATFORM_MMIO] =
                &*TYPED_PLATFORM_MMIO.0.get();
            &all[..self.platform_mmio_count]
        };
        let devices: &'static [PlatformDevice] = unsafe {
            let all: &'static [PlatformDevice; MAX_PLATFORM_DEVICES] =
                &*TYPED_PLATFORM_DEVICES.0.get();
            &all[..self.device_count]
        };
        unsafe {
            *DEVICE_RESOURCE_GRAPH.0.get() = DeviceResourceGraph {
                platform_mmio,
                devices,
                dma_domains: &DEFAULT_DMA_DOMAINS,
            };
            &*DEVICE_RESOURCE_GRAPH.0.get()
        }
    }
}

fn mapped_mmio_resource(phys: PhysRange, origin: ResourceOrigin) -> MmioResource {
    MmioResource {
        role: ResourceRole::Index(0),
        phys,
        virt: VirtRange {
            start: VirtAddr(DIRECT_MAP_BASE + phys.start.0),
            size: phys.size,
        },
        flags: MMIO_RW_DEVICE,
        origin,
    }
}

// ===== MMIO 区构建:把解析出的设备列表翻译成"寄存器窗口 → 虚拟地址"映射表 =====
const MMIO_RW_DEVICE: MmioFlags = MmioFlags::DEVICE_NGNRNE
    .union(MmioFlags::READ)
    .union(MmioFlags::WRITE);

const fn empty_mmio_region() -> MmioRegion {
    MmioRegion {
        name: "",
        phys: PhysRange::empty(),
        virt: VirtRange::empty(),
        flags: MmioFlags::empty(),
    }
}

/// Build the published MMIO region list. With a device table (parsed
/// from the firmware DTB) every discovered device gets a mapped,
/// named region — all virtio slots included, so slot-probing device
/// registration works regardless of QEMU's device-to-slot ordering.
/// Without one (parse failed), fall back to the legacy static QEMU
/// list. The clint keeps a static entry either way: it has no
/// DeviceKind, but its MMIO must stay mapped.
fn build_mmio_regions(
    devices: &[DeviceInfo],
    out: &mut [MmioRegion; GENERATED_MMIO_REGIONS],
) -> Result<usize, crate::dtb::DtbDeviceError> {
    *out = [empty_mmio_region(); GENERATED_MMIO_REGIONS];
    if devices.is_empty() {
        let legacy = qemu_mmio_regions();
        if legacy.len() > out.len() {
            return Err(crate::dtb::DtbDeviceError::LegacyMmioCapacityExceeded {
                capacity: out.len(),
                required: legacy.len(),
            });
        }
        out[..legacy.len()].copy_from_slice(&legacy);
        return Ok(legacy.len());
    }

    out[0] = clint_mmio_region();
    let mut count = 1usize;
    let mut virtio_index = 0usize;
    let mut uart_index = 0usize;
    let mut sdio_index = 0usize;
    let mut dwmac_index = 0usize;
    let mut clock_index = 0usize;
    let mut cache_index = 0usize;
    for device in devices {
        if count == out.len() {
            return Err(crate::dtb::DtbDeviceError::LegacyMmioCapacityExceeded {
                capacity: out.len(),
                required: count + 1,
            });
        }
        let name = match device.kind {
            DeviceKind::VirtioMmio => {
                let name = VIRTIO_REGION_NAMES.get(virtio_index).ok_or(
                    crate::dtb::DtbDeviceError::LegacyMmioNameCapacityExceeded {
                        kind: device.kind,
                        required: virtio_index + 1,
                    },
                )?;
                virtio_index += 1;
                Some(name)
            }
            DeviceKind::Uart => {
                let name = UART_REGION_NAMES.get(uart_index).ok_or(
                    crate::dtb::DtbDeviceError::LegacyMmioNameCapacityExceeded {
                        kind: device.kind,
                        required: uart_index + 1,
                    },
                )?;
                uart_index += 1;
                Some(name)
            }
            DeviceKind::SdController => {
                let name = SDIO_REGION_NAMES.get(sdio_index).ok_or(
                    crate::dtb::DtbDeviceError::LegacyMmioNameCapacityExceeded {
                        kind: device.kind,
                        required: sdio_index + 1,
                    },
                )?;
                sdio_index += 1;
                Some(name)
            }
            DeviceKind::Dwmac => {
                let name = DWMAC_REGION_NAMES.get(dwmac_index).ok_or(
                    crate::dtb::DtbDeviceError::LegacyMmioNameCapacityExceeded {
                        kind: device.kind,
                        required: dwmac_index + 1,
                    },
                )?;
                dwmac_index += 1;
                Some(name)
            }
            DeviceKind::ClockController => {
                let name = CLOCK_REGION_NAMES.get(clock_index).ok_or(
                    crate::dtb::DtbDeviceError::LegacyMmioNameCapacityExceeded {
                        kind: device.kind,
                        required: clock_index + 1,
                    },
                )?;
                clock_index += 1;
                Some(name)
            }
            DeviceKind::CacheController => {
                let name = CACHE_REGION_NAMES.get(cache_index).ok_or(
                    crate::dtb::DtbDeviceError::LegacyMmioNameCapacityExceeded {
                        kind: device.kind,
                        required: cache_index + 1,
                    },
                )?;
                cache_index += 1;
                Some(name)
            }
            DeviceKind::IntController => Some(&"plic"),
            DeviceKind::GoldfishRtc => Some(&"goldfish-rtc"),
            DeviceKind::PciEcam => Some(&"pcie-ecam"),
        };
        let Some(&name) = name else {
            continue;
        };
        out[count] = MmioRegion {
            name,
            phys: device.mmio,
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + device.mmio.start.0),
                size: device.mmio.size,
            },
            flags: MMIO_RW_DEVICE,
        };
        count += 1;
    }
    Ok(count)
}

fn clint_mmio_region() -> MmioRegion {
    MmioRegion {
        name: "clint",
        phys: PhysRange {
            start: PhysAddr(0x0200_0000),
            size: 0x1_0000,
        },
        virt: VirtRange {
            start: VirtAddr(DIRECT_MAP_BASE + 0x0200_0000),
            size: 0x1_0000,
        },
        flags: MMIO_RW_DEVICE,
    }
}

fn goldfish_rtc_mmio_region() -> MmioRegion {
    MmioRegion {
        name: "goldfish-rtc",
        phys: PhysRange {
            start: PhysAddr(0x0010_1000),
            size: 0x1000,
        },
        virt: VirtRange {
            start: VirtAddr(DIRECT_MAP_BASE + 0x0010_1000),
            size: 0x1000,
        },
        flags: MMIO_RW_DEVICE,
    }
}

pub(crate) fn qemu_mmio_regions() -> [MmioRegion; 6] {
    [
        // QEMU virt goldfish-rtc (`rtc@101000`). One page; read once at boot to
        // seed CLOCK_REALTIME from real host time. Without this mapping the
        // boot-time RTC read faults (load page fault at the direct-map VA).
        goldfish_rtc_mmio_region(),
        MmioRegion {
            name: "clint",
            phys: PhysRange {
                start: PhysAddr(0x0200_0000),
                size: 0x1_0000,
            },
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + 0x0200_0000),
                size: 0x1_0000,
            },
            flags: MMIO_RW_DEVICE,
        },
        MmioRegion {
            name: "plic",
            phys: PhysRange {
                start: PhysAddr(0x0c00_0000),
                size: 0x400_0000,
            },
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + 0x0c00_0000),
                size: 0x400_0000,
            },
            flags: MMIO_RW_DEVICE,
        },
        MmioRegion {
            name: "uart0",
            phys: PhysRange {
                start: PhysAddr(0x1000_0000),
                size: 0x1000,
            },
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + 0x1000_0000),
                size: 0x1000,
            },
            flags: MMIO_RW_DEVICE,
        },
        MmioRegion {
            name: "virtio0",
            phys: PhysRange {
                start: PhysAddr(0x1000_1000),
                size: 0x1000,
            },
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + 0x1000_1000),
                size: 0x1000,
            },
            flags: MMIO_RW_DEVICE,
        },
        // Second QEMU virt virtio-mmio slot (0x1000_2000). The block driver
        // probes "virtio0"; exposing "virtio1" lets the net driver bind a
        // SEPARATE device so virtio-blk (root/ext4) and virtio-net (eth0) can
        // coexist. QEMU: `-device virtio-blk-device,...,bus=virtio-mmio-bus.0`
        // (-> virtio0) and `-device virtio-net-device,...,bus=virtio-mmio-bus.1`
        // (-> virtio1). Needed for the git Task2 outbound-network path.
        MmioRegion {
            name: "virtio1",
            phys: PhysRange {
                start: PhysAddr(0x1000_2000),
                size: 0x1000,
            },
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + 0x1000_2000),
                size: 0x1000,
            },
            flags: MMIO_RW_DEVICE,
        },
    ]
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn secondary_start_entry() -> usize {
    let entry = unsafe { core::ptr::addr_of!(SECONDARY_START_ENTRY).read_volatile() };
    entry as *const () as usize
}

// ===== 核心:启动包 BootStaticBag + 类型状态机(全文真正的"逻辑",约 60 行) =====
// 全局槽的四个状态:没造 → 桥还在(Live) → 取走处理中(Taken) → 桥已拆(Dropped)
enum StoredBootStaticBag {
    Uninit,
    IdentityLive(BootStaticBag<IdentityLive>),
    Taken,
    IdentityDropped(BootStaticBag<IdentityDropped>),
}

/// 内核"启动地址登记表":记录内核自身和启动期固定资源在物理内存里的位置。
/// 开机时由 capture() 填一次,此后长期存在——fork 建进程、分配页表节点时都要
/// 来这里查"预留的页表内存在哪"。字段全是物理地址(用 BootLinkedAddr 归一化)。
/// 泛型 State 是状态标记:IdentityLive=低地址恒等映射还在,IdentityDropped=已拆。
pub(crate) struct BootStaticBag<State> {
    // —— 内核镜像各段的物理起止地址(启动时从链接器符号读出)——
    kernel_start: BootLinkedAddr, // 整个内核镜像
    kernel_end: BootLinkedAddr,
    text_start: BootLinkedAddr, // 代码段
    text_end: BootLinkedAddr,
    rodata_start: BootLinkedAddr, // 只读数据段
    rodata_end: BootLinkedAddr,
    data_start: BootLinkedAddr, // 可写数据段
    data_end: BootLinkedAddr,
    bss_start: BootLinkedAddr, // 未初始化数据段
    bss_end: BootLinkedAddr,
    // —— 启动栈、入口点、关键寄存器初值 ——
    boot_stack_bottom: BootLinkedAddr, // 启动栈
    boot_stack_top: BootLinkedAddr,
    global_pointer: BootLinkedAddr, // gp 寄存器初值
    rust_entry: BootLinkedAddr,     // rust_entry 函数地址
    trap_vector: BootLinkedAddr,    // 陷入向量地址
    // —— 引导页表的各块存储地址(供 pmap 长期取用)——
    bootstrap_root: BootLinkedAddr,         // 引导页表根
    kernel_alias_l1: BootLinkedAddr,        // 高地址别名 L1 表
    kernel_alias_l0_tables: BootLinkedAddr, // 高地址别名 L0 表
    pt_node_pool: BootLinkedAddr,           // 页表节点池(fork 分配页表用)
    // —— 固件事实 + 状态标记 ——
    dtb: FirmwareDtb,           // 固件给的设备树物理地址
    _state: PhantomData<State>, // 零大小状态标记,不占内存
}

// "桥还在(恒等映射未拆)"阶段的方法:造包、取包、读链接器符号构造
impl BootStaticBag<IdentityLive> {
    /// 构造这张登记表,且全局只允许造一次;造好存进全局槽。重复调用直接 panic。
    pub(crate) fn capture_once(dtb_addr: usize) -> &'static mut Self {
        unsafe {
            let slot = &mut *STORED_BOOT_STATIC_BAG.0.get();
            match slot {
                StoredBootStaticBag::Uninit => {
                    // 只有"还没造"才允许构造
                    *slot = StoredBootStaticBag::IdentityLive(Self::capture(dtb_addr));
                }
                StoredBootStaticBag::IdentityLive(_)
                | StoredBootStaticBag::Taken
                | StoredBootStaticBag::IdentityDropped(_) => {
                    panic!("BootStaticBag constructed more than once")
                }
            }
            match slot {
                StoredBootStaticBag::IdentityLive(bag) => bag,
                _ => unreachable!(),
            }
        }
    }

    /// 把这张表从全局槽里"拿出来"(槽临时置成 Taken,防止别人同时动它)。
    pub(crate) fn take_global() -> Self {
        unsafe {
            match core::mem::replace(
                &mut *STORED_BOOT_STATIC_BAG.0.get(),
                StoredBootStaticBag::Taken,
            ) {
                StoredBootStaticBag::IdentityLive(bag) => bag,
                StoredBootStaticBag::Uninit => panic!("BootStaticBag not constructed"),
                StoredBootStaticBag::Taken => panic!("BootStaticBag transition in progress"),
                StoredBootStaticBag::IdentityDropped(_) => panic!("BootStaticBag identity dropped"),
            }
        }
    }

    fn store_dropped(bag: BootStaticBag<IdentityDropped>) {
        unsafe {
            let slot = &mut *STORED_BOOT_STATIC_BAG.0.get();
            match slot {
                StoredBootStaticBag::Taken => *slot = StoredBootStaticBag::IdentityDropped(bag),
                StoredBootStaticBag::Uninit => panic!("BootStaticBag not constructed"),
                StoredBootStaticBag::IdentityLive(_) => panic!("BootStaticBag identity still live"),
                StoredBootStaticBag::IdentityDropped(_) => panic!("BootStaticBag already dropped"),
            }
        }
    }

    pub(crate) fn firmware_dtb(&self) -> FirmwareDtb {
        self.dtb
    }

    /// 状态跃迁:把"恒等映射还在(Live)"的表转成"已拆(Dropped)"的表(仅换类型标记)。
    pub(crate) fn into_dropped(self) -> BootStaticBag<IdentityDropped> {
        BootStaticBag {
            kernel_start: self.kernel_start,
            kernel_end: self.kernel_end,
            text_start: self.text_start,
            text_end: self.text_end,
            rodata_start: self.rodata_start,
            rodata_end: self.rodata_end,
            data_start: self.data_start,
            data_end: self.data_end,
            bss_start: self.bss_start,
            bss_end: self.bss_end,
            boot_stack_bottom: self.boot_stack_bottom,
            boot_stack_top: self.boot_stack_top,
            global_pointer: self.global_pointer,
            rust_entry: self.rust_entry,
            trap_vector: self.trap_vector,
            bootstrap_root: self.bootstrap_root,
            kernel_alias_l1: self.kernel_alias_l1,
            kernel_alias_l0_tables: self.kernel_alias_l0_tables,
            pt_node_pool: self.pt_node_pool,
            dtb: self.dtb,
            _state: PhantomData,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(dtb_addr: usize) -> Self {
        let mut bag = Self::capture(dtb_addr);
        bag.kernel_start = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE);
        bag.text_start = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE);
        bag.text_end = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x2000);
        bag.rodata_start = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x2000);
        bag.rodata_end = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x3000);
        bag.data_start = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x3000);
        bag.data_end = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x4000);
        bag.bss_start = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x4000);
        bag.bss_end = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x5000);
        bag.boot_stack_bottom = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x5000);
        bag.boot_stack_top = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x6000);
        bag.kernel_end = BootLinkedAddr::from_linked(QEMU_KERNEL_PHYS_BASE + 0x6000);
        bag
    }

    #[cfg(test)]
    pub(crate) unsafe fn reset_global_for_test() {
        unsafe {
            *STORED_BOOT_STATIC_BAG.0.get() = StoredBootStaticBag::Uninit;
            *TIMEBASE_FREQUENCY_HZ.0.get() = QEMU_VIRT_FALLBACK_TIMEBASE_HZ;
            *POSSIBLE_CPU_COUNT.0.get() = 1;
            *PLATFORM_DEVICE_COUNT.0.get() = 0;
            (*PLATFORM_DEVICES.0.get()).fill(EMPTY_DEVICE);
            (*PLATFORM_MMIO_REGIONS.0.get()) = [empty_mmio_region(); GENERATED_MMIO_REGIONS];
            let seed = TypedSeedWriter::new();
            let _ = seed.finish();
            *PLATFORM_INFO.0.get() = PlatformInfo {
                board: "",
                spi_sd: None,
                mmio_regions: &[],
                device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
                timebase_frequency_hz: QEMU_VIRT_FALLBACK_TIMEBASE_HZ,
                possible_cpu_count: 1,
            };
        }
    }

    /// 把板子启动时独占的静态存储,捕获成"地址事实"。
    /// 唯一真正的构造函数;运行时把结果存为单一 boot-static 权威,再靠类型状态推进。
    fn capture(dtb_addr: usize) -> Self {
        #[cfg(target_arch = "riscv64")]
        let (
            kernel_start,
            kernel_end,
            text_start,
            text_end,
            rodata_start,
            rodata_end,
            data_start,
            data_end,
            bss_start,
            bss_end,
            boot_stack_bottom,
            boot_stack_top,
            global_pointer,
            rust_entry,
            trap_vector,
        ) = {
            unsafe extern "C" {
                static __kernel_start: u8;
                static __kernel_end: u8;
                static __text_start: u8;
                static __text_end: u8;
                static __rodata_start: u8;
                static __rodata_end: u8;
                static __data_start: u8;
                static __data_end: u8;
                static __bss_start: u8;
                static __bss_end: u8;
                static __tx_boot_stack_bottom: u8;
                static __tx_boot_stack_top: u8;
                #[link_name = "__global_pointer$"]
                static GLOBAL_POINTER: u8;
                fn rust_entry(cpu_id: usize, firmware_arg: usize) -> !;
                fn tx_rv64_qemu_minimal_trap_vector();
            }

            (
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__kernel_start) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__kernel_end) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__text_start) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__text_end) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__rodata_start) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__rodata_end) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__data_start) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__data_end) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__bss_start) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__bss_end) as usize),
                BootLinkedAddr::from_runtime_addr(
                    core::ptr::addr_of!(__tx_boot_stack_bottom) as usize
                ),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(__tx_boot_stack_top) as usize),
                BootLinkedAddr::from_runtime_addr(core::ptr::addr_of!(GLOBAL_POINTER) as usize),
                BootLinkedAddr::from_runtime_addr(rust_entry as *const () as usize),
                BootLinkedAddr::from_runtime_addr(
                    tx_rv64_qemu_minimal_trap_vector as *const () as usize,
                ),
            )
        };

        #[cfg(not(target_arch = "riscv64"))]
        let (
            kernel_start,
            kernel_end,
            text_start,
            text_end,
            rodata_start,
            rodata_end,
            data_start,
            data_end,
            bss_start,
            bss_end,
            boot_stack_bottom,
            boot_stack_top,
            global_pointer,
            rust_entry,
            trap_vector,
        ) = (
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
            BootLinkedAddr::from_linked(0),
        );

        Self {
            kernel_start,
            kernel_end,
            text_start,
            text_end,
            rodata_start,
            rodata_end,
            data_start,
            data_end,
            bss_start,
            bss_end,
            boot_stack_bottom,
            boot_stack_top,
            global_pointer,
            rust_entry,
            trap_vector,
            bootstrap_root: BootLinkedAddr::from_runtime_addr(BOOTSTRAP_ROOT.0.get() as usize),
            kernel_alias_l1: BootLinkedAddr::from_runtime_addr(KERNEL_ALIAS_L1.0.get() as usize),
            kernel_alias_l0_tables: BootLinkedAddr::from_runtime_addr(
                KERNEL_ALIAS_L0_TABLES_STORAGE.0.get() as usize,
            ),
            pt_node_pool: BootLinkedAddr::from_runtime_addr(PT_NODE_POOL.0.get() as usize),
            dtb: FirmwareDtb::from_addr(dtb_addr),
            _state: PhantomData,
        }
    }
}

// "桥已拆(恒等映射移除)"阶段才有的方法:装回全局、取只读引用
impl BootStaticBag<IdentityDropped> {
    /// 拆桥完成后,把这张最终的表装回全局槽(定案);此后全内核只读它。
    pub(crate) fn install_global(self) {
        BootStaticBag::<IdentityLive>::store_dropped(self);
    }

    /// 取全局那张已定案的表的只读引用——fork/pmap 全程靠它查页表内存地址。
    #[track_caller]
    pub(crate) fn global_ref() -> &'static Self {
        unsafe {
            match &*STORED_BOOT_STATIC_BAG.0.get() {
                StoredBootStaticBag::IdentityDropped(bag) => bag,
                StoredBootStaticBag::Uninit => panic!("BootStaticBag not constructed"),
                StoredBootStaticBag::Taken => panic!("BootStaticBag transition in progress"),
                StoredBootStaticBag::IdentityLive(_) => panic!("BootStaticBag identity still live"),
            }
        }
    }
}

// ===== 访问器海:两个阶段共用,取/写全局缓冲区、算各段物理地址,约 30 个三行方法 =====
impl<State> BootStaticBag<State> {
    pub(crate) const fn firmware_dtb_parse_addr(&self) -> usize {
        self.dtb.parse_addr()
    }

    pub(crate) fn current_trap_vector_kernel_alias() -> VirtAddr {
        unsafe {
            match &*STORED_BOOT_STATIC_BAG.0.get() {
                StoredBootStaticBag::IdentityLive(bag) => bag.trap_vector_kernel_alias(),
                StoredBootStaticBag::IdentityDropped(bag) => bag.trap_vector_kernel_alias(),
                StoredBootStaticBag::Uninit | StoredBootStaticBag::Taken => {
                    linked_trap_vector_kernel_alias()
                }
            }
        }
    }

    pub(crate) fn boot_info_ref(&self) -> &'static BootInfo {
        unsafe { &*BOOT_INFO.0.get() }
    }

    pub(crate) unsafe fn boot_info_mut(&self) -> &'static mut BootInfo {
        unsafe { &mut *BOOT_INFO.0.get() }
    }

    pub(crate) fn bootstrap_pmap_info_ref(&self) -> Option<&'static BootstrapPmapInfo> {
        unsafe { (&*BOOTSTRAP_PMAP_INFO.0.get()).as_ref() }
    }

    pub(crate) unsafe fn bootstrap_pmap_info_mut(&self) -> &'static mut Option<BootstrapPmapInfo> {
        unsafe { &mut *BOOTSTRAP_PMAP_INFO.0.get() }
    }

    pub(crate) unsafe fn reserved_page_tables_mut(
        &self,
    ) -> &'static mut [PhysRange; BOOTSTRAP_PMAP_RESERVED_RANGES] {
        unsafe { &mut *RESERVED_PAGE_TABLES.0.get() }
    }

    #[cfg(test)]
    pub(crate) fn bootstrap_root_ref(&self) -> &'static PageTable {
        unsafe { &*BOOTSTRAP_ROOT.0.get() }
    }

    pub(crate) unsafe fn bootstrap_root_mut(&self) -> &'static mut PageTable {
        unsafe { &mut *BOOTSTRAP_ROOT.0.get() }
    }

    #[cfg(test)]
    pub(crate) unsafe fn kernel_alias_l1_mut(&self) -> &'static mut PageTable {
        unsafe { &mut *KERNEL_ALIAS_L1.0.get() }
    }

    pub(crate) unsafe fn kernel_alias_l0_mut(&self, index: usize) -> &'static mut PageTable {
        unsafe { &mut (*KERNEL_ALIAS_L0_TABLES_STORAGE.0.get())[index] }
    }

    pub(crate) unsafe fn memory_regions_mut(
        &self,
    ) -> &'static mut [MemoryRegion; MAX_MEMORY_REGIONS] {
        unsafe { &mut *MEMORY_REGIONS.0.get() }
    }

    pub(crate) unsafe fn cmdline_mut(&self) -> &'static mut [u8; CMDLINE_CAPACITY] {
        unsafe { &mut *CMDLINE.0.get() }
    }

    pub(crate) fn platform_devices_ref(&self) -> &'static [DeviceInfo] {
        unsafe {
            let count = *PLATFORM_DEVICE_COUNT.0.get();
            assert!(
                count <= MAX_PLATFORM_DEVICES,
                "published platform DeviceInfo count exceeds boot-static capacity"
            );
            let devices: &'static [DeviceInfo; MAX_PLATFORM_DEVICES] = &*PLATFORM_DEVICES.0.get();
            &devices[..count]
        }
    }

    pub(crate) fn platform_info_ref(&self) -> &'static PlatformInfo {
        unsafe { &*PLATFORM_INFO.0.get() }
    }

    pub(crate) unsafe fn publish_device_facts_from_fdt(
        &self,
        dtb_addr: usize,
    ) -> Result<usize, crate::dtb::DtbDeviceError> {
        let devices = unsafe { self.platform_devices_mut() };
        devices.fill(EMPTY_DEVICE);
        let mut seed = unsafe { TypedSeedWriter::new() };
        let count = unsafe {
            crate::dtb::parse_devices_from_fdt_with(dtb_addr, devices, |fact| seed.push(fact))
        }?;
        unsafe {
            seed.finish();
        }
        self.publish_platform_device_count(count);
        Ok(count)
    }

    pub(crate) fn publish_platform_info(&self) -> Result<(), crate::dtb::DtbDeviceError> {
        unsafe {
            let mmio_regions = &mut *PLATFORM_MMIO_REGIONS.0.get();
            let region_count = build_mmio_regions(self.platform_devices_ref(), mmio_regions)?;
            let mmio_regions: &'static [MmioRegion; GENERATED_MMIO_REGIONS] =
                &*PLATFORM_MMIO_REGIONS.0.get();
            let device_resources: &'static DeviceResourceGraph = &*DEVICE_RESOURCE_GRAPH.0.get();

            *PLATFORM_INFO.0.get() = PlatformInfo {
                board: Platform::BOARD,
                spi_sd: None,
                mmio_regions: &mmio_regions[..region_count],
                device_resources,
                timebase_frequency_hz: *TIMEBASE_FREQUENCY_HZ.0.get(),
                possible_cpu_count: *POSSIBLE_CPU_COUNT.0.get(),
            };
        }
        Ok(())
    }

    pub(crate) unsafe fn platform_devices_mut(
        &self,
    ) -> &'static mut [DeviceInfo; MAX_PLATFORM_DEVICES] {
        unsafe { &mut *PLATFORM_DEVICES.0.get() }
    }

    pub(crate) fn publish_platform_device_count(&self, count: usize) {
        assert!(
            count <= MAX_PLATFORM_DEVICES,
            "platform DeviceInfo capacity exceeded"
        );
        unsafe {
            *PLATFORM_DEVICE_COUNT.0.get() = count;
        }
    }

    pub(crate) unsafe fn plic_scontexts_mut(
        &self,
    ) -> &'static mut [Option<u32>; crate::dtb::MAX_PLIC_HARTS] {
        unsafe { &mut *PLIC_SCONTEXTS.0.get() }
    }

    pub(crate) fn publish_plic_phys_base(&self, phys_base: usize) {
        unsafe {
            *PLIC_PHYS_BASE_PUBLISHED.0.get() = phys_base;
        }
    }

    pub(crate) fn publish_startable_harts(&self, mask: u64) {
        unsafe {
            *STARTABLE_HARTS.0.get() = mask;
        }
    }

    pub(crate) fn publish_timebase_frequency_hz(&self, frequency_hz: u64) {
        unsafe {
            *TIMEBASE_FREQUENCY_HZ.0.get() = frequency_hz;
        }
    }

    pub(crate) fn publish_possible_cpu_count(&self, possible_cpu_count: usize) {
        unsafe {
            *POSSIBLE_CPU_COUNT.0.get() = possible_cpu_count.max(1);
        }
    }

    pub(crate) fn kernel_image_phys(&self) -> PhysRange {
        linked_range(self.kernel_start, self.kernel_end)
    }

    pub(crate) fn kernel_text_phys(&self) -> PhysRange {
        linked_range(self.text_start, self.text_end)
    }

    pub(crate) fn kernel_rodata_phys(&self) -> PhysRange {
        linked_range(self.rodata_start, self.rodata_end)
    }

    pub(crate) fn kernel_data_phys(&self) -> PhysRange {
        linked_range(self.data_start, self.data_end)
    }

    pub(crate) fn kernel_bss_phys(&self) -> PhysRange {
        linked_range(self.bss_start, self.bss_end)
    }

    pub(crate) fn kernel_stack_phys(&self) -> PhysRange {
        linked_range(self.boot_stack_bottom, self.boot_stack_top)
    }

    #[cfg(test)]
    pub(crate) fn high_boot_transition(&self) -> Option<HighBootTransition> {
        HighBootTransition::from_linked(self.boot_stack_top, self.global_pointer, self.rust_entry)
    }

    fn trap_vector_kernel_alias(&self) -> VirtAddr {
        self.trap_vector
            .kernel_alias_va()
            .unwrap_or(VirtAddr(self.trap_vector.raw()))
    }

    pub(crate) fn bootstrap_root_phys(&self) -> PhysAddr {
        self.bootstrap_root.phys()
    }

    pub(crate) fn kernel_alias_l1_phys(&self) -> PhysAddr {
        self.kernel_alias_l1.phys()
    }

    #[cfg(test)]
    pub(crate) fn kernel_alias_l0_phys(&self, index: usize) -> PhysAddr {
        PhysAddr(self.kernel_alias_l0_tables.raw() + index * PAGE_SIZE)
    }

    pub(crate) fn kernel_alias_l0_phys_range(&self) -> PhysRange {
        PhysRange {
            start: self.kernel_alias_l0_tables.phys(),
            size: KERNEL_ALIAS_L0_TABLES * PAGE_SIZE,
        }
    }

    pub(crate) fn pt_node_phys(&self, index: usize) -> PhysAddr {
        PhysAddr(self.pt_node_pool.raw() + index * PAGE_SIZE)
    }

    #[cfg(target_arch = "riscv64")]
    pub(crate) fn pt_node_direct_va(&self, index: usize) -> VirtAddr {
        VirtAddr(DIRECT_MAP_BASE + self.pt_node_phys(index).0)
    }

    pub(crate) fn pt_node_pool_phys_range(&self) -> PhysRange {
        PhysRange {
            start: self.pt_node_pool.phys(),
            size: PT_NODE_POOL_ENTRIES * PAGE_SIZE,
        }
    }
}

pub(crate) fn current_trap_vector_kernel_alias() -> VirtAddr {
    BootStaticBag::<IdentityLive>::current_trap_vector_kernel_alias()
}

fn linked_trap_vector_kernel_alias() -> VirtAddr {
    #[cfg(target_arch = "riscv64")]
    {
        unsafe extern "C" {
            fn tx_rv64_qemu_minimal_trap_vector();
        }

        let linked = BootLinkedAddr::from_runtime_addr(
            tx_rv64_qemu_minimal_trap_vector as *const () as usize,
        );
        linked.kernel_alias_va().unwrap_or(VirtAddr(linked.raw()))
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        VirtAddr(0)
    }
}

fn linked_range(start: BootLinkedAddr, end: BootLinkedAddr) -> PhysRange {
    let start = start.phys();
    let end = end.phys();
    PhysRange {
        start,
        size: end.0.saturating_sub(start.0),
    }
}

pub(crate) const fn reserved_region() -> MemoryRegion {
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const QEMU_RV64_VIRT_DTB: &[u8] = include_bytes!("../../dtbs/qemu-rv64-virt.dtb");
    const JH7110_VF2_DTB: &[u8] =
        include_bytes!("../../dtbs/jh7110-starfive-visionfive-2-v1.3b.dtb");

    fn mmio_regions_from_dtb(dtb: &[u8]) -> ([MmioRegion; GENERATED_MMIO_REGIONS], usize) {
        let mut devices = [EMPTY_DEVICE; MAX_PLATFORM_DEVICES];
        let device_count =
            unsafe { crate::dtb::parse_devices_from_fdt(dtb.as_ptr() as usize, &mut devices) }
                .expect("fixture device projection should fit");
        let mut regions = [empty_mmio_region(); GENERATED_MMIO_REGIONS];
        let region_count = build_mmio_regions(&devices[..device_count], &mut regions)
            .expect("fixture MMIO projection should fit");
        (regions, region_count)
    }

    fn with_fixture_platform_info(dtb: &[u8], check: impl FnOnce(&PlatformInfo, &[DeviceInfo])) {
        unsafe {
            BootStaticBag::<IdentityLive>::reset_global_for_test();
        }
        let bag = BootStaticBag::<IdentityLive>::new_for_test(dtb.as_ptr() as usize);
        unsafe {
            bag.publish_device_facts_from_fdt(dtb.as_ptr() as usize)
                .expect("fixture typed seed should fit boot-static storage");
        }
        bag.publish_platform_info()
            .expect("fixture legacy MMIO projection should fit");

        let first = bag.platform_info_ref();
        let first_graph = first.device_resources as *const DeviceResourceGraph;
        let second = bag.platform_info_ref();
        assert!(core::ptr::eq(first, second));
        assert_eq!(first_graph, second.device_resources as *const _);
        check(first, bag.platform_devices_ref());

        unsafe {
            BootStaticBag::<IdentityLive>::reset_global_for_test();
        }
    }

    fn device_path(device: &PlatformDevice) -> &'static str {
        match device.id.local {
            DeviceLocalId::FirmwarePath(path) => path,
            _ => panic!("FDT seed must use firmware-path identity"),
        }
    }

    fn device_by_path<'a>(graph: &'a DeviceResourceGraph, path: &str) -> &'a PlatformDevice {
        graph
            .devices
            .iter()
            .find(|device| device_path(device) == path)
            .unwrap_or_else(|| panic!("missing typed device {path}"))
    }

    fn device_mmio(device: &PlatformDevice) -> MmioResource {
        device
            .resources
            .iter()
            .find_map(|resource| match resource {
                DeviceResource::Mmio(mmio) => Some(*mmio),
                _ => None,
            })
            .expect("typed device MMIO")
    }

    fn device_irq(device: &PlatformDevice) -> Option<IrqResource> {
        device.resources.iter().find_map(|resource| match resource {
            DeviceResource::Irq(irq) => Some(*irq),
            _ => None,
        })
    }

    fn device_dma(device: &PlatformDevice) -> Option<DmaDomainRef> {
        device.resources.iter().find_map(|resource| match resource {
            DeviceResource::DmaDomain(domain) => Some(*domain),
            _ => None,
        })
    }

    fn assert_legacy_typed_projection(info: &PlatformInfo, legacy_devices: &[DeviceInfo]) {
        let graph = info.device_resources;
        tx_hal::DeviceGraphBuilder::from_seed(graph).expect("fixture seed validates");
        assert_eq!(graph.dma_domains, &DEFAULT_DMA_DOMAINS);
        assert!(graph
            .platform_mmio
            .iter()
            .all(|mmio| mmio.origin.kind == ResourceOriginKind::Firmware));

        let mut typed_mmio = Vec::new();
        typed_mmio.extend(graph.platform_mmio.iter().copied());
        for device in graph.devices {
            typed_mmio.extend(
                device
                    .resources
                    .iter()
                    .filter_map(|resource| match resource {
                        DeviceResource::Mmio(mmio) => Some(*mmio),
                        _ => None,
                    }),
            );
        }
        assert_eq!(typed_mmio.len(), info.mmio_regions.len());
        for legacy in info.mmio_regions {
            assert_eq!(
                typed_mmio
                    .iter()
                    .filter(|typed| {
                        typed.phys == legacy.phys
                            && typed.virt == legacy.virt
                            && typed.flags == legacy.flags
                    })
                    .count(),
                1,
                "legacy MMIO {} must have exactly one typed source",
                legacy.name,
            );
        }

        let tier2_legacy_count = legacy_devices
            .iter()
            .filter(|device| {
                matches!(
                    device.kind,
                    DeviceKind::VirtioMmio
                        | DeviceKind::PciEcam
                        | DeviceKind::SdController
                        | DeviceKind::Dwmac
                )
            })
            .count();
        assert_eq!(graph.devices.len(), tier2_legacy_count);
        for device in graph.devices {
            assert_eq!(device.status, DeviceStatus::Enabled);
            assert_eq!(device.origin.kind, ResourceOriginKind::Firmware);
            assert_eq!(device.id.provider, FDT_RESOURCE_PROVIDER);
            assert_eq!(device_mmio(device).role, ResourceRole::Index(0));
            assert!(device.resources.iter().all(|resource| matches!(
                resource,
                DeviceResource::Mmio(_) | DeviceResource::Irq(_) | DeviceResource::DmaDomain(_)
            )));
            if device.matches.iter().any(|candidate| {
                matches!(candidate, DeviceMatchId::FirmwareCompatible(value)
                    if *value == "virtio,mmio"
                        || *value == "starfive,dwmac"
                        || *value == "starfive,jh7110-dwmac")
            }) {
                assert_eq!(
                    device_dma(device),
                    Some(DmaDomainRef {
                        role: ResourceRole::Index(0),
                        domain: DEFAULT_DMA_DOMAIN_ID,
                    })
                );
            } else {
                assert_eq!(device_dma(device), None);
            }
            if let Some(irq) = device_irq(device) {
                assert_eq!(irq.role, ResourceRole::Index(0));
                assert_eq!(irq.origin.kind, ResourceOriginKind::Firmware);
            }
        }
    }

    fn assert_string_outside_fixture(value: &str, dtb: &[u8]) {
        let value_start = value.as_ptr() as usize;
        let value_end = value_start + value.len();
        let dtb_start = dtb.as_ptr() as usize;
        let dtb_end = dtb_start + dtb.len();
        assert!(value_end <= dtb_start || value_start >= dtb_end);
    }

    #[test]
    fn qemu_dtb_maps_discovered_goldfish_rtc_once() {
        let (regions, count) = mmio_regions_from_dtb(QEMU_RV64_VIRT_DTB);
        let rtc: std::vec::Vec<_> = regions[..count]
            .iter()
            .filter(|region| region.name == "goldfish-rtc")
            .collect();

        assert_eq!(rtc.len(), 1);
        assert_eq!(rtc[0].phys.start, PhysAddr(0x0010_1000));
        assert_eq!(rtc[0].phys.size, 0x1000);
    }

    #[test]
    fn visionfive2_dtb_does_not_map_goldfish_rtc() {
        let (regions, count) = mmio_regions_from_dtb(JH7110_VF2_DTB);
        assert!(regions[..count].iter().all(|region| {
            region.name != "goldfish-rtc" && region.phys.start != PhysAddr(0x0010_1000)
        }));
    }

    #[test]
    fn qemu_fixture_publishes_typed_seed_and_matching_legacy_projection() {
        with_fixture_platform_info(QEMU_RV64_VIRT_DTB, |info, legacy_devices| {
            assert_legacy_typed_projection(info, legacy_devices);
            let graph = info.device_resources;
            assert_eq!(graph.platform_mmio.len(), 4);
            assert_eq!(graph.devices.len(), 9);
            for path in [
                "/soc/rtc@101000",
                "/soc/serial@10000000",
                "/soc/plic@c000000",
                "/soc/clint@2000000",
            ] {
                assert!(graph
                    .platform_mmio
                    .iter()
                    .any(|mmio| mmio.origin.record == path));
                assert!(graph
                    .devices
                    .iter()
                    .all(|device| device_path(device) != path));
            }

            let uart = graph
                .platform_mmio
                .iter()
                .find(|mmio| mmio.origin.record == "/soc/serial@10000000")
                .expect("QEMU UART platform MMIO");
            assert_eq!(uart.phys.start, PhysAddr(0x1000_0000));
            assert_eq!(uart.virt.start, VirtAddr(DIRECT_MAP_BASE + 0x1000_0000));
            assert_eq!(uart.flags, MMIO_RW_DEVICE);
            assert_eq!(uart.origin.kind, ResourceOriginKind::Firmware);

            let virtio = device_by_path(graph, "/soc/virtio_mmio@10001000");
            assert!(virtio
                .matches
                .contains(&DeviceMatchId::FirmwareCompatible("virtio,mmio")));
            assert_eq!(device_mmio(virtio).phys.start, PhysAddr(0x1000_1000));
            assert_eq!(device_irq(virtio).map(|irq| irq.line), Some(1));
            assert_string_outside_fixture(device_path(virtio), QEMU_RV64_VIRT_DTB);
            let DeviceMatchId::FirmwareCompatible(compatible) = virtio.matches[0] else {
                panic!("QEMU virtio must use firmware compatible matching")
            };
            assert_string_outside_fixture(compatible, QEMU_RV64_VIRT_DTB);

            let pci = device_by_path(graph, "/soc/pci@30000000");
            assert!(pci
                .matches
                .contains(&DeviceMatchId::FirmwareCompatible("pci-host-ecam-generic")));
            assert_eq!(device_mmio(pci).phys.start, PhysAddr(0x3000_0000));
            assert_eq!(device_irq(pci), None);
        });
    }

    #[test]
    fn vf2_fixture_publishes_stable_dwmac_paths_and_filters_disabled_uart() {
        with_fixture_platform_info(JH7110_VF2_DTB, |info, legacy_devices| {
            assert_legacy_typed_projection(info, legacy_devices);
            let graph = info.device_resources;
            assert_eq!(graph.platform_mmio.len(), 7);
            assert_eq!(graph.devices.len(), 4);
            for path in [
                "/soc/serial@10000000",
                "/soc/plic@c000000",
                "/soc/clint@2000000",
                "/soc/cache-controller@2010000",
            ] {
                assert!(graph
                    .platform_mmio
                    .iter()
                    .any(|mmio| mmio.origin.record == path));
            }
            assert!(graph
                .platform_mmio
                .iter()
                .all(|mmio| mmio.origin.record != "/soc/serial@10010000"));
            assert_eq!(
                legacy_devices
                    .iter()
                    .filter(|device| device.kind == DeviceKind::Uart)
                    .count(),
                1
            );

            let dwmac = device_by_path(graph, "/soc/ethernet@16030000");
            assert!(dwmac
                .matches
                .contains(&DeviceMatchId::FirmwareCompatible("starfive,dwmac")));
            assert!(dwmac
                .matches
                .contains(&DeviceMatchId::FirmwareCompatible("snps,dwmac-5.10a")));
            assert_eq!(device_mmio(dwmac).phys.start, PhysAddr(0x1603_0000));
            assert_eq!(device_mmio(dwmac).phys.size, 0x1_0000);
            assert_eq!(device_irq(dwmac).map(|irq| irq.line), Some(7));
            assert_string_outside_fixture(device_path(dwmac), JH7110_VF2_DTB);

            let dwmac1 = device_by_path(graph, "/soc/ethernet@16040000");
            assert_eq!(device_irq(dwmac1).map(|irq| irq.line), Some(78));

            let sd = device_by_path(graph, "/soc/sdio1@16020000");
            assert!(sd
                .matches
                .contains(&DeviceMatchId::FirmwareCompatible("starfive,jh7110-sdio")));
            assert_eq!(device_mmio(sd).phys.start, PhysAddr(0x1602_0000));
            assert_eq!(device_irq(sd).map(|irq| irq.line), Some(75));
        });
    }

    #[test]
    fn typed_seed_string_arena_overflow_is_explicit() {
        let mut writer = TypedSeedWriter {
            platform_mmio_count: 0,
            device_count: 0,
            match_count: 0,
            resource_count: 0,
            string_cursor: DEVICE_RESOURCE_STRING_CAPACITY - 1,
        };
        assert_eq!(
            writer.copy_string_parts(&["xx"]),
            Err(crate::dtb::DtbDeviceError::StringArenaCapacityExceeded {
                capacity: DEVICE_RESOURCE_STRING_CAPACITY,
                required: DEVICE_RESOURCE_STRING_CAPACITY + 1,
            })
        );
    }
}
