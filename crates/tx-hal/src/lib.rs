#![no_std]

#[cfg_attr(not(test), allow(unused_extern_crates))]
extern crate alloc;

pub mod hart_local;
pub mod time;

pub use hart_local::{HartLocal, MAX_HARTS};

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
    unpin: Option<fn(CpuId)>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl CpuPinGuard {
    pub const fn new(cpu_id: CpuId) -> Self {
        Self {
            cpu_id,
            unpin: None,
            _not_send_sync: PhantomData,
        }
    }

    /// Construct a platform-backed CPU pin. The platform must have already
    /// entered its non-migratable section. `unpin` leaves that section when
    /// the guard drops.
    pub const fn with_unpin(cpu_id: CpuId, unpin: fn(CpuId)) -> Self {
        Self {
            cpu_id,
            unpin: Some(unpin),
            _not_send_sync: PhantomData,
        }
    }

    pub const fn cpu_id(&self) -> CpuId {
        self.cpu_id
    }
}

impl Drop for CpuPinGuard {
    fn drop(&mut self) {
        if let Some(unpin) = self.unpin {
            unpin(self.cpu_id);
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

/// 平台编译期常量表：默认值是占位（多为 0），真值由每块板的 impl 覆盖。
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

/// 读取端:取"内核初始化信息"(内存地图/命令行/initrd/内核镜像)。
/// 数据由 boot_handoff 开机时发布,这里只负责取出,启动后随时可读。
pub trait BootInfoIf {
    fn boot_info() -> &'static BootInfo;
}

/// 读取端:取"平台硬件信息"(设备寄存器窗口/时钟频率/CPU数/设备列表)。
pub trait PlatformInfoIf {
    fn platform_info() -> &'static PlatformInfo;

    /// 开机发现的设备列表(通常来自固件设备树)。默认空,mock/测试板免接线;
    /// 有设备发现的真板覆盖它返回真实列表。
    fn devices() -> &'static [DeviceInfo] {
        &[]
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

mod platform_info;
pub use platform_info::*;

mod signal;
pub use signal::*;

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

pub trait IrqIf {
    const MAX_IRQ: u32 = 0;

    /// Platform-specific IRQ number for the boot console UART.
    ///
    /// The kernel's `install_irq_handlers` reads this through
    /// `<P as IrqIf>::UART_IRQ` to register the UART RX dispatcher
    /// without naming a board constant directly. Boards that have no
    /// dedicated UART IRQ (or run on a host-only test platform) keep
    /// the `0` sentinel default; production boards override.
    /// See `docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`
    /// §"Open questions #6".
    const UART_IRQ: u32 = 0;

    /// Runtime UART IRQ number. Defaults to the static constant;
    /// boards with device-tree discovery override this to serve the
    /// probed value (QEMU virt wires the UART at 10, VisionFive 2 at
    /// 32 — same kernel, different trees).
    fn uart_irq() -> u32 {
        Self::UART_IRQ
    }

    /// Platform-specific IRQ number for the boot virtio-net device
    /// (`0` sentinel = no net IRQ wired; the net delegate then relies on
    /// poll kicks alone). On QEMU rv64 virt, virtio-mmio slot N maps to
    /// PLIC IRQ `1 + N`, so the `virtio1` net slot (0x1000_2000) is IRQ 2.
    const NET_IRQ: u32 = 0;

    fn in_irq_context() -> bool {
        false
    }

    /// Return whether the current execution is using an architecture trap
    /// stack.
    ///
    /// Synchronous exceptions such as syscalls are not IRQ context, but they
    /// still run on a small per-hart trap stack on stackless platforms.
    /// Substrates use this fact to defer destructor-heavy maintenance until
    /// control has returned to a normal kernel/reactor stack.
    fn in_trap_context() -> bool {
        Self::in_irq_context()
    }

    fn interrupts_enabled() -> bool {
        true
    }

    fn claim() -> u32 {
        0
    }

    fn complete(_irq: u32) {}

    fn mask(_irq: u32) {}

    fn unmask(_irq: u32) {}

    fn set_priority(_irq: u32, _priority: u8) {}

    fn install_dispatch_table(_table: &'static IrqDispatchTable) {}

    fn dispatch_irq(_irq: u32) -> IrqHandled {
        IrqHandled::Done
    }
}

pub const IRQ_DISPATCH_TABLE_SIZE: usize = 1024;

pub type IrqHandlerFn = fn(irq: u32) -> IrqHandled;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqHandled {
    Done,
    Wake,
    NotMine,
}

pub struct IrqDispatchTable {
    pub entries: [Option<IrqHandlerFn>; IRQ_DISPATCH_TABLE_SIZE],
}

impl IrqDispatchTable {
    pub const SIZE: usize = IRQ_DISPATCH_TABLE_SIZE;

    pub const fn new() -> Self {
        Self {
            entries: [None; IRQ_DISPATCH_TABLE_SIZE],
        }
    }
}

impl Default for IrqDispatchTable {
    fn default() -> Self {
        Self::new()
    }
}

pub trait TimeIf {
    /// Read monotonic nanoseconds since the platform's boot-time epoch.
    ///
    /// Values must be non-decreasing on the current hart and cheap enough for
    /// scheduler/reactor hot paths.
    fn read_ns() -> u64;

    /// Program the current hart's timer for an absolute monotonic deadline.
    ///
    /// `deadline` uses the same nanosecond epoch as `read_ns()`. Platforms
    /// must not intentionally arm an earlier hardware deadline than requested;
    /// interrupts may arrive late due to firmware, hardware, or emulator
    /// latency. A past deadline should fire as soon as the platform can arrange.
    fn set_deadline_ns(deadline: u64);

    /// Cancel the current hart's pending timer deadline when the platform has
    /// a cancellation mechanism.
    fn cancel_deadline();

    /// Prepare the current hart so a programmed timer deadline can wake or
    /// trap out of the platform idle path.
    fn enable_timer_wakeups() {}

    /// Return the hardware timer frequency used for ns/tick conversion.
    fn frequency_hz() -> u64;

    /// Read the platform's hardware real-time clock as nanoseconds since the
    /// Unix epoch, if the platform exposes a readable RTC. Returns `None` when
    /// there is none, in which case the kernel wall clock keeps its default
    /// epoch base. Called once at boot to seed `CLOCK_REALTIME`; never on a
    /// hot path.
    fn read_rtc_epoch_ns() -> Option<u64> {
        None
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
}

pub type SecondaryEntry = unsafe extern "C" fn(cpu_id: usize) -> !;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpiKind {
    Reschedule,
    TlbShootdown,
    /// Memory barrier IPI — used by `membarrier(2)` to force every
    /// hart through a `fence` sequence so that all prior memory
    /// operations are globally visible.
    Membarrier,
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

    /// Size of the possible-CPU **id space**: highest possible cpu id
    /// plus one — deliberately NOT the population count. Dense-index
    /// consumers (epoch/zone per-cpu domains) allocate and range-check
    /// per-cpu slots by raw `CpuId`, and real boards boot on a
    /// non-zero hart (VisionFive 2's BSP is hart 1), so a sparse mask
    /// like {1} must report 2, and {1,2,3} must report 4. For
    /// contiguous masks starting at 0 (QEMU, host tests) this equals
    /// the count, so existing platforms see no change. Popcount
    /// consumers should use `possible_cpus().count()` directly.
    fn possible_cpu_count() -> usize {
        let bits = Self::possible_cpus().bits();
        (u64::BITS - bits.leading_zeros()) as usize
    }

    fn online_cpu_count() -> usize {
        Self::online_cpus().count()
    }

    fn is_cpu_online(cpu: CpuId) -> bool {
        Self::online_cpus().contains(cpu)
    }

    fn mark_cpu_online(_cpu: CpuId) {}

    fn boot_secondary_cpus(_entry: SecondaryEntry) -> usize {
        0
    }

    fn enable_ipi_wakeups() {}

    fn wait_for_interrupt_once() {
        core::hint::spin_loop();
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
    + TimeIf
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
        + TimeIf
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

pub fn console_write_bytes<P: ConsoleIf>(bytes: &[u8]) {
    P::write_bytes(bytes);
}

pub fn console_read_bytes<P: ConsoleIf>(buf: &mut [u8]) -> usize {
    P::read_bytes(buf)
}

pub fn console_write_str<P: ConsoleIf>(message: &str) {
    P::write_bytes(message.as_bytes());
}
