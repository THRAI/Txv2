use core::cell::UnsafeCell;
use core::marker::PhantomData;

use tx_hal::{
    BootInfo, BootstrapPmapInfo, DeviceInfo, DeviceKind, MemoryRegion, MemoryRegionKind, MmioFlags,
    MmioRegion, PhysAddr, PhysRange, PlatformConfig, PlatformInfo, VirtAddr, VirtRange,
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
// QEMU virt 有 11 个可识别节点(8×virtio-mmio + uart + plic + pci ecam);VF2 更少,留余量
pub(crate) const MAX_PLATFORM_DEVICES: usize = 24;
// 每个设备一条 MmioRegion,外加固定的 clint 项(clint 无 DeviceKind 但其 MMIO 必须保持映射)
const GENERATED_MMIO_REGIONS: usize = MAX_PLATFORM_DEVICES + 2;

// 生成的 MMIO 区名字表,按各类型在设备树里出现的顺序取用
const VIRTIO_REGION_NAMES: [&str; 12] = [
    "virtio0", "virtio1", "virtio2", "virtio3", "virtio4", "virtio5", "virtio6", "virtio7",
    "virtio8", "virtio9", "virtio10", "virtio11",
];
const UART_REGION_NAMES: [&str; 6] = ["uart0", "uart1", "uart2", "uart3", "uart4", "uart5"];
const SDIO_REGION_NAMES: [&str; 4] = ["sdio0", "sdio1", "sdio2", "sdio3"];

const EMPTY_DEVICE: DeviceInfo = DeviceInfo {
    kind: DeviceKind::Uart,
    mmio: PhysRange::empty(),
    irq: None,
    reg_shift: 0,
    reg_io_width: 1,
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
    timebase_frequency_hz: QEMU_VIRT_FALLBACK_TIMEBASE_HZ,
    possible_cpu_count: 1,
}));
static PLATFORM_MMIO_REGIONS: PlatformMmioRegionsCell = PlatformMmioRegionsCell(UnsafeCell::new(
    [empty_mmio_region(); GENERATED_MMIO_REGIONS],
));
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
) -> usize {
    *out = [empty_mmio_region(); GENERATED_MMIO_REGIONS];
    if devices.is_empty() {
        let legacy = qemu_mmio_regions();
        out[..legacy.len()].copy_from_slice(&legacy);
        return legacy.len();
    }

    out[0] = clint_mmio_region();
    // The goldfish-rtc has no DeviceKind (same situation as the clint) but
    // must stay mapped so the boot-time CLOCK_REALTIME seed read from
    // `rtc@101000` doesn't fault. Add it statically alongside the clint.
    out[1] = goldfish_rtc_mmio_region();
    let mut count = 2usize;
    let mut virtio_index = 0usize;
    let mut uart_index = 0usize;
    let mut sdio_index = 0usize;
    for device in devices {
        if count == out.len() {
            break;
        }
        let name = match device.kind {
            DeviceKind::VirtioMmio => {
                let name = VIRTIO_REGION_NAMES.get(virtio_index);
                virtio_index += 1;
                name
            }
            DeviceKind::Uart => {
                let name = UART_REGION_NAMES.get(uart_index);
                uart_index += 1;
                name
            }
            DeviceKind::SdController => {
                let name = SDIO_REGION_NAMES.get(sdio_index);
                sdio_index += 1;
                name
            }
            DeviceKind::IntController => Some(&"plic"),
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
    count
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
        name: "rtc",
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

fn qemu_mmio_regions() -> [MmioRegion; 6] {
    [
        // QEMU virt goldfish-rtc (`rtc@101000`). One page; read once at boot to
        // seed CLOCK_REALTIME from real host time. Without this mapping the
        // boot-time RTC read faults (load page fault at the direct-map VA).
        MmioRegion {
            name: "rtc",
            phys: PhysRange {
                start: PhysAddr(0x0010_1000),
                size: 0x1000,
            },
            virt: VirtRange {
                start: VirtAddr(DIRECT_MAP_BASE + 0x0010_1000),
                size: 0x1000,
            },
            flags: MMIO_RW_DEVICE,
        },
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
            let count = (*PLATFORM_DEVICE_COUNT.0.get()).min(MAX_PLATFORM_DEVICES);
            let devices: &'static [DeviceInfo; MAX_PLATFORM_DEVICES] = &*PLATFORM_DEVICES.0.get();
            &devices[..count]
        }
    }

    pub(crate) fn platform_info_ref(&self) -> &'static PlatformInfo {
        unsafe {
            let mmio_regions = &mut *PLATFORM_MMIO_REGIONS.0.get();
            let region_count = build_mmio_regions(self.platform_devices_ref(), mmio_regions);
            let mmio_regions: &'static [MmioRegion; GENERATED_MMIO_REGIONS] =
                &*PLATFORM_MMIO_REGIONS.0.get();

            let platform_info = &mut *PLATFORM_INFO.0.get();
            *platform_info = PlatformInfo {
                board: Platform::BOARD,
                spi_sd: None,
                mmio_regions: &mmio_regions[..region_count],
                timebase_frequency_hz: *TIMEBASE_FREQUENCY_HZ.0.get(),
                possible_cpu_count: *POSSIBLE_CPU_COUNT.0.get(),
            };
            platform_info
        }
    }

    pub(crate) unsafe fn platform_devices_mut(
        &self,
    ) -> &'static mut [DeviceInfo; MAX_PLATFORM_DEVICES] {
        unsafe { &mut *PLATFORM_DEVICES.0.get() }
    }

    pub(crate) fn publish_platform_device_count(&self, count: usize) {
        unsafe {
            *PLATFORM_DEVICE_COUNT.0.get() = count.min(MAX_PLATFORM_DEVICES);
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
