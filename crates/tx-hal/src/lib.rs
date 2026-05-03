#![no_std]

pub mod time;

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
    _not_send_sync: PhantomData<*mut ()>,
}

impl CpuPinGuard {
    pub const fn new(cpu_id: CpuId) -> Self {
        Self {
            cpu_id,
            _not_send_sync: PhantomData,
        }
    }

    pub const fn cpu_id(&self) -> CpuId {
        self.cpu_id
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
    pub timebase_frequency_hz: u64,
    pub possible_cpu_count: usize,
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

pub trait PlatformConfig {
    const ARCH: Arch;
    const BOARD: &'static str;
    const SUBSTRATE_BOOT_READY: bool = false;
    const PAGE_SIZE: usize = 4096;
    const PAGE_SHIFT: usize = 12;
    const PHYS_ADDR_BITS: u8 = 0;
    const VIRT_ADDR_BITS: u8 = 0;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0);
    const DIRECT_MAP_SIZE: usize = 0;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(0);
    const USER_TOP: VirtAddr = VirtAddr(0);
    const USER_RESERVED_TOP_SIZE: usize = 0;
    const USER_ALLOC_TOP: VirtAddr = Self::USER_TOP;
    const KERNEL_STACK_SIZE: usize = 0;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 0;
    const ASID_BITS: u8 = 0;
    const CACHE_LINE_SIZE: usize = 0;
    const DMA_COHERENT: bool = false;
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
pub enum PmapError {
    InvalidRequest,
    AlreadyMapped,
    Exhausted,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
pub struct PmapPermissions {
    bits: u8,
}

impl PmapPermissions {
    pub const READ: Self = Self { bits: 1 << 0 };
    pub const WRITE: Self = Self { bits: 1 << 1 };
    pub const EXECUTE: Self = Self { bits: 1 << 2 };
    pub const USER: Self = Self { bits: 1 << 3 };
    pub const GLOBAL: Self = Self { bits: 1 << 4 };

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
pub struct PmapReservationIntermediates {
    pub l1: Option<PtNode>,
    pub l0: Option<PtNode>,
}

impl PmapReservationIntermediates {
    pub const fn empty() -> Self {
        Self { l1: None, l0: None }
    }
}

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
pub struct Asid(pub u16);

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

pub trait PmapIf {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        None
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        Err(AllocError::Exhausted)
    }

    fn free_pt_node(_node: PtNode) {}

    fn install_pt_node_allocator(_allocator: PtNodeAllocator) -> Result<(), PmapError> {
        Err(PmapError::Unsupported)
    }

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

    fn commit_kernel_mapping(_reservation: PmapReservation) {}

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

    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        Err(PmapError::Unsupported)
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

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

    fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {}

    fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        for invalidation in invalidations {
            Self::shootdown_mapping(asid, *invalidation);
        }
    }
}

pub mod pmap;
pub mod trap;
pub use trap::{
    FaultInfo, KernelTrapSink, SignalHandlerRegs, TrapAction, TrapClass, TrapFrameMut,
    TrapFrameMutVtable, TrapFrameSnapshot, TrapFrameView, TrapIf, TrapPreviousMode, TrapSnapshot,
    UserTrapContext,
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
unsafe impl Pod for UserTrapContext {}

// ---------------------------------------------------------------------------
// KernelPtr<T> and UserPtr<T> — typed address-space wrappers
// ---------------------------------------------------------------------------

/// A typed pointer into kernel virtual address space.
///
/// Prevents accidental use of user-space addresses where kernel addresses
/// are expected. Constructing a `KernelPtr<T>` is `unsafe` because the
/// caller must ensure the underlying pointer is valid kernel memory.
#[repr(transparent)]
pub struct KernelPtr<T>(*mut T);

impl<T> KernelPtr<T> {
    /// Wrap a raw kernel pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid kernel virtual address that remains live for the
    /// duration of its use through this `KernelPtr`.
    pub unsafe fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(self) -> *mut T {
        self.0
    }
}

impl<T> Copy for KernelPtr<T> {}
impl<T> Clone for KernelPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

// SAFETY: same restrictions as *mut T — use is guarded by the caller.
unsafe impl<T: Send> Send for KernelPtr<T> {}
unsafe impl<T: Sync> Sync for KernelPtr<T> {}

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
// FixupEntry — describes one faulting-PC range and its recovery label
// ---------------------------------------------------------------------------

/// Describes a kernel-mode user-access PC range and its recovery stub.
///
/// When the trap shell sees a kernel-mode page fault whose `sepc` falls in
/// `[pc_start, pc_end)`, it redirects execution to `recovery_pc` and places
/// the fault address in the platform's agreed error register so the recovery
/// stub can return an `Err(FaultInfo)` to its caller.
#[repr(C)]
pub struct FixupEntry {
    /// First PC (inclusive) of the faulting instruction range.
    pub pc_start: VirtAddr,
    /// First PC (exclusive) past the faulting instruction range.
    pub pc_end: VirtAddr,
    /// PC of the recovery stub to jump to on fault.
    pub recovery_pc: VirtAddr,
}

// ---------------------------------------------------------------------------
// UserAccessIf
// ---------------------------------------------------------------------------

pub trait UserAccessIf {
    /// Copy `len` bytes from user-space address `src` to kernel address `dst`.
    ///
    /// Returns `Ok(())` on success, or `Err(FaultInfo)` describing the
    /// kernel-mode page fault that occurred (callers should propagate as
    /// `EFAULT`).  The default implementation always returns `Err` for any
    /// non-zero `len`; platforms override it with architecture-specific
    /// assembly backed by a fixup table.
    ///
    /// # Safety
    ///
    /// * `dst` must be a valid, writable kernel pointer for `len` bytes.
    /// * `src` is interpreted in the currently installed user page table.
    /// * The platform's fixup table must be fully populated before this is
    ///   called (guaranteed after `init_early` returns).
    unsafe fn copy_from_user(
        dst: KernelPtr<u8>,
        src: UserPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo> {
        let _ = dst;
        if len == 0 {
            return Ok(());
        }
        Err(FaultInfo {
            address: VirtAddr(src.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }

    /// Copy `len` bytes from kernel address `src` to user-space address `dst`.
    ///
    /// Returns `Ok(())` on success, or `Err(FaultInfo)` on kernel-mode fault.
    ///
    /// # Safety
    ///
    /// * `src` must be a valid, readable kernel pointer for `len` bytes.
    /// * `dst` is interpreted in the currently installed user page table.
    unsafe fn copy_to_user(
        dst: UserPtr<u8>,
        src: KernelPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo> {
        let _ = src;
        if len == 0 {
            return Ok(());
        }
        Err(FaultInfo {
            address: VirtAddr(dst.addr()),
            write: true,
            instruction: false,
            from_user: false,
        })
    }

    /// Read a `Pod` value from user space.
    ///
    /// Implemented via [`copy_from_user`](UserAccessIf::copy_from_user) by
    /// default; platforms may override for optimized single-instruction paths.
    ///
    /// # Safety
    ///
    /// `src` is interpreted in the currently installed user page table.
    unsafe fn read_user<T: Pod>(src: UserPtr<T>) -> Result<T, FaultInfo> {
        let mut val = core::mem::MaybeUninit::<T>::uninit();
        // SAFETY: val is valid kernel stack memory; copy_from_user reads into it.
        unsafe {
            Self::copy_from_user(
                KernelPtr::new(val.as_mut_ptr().cast::<u8>()),
                UserPtr::new(src.addr()),
                core::mem::size_of::<T>(),
            )?;
            Ok(val.assume_init())
        }
    }

    /// Write a `Pod` value to user space.
    ///
    /// Implemented via [`copy_to_user`](UserAccessIf::copy_to_user) by default.
    ///
    /// # Safety
    ///
    /// `dst` is interpreted in the currently installed user page table.
    unsafe fn write_user<T: Pod>(dst: UserPtr<T>, value: T) -> Result<(), FaultInfo> {
        // SAFETY: value is on the kernel stack and lives for the duration of
        // copy_to_user; cast_mut is safe because copy_to_user only reads from
        // the kernel source pointer.
        unsafe {
            Self::copy_to_user(
                UserPtr::new(dst.addr()),
                KernelPtr::new(core::ptr::addr_of!(value).cast_mut().cast::<u8>()),
                core::mem::size_of::<T>(),
            )
        }
    }
}

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
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalFramePlacement {
    pub frame_addr: UserPtr<()>,
    pub trampoline_pc: UserPtr<()>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SavedSignalFrame {
    pub saved_mask: UserSignalMaskAbi,
    pub user_context: UserTrapContext,
}

unsafe impl Pod for SavedSignalFrame {}

pub trait SignalFrameIf: TrapIf + UserAccessIf {
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

    fn restore_signal_frame(_tf: TrapFrameMut<'_>, _frame: &SavedSignalFrame) {}

    fn rewind_syscall_pc(mut tf: TrapFrameMut<'_>) {
        tf.rewind_pc(4);
    }
}
pub trait IrqIf {
    const MAX_IRQ: u32 = 0;

    fn in_irq_context() -> bool {
        false
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
    + UserAccessIf
    + SignalFrameIf
    + IrqIf
    + TimeIf
    + PercpuIf
    + CacheIf
    + DmaIf
    + SmpIf
    + PowerIf
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
        + UserAccessIf
        + SignalFrameIf
        + IrqIf
        + TimeIf
        + PercpuIf
        + CacheIf
        + DmaIf
        + SmpIf
        + PowerIf
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
