#![no_std]

#[cfg_attr(not(test), allow(unused_extern_crates))]
extern crate alloc;

pub mod device_resource;
pub mod hart_local;
mod irq;
pub mod time;

pub use device_resource::*;
pub use hart_local::{HartLocal, MAX_HARTS};
pub use irq::*;

use core::marker::PhantomData;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arch {
    Riscv64,
    LoongArch64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuId(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuMask(pub u64);

impl CpuMask {
    pub const EMPTY: Self = Self(0);

    pub const fn single(cpu: CpuId) -> Self {
        if cpu.0 < u64::BITS as usize {
            Self(1u64 << cpu.0)
        } else {
            Self::EMPTY
        }
    }

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn first(count: usize) -> Self {
        if count == 0 {
            Self::EMPTY
        } else if count >= u64::BITS as usize {
            Self(u64::MAX)
        } else {
            Self((1u64 << count) - 1)
        }
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn contains(self, cpu: CpuId) -> bool {
        cpu.0 < u64::BITS as usize && (self.0 & (1u64 << cpu.0)) != 0
    }

    pub const fn without(self, cpu: CpuId) -> Self {
        Self(self.0 & !Self::single(cpu).0)
    }

    pub fn count(self) -> usize {
        self.0.count_ones() as usize
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug)]
#[must_use]
pub struct CpuPinGuard {
    cpu_id: CpuId,
    reason: CpuPinReason,
    unpin: Option<fn(CpuId, CpuPinReason)>,
    _not_send_sync: PhantomData<*mut ()>,
}

/// Stable, low-cardinality reason attached to a platform CPU pin.
///
/// Platforms may use this value for bounded diagnostics. It is deliberately
/// semantic rather than caller-address based so observation does not require
/// stack walking or serial output on the pin/unpin hot path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CpuPinReason {
    Unclassified = 0,
    EpochGuard = 1,
    EpochBorrow = 2,
    EpochRetire = 3,
    EpochDrain = 4,
    ZoneBucketPop = 5,
    ZoneBucketMerge = 6,
    ZoneBucketFlush = 7,
}

impl CpuPinGuard {
    pub const fn new(cpu_id: CpuId) -> Self {
        Self {
            cpu_id,
            reason: CpuPinReason::Unclassified,
            unpin: None,
            _not_send_sync: PhantomData,
        }
    }

    /// Construct a platform-backed CPU pin. The platform must have already
    /// entered its non-migratable section. `unpin` leaves that section when
    /// the guard drops.
    pub const fn with_unpin(cpu_id: CpuId, unpin: fn(CpuId, CpuPinReason)) -> Self {
        Self::with_reasoned_unpin(cpu_id, CpuPinReason::Unclassified, unpin)
    }

    /// Construct a platform-backed CPU pin with a bounded diagnostic reason.
    pub const fn with_reasoned_unpin(
        cpu_id: CpuId,
        reason: CpuPinReason,
        unpin: fn(CpuId, CpuPinReason),
    ) -> Self {
        Self {
            cpu_id,
            reason,
            unpin: Some(unpin),
            _not_send_sync: PhantomData,
        }
    }

    /// Attach a diagnostic reason while preserving ownership of this guard.
    pub const fn with_reason(mut self, reason: CpuPinReason) -> Self {
        self.reason = reason;
        self
    }

    pub const fn cpu_id(&self) -> CpuId {
        self.cpu_id
    }

    pub const fn reason(&self) -> CpuPinReason {
        self.reason
    }
}

impl Drop for CpuPinGuard {
    fn drop(&mut self) {
        if let Some(unpin) = self.unpin {
            unpin(self.cpu_id, self.reason);
        }
    }
}

/// RAII token that masks ordinary interrupt-driven execution on the current CPU.
///
/// This is deliberately separate from [`CpuPinGuard`]: excluding local IRQ
/// execution does not pin a task to a CPU, while an SMP CPU pin does not by
/// itself preserve the local interrupt-enable state.
#[derive(Debug)]
#[must_use]
pub struct LocalExecutionGuard {
    restore: unsafe fn(usize),
    saved_state: usize,
    _not_send_sync: PhantomData<*mut ()>,
}

impl LocalExecutionGuard {
    /// Construct a guard from an already-saved and already-disabled platform
    /// interrupt state.
    ///
    /// # Safety
    ///
    /// `restore(saved_state)` must restore exactly the state captured by the
    /// matching exclusion operation, and the guard must not cross a yield or
    /// CPU migration point.
    pub const unsafe fn new(saved_state: usize, restore: unsafe fn(usize)) -> Self {
        Self {
            restore,
            saved_state,
            _not_send_sync: PhantomData,
        }
    }
}

impl Drop for LocalExecutionGuard {
    fn drop(&mut self) {
        unsafe {
            (self.restore)(self.saved_state);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootArg(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysAddr(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaAddr(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaDirection {
    ToDevice,
    FromDevice,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ppn(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtAddr(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtRange {
    pub start: VirtAddr,
    pub size: usize,
}

impl VirtRange {
    pub const fn empty() -> Self {
        Self {
            start: VirtAddr(0),
            size: 0,
        }
    }

    pub const fn end(self) -> VirtAddr {
        VirtAddr(self.start.0 + self.size)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysRange {
    pub start: PhysAddr,
    pub size: usize,
}

impl PhysRange {
    pub const fn empty() -> Self {
        Self {
            start: PhysAddr(0),
            size: 0,
        }
    }

    pub const fn end(self) -> PhysAddr {
        PhysAddr(self.start.0 + self.size)
    }
}

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
    pub device_resources: &'static DeviceResourceGraph,
    pub timebase_frequency_hz: u64,
    pub possible_cpu_count: usize,
}

/// Firmware- or board-described device class published by the selected
/// platform.  The generic kernel consumes these facts to choose a driver; the
/// HAL does not own the resulting semantic device object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceKind {
    Uart,
    IntController,
    /// Firmware-described clock-controller register bank mapped for a
    /// platform device's clock/reset preparation hook.
    ClockController,
    /// Firmware-described outer-cache controller used for non-coherent DMA
    /// maintenance. Platforms without such a controller simply omit it.
    CacheController,
    /// QEMU's `google,goldfish-rtc` register model.
    ///
    /// Keep this backend-specific: a board-local RTC such as the JH7110 RTC
    /// is not register-compatible and must not be routed through Goldfish
    /// MMIO merely because both devices are clocks.
    GoldfishRtc,
    VirtioMmio,
    PciEcam,
    SdController,
    Dwmac,
}

/// One statically published platform-device fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub kind: DeviceKind,
    pub mmio: PhysRange,
    pub irq: Option<u32>,
    pub reg_shift: u8,
    pub reg_io_width: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchAuxvFacts {
    pub page_size: usize,
    pub hwcap: u64,
    pub hwcap2: u64,
    pub platform: &'static str,
}

impl ArchAuxvFacts {
    pub const fn new(page_size: usize, hwcap: u64, hwcap2: u64, platform: &'static str) -> Self {
        Self {
            page_size,
            hwcap,
            hwcap2,
            platform,
        }
    }
}

pub const RISCV_HWCAP_ISA_A: u64 = 1 << 0;
pub const RISCV_HWCAP_ISA_C: u64 = 1 << (b'C' - b'A');
pub const RISCV_HWCAP_ISA_D: u64 = 1 << (b'D' - b'A');
pub const RISCV_HWCAP_ISA_F: u64 = 1 << (b'F' - b'A');
pub const RISCV_HWCAP_ISA_I: u64 = 1 << (b'I' - b'A');
pub const RISCV_HWCAP_ISA_M: u64 = 1 << (b'M' - b'A');
pub const RISCV_HWCAP_IMAFDC: u64 = RISCV_HWCAP_ISA_I
    | RISCV_HWCAP_ISA_M
    | RISCV_HWCAP_ISA_A
    | RISCV_HWCAP_ISA_F
    | RISCV_HWCAP_ISA_D
    | RISCV_HWCAP_ISA_C;

/// Linux LoongArch auxv bit advertising usable unaligned loads and stores.
/// A platform may publish this when hardware accepts them directly or when
/// its user-trap path provides transparent emulation.
pub const LOONGARCH_HWCAP_UAL: u64 = 1 << 2;

pub trait PlatformConfig {
    const ARCH: Arch; // 架构 Riscv64/LoongArch64，必填
    const BOARD: &'static str; // 板名字符串，必填
    const SUBSTRATE_BOOT_READY: bool = false; // 启动是否已对接 substrate
    const PAGE_SIZE: usize = 4096; // 一页字节数（分页最小单位）
    const PAGE_SHIFT: usize = 12; // log2(页大小)，地址右移求页号
    const PHYS_ADDR_BITS: u8 = 0; // 物理地址位数（riscv 56）
    const VIRT_ADDR_BITS: u8 = 0; // 虚拟地址位数（riscv 39=Sv39）
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0); // 直连区起点，虚拟=物理+此值
    const DIRECT_MAP_SIZE: usize = 0; // 直连区大小（覆盖全物理内存，128GB）
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(0); // 内核代码虚拟基址（住最高处）
    /// Whether page-table-backed kernel mappings are accessible while the
    /// substrate initializes the heap. LA64 enables PGDH later.
    const KERNEL_PAGE_TABLE_ACTIVE_AT_SUBSTRATE_INIT: bool = false;
    const USER_TOP: VirtAddr = VirtAddr(0); // 用户地址天花板（riscv 256GB）
    const USER_RESERVED_TOP_SIZE: usize = 0; // 顶部保留、不给用户的大小（4MB）
    const USER_ALLOC_TOP: VirtAddr = Self::USER_TOP; // 用户可分配上限=天花板-保留（派生）
    const KERNEL_STACK_SIZE: usize = 0; // 内核栈大小（riscv 128KB）
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE; // 内核栈对齐（页对齐）
    const PAGE_TABLE_LEVELS: u8 = 0; // 页表层数（riscv Sv39 = 3 级）
    const ASID_BITS: u8 = 0; // ASID 位数，切进程免清整个 TLB（16）
    const CACHE_LINE_SIZE: usize = 0; // 缓存行字节数，防多核伪共享（64）
    const DMA_COHERENT: bool = false; // DMA 是否与缓存一致，false 需手动刷
}

pub trait BootPlatformIf {
    const BOOT_PROTOCOL: BootProtocol;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        BootHandoff {
            cpu_id: CpuId(cpu_id),
            firmware_arg: BootArg(firmware_arg),
            protocol: Self::BOOT_PROTOCOL,
        }
    }
}

pub trait InitIf {
    fn init_early(handoff: BootHandoff);
    fn init_later(handoff: BootHandoff);

    fn init_early_secondary(_cpu_id: CpuId) {}

    fn init_later_secondary(_cpu_id: CpuId) {}
}

pub trait BootInfoIf {
    fn boot_info() -> &'static BootInfo;
}

pub trait PlatformInfoIf {
    fn platform_info() -> &'static PlatformInfo;

    /// Make firmware-described platform controls (for example clocks and
    /// resets) usable before a concrete driver touches its device MMIO.
    fn prepare_platform_device(
        _device: &'static PlatformDevice,
    ) -> Result<(), PlatformDevicePrepareError> {
        Ok(())
    }

    /// Platform devices discovered before the heap becomes available.
    ///
    /// Boards without a firmware device table may keep the empty default and
    /// let their generic-device setup use its documented legacy fallback.
    fn devices() -> &'static [DeviceInfo] {
        &[]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformDevicePrepareError {
    MissingFirmwareNode,
    MalformedFirmwareProperty,
    UnsupportedProvider,
    ControlTimeout,
}

impl PlatformDevicePrepareError {
    pub const fn label(self) -> &'static str {
        match self {
            Self::MissingFirmwareNode => "missing-firmware-node",
            Self::MalformedFirmwareProperty => "malformed-firmware-property",
            Self::UnsupportedProvider => "unsupported-provider",
            Self::ControlTimeout => "control-timeout",
        }
    }
}

pub trait AuxvIf: PlatformConfig {
    fn arch_auxv_facts() -> ArchAuxvFacts {
        let platform = match Self::ARCH {
            Arch::Riscv64 => "riscv64",
            Arch::LoongArch64 => "loongarch64",
        };

        ArchAuxvFacts::new(Self::PAGE_SIZE, 0, 0, platform)
    }
}
pub trait ConsoleIf {
    fn write_bytes(bytes: &[u8]);

    fn read_bytes(_buf: &mut [u8]) -> usize {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocError {
    Exhausted,
}

pub type PtNodeAllocator = fn() -> Result<PtNode, AllocError>;
pub type PtNodeReleaser = unsafe fn(PhysAddr);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtNodeSourceKind {
    BootPool,
    TypedFrame,
}

#[derive(Clone, Copy, Debug)]
enum PtNodeSource {
    BootPool,
    TypedFrame(PtNodeReleaser),
}

impl PtNodeSource {
    const fn kind(self) -> PtNodeSourceKind {
        match self {
            Self::BootPool => PtNodeSourceKind::BootPool,
            Self::TypedFrame(_) => PtNodeSourceKind::TypedFrame,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PtNode {
    pub phys: PhysAddr,
    source: PtNodeSource,
}

impl PtNode {
    pub const fn boot_pool(phys: PhysAddr) -> Self {
        Self {
            phys,
            source: PtNodeSource::BootPool,
        }
    }

    pub const fn typed_frame(phys: PhysAddr, release: PtNodeReleaser) -> Self {
        Self {
            phys,
            source: PtNodeSource::TypedFrame(release),
        }
    }

    pub const fn source_kind(self) -> PtNodeSourceKind {
        self.source.kind()
    }

    /// Release typed-frame ownership carried by this pmap node.
    ///
    /// Returns `true` when the node was released through its typed-frame
    /// authority. Boot-pool nodes return `false` so the platform can return
    /// them to its static pool.
    ///
    /// # Safety
    ///
    /// The caller must ensure the page-table node is no longer reachable from a
    /// live page-table walk before invoking the release hook.
    pub unsafe fn release_typed_frame(self) -> bool {
        match self.source {
            PtNodeSource::BootPool => false,
            PtNodeSource::TypedFrame(release) => {
                unsafe { release(self.phys) };
                true
            }
        }
    }
}

impl PartialEq for PtNode {
    fn eq(&self, other: &Self) -> bool {
        self.phys == other.phys && self.source.kind() == other.source.kind()
    }
}

impl Eq for PtNode {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// pmap 操作的错误类型:非法请求 / 已映射冲突 / 内存耗尽 / 不支持
pub enum PmapError {
    InvalidRequest,
    AlreadyMapped,
    Exhausted,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 映射粒度:1GB 大页 / 2MB 大页 / 4KB 普通页(决定走几级 Sv39 页表)
pub enum PmapReserveKind {
    Superpage1G,
    Superpage2M,
    Page4K,
}

impl PmapReserveKind {
    pub const fn size(self) -> usize {
        match self {
            Self::Superpage1G => 1024 * 1024 * 1024,
            Self::Superpage2M => 2 * 1024 * 1024,
            Self::Page4K => 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 一页的访问权限位图(READ/WRITE/EXECUTE/USER/GLOBAL/DEVICE 及常用组合)
pub struct PmapPermissions {
    bits: u8,
}

impl PmapPermissions {
    pub const READ: Self = Self { bits: 1 << 0 };
    pub const WRITE: Self = Self { bits: 1 << 1 };
    pub const EXECUTE: Self = Self { bits: 1 << 2 };
    pub const USER: Self = Self { bits: 1 << 3 };
    pub const GLOBAL: Self = Self { bits: 1 << 4 };
    pub const DEVICE: Self = Self { bits: 1 << 5 };

    pub const KERNEL_RO: Self = Self {
        bits: Self::READ.bits | Self::GLOBAL.bits,
    };
    pub const KERNEL_RW: Self = Self {
        bits: Self::READ.bits | Self::WRITE.bits | Self::GLOBAL.bits,
    };
    pub const KERNEL_RX: Self = Self {
        bits: Self::READ.bits | Self::EXECUTE.bits | Self::GLOBAL.bits,
    };

    pub const fn from_bits(bits: u8) -> Self {
        Self { bits }
    }

    pub const fn bits(self) -> u8 {
        self.bits
    }

    pub const fn contains(self, flag: Self) -> bool {
        self.bits & flag.bits == flag.bits
    }

    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 预约时新分配的中间层页表节点(L2/L1/L0),挂在预约单上,提交前可回滚
pub struct PmapReservationIntermediates {
    pub l2: Option<PtNode>,
    pub l1: Option<PtNode>,
    pub l0: Option<PtNode>,
}

impl PmapReservationIntermediates {
    pub const fn empty() -> Self {
        Self {
            l2: None,
            l1: None,
            l0: None,
        }
    }
}

/// 一次"加映射"的预约单(reserve→commit 两阶段的载体):
/// 记着要映射的虚拟/物理地址、粒度、以及预分配好的中间节点。
/// `#[must_use]`:预约了必须 commit 或 rollback,不能丢弃(否则中间节点泄漏)。
#[derive(Debug, Eq, PartialEq)]
#[must_use]
pub struct PmapReservation {
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
    intermediates: PmapReservationIntermediates,
}

impl PmapReservation {
    pub const fn new(virt: VirtAddr, phys: PhysAddr, kind: PmapReserveKind) -> Self {
        Self {
            virt,
            phys,
            kind,
            intermediates: PmapReservationIntermediates::empty(),
        }
    }

    pub const fn new_with_intermediates(
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
        intermediates: PmapReservationIntermediates,
    ) -> Self {
        Self {
            virt,
            phys,
            kind,
            intermediates,
        }
    }

    pub const fn virt(&self) -> VirtAddr {
        self.virt
    }

    pub const fn phys(&self) -> PhysAddr {
        self.phys
    }

    pub const fn kind(&self) -> PmapReserveKind {
        self.kind
    }

    pub const fn intermediates(&self) -> PmapReservationIntermediates {
        self.intermediates
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 地址空间编号(TLB 优化:切进程不必清整个 TLB,不同 ASID 的翻译可共存)
pub struct Asid(pub u16);

/// 一个进程整套页表的入口 = 根页表物理页 + ASID。切进程 = 换它写进 satp。
/// `#[must_use]`:建出来必须激活或销毁。
#[derive(Debug)]
#[must_use]
pub struct PmapRoot {
    node: PtNode,
    asid: Asid,
}

impl PmapRoot {
    pub const fn new(node: PtNode, asid: Asid) -> Self {
        Self { node, asid }
    }

    pub const fn phys(&self) -> PhysAddr {
        self.node.phys
    }

    pub const fn asid(&self) -> Asid {
        self.asid
    }

    pub const fn node(&self) -> PtNode {
        self.node
    }

    pub fn into_node(self) -> PtNode {
        self.node
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// TLB 失效凭据:"这段虚拟地址的旧翻译作废了",交给 shootdown 去刷 TLB
pub struct PmapInvalidation {
    virt: VirtAddr,
    size: usize,
}

impl PmapInvalidation {
    pub const fn new(virt: VirtAddr, size: usize) -> Self {
        Self { virt, size }
    }

    pub const fn virt(self) -> VirtAddr {
        self.virt
    }

    pub const fn size(self) -> usize {
        self.size
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 删映射的结果:被删的虚拟/物理地址、粒度,外加一个 TLB 失效凭据。
/// 物理地址供上层把这页归还给页分配器。
pub struct PmapUnmapResult {
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
    invalidation: PmapInvalidation,
}

impl PmapUnmapResult {
    pub const fn new(virt: VirtAddr, phys: PhysAddr, kind: PmapReserveKind) -> Self {
        Self {
            virt,
            phys,
            kind,
            invalidation: PmapInvalidation::new(virt, kind.size()),
        }
    }

    pub const fn virt(self) -> VirtAddr {
        self.virt
    }

    pub const fn phys(self) -> PhysAddr {
        self.phys
    }

    pub const fn base_ppn(self) -> Ppn {
        Ppn(self.phys.0 / 4096)
    }

    pub const fn kind(self) -> PmapReserveKind {
        self.kind
    }

    pub const fn page_count(self) -> usize {
        self.kind.size() / 4096
    }

    pub const fn invalidation(self) -> PmapInvalidation {
        self.invalidation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 引导页表的自述事实(启动时发布,substrate 建堆时读):
/// 根表地址、已映射物理范围、直接映射区、内核镜像、临时恒等桥、页表节点池、保留的页表页。
pub struct BootstrapPmapInfo {
    pub root: PhysAddr,
    pub mapped: PhysRange,
    pub direct_map_base: VirtAddr,
    pub direct_map: VirtRange,
    pub kernel_image: VirtRange,
    pub identity: Option<VirtRange>,
    pub pt_node_pool: PhysRange,
    pub reserved_page_tables: &'static [PhysRange],
}

/// Local cadence state for waits which must make lock-free TLB progress.
///
/// The first failed wait runs the supplied progress operation. Later calls run
/// it at attempts 64, 128, and so on, while retaining a processor spin hint on
/// every attempt. Keeping this pure mechanism in HAL lets a board use it
/// without depending upward on substrate.
#[derive(Clone, Copy, Debug, Default)]
pub struct TlbProgressSpinWait {
    failed_attempts: usize,
}

impl TlbProgressSpinWait {
    const CADENCE: usize = 64;

    pub const fn new() -> Self {
        Self { failed_attempts: 0 }
    }

    /// Record one failed wait and periodically run `progress`.
    ///
    /// `progress` must not allocate, block, or acquire the resource being
    /// waited on.
    #[inline(always)]
    pub fn spin_with<F>(&mut self, mut progress: F)
    where
        F: FnMut(),
    {
        self.failed_attempts = self.failed_attempts.wrapping_add(1);
        if self.failed_attempts == 1 || self.failed_attempts & (Self::CADENCE - 1) == 0 {
            progress();
        }
        core::hint::spin_loop();
    }
}

/// 虚拟内存/页表子系统的接口:让通用内核建/改/删地址空间映射,不碰架构页表格式。
/// HAL 最大的 trait;fork/mmap/exec/缺页/切进程全靠它。方法几乎都有默认实现
/// (返回 Unsupported 或空),mock/测试板不实现也能链接,真板覆盖。约分五组(见下)。
pub trait PmapIf {
    // ===== 组1:页表节点分配(建页表要内存,来源是 boot_static 的 pt_node 池) =====
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        // 引导页表自述事实
        None
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        // 要一页当页表节点
        Err(AllocError::Exhausted)
    }

    fn free_pt_node(_node: PtNode) {} // 还回去

    fn install_pt_node_allocator(_allocator: PtNodeAllocator) -> Result<(), PmapError> {
        // 装 substrate 的正式分配器
        Err(PmapError::Unsupported)
    }

    // ===== 组2:内核空间映射(操作全局唯一的内核页表,所有进程共享的那半) =====
    // reserve→commit 两阶段:reserve 干会失败的分配,commit 干不会失败的写入;
    // 下面 direct_map 是直接映射区专用,extend_direct_map 供 substrate 建堆时扩展。
    fn reserve_kernel_direct_map_1g(_phys: PhysAddr) -> Result<Option<PmapReservation>, PmapError> {
        Err(PmapError::Unsupported)
    }

    fn commit_kernel_direct_map_1g(_reservation: PmapReservation) {}

    fn extend_direct_map(_phys_end: PhysAddr) -> Result<(), PmapError> {
        Err(PmapError::Unsupported)
    }

    fn reserve_kernel_mapping(
        _virt: VirtAddr,
        _phys: PhysAddr,
        _kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Err(PmapError::Unsupported)
    }

    fn rollback_kernel_mapping(_reservation: PmapReservation) {}

    fn commit_kernel_mapping(_reservation: PmapReservation, _permissions: PmapPermissions) {}

    /// Publish a mapping into a leaf slot that was previously unmapped.
    ///
    /// Unlike `commit_kernel_mapping`, implementations may omit the immediate
    /// per-leaf TLB synchronization because the reservation proved that no old
    /// valid mapping existed. The caller must issue one architecture-appropriate
    /// publication fence/shootdown after completing the whole new range. The
    /// default preserves the conservative legacy behaviour.
    fn commit_new_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
        Self::commit_kernel_mapping(reservation, permissions);
    }

    fn unmap_kernel_mapping(
        _virt: VirtAddr,
        _kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Err(PmapError::Unsupported)
    }

    fn protect_kernel_mapping(
        _virt: VirtAddr,
        _kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        Err(PmapError::Unsupported)
    }

    fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {}

    fn shootdown_kernel_mappings(invalidations: &[PmapInvalidation]) {
        for invalidation in invalidations {
            Self::shootdown_kernel_mapping(*invalidation);
        }
    }

    /// Make progress on a remote TLB invalidation addressed to the current CPU.
    ///
    /// Most platforms can rely on their architectural interrupt/firmware
    /// machinery and keep this as a no-op. Platforms whose shootdown transport
    /// is a maskable supervisor interrupt may override it so lock-contention
    /// paths can service a pending invalidation while ordinary interrupts are
    /// masked. The implementation must not acquire VM or heap locks.
    fn service_pending_tlb_shootdown() {}

    // ===== 组3:用户地址空间的创建/销毁/激活(fork/exec/切进程的核心) =====
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        // 建一个新进程的页表根
        Err(PmapError::Unsupported)
    }

    fn destroy_pmap_root(_root: PmapRoot) {} // 销毁

    /// 激活一个用户地址空间根(面向 VM 的别名,默认转调 activate_user_pmap)。
    ///
    /// `PmapRoot` 有意做成架构自定义:RV64 板可以让它是包含用户半+拷贝内核半的完整根;
    /// LA64 板可以让它是每进程的 PGDL,而内核映射放在板级全局的 PGDH。
    fn activate_pmap(root: &PmapRoot) -> Result<(), PmapError> {
        Self::activate_user_pmap(root);
        Ok(())
    }

    // ===== 组4:用户空间映射(针对某个进程 root,与组2内核版镜像,只多了 root 参数) =====
    // mmap 走 reserve_mapping + commit_mapping。
    fn reserve_mapping(
        _root: &PmapRoot,
        _virt: VirtAddr,
        _phys: PhysAddr,
        _kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Err(PmapError::Unsupported)
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
        _virt: VirtAddr,
        _kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Err(PmapError::Unsupported)
    }

    fn protect_mapping(
        _root: &PmapRoot,
        _virt: VirtAddr,
        _kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        Err(PmapError::Unsupported)
    }

    // ===== 组5:用户映射的 TLB shootdown(带 asid,只刷该地址空间的 TLB 项) =====
    fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {}

    fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        // 批量版
        for invalidation in invalidations {
            Self::shootdown_mapping(asid, *invalidation);
        }
    }

    /// Complete publication of newly-valid user leaves on the current hart.
    ///
    /// A new mapping does not replace a valid translation, so remote harts do
    /// not need an eager shootdown: a hart that retained a failed walk will
    /// trap and synchronize before retrying. The publishing/faulting hart must
    /// nevertheless cross the architecture's local translation barrier.
    ///
    /// Platforms may override this with a local-only batch operation. The
    /// conservative default uses the ordinary shootdown surface.
    fn synchronize_new_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        Self::shootdown_mappings(asid, invalidations);
    }

    /// 把 `root` 激活为当前 hart 的用户页表。
    ///
    /// RV64 上就是 `csrw satp, ((root.phys >> 12) | Sv39 模式位) + sfence.vma`。
    /// LA64 上写 ASID/PGDL/PGDH 状态:`root` 是用户 PGDL,内核映射在板级全局 PGDH。
    ///
    /// 线程运行时在 `TrapIf::enter_userspace_with_context` 之前紧接着调它,
    /// 好让 MMU 在接下来的用户取指/取数时查这个进程自己的页表。
    /// 不调的话 satp 一直指着内核引导根(没有用户映射),用户态每条取指都缺页。
    fn activate_user_pmap(_root: &PmapRoot) {
        // 默认空实现,好让 host 平台能链接;产品板覆盖它。
    }
}

pub mod observer;
pub use observer::{ObserverIf, RingDescriptor};
pub mod pmap;
pub mod trap;
pub use trap::{
    FaultInfo, KernelTrapSink, SignalHandlerRegs, TrapAction, TrapClass, TrapFrameMut,
    TrapFrameMutVtable, TrapFrameSnapshot, TrapFrameView, TrapIf, TrapPreviousMode, TrapSnapshot,
    UserFpContext, UserTrapContext,
};

// ---------------------------------------------------------------------------
// Pod — plain-old-data marker trait
// ---------------------------------------------------------------------------

/// Marker trait for types that are safe to read/write as raw bytes.
///
/// # Safety
///
/// Any bit pattern must be a valid, initialized value of `Self`.  This is
/// required so the byte-level `copy_from_user` / `copy_to_user` paths can
/// produce a well-formed `T` after copying raw bytes from user space.
pub unsafe trait Pod: Copy + 'static {}

unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for u128 {}
unsafe impl Pod for i8 {}
unsafe impl Pod for i16 {}
unsafe impl Pod for i32 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for i128 {}
unsafe impl Pod for usize {}
unsafe impl Pod for isize {}
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}
unsafe impl Pod for UserFpContext {}
unsafe impl Pod for UserTrapContext {}

// ---------------------------------------------------------------------------
// UserPtr<T> — typed user-VA wrapper
// ---------------------------------------------------------------------------

/// A typed pointer into the current process's user virtual address space.
///
/// Prevents accidental use of kernel addresses where user addresses are
/// expected.  The contained address is interpreted against the currently
/// installed user page table.
#[repr(transparent)]
pub struct UserPtr<T>(*mut T);

impl<T> UserPtr<T> {
    /// Construct a `UserPtr` from a raw user virtual address.
    pub fn new(addr: usize) -> Self {
        Self(addr as *mut T)
    }

    pub fn addr(self) -> usize {
        self.0 as usize
    }

    pub fn as_ptr(self) -> *mut T {
        self.0
    }
}

impl<T> Copy for UserPtr<T> {}
impl<T> Clone for UserPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> core::fmt::Debug for UserPtr<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("UserPtr").field(&(self.0 as usize)).finish()
    }
}

impl<T> PartialEq for UserPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> Eq for UserPtr<T> {}

unsafe impl<T: Send> Send for UserPtr<T> {}
unsafe impl<T: Sync> Sync for UserPtr<T> {}

// ---------------------------------------------------------------------------
// SignalFrameIf
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSigInfoAbi {
    pub bytes: [u8; 128],
}

impl UserSigInfoAbi {
    pub const ZERO: Self = Self { bytes: [0; 128] };
}

unsafe impl Pod for UserSigInfoAbi {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSignalMaskAbi {
    pub bits: u64,
}

impl UserSignalMaskAbi {
    pub const EMPTY: Self = Self { bits: 0 };
}

unsafe impl Pod for UserSignalMaskAbi {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSaFlagsAbi {
    pub bits: u64,
}

impl UserSaFlagsAbi {
    pub const EMPTY: Self = Self { bits: 0 };
}

unsafe impl Pod for UserSaFlagsAbi {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalFrameWrite {
    pub stack_top: UserPtr<u8>,
    pub sig_no: u32,
    pub siginfo: UserSigInfoAbi,
    pub old_mask: UserSignalMaskAbi,
    pub flags: UserSaFlagsAbi,
    pub handler_pc: UserPtr<()>,
    pub restorer_pc: UserPtr<()>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalFramePlacement {
    pub frame_addr: UserPtr<()>,
    pub trampoline_pc: UserPtr<()>,
}

/// Raw bytes of a signal frame (platform-specific layout).
/// Carried from `prepare_signal_frame` to the caller, who writes
/// them to the user stack via `AddressSpace::copy_to_user`.
///
/// The buffer must be at least as large as the platform-specific
/// `*SignalFrame` struct (RV64 ~720 bytes including UserTrapContext +
/// FpContext + trampoline). The previous 512-byte buffer silently
/// truncated `from_slice`, dropping the trailing fields — most
/// catastrophically the on-stack `rt_sigreturn` trampoline at
/// `offset_of!(SignalFrame, trampoline) = 712` — so the handler
/// returned through `ra = frame_addr + 712` and the CPU fetched
/// uninitialised stack bytes instead of the trampoline. Musl-compatible
/// RV64 `ucontext_t` now includes the full floating-point union, so
/// keep the carrier above the board frame sizes rather than trimming
/// the userspace ABI shape.
pub struct SignalFrameBytes {
    pub data: [u8; Self::CAPACITY],
    pub len: usize,
}

impl SignalFrameBytes {
    // LA64's full LASX register file extends UserFpContext to 1,312 bytes.
    // RV64 keeps that opaque context in its private validation header in
    // addition to the Linux-compatible ucontext, making Rv64SignalFrame 2,464
    // bytes. Keep one page here so both board layouts fit without truncation.
    pub const CAPACITY: usize = 4096;

    pub fn from_slice(bytes: &[u8]) -> Self {
        let len = bytes.len();
        assert!(
            len <= Self::CAPACITY,
            "signal frame layout ({len} bytes) exceeds SignalFrameBytes buffer ({})",
            Self::CAPACITY,
        );
        let mut data = [0u8; Self::CAPACITY];
        data[..len].copy_from_slice(bytes);
        Self { data, len }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SavedSignalFrame {
    pub saved_mask: UserSignalMaskAbi,
    pub user_context: UserTrapContext,
}

unsafe impl Pod for SavedSignalFrame {}

pub trait SignalFrameIf: TrapIf {
    fn write_signal_frame(
        tf: TrapFrameMut<'_>,
        setup: SignalFrameWrite,
    ) -> Result<SignalFramePlacement, FaultInfo> {
        let _ = tf;
        Err(FaultInfo {
            address: VirtAddr(setup.stack_top.addr()),
            write: true,
            instruction: false,
            from_user: false,
        })
    }

    fn read_signal_frame(user_sp: UserPtr<u8>) -> Result<SavedSignalFrame, FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(user_sp.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }

    fn signal_frame_size() -> usize {
        0
    }

    fn decode_signal_frame_bytes(
        user_sp: UserPtr<u8>,
        _bytes: &[u8],
    ) -> Result<SavedSignalFrame, FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(user_sp.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }

    fn restore_signal_frame(_tf: TrapFrameMut<'_>, _frame: &SavedSignalFrame) {}

    fn rewind_syscall_pc(mut tf: TrapFrameMut<'_>) {
        tf.rewind_pc(4);
    }

    /// Build a signal-handler entry context and frame bytes
    /// WITHOUT accessing a live TrapFrameMut. Returns the modified
    /// `UserTrapContext` (sepc=handler, sp=frame_addr, ra=trampoline)
    /// and the raw signal frame bytes to write to the user stack.
    ///
    /// Used by the thread-future AST checkpoint, which runs before
    /// `enter_userspace_with_context` (where TrapFrameMut is
    /// available). The caller writes `frame_bytes` to user memory
    /// via `AddressSpace::copy_to_user`, then stores the modified
    /// context as `saved_user_context`.
    ///
    /// Default: returns `ENOSYS`-shaped fallback.
    fn prepare_signal_frame(
        _ctx: &UserTrapContext,
        _setup: &SignalFrameWrite,
    ) -> Result<(UserTrapContext, SignalFrameBytes), FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(0),
            write: true,
            instruction: false,
            from_user: false,
        })
    }
}

pub trait FpSimdIf {
    const SUPPORTED: bool;

    type State: Default;

    fn init_state() -> Self::State;

    fn enable_for_current();

    fn disable_for_current();

    fn save(state: &mut Self::State);

    fn restore(state: &Self::State);
}
/// Platform-supplied entropy. Used to seed the AT_RANDOM auxv
/// region at exec time (`build_initial_user_stack` consumes
/// `AuxvFacts.at_random_bytes`; the exec front-end fills it via
/// `<P as EntropyIf>::fill_random`).
///
/// The default impl produces a deterministic boot-counter seed —
/// safe for txKernel's current trust model (no untrusted input,
/// no ASLR, no userspace-visible PRF stretching). Real platforms
/// override with hardware entropy: RV64 boards may use the Zkr
/// `seed` CSR or `mtime`; future platforms may use virtio-rng or
/// platform-specific RNG MMIO.
///
/// The contract is "always succeed". Implementations that talk to
/// hardware must fall back to the deterministic counter when the
/// entropy source is unavailable.
///
/// Cites: `txdoc:HAL-V1`.
pub trait EntropyIf {
    /// Fill `out` with random bytes. Must always succeed; on
    /// hardware-entropy unavailability, fall back to the
    /// deterministic counter seed.
    fn fill_random(out: &mut [u8]) {
        // Default: deterministic boot-counter seed. Mix the
        // counter through a tiny xorshift64 to spread bits.
        // Safe for the current trust model — no ASLR, no
        // stack-canary checks, no untrusted input.
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0xDEAD_BEEF_CAFE_F00D);
        let mut s = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Avoid the all-zero xorshift fixed point.
        if s == 0 {
            s = 0xDEAD_BEEF_CAFE_F00D;
        }
        for byte in out.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *byte = (s & 0xff) as u8;
        }
    }
}

/// The instruction or architectural source a vDSO may read without entering
/// the kernel. `None` keeps the vDSO on its syscall fallback path.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VdsoCounterMode {
    None = 0,
    RiscvTime = 1,
}

/// Static platform facts for a raw counter that can back vDSO time reads.
///
/// This is intentionally a value returned by the selected board type rather
/// than a runtime HAL service. The timekeeper validates eligibility before it
/// reads the raw counter or publishes a fast-path calibration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VdsoCounterInfo {
    pub frequency_hz: u64,
    pub mask: u64,
    pub stable: bool,
    pub user_readable: bool,
    pub mode: VdsoCounterMode,
}

pub trait MonotonicCounterIf {
    /// Read monotonic nanoseconds since the platform's boot-time epoch.
    ///
    /// Values must be non-decreasing on the current hart and cheap enough for
    /// scheduler/reactor hot paths.
    fn read_ns() -> u64;

    /// Return the hardware timer frequency used for ns/tick conversion.
    fn frequency_hz() -> u64;

    /// Describe the raw counter for the optional vDSO fast path.
    ///
    /// Platforms that do not expose a stable user-readable counter retain the
    /// default and the vDSO uses its syscall fallback.
    fn vdso_counter_info() -> Option<VdsoCounterInfo> {
        None
    }

    /// Read the same raw counter described by [`Self::vdso_counter_info`].
    ///
    /// Callers must only use this after accepting the descriptor. The default
    /// avoids imposing an architecture-specific counter read on other boards.
    fn read_vdso_counter() -> u64 {
        0
    }
}

pub trait DeadlineTimerIf {
    /// Program the current hart's timer for an absolute monotonic deadline.
    ///
    /// `deadline` uses the same nanosecond epoch as
    /// `MonotonicCounterIf::read_ns()`. Platforms must not intentionally arm an
    /// earlier hardware deadline than requested; interrupts may arrive late due
    /// to firmware, hardware, or emulator latency. A past deadline should fire
    /// as soon as the platform can arrange.
    fn set_deadline_ns(deadline: u64);

    /// Cancel the current hart's pending timer deadline when the platform has
    /// a cancellation mechanism.
    fn cancel_deadline();

    /// Prepare the current hart so a programmed timer deadline can wake or
    /// trap out of the platform idle path.
    fn enable_timer_wakeups() {}
}

/// Compatibility view used by subsystems that still consume monotonic time and
/// per-hart deadlines as one capability.
///
/// Platforms continue to implement the split mainline interfaces. This blanket
/// bridge keeps the final-SMP syscall and wait paths source-compatible without
/// restoring a second platform-time implementation.
pub trait TimeIf {
    fn read_ns() -> u64;
    fn set_deadline_ns(deadline: u64);
    fn cancel_deadline();
    fn frequency_hz() -> u64;
}

impl<T> TimeIf for T
where
    T: MonotonicCounterIf + DeadlineTimerIf,
{
    fn read_ns() -> u64 {
        <T as MonotonicCounterIf>::read_ns()
    }

    fn set_deadline_ns(deadline: u64) {
        <T as DeadlineTimerIf>::set_deadline_ns(deadline);
    }

    fn cancel_deadline() {
        <T as DeadlineTimerIf>::cancel_deadline();
    }

    fn frequency_hz() -> u64 {
        <T as MonotonicCounterIf>::frequency_hz()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistentClockError {
    Unsupported,
    Invalid,
    Range,
    Hardware,
}

pub trait PersistentClockIf {
    /// Read persistent realtime in nanoseconds since the Unix epoch.
    ///
    /// This is RTC/firmware wall-clock capability, not the hot
    /// `CLOCK_REALTIME` path. Platforms without persistent wall-clock hardware
    /// should return [`PersistentClockError::Unsupported`].
    fn read_realtime_ns() -> Result<u64, PersistentClockError> {
        Err(PersistentClockError::Unsupported)
    }

    /// Set persistent realtime in nanoseconds since the Unix epoch.
    fn set_realtime_ns(_ns: u64) -> Result<(), PersistentClockError> {
        Err(PersistentClockError::Unsupported)
    }

    /// Program a persistent-clock wake alarm when the platform supports one.
    fn set_wake_alarm_ns(_ns: u64) -> Result<(), PersistentClockError> {
        Err(PersistentClockError::Unsupported)
    }

    /// Clear a persistent-clock wake alarm when the platform supports one.
    fn clear_wake_alarm() -> Result<(), PersistentClockError> {
        Err(PersistentClockError::Unsupported)
    }

    /// Acknowledge a persistent-clock wake-alarm interrupt after the platform
    /// IRQ dispatcher has identified the RTC source.
    ///
    /// This is a hardware acknowledgement hook. It must not publish devfs
    /// events, inspect userspace RTC state, or route scheduler wakes.
    fn acknowledge_wake_alarm_irq() -> Result<(), PersistentClockError> {
        Ok(())
    }
}

pub trait PercpuIf {
    fn current_cpu_id() -> CpuId {
        CpuId(0)
    }

    fn install_early_percpu(_cpu_id: CpuId) {}

    fn read_kernel_tls() -> u64 {
        0
    }

    fn write_kernel_tls(_value: u64) {}

    /// Install a new kernel stack pointer for the current hart.
    ///
    /// # Safety
    ///
    /// The caller must ensure `top` points to a valid kernel stack and that no
    /// live stack references from the old stack are used after this call.
    unsafe fn install_kernel_stack(_top: VirtAddr) {}

    fn pin_current_cpu() -> CpuPinGuard {
        CpuPinGuard::new(Self::current_cpu_id())
    }

    /// Pin the current CPU and attach a bounded semantic reason for platform
    /// diagnostics. Platforms that need the reason at acquisition time may
    /// override this method; existing platform pins inherit it automatically.
    fn pin_current_cpu_for(reason: CpuPinReason) -> CpuPinGuard {
        Self::pin_current_cpu().with_reason(reason)
    }

    /// Current nesting depth of platform CPU pins.
    ///
    /// A non-zero value means the current execution context must not migrate
    /// to another hart. Platforms with a non-preemptive kernel may implement
    /// this as a checked per-hart nesting counter instead of masking IRQs.
    fn cpu_pin_depth() -> usize {
        0
    }
}
pub trait CacheIf {
    fn fence_all() {}

    fn fence_i_local() {}

    fn fence_i_all() {
        Self::fence_i_local();
    }

    fn flush_icache_range(_start: VirtAddr, _len: usize) {
        Self::fence_i_local();
    }

    fn dcache_clean_range(_start: PhysAddr, _len: usize) {}

    fn dcache_invalidate_range(_start: PhysAddr, _len: usize) {}

    fn dcache_clean_invalidate_range(_start: PhysAddr, _len: usize) {}
}

pub trait DmaIf: PlatformConfig {
    const DMA_COHERENT: bool = <Self as PlatformConfig>::DMA_COHERENT;

    fn phys_to_dma(paddr: PhysAddr) -> DmaAddr {
        DmaAddr(paddr.0 as u64)
    }

    fn dma_to_phys(daddr: DmaAddr) -> PhysAddr {
        PhysAddr(daddr.0 as usize)
    }

    fn sync_for_device(_paddr: PhysAddr, _len: usize, _dir: DmaDirection) {}

    fn sync_for_cpu(_paddr: PhysAddr, _len: usize, _dir: DmaDirection) {}

    /// Order descriptor/buffer publication before a following device MMIO
    /// notification such as a DMA tail-pointer write.
    ///
    /// The default is sufficient for coherent host mocks. Real platforms
    /// whose architecture distinguishes normal-memory and device-I/O ordering
    /// must override this with the architecture's DMA/MMIO write barrier.
    fn publish_to_device() {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    }
}

pub type SecondaryEntry = unsafe extern "C" fn(cpu_id: usize) -> !;

/// Opaque platform state captured while entering an interrupt-wait window.
///
/// The kernel must pass the value returned by
/// [`SmpIf::prepare_interrupt_wait`] exactly once to either
/// [`SmpIf::cancel_interrupt_wait`] or
/// [`SmpIf::wait_for_interrupt_prepared`].  Its raw contents are private to
/// the selected platform.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "a prepared interrupt wait must be cancelled or committed"]
pub struct InterruptWaitState(usize);

impl InterruptWaitState {
    /// Construct platform-private wait state.
    ///
    /// Board crates use this to preserve the architecture interrupt-enable
    /// state that must be restored when the wait window closes.
    pub const fn from_raw(raw: usize) -> Self {
        Self(raw)
    }

    /// Return the platform-private raw state.
    pub const fn raw(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpiKind {
    Reschedule,
    TlbShootdown,
    /// Memory barrier IPI — used by `membarrier(2)` to force every
    /// hart through a `fence` sequence so that all prior memory
    /// operations are globally visible.
    Membarrier,
    /// Run deferred cross-CPU maintenance work such as RCU/EBR quiescence.
    Maintenance,
    Stop,
}

pub trait SmpIf {
    fn current_cpu_id() -> CpuId {
        CpuId(0)
    }

    fn possible_cpus() -> CpuMask {
        CpuMask::single(CpuId(0))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::single(CpuId(0))
    }

    fn possible_cpu_count() -> usize {
        Self::possible_cpus().count()
    }

    fn online_cpu_count() -> usize {
        Self::online_cpus().count()
    }

    fn is_cpu_online(cpu: CpuId) -> bool {
        Self::online_cpus().contains(cpu)
    }

    fn mark_cpu_online(_cpu: CpuId) {}

    /// Withdraw the current CPU from synchronous cross-CPU work before it is
    /// permanently parked.
    ///
    /// Platforms with a maskable-IPI TLB shootdown transport use this hook to
    /// stop new target acquisitions, drain requests which were already
    /// acquired by senders, and only then publish the CPU offline. The method
    /// runs in normal kernel context and must return with no future
    /// synchronous request able to wait on this CPU.
    fn prepare_cpu_offline() {}

    fn boot_secondary_cpus(_entry: SecondaryEntry) -> usize {
        0
    }

    fn enable_ipi_wakeups() {}

    fn wait_for_interrupt_once() {
        core::hint::spin_loop();
    }

    /// Start the race-free half of an idle transition.
    ///
    /// The caller performs its final runnable-work check after this method
    /// returns. Platforms whose interrupt architecture permits it should
    /// defer interrupt delivery until the matching cancel/commit operation,
    /// while keeping wake sources pending. This closes the classic
    /// check-empty -> interrupt-arrives -> handler-clears -> WFI lost-wake
    /// window.
    fn prepare_interrupt_wait() -> InterruptWaitState {
        InterruptWaitState::from_raw(0)
    }

    /// Abort a prepared idle transition because the final work check found
    /// runnable work.
    fn cancel_interrupt_wait(_state: InterruptWaitState) {}

    /// Commit a prepared idle transition, wait once, and restore the
    /// interrupt state captured by [`Self::prepare_interrupt_wait`].
    fn wait_for_interrupt_prepared(_state: InterruptWaitState) {
        Self::wait_for_interrupt_once();
    }

    fn pending_ipi(_kind: IpiKind) -> bool {
        false
    }

    fn park_this_cpu() -> ! {
        loop {
            Self::wait_for_interrupt_once();
        }
    }

    /// Permanently stop the current CPU after it has left all shared runtime
    /// code.
    ///
    /// Unlike [`Self::park_this_cpu`], this is a shutdown primitive: platform
    /// implementations must prevent timer/device/IPI handlers from re-entering
    /// kernel services after this call.  The default is sufficient for
    /// single-CPU/test platforms that never call the SMP shutdown path.
    fn quiesce_this_cpu() -> ! {
        Self::park_this_cpu()
    }

    fn send_ipi(target: CpuId, _kind: IpiKind) {
        assert_eq!(target, Self::current_cpu_id());
    }

    fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
        if !mask.is_empty() {
            Self::send_ipi(Self::current_cpu_id(), kind);
        }
    }

    fn ack_ipi(_kind: IpiKind) {}

    fn clear_ipi_ack_cpus(_kind: IpiKind, _mask: CpuMask) {}

    fn ipi_ack_cpus(_kind: IpiKind) -> CpuMask {
        CpuMask::EMPTY
    }

    fn wait_for_ipi_ack_cpus(mask: CpuMask, kind: IpiKind) -> usize {
        for _ in 0..100_000 {
            let acked = Self::ipi_ack_cpus(kind);
            if (acked.bits() & mask.bits()) == mask.bits() {
                return mask.count();
            }
            core::hint::spin_loop();
        }
        (Self::ipi_ack_cpus(kind).bits() & mask.bits()).count_ones() as usize
    }
}
pub trait PowerIf {
    fn system_off() -> !;
}

pub trait TxPlatform:
    PlatformConfig
    + BootPlatformIf
    + InitIf
    + BootInfoIf
    + PlatformInfoIf
    + AuxvIf
    + ConsoleIf
    + PmapIf
    + TrapIf
    + SignalFrameIf
    + IrqIf
    + MonotonicCounterIf
    + DeadlineTimerIf
    + PersistentClockIf
    + PercpuIf
    + CacheIf
    + DmaIf
    + SmpIf
    + PowerIf
    + EntropyIf
    + ObserverIf
    + 'static
{
}

impl<T> TxPlatform for T where
    T: PlatformConfig
        + BootPlatformIf
        + InitIf
        + BootInfoIf
        + PlatformInfoIf
        + AuxvIf
        + ConsoleIf
        + PmapIf
        + TrapIf
        + SignalFrameIf
        + IrqIf
        + MonotonicCounterIf
        + DeadlineTimerIf
        + PersistentClockIf
        + PercpuIf
        + CacheIf
        + DmaIf
        + SmpIf
        + PowerIf
        + EntropyIf
        + ObserverIf
        + 'static
{
}

pub trait KernelMain<P: TxPlatform> {
    fn kernel_main(handoff: BootHandoff) -> !;
}

pub fn console_write_bytes<P: ConsoleIf>(bytes: &[u8]) {
    P::write_bytes(bytes);
}

pub fn console_read_bytes<P: ConsoleIf>(buf: &mut [u8]) -> usize {
    P::read_bytes(buf)
}

pub fn console_write_str<P: ConsoleIf>(message: &str) {
    P::write_bytes(message.as_bytes());
}

pub fn entry<P, K>(cpu_id: usize, firmware_arg: usize) -> !
where
    P: TxPlatform,
    K: KernelMain<P>,
{
    P::install_minimal_trap_vector();
    let handoff = P::boot_handoff(cpu_id, firmware_arg);
    P::install_early_percpu(handoff.cpu_id);
    P::mark_cpu_online(handoff.cpu_id);
    K::kernel_main(handoff)
}
