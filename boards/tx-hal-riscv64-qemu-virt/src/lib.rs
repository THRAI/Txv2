#![no_std]

#[cfg(test)]
extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

mod boot_static;
mod debug_trace;
mod dtb;
mod pmap;
mod signal_frame;
mod time;
mod trap;
mod user_access;
pub use trap::{dispatch_trap_frame, return_to_userspace, Rv64TrapFrame};

use boot_static::{
    reserved_region, BootStaticBag, IdentityDropped, IdentityLive, CMDLINE_CAPACITY,
};
use dtb::parse_boot_info_from_fdt;
use pmap::topology as pmap_topology;
use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask, DmaAddr,
    DmaDirection, DmaIf, EntropyIf, InitIf, IpiKind, IrqDispatchTable, IrqHandled, IrqIf,
    MemoryRegion, MemoryRegionKind, PercpuIf, PhysAddr, PlatformConfig, PlatformInfo,
    PlatformInfoIf, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SecondaryEntry,
    SmpIf, TimeIf, VirtAddr,
};

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.trampoline, "ax"
    .equ TX_RV64_KERNEL_VIRT_OFFSET, 0xffffffff00000000
    .equ TX_RV64_QEMU_RAM_BASE, 0x80000000
    .equ TX_RV64_DIRECT_MAP_ROOT_SLOT, 258
    .equ TX_RV64_IDENTITY_ROOT_SLOT, 2
    .equ TX_RV64_KERNEL_ROOT_SLOT, 510
    .equ TX_RV64_KERNEL_L1_START_SLOT, 1
    .equ TX_RV64_KERNEL_ALIAS_L0_TABLES, 8
    .equ TX_RV64_PAGE_SIZE, 4096
    .equ TX_RV64_MAX_BOOT_CPUS, 4
    .equ TX_RV64_SATP_SV39, 0x8000000000000000
    .equ TX_RV64_PTE_V, 0x001
    .equ TX_RV64_PTE_R, 0x002
    .equ TX_RV64_PTE_W, 0x004
    .equ TX_RV64_PTE_X, 0x008
    .equ TX_RV64_PTE_G, 0x020
    .equ TX_RV64_PTE_A, 0x040
    .equ TX_RV64_PTE_D, 0x080
    .equ TX_RV64_PTE_IDENTITY, TX_RV64_PTE_V | TX_RV64_PTE_R | TX_RV64_PTE_W | TX_RV64_PTE_X | TX_RV64_PTE_A | TX_RV64_PTE_D
    .equ TX_RV64_PTE_DIRECT, TX_RV64_PTE_V | TX_RV64_PTE_R | TX_RV64_PTE_W | TX_RV64_PTE_G | TX_RV64_PTE_A | TX_RV64_PTE_D
    .equ TX_RV64_PTE_KERNEL_BOOT, TX_RV64_PTE_V | TX_RV64_PTE_R | TX_RV64_PTE_W | TX_RV64_PTE_X | TX_RV64_PTE_G | TX_RV64_PTE_A | TX_RV64_PTE_D

    .globl _start
_start:
    mv s0, a0
    mv s1, a1
    la sp, __tx_boot_stack_top_load
    li t0, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t0, .Ltx_bsp_stack_ready
    slli t1, s0, 16
    sub sp, sp, t1
.Ltx_bsp_stack_ready:

    la t0, __bss_start_load
    la t1, __bss_end_load
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b

2:
    la s2, __bootstrap_root_load
    li t0, TX_RV64_QEMU_RAM_BASE
    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_RV64_PTE_IDENTITY
    li t2, TX_RV64_IDENTITY_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_RV64_PTE_DIRECT
    li t2, TX_RV64_DIRECT_MAP_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    la s3, __kernel_alias_l1_load
    srli t1, s3, 12
    slli t1, t1, 10
    ori t1, t1, TX_RV64_PTE_V
    li t2, TX_RV64_KERNEL_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    la s4, __kernel_alias_l0_tables_load
    li t0, 0
    li t1, TX_RV64_KERNEL_ALIAS_L0_TABLES
3:
    bgeu t0, t1, 4f
    slli t2, t0, 12
    add t3, s4, t2
    srli t4, t3, 12
    slli t4, t4, 10
    ori t4, t4, TX_RV64_PTE_V
    li t5, TX_RV64_KERNEL_L1_START_SLOT
    add t5, t5, t0
    slli t5, t5, 3
    add t6, s3, t5
    sd t4, 0(t6)
    addi t0, t0, 1
    j 3b

4:
    la s5, __kernel_start_load
    la s6, __kernel_end_load
    li s7, TX_RV64_PAGE_SIZE
    mv t0, s5
5:
    bgeu t0, s6, 6f
    sub t1, t0, s5
    srli t2, t1, 21
    slli t2, t2, 12
    add t3, s4, t2
    srli t4, t1, 12
    andi t4, t4, 0x1ff
    slli t4, t4, 3
    add t3, t3, t4
    srli t5, t0, 12
    slli t5, t5, 10
    ori t5, t5, TX_RV64_PTE_KERNEL_BOOT
    sd t5, 0(t3)
    add t0, t0, s7
    j 5b

6:
    srli t0, s2, 12
    li t1, TX_RV64_SATP_SV39
    or a0, t0, t1

    csrw satp, a0
    sfence.vma

    li t0, TX_RV64_KERNEL_VIRT_OFFSET
    la sp, __tx_boot_stack_top_load
    li t1, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t1, .Ltx_bsp_high_stack_ready
    slli t2, s0, 16
    sub sp, sp, t2
.Ltx_bsp_high_stack_ready:
    add sp, sp, t0
    .option push
    .option norelax
    la gp, __global_pointer_load
    add gp, gp, t0
    .option pop

    mv a0, s0
    mv a1, s1
    la t1, __rust_entry_load
    add t1, t1, t0
    jr t1

    .globl tx_rv64_qemu_secondary_start
    .type tx_rv64_qemu_secondary_start, @function
tx_rv64_qemu_secondary_start:
    mv s0, a0
    mv s1, a1
    li t0, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t0, 9f

    la sp, __tx_boot_stack_top_load
    slli t1, s0, 16
    sub sp, sp, t1
    li t0, TX_RV64_KERNEL_VIRT_OFFSET
    add sp, sp, t0
    .option push
    .option norelax
    la gp, __global_pointer_load
    add gp, gp, t0
    .option pop

    la t0, __bootstrap_root_load
    srli t0, t0, 12
    li t1, TX_RV64_SATP_SV39
    or t0, t0, t1
    csrw satp, t0
    sfence.vma

    mv a0, s0
    jr s1

9:
    wfi
    j 9b
    .size tx_rv64_qemu_secondary_start, . - tx_rv64_qemu_secondary_start

    .globl tx_rv64_qemu_install_kernel_stack
    .type tx_rv64_qemu_install_kernel_stack, @function
tx_rv64_qemu_install_kernel_stack:
    mv sp, a0
    ret
    .size tx_rv64_qemu_install_kernel_stack, . - tx_rv64_qemu_install_kernel_stack

7:
    wfi
    j 7b

"#
);

pub struct Platform;

const QEMU_VIRT_RAM_BASE: usize = 0x8000_0000;
const QEMU_VIRT_FALLBACK_RAM_SIZE: usize = 256 * 1024 * 1024;
const MAX_BOOT_CPUS: usize = 4;
#[cfg(target_arch = "riscv64")]
const PLIC_PHYS_BASE: usize = 0x0c00_0000;
#[cfg(target_arch = "riscv64")]
const PLIC_BASE: usize = pmap_topology::DIRECT_MAP_BASE + PLIC_PHYS_BASE;
const PLIC_MAX_IRQ: u32 = tx_hal::IRQ_DISPATCH_TABLE_SIZE as u32;
#[cfg(all(not(target_arch = "riscv64"), test))]
const PLIC_IRQ_SOURCES: usize = tx_hal::IRQ_DISPATCH_TABLE_SIZE;
#[cfg(all(not(target_arch = "riscv64"), test))]
const PLIC_ENABLE_WORDS: usize = PLIC_IRQ_SOURCES / u32::BITS as usize;
const PLIC_PRIORITY_BASE: usize = 0x0;
const PLIC_ENABLE_BASE: usize = 0x2000;
const PLIC_ENABLE_CONTEXT_STRIDE: usize = 0x80;
const PLIC_CONTEXT_BASE: usize = 0x20_0000;
const PLIC_CONTEXT_STRIDE: usize = 0x1000;
const PLIC_CLAIM_COMPLETE: usize = 0x4;
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0);
static IPI_ACKED_CPUS: AtomicU64 = AtomicU64::new(0);
static FALLBACK_IRQ_DEPTH: AtomicUsize = AtomicUsize::new(0);
static INSTALLED_IRQ_TABLE: AtomicPtr<IrqDispatchTable> = AtomicPtr::new(core::ptr::null_mut());

#[repr(C, align(64))]
pub struct Rv64PerCpuArea {
    cpu_id: usize,
    kernel_stack_top: AtomicUsize,
    irq_depth: AtomicUsize,
}

impl Rv64PerCpuArea {
    pub const fn new(cpu_id: usize) -> Self {
        Self {
            cpu_id,
            kernel_stack_top: AtomicUsize::new(0),
            irq_depth: AtomicUsize::new(0),
        }
    }

    pub fn cpu_id(&self) -> CpuId {
        CpuId(self.cpu_id)
    }

    pub fn kernel_stack_top(&self) -> VirtAddr {
        VirtAddr(self.kernel_stack_top.load(Ordering::Acquire))
    }

    pub fn irq_depth(&self) -> usize {
        self.irq_depth.load(Ordering::Acquire)
    }
}

static RV64_PERCPU_AREAS: [Rv64PerCpuArea; MAX_BOOT_CPUS] = [
    Rv64PerCpuArea::new(0),
    Rv64PerCpuArea::new(1),
    Rv64PerCpuArea::new(2),
    Rv64PerCpuArea::new(3),
];

/// Per-hart save area for the reschedule longjmp (slice 2 of the
/// userspace-first-entry fix per
/// `docs/progress/decisions/2026-05-08-userspace-first-entry-gap.md`).
///
/// The userspace-entry shim (`tx_rv64_enter_userspace_save_resume`)
/// stashes (sp, ra, s0..s11) here before `sret`. The trap-shell
/// longjmp helper (`tx_rv64_resume_kernel_after_reschedule`)
/// restores them on `TrapAction::Reschedule` and `ret`s back to the
/// kernel-side caller of `enter_userspace_with_context`.
///
/// Field offsets are load-bearing: the asm helpers reference them
/// by literal byte offset. Keep `KERNEL_RESUME_CTX_*_OFFSET` in
/// sync with the field order.
#[repr(C, align(8))]
pub struct KernelResumeCtx {
    pub sp: usize,        // offset 0
    pub ra: usize,        // offset 8
    pub s: [usize; 12],   // offset 16..112
}

// These offsets are referenced by literal byte offset in the trap-vector
// asm (`TX_RV64_RCTX_SP`, `TX_RV64_RCTX_RA`, `TX_RV64_RCTX_S0`). The
// Rust constants below pin the layout from the Rust side so a struct
// reorder triggers a compile-time mismatch with the static_assert.
#[allow(dead_code)]
const KERNEL_RESUME_CTX_SP_OFFSET: usize = 0;
#[allow(dead_code)]
const KERNEL_RESUME_CTX_RA_OFFSET: usize = 8;
#[allow(dead_code)]
const KERNEL_RESUME_CTX_S0_OFFSET: usize = 16;
const _: () = assert!(core::mem::size_of::<KernelResumeCtx>() == 14 * 8);
const _: () =
    assert!(core::mem::offset_of!(KernelResumeCtx, sp) == KERNEL_RESUME_CTX_SP_OFFSET);
const _: () =
    assert!(core::mem::offset_of!(KernelResumeCtx, ra) == KERNEL_RESUME_CTX_RA_OFFSET);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, s) == KERNEL_RESUME_CTX_S0_OFFSET);

/// Per-hart cell with `Sync` because the only writer/reader is the
/// local hart's trap-vector / userspace-entry shim. Cross-hart
/// concurrent access would be a load-bearing invariant violation.
#[repr(transparent)]
pub struct PerHartCell<T>(core::cell::UnsafeCell<T>);

unsafe impl<T> Sync for PerHartCell<T> {}

impl<T> PerHartCell<T> {
    pub const fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

static RV64_KERNEL_RESUME_CTX: [PerHartCell<KernelResumeCtx>; MAX_BOOT_CPUS] = [
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
];

/// Per-CPU trap-handler stack. Sized 16 KiB; the trap vector
/// `csrrw`-swaps onto its top via `sscratch` so trap-handler frames
/// don't trample the BSP/AP runtime kernel stack (which holds the
/// reactor + thread future frames at the moment a user trap fires).
///
/// Lives in `.data` (writable) rather than substrate-allocated
/// frames because the trap stack must be ready *before* substrate
/// is — the trap vector is installed early in boot, well before
/// the page allocator. Wrapped in `PerHartCell<UnsafeCell<...>>`
/// so the linker keeps it out of `.rodata` (which `pmap` maps
/// `KERNEL_RO`); plain `static [u8; N]` lands in `.rodata` and
/// the trap-vector's first store would fault.
///
/// Total static cost is `MAX_BOOT_CPUS × 16 KiB = 64 KiB`.
const RV64_TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
pub struct Rv64TrapStack(pub [u8; RV64_TRAP_STACK_SIZE]);

static RV64_TRAP_STACKS: [PerHartCell<Rv64TrapStack>; MAX_BOOT_CPUS] = [
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
];

/// Per-hart trap-stack top: the value the boot primer writes into
/// `sscratch`, and the value the reschedule longjmp restores
/// `sscratch` to before unwinding back to the kernel caller.
pub fn trap_stack_top_for_cpu(cpu: CpuId) -> usize {
    let stack = RV64_TRAP_STACKS[cpu.0].as_ptr();
    let base = stack as usize;
    base + RV64_TRAP_STACK_SIZE
}

/// Pointer to the local hart's [`KernelResumeCtx`]. Asm helpers
/// load/store at the documented field offsets; Rust callers in
/// `apply_trap_action` use the pointer directly.
pub fn current_kernel_resume_ctx_ptr() -> *mut KernelResumeCtx {
    let cpu = current_cpu_id();
    RV64_KERNEL_RESUME_CTX[cpu.0].as_ptr()
}

#[cfg(not(target_arch = "riscv64"))]
static HOST_KERNEL_TLS: AtomicUsize = AtomicUsize::new(0);

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "qemu-riscv64-virt";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 56;
    const VIRT_ADDR_BITS: u8 = 39;
    const DIRECT_MAP_BASE: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::DIRECT_MAP_BASE);
    const DIRECT_MAP_SIZE: usize = pmap_topology::DIRECT_MAP_SIZE;
    const KERNEL_VIRT_BASE: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::KERNEL_VIRT_BASE);
    const USER_TOP: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::SV39_USER_TOP);
    const USER_RESERVED_TOP_SIZE: usize = pmap_topology::USER_RESERVED_TOP_SIZE;
    const USER_ALLOC_TOP: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::SV39_USER_ALLOC_TOP);
    const KERNEL_STACK_SIZE: usize = 64 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 3;
    const ASID_BITS: u8 = 16;
    const CACHE_LINE_SIZE: usize = 64;
    const DMA_COHERENT: bool = true;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvSbi;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        let bag = BootStaticBag::<IdentityLive>::capture_once(firmware_arg);
        pmap::adopt_high_linked_bootstrap_pmap(bag);

        BootStaticBag::<IdentityLive>::take_global()
            .publish_boot_info_before_identity_drop(firmware_arg)
            .complete_post_entry_pipeline()
            .install_global();

        BootHandoff {
            cpu_id: CpuId(cpu_id),
            firmware_arg: BootArg(firmware_arg),
            protocol: Self::BOOT_PROTOCOL,
        }
    }
}

impl InitIf for Platform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

impl BootInfoIf for Platform {
    fn boot_info() -> &'static BootInfo {
        BootStaticBag::<IdentityDropped>::global_ref().boot_info_ref()
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        BootStaticBag::<IdentityDropped>::global_ref().platform_info_ref()
    }
}

impl AuxvIf for Platform {
    fn arch_auxv_facts() -> ArchAuxvFacts {
        ArchAuxvFacts::new(Self::PAGE_SIZE, tx_hal::RISCV_HWCAP_IMAFDC, 0, "riscv64")
    }
}
impl ConsoleIf for Platform {
    fn write_bytes(bytes: &[u8]) {
        #[cfg(target_arch = "riscv64")]
        {
            for &byte in bytes {
                sbi_console_putchar(byte);
            }
        }

        #[cfg(not(target_arch = "riscv64"))]
        let _ = bytes;
    }

    fn read_bytes(buf: &mut [u8]) -> usize {
        read_sbi_console_bytes(buf)
    }
}
impl PmapIf for Platform {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        pmap::bootstrap_pmap_info()
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        pmap::alloc_pt_node()
    }

    fn free_pt_node(node: PtNode) {
        pmap::free_pt_node(node);
    }

    fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
        pmap::install_pt_node_allocator(allocator)
    }

    fn reserve_kernel_direct_map_1g(phys: PhysAddr) -> Result<Option<PmapReservation>, PmapError> {
        pmap::reserve_kernel_direct_map_1g(phys)
    }

    fn commit_kernel_direct_map_1g(reservation: PmapReservation) {
        pmap::commit_kernel_direct_map_1g(reservation);
    }

    fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError> {
        pmap::extend_direct_map(phys_end)
    }

    fn reserve_kernel_mapping(
        virt: tx_hal::VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        pmap::reserve_kernel_mapping(virt, phys, kind)
    }

    fn rollback_kernel_mapping(reservation: PmapReservation) {
        pmap::rollback_kernel_mapping(reservation);
    }

    fn commit_kernel_mapping(reservation: PmapReservation) {
        pmap::commit_kernel_mapping(reservation);
    }

    fn unmap_kernel_mapping(
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        pmap::unmap_kernel_mapping(virt, kind)
    }

    fn protect_kernel_mapping(
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        pmap::protect_kernel_mapping(virt, kind, permissions)
    }

    fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
        pmap::shootdown_kernel_mapping(invalidation);
        remote_sfence_vma(invalidation);
    }

    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        pmap::create_pmap_root()
    }

    fn destroy_pmap_root(root: PmapRoot) {
        pmap::destroy_pmap_root(root);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        pmap::reserve_mapping(root, virt, phys, kind)
    }

    fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
        pmap::rollback_mapping(root, reservation);
    }

    fn commit_mapping(root: &PmapRoot, reservation: PmapReservation, permissions: PmapPermissions) {
        pmap::commit_mapping(root, reservation, permissions);
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        pmap::unmap_mapping(root, virt, kind)
    }

    fn protect_mapping(
        root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        pmap::protect_mapping(root, virt, kind, permissions)
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        pmap::shootdown_mapping(asid, invalidation);
        remote_sfence_vma_asid(asid, invalidation);
    }

    /// Write `satp` to point at `root.phys()` with Sv39 mode bits and
    /// the root's ASID, then issue a local `sfence.vma`.
    ///
    /// Called from the thread runtime right before
    /// `TrapIf::enter_userspace_with_context` so user-mode fetches see
    /// the per-process pmap. Without this satp would still point at the
    /// kernel bootstrap root from the boot trampoline (which has no
    /// user mappings), and every user-mode instruction fetch would
    /// fault forever.
    fn activate_user_pmap(root: &PmapRoot) {
        #[cfg(target_arch = "riscv64")]
        unsafe {
            const SATP_MODE_SV39: usize = 0x8 << 60;
            let ppn = root.phys().0 >> 12;
            let asid = root.asid().0 as usize;
            let satp = SATP_MODE_SV39 | (asid << 44) | ppn;
            core::arch::asm!(
                "csrw satp, {satp}",
                "sfence.vma",
                satp = in(reg) satp,
                options(nostack)
            );
        }
        #[cfg(not(target_arch = "riscv64"))]
        let _ = root;
    }
}
impl IrqIf for Platform {
    const MAX_IRQ: u32 = PLIC_MAX_IRQ;

    /// QEMU `virt` machine's 16550 UART is wired at PLIC IRQ 10.
    /// Source: `qemu/hw/riscv/virt.c::UART0_IRQ`.
    const UART_IRQ: u32 = 10;

    fn in_irq_context() -> bool {
        irq_context_depth() != 0
    }

    fn interrupts_enabled() -> bool {
        supervisor_interrupts_enabled()
    }

    fn claim() -> u32 {
        plic_claim(current_plic_context())
    }

    fn complete(irq: u32) {
        if valid_plic_irq(irq) {
            plic_complete(current_plic_context(), irq);
        }
    }

    fn mask(irq: u32) {
        if valid_plic_irq(irq) {
            plic_set_enabled(current_plic_context(), irq, false);
        }
    }

    fn unmask(irq: u32) {
        if valid_plic_irq(irq) {
            plic_set_enabled(current_plic_context(), irq, true);
        }
    }

    fn set_priority(irq: u32, priority: u8) {
        if valid_plic_irq(irq) {
            plic_set_priority(irq, priority);
        }
    }

    fn install_dispatch_table(table: &'static IrqDispatchTable) {
        INSTALLED_IRQ_TABLE.store(
            table as *const IrqDispatchTable as *mut _,
            Ordering::Release,
        );
    }

    fn dispatch_irq(irq: u32) -> IrqHandled {
        if !valid_plic_irq(irq) {
            return IrqHandled::Done;
        }

        if let Some(table) = installed_irq_table() {
            if let Some(handler) = table.entries[irq as usize] {
                return handler(irq);
            }
        }

        Self::mask(irq);
        IrqHandled::Done
    }
}
impl TimeIf for Platform {
    fn read_ns() -> u64 {
        time::read_ns(Self::frequency_hz())
    }

    fn set_deadline_ns(deadline: u64) {
        time::set_deadline_ns(deadline, Self::frequency_hz());
    }

    fn cancel_deadline() {
        time::cancel_deadline();
    }

    fn enable_timer_wakeups() {
        time::enable_timer_wakeups();
    }

    fn frequency_hz() -> u64 {
        Self::platform_info().timebase_frequency_hz
    }
}
impl PercpuIf for Platform {
    fn current_cpu_id() -> CpuId {
        current_cpu_id()
    }

    fn install_early_percpu(cpu_id: CpuId) {
        install_early_percpu(cpu_id);
    }

    fn read_kernel_tls() -> u64 {
        read_kernel_tls() as u64
    }

    fn write_kernel_tls(value: u64) {
        write_kernel_tls(value as usize);
    }

    unsafe fn install_kernel_stack(top: VirtAddr) {
        unsafe { install_kernel_stack(top) };
    }
}
impl CacheIf for Platform {
    fn fence_all() {
        rv64_fence_all();
    }

    fn fence_i_local() {
        rv64_fence_i();
    }

    fn fence_i_all() {
        rv64_remote_fence_i();
    }

    fn flush_icache_range(_start: VirtAddr, _len: usize) {
        rv64_fence_i();
    }

    fn dcache_clean_range(_start: PhysAddr, _len: usize) {}

    fn dcache_invalidate_range(_start: PhysAddr, _len: usize) {}

    fn dcache_clean_invalidate_range(_start: PhysAddr, _len: usize) {}
}

impl DmaIf for Platform {
    const DMA_COHERENT: bool = true;

    fn phys_to_dma(paddr: PhysAddr) -> DmaAddr {
        DmaAddr(paddr.0 as u64)
    }

    fn dma_to_phys(daddr: DmaAddr) -> PhysAddr {
        PhysAddr(daddr.0 as usize)
    }

    fn sync_for_device(_paddr: PhysAddr, _len: usize, _dir: DmaDirection) {}

    fn sync_for_cpu(_paddr: PhysAddr, _len: usize, _dir: DmaDirection) {}
}
impl SmpIf for Platform {
    fn current_cpu_id() -> CpuId {
        current_cpu_id()
    }

    fn possible_cpus() -> CpuMask {
        CpuMask::first(Self::platform_info().possible_cpu_count.min(MAX_BOOT_CPUS))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(ONLINE_CPUS.load(Ordering::Acquire))
    }

    fn mark_cpu_online(cpu: CpuId) {
        if cpu.0 < u64::BITS as usize {
            ONLINE_CPUS.fetch_or(1u64 << cpu.0, Ordering::AcqRel);
        }
    }

    fn boot_secondary_cpus(entry: SecondaryEntry) -> usize {
        let possible = Self::possible_cpus();
        let current = current_cpu_id();
        let mut started_mask = CpuMask::EMPTY;

        IPI_ACKED_CPUS.store(0, Ordering::Release);
        pmap::install_secondary_identity_bridge();
        for cpu in 0..MAX_BOOT_CPUS {
            let cpu = CpuId(cpu);
            if cpu == current || !possible.contains(cpu) {
                continue;
            }
            if start_secondary_hart(cpu, entry) {
                started_mask =
                    CpuMask::from_bits(started_mask.bits() | CpuMask::single(cpu).bits());
            }
        }
        let online = wait_for_online_secondaries(started_mask);
        pmap::remove_secondary_identity_bridge();

        online
    }

    fn enable_ipi_wakeups() {
        enable_supervisor_software_wakeups();
    }

    fn wait_for_interrupt_once() {
        #[cfg(target_arch = "riscv64")]
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack));
        }

        #[cfg(not(target_arch = "riscv64"))]
        core::hint::spin_loop();
    }

    fn pending_ipi(_kind: IpiKind) -> bool {
        supervisor_software_interrupt_pending()
    }

    fn park_this_cpu() -> ! {
        enable_supervisor_software_interrupts();
        loop {
            Self::wait_for_interrupt_once();
        }
    }

    fn send_ipi(target: CpuId, _kind: IpiKind) {
        if target == current_cpu_id() {
            return;
        }
        send_sbi_ipi(CpuMask::single(target));
    }

    fn broadcast_ipi(mask: CpuMask, _kind: IpiKind) {
        send_sbi_ipi(mask);
    }

    fn ack_ipi(_kind: IpiKind) {
        mark_ipi_ack(current_cpu_id());
        clear_supervisor_software_interrupt();
    }

    fn clear_ipi_ack_cpus(_kind: IpiKind, mask: CpuMask) {
        IPI_ACKED_CPUS.fetch_and(!mask.bits(), Ordering::AcqRel);
    }

    fn ipi_ack_cpus(_kind: IpiKind) -> CpuMask {
        CpuMask::from_bits(IPI_ACKED_CPUS.load(Ordering::Acquire))
    }
}

impl PowerIf for Platform {
    fn system_off() -> ! {
        #[cfg(target_arch = "riscv64")]
        sbi_shutdown();

        loop {
            core::hint::spin_loop();
        }
    }
}

impl EntropyIf for Platform {
    /// RV64 QEMU virt entropy: mix the unprivileged `rdtime` CSR
    /// (always available, sub-µs resolution on the QEMU virt
    /// timebase) into the trait-default xorshift counter. This is
    /// not a CSPRNG, but for txKernel's current trust model — no
    /// ASLR, no untrusted input, musl SSP only — it is materially
    /// stronger than the static `[0; 16]` it replaces.
    ///
    /// The Zkr `seed` CSR (CSR 0x015) was considered but skipped:
    /// `riscv64gc` does not include Zkr, and a runtime probe would
    /// hook the trap shell. `rdtime` ships cleanly today; a Zkr
    /// upgrade can land later behind a board-config flag.
    fn fill_random(out: &mut [u8]) {
        // Mix rdtime ticks (per-exec varying) with a per-call
        // xorshift counter so back-to-back execs at the same tick
        // still diverge.
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0xA5A5_5A5A_DEAD_BEEF);

        let ticks: u64 = read_rdtime_ticks();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut s = ticks ^ counter.rotate_left(13);
        // Avoid the all-zero xorshift fixed point.
        if s == 0 {
            s = 0xDEADBEEF_CAFE_F00D;
        }
        for byte in out.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *byte = (s & 0xff) as u8;
        }
    }
}

#[cfg(target_arch = "riscv64")]
fn read_rdtime_ticks() -> u64 {
    let ticks: u64;
    unsafe {
        core::arch::asm!(
            "rdtime {ticks}",
            ticks = out(reg) ticks,
            options(nomem, nostack)
        );
    }
    ticks
}

#[cfg(not(target_arch = "riscv64"))]
fn read_rdtime_ticks() -> u64 {
    // Host-test fallback: return 0 so the trait default counter
    // alone provides variance. The host test
    // `entropy_fill_random_distinct_calls_diverge` exercises this
    // path.
    0
}

fn current_cpu_id() -> CpuId {
    let kernel_tls = read_kernel_tls();
    cpu_id_from_kernel_tls(kernel_tls).unwrap_or({
        if kernel_tls < MAX_BOOT_CPUS {
            CpuId(kernel_tls)
        } else {
            CpuId(0)
        }
    })
}

fn install_early_percpu(cpu_id: CpuId) {
    let kernel_tls = percpu_tls_for_cpu(cpu_id).unwrap_or(cpu_id.0);
    write_kernel_tls(kernel_tls);

    // Slice 2 boot-time sscratch primer (one-shot per hart). The
    // trap-vector prologue does `csrrw sp, sscratch, sp` to swap
    // onto the per-CPU trap-handler stack; sscratch must therefore
    // be primed before any trap can fire on this hart. We are
    // called from `tx_hal::entry()` on every hart's boot path,
    // immediately after the trap vector is installed, which is
    // the earliest moment we have a valid `cpu_id` and a populated
    // trap-stack array. Subsequent traps re-prime sscratch through
    // the CSR-swap discipline (trap-vector epilogue +
    // userspace-entry shim + reschedule-longjmp helper); this is
    // genuinely one-shot.
    //
    // Note: this also runs on the host build target via the trait
    // impl, but the asm is gated on `target_arch = "riscv64"`.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        let trap_stack_top = trap_stack_top_for_cpu(cpu_id);
        core::arch::asm!("csrw sscratch, {top}", top = in(reg) trap_stack_top);
    }
}

fn read_kernel_tls() -> usize {
    #[cfg(target_arch = "riscv64")]
    {
        let kernel_tls: usize;
        unsafe {
            core::arch::asm!(
                "mv {kernel_tls}, tp",
                kernel_tls = out(reg) kernel_tls,
                options(nomem, nostack)
            );
        }
        kernel_tls
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        HOST_KERNEL_TLS.load(Ordering::Acquire)
    }
}

fn write_kernel_tls(value: usize) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("mv tp, {value}", value = in(reg) value, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        HOST_KERNEL_TLS.store(value, Ordering::Release);
    }
}

unsafe fn install_kernel_stack(top: VirtAddr) {
    record_kernel_stack_top(top);

    #[cfg(target_arch = "riscv64")]
    unsafe {
        tx_rv64_qemu_install_kernel_stack(top.0);
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = top;
}

#[cfg(target_arch = "riscv64")]
extern "C" {
    fn tx_rv64_qemu_install_kernel_stack(top: usize);
}

fn record_kernel_stack_top(top: VirtAddr) {
    if let Some(cpu) = cpu_id_from_kernel_tls(read_kernel_tls()) {
        RV64_PERCPU_AREAS[cpu.0]
            .kernel_stack_top
            .store(top.0, Ordering::Release);
    }
}

fn percpu_tls_for_cpu(cpu_id: CpuId) -> Option<usize> {
    RV64_PERCPU_AREAS
        .get(cpu_id.0)
        .map(|area| area as *const Rv64PerCpuArea as usize)
}

fn cpu_id_from_kernel_tls(kernel_tls: usize) -> Option<CpuId> {
    let base = RV64_PERCPU_AREAS.as_ptr() as usize;
    let stride = core::mem::size_of::<Rv64PerCpuArea>();
    let end = base.checked_add(stride.checked_mul(RV64_PERCPU_AREAS.len())?)?;

    if kernel_tls < base || kernel_tls >= end {
        return None;
    }

    let offset = kernel_tls - base;
    if !offset.is_multiple_of(stride) {
        return None;
    }

    let cpu = offset / stride;
    Some(RV64_PERCPU_AREAS[cpu].cpu_id())
}

fn installed_irq_table() -> Option<&'static IrqDispatchTable> {
    let ptr = INSTALLED_IRQ_TABLE.load(Ordering::Acquire);
    NonNull::new(ptr).map(|ptr| unsafe { ptr.as_ref() })
}

fn valid_plic_irq(irq: u32) -> bool {
    irq != 0 && irq < PLIC_MAX_IRQ
}

fn current_plic_context() -> usize {
    plic_context_for_cpu(current_cpu_id())
}

fn plic_context_for_cpu(cpu: CpuId) -> usize {
    cpu.0.saturating_mul(2).saturating_add(1)
}

fn plic_priority_offset(irq: u32) -> usize {
    PLIC_PRIORITY_BASE + irq as usize * core::mem::size_of::<u32>()
}

fn plic_enable_word_offset(context: usize, word: usize) -> usize {
    PLIC_ENABLE_BASE + context * PLIC_ENABLE_CONTEXT_STRIDE + word * core::mem::size_of::<u32>()
}

fn plic_claim_complete_offset(context: usize) -> usize {
    PLIC_CONTEXT_BASE + context * PLIC_CONTEXT_STRIDE + PLIC_CLAIM_COMPLETE
}

fn plic_set_priority(irq: u32, priority: u8) {
    plic_write_u32(plic_priority_offset(irq), u32::from(priority));
}

fn plic_claim(context: usize) -> u32 {
    plic_read_u32(plic_claim_complete_offset(context))
}

fn plic_complete(context: usize, irq: u32) {
    plic_write_u32(plic_claim_complete_offset(context), irq);
}

fn plic_set_enabled(context: usize, irq: u32, enabled: bool) {
    let word = irq as usize / u32::BITS as usize;
    let bit = irq as usize % u32::BITS as usize;
    let offset = plic_enable_word_offset(context, word);
    let mask = 1u32 << bit;
    let current = plic_read_u32(offset);
    let next = if enabled {
        current | mask
    } else {
        current & !mask
    };
    plic_write_u32(offset, next);
}

#[cfg(target_arch = "riscv64")]
fn plic_read_u32(offset: usize) -> u32 {
    unsafe { ((PLIC_BASE + offset) as *const u32).read_volatile() }
}

#[cfg(target_arch = "riscv64")]
fn plic_write_u32(offset: usize, value: u32) {
    unsafe { ((PLIC_BASE + offset) as *mut u32).write_volatile(value) };
}

#[cfg(all(not(target_arch = "riscv64"), not(test)))]
fn plic_read_u32(_offset: usize) -> u32 {
    0
}

#[cfg(all(not(target_arch = "riscv64"), not(test)))]
fn plic_write_u32(_offset: usize, _value: u32) {}

#[cfg(all(not(target_arch = "riscv64"), test))]
fn plic_read_u32(offset: usize) -> u32 {
    HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .read_u32(offset)
}

#[cfg(all(not(target_arch = "riscv64"), test))]
fn plic_write_u32(offset: usize, value: u32) {
    HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .write_u32(offset, value);
}

#[cfg(all(not(target_arch = "riscv64"), test))]
struct HostPlicState {
    priorities: [u32; PLIC_IRQ_SOURCES],
    enables: [[u32; PLIC_ENABLE_WORDS]; MAX_BOOT_CPUS * 2],
    claim_complete: [u32; MAX_BOOT_CPUS * 2],
}

#[cfg(all(not(target_arch = "riscv64"), test))]
impl HostPlicState {
    const fn new() -> Self {
        Self {
            priorities: [0; PLIC_IRQ_SOURCES],
            enables: [[0; PLIC_ENABLE_WORDS]; MAX_BOOT_CPUS * 2],
            claim_complete: [0; MAX_BOOT_CPUS * 2],
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn read_u32(&self, offset: usize) -> u32 {
        if offset < PLIC_ENABLE_BASE {
            let irq = (offset - PLIC_PRIORITY_BASE) / core::mem::size_of::<u32>();
            return self.priorities.get(irq).copied().unwrap_or(0);
        }

        if (PLIC_ENABLE_BASE..PLIC_CONTEXT_BASE).contains(&offset) {
            let rel = offset - PLIC_ENABLE_BASE;
            let context = rel / PLIC_ENABLE_CONTEXT_STRIDE;
            let word = (rel % PLIC_ENABLE_CONTEXT_STRIDE) / core::mem::size_of::<u32>();
            return self
                .enables
                .get(context)
                .and_then(|words| words.get(word))
                .copied()
                .unwrap_or(0);
        }

        if offset >= PLIC_CONTEXT_BASE {
            let rel = offset - PLIC_CONTEXT_BASE;
            let context = rel / PLIC_CONTEXT_STRIDE;
            let context_offset = rel % PLIC_CONTEXT_STRIDE;
            if context_offset == PLIC_CLAIM_COMPLETE {
                return self.claim_complete.get(context).copied().unwrap_or(0);
            }
        }

        0
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        if offset < PLIC_ENABLE_BASE {
            let irq = (offset - PLIC_PRIORITY_BASE) / core::mem::size_of::<u32>();
            if let Some(priority) = self.priorities.get_mut(irq) {
                *priority = value;
            }
            return;
        }

        if (PLIC_ENABLE_BASE..PLIC_CONTEXT_BASE).contains(&offset) {
            let rel = offset - PLIC_ENABLE_BASE;
            let context = rel / PLIC_ENABLE_CONTEXT_STRIDE;
            let word = (rel % PLIC_ENABLE_CONTEXT_STRIDE) / core::mem::size_of::<u32>();
            if let Some(enable_word) = self
                .enables
                .get_mut(context)
                .and_then(|words| words.get_mut(word))
            {
                *enable_word = value;
            }
            return;
        }

        if offset >= PLIC_CONTEXT_BASE {
            let rel = offset - PLIC_CONTEXT_BASE;
            let context = rel / PLIC_CONTEXT_STRIDE;
            let context_offset = rel % PLIC_CONTEXT_STRIDE;
            if context_offset == PLIC_CLAIM_COMPLETE {
                if let Some(claim_complete) = self.claim_complete.get_mut(context) {
                    *claim_complete = value;
                }
            }
        }
    }
}

#[cfg(all(not(target_arch = "riscv64"), test))]
static HOST_PLIC_STATE: std::sync::Mutex<HostPlicState> =
    std::sync::Mutex::new(HostPlicState::new());

pub(crate) struct IrqContextGuard {
    depth: &'static AtomicUsize,
}

impl Drop for IrqContextGuard {
    fn drop(&mut self) {
        let previous = self
            .depth
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                Some(depth.saturating_sub(1))
            })
            .unwrap_or(0);
        debug_assert!(previous > 0);
    }
}

pub(crate) fn enter_irq_context() -> IrqContextGuard {
    let depth = current_irq_depth_cell();
    depth.fetch_add(1, Ordering::AcqRel);
    IrqContextGuard { depth }
}

fn irq_context_depth() -> usize {
    current_irq_depth_cell().load(Ordering::Acquire)
}

fn current_irq_depth_cell() -> &'static AtomicUsize {
    current_percpu_area()
        .map(|area| &area.irq_depth)
        .unwrap_or(&FALLBACK_IRQ_DEPTH)
}

fn current_percpu_area() -> Option<&'static Rv64PerCpuArea> {
    let cpu = cpu_id_from_kernel_tls(read_kernel_tls())?;
    RV64_PERCPU_AREAS.get(cpu.0)
}

fn supervisor_interrupts_enabled() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        const RV64_SSTATUS_SIE: usize = 1 << 1;
        let sstatus: usize;
        unsafe {
            core::arch::asm!("csrr {sstatus}, sstatus", sstatus = out(reg) sstatus, options(nomem, nostack));
        }
        sstatus & RV64_SSTATUS_SIE != 0
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        true
    }
}

fn enable_supervisor_software_interrupts() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrsi sie, 2", "csrsi sstatus, 2", options(nomem, nostack));
    }
}

fn enable_supervisor_software_wakeups() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrsi sie, 2", "csrci sstatus, 2", options(nomem, nostack));
    }
}

fn supervisor_software_interrupt_pending() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        let sip: usize;
        unsafe {
            core::arch::asm!("csrr {sip}, sip", sip = out(reg) sip, options(nomem, nostack));
        }
        sip & 0x2 != 0
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        false
    }
}

fn mark_ipi_ack(cpu_id: CpuId) {
    if cpu_id.0 < u64::BITS as usize {
        IPI_ACKED_CPUS.fetch_or(1u64 << cpu_id.0, Ordering::AcqRel);
    }
}

fn start_secondary_hart(cpu: CpuId, entry: SecondaryEntry) -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        let start_addr = boot_static::secondary_start_entry();
        sbi_hart_start(cpu.0, start_addr, entry as usize) == 0
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = (cpu, entry);
        false
    }
}

fn wait_for_online_secondaries(target: CpuMask) -> usize {
    let target = target.bits();
    if target == 0 {
        return 0;
    }

    for _ in 0..100_000 {
        let online = ONLINE_CPUS.load(Ordering::Acquire) & target;
        if online == target {
            return online.count_ones() as usize;
        }
        core::hint::spin_loop();
    }
    (ONLINE_CPUS.load(Ordering::Acquire) & target).count_ones() as usize
}

fn remote_sfence_vma(invalidation: PmapInvalidation) {
    let targets = remote_sfence_targets();
    if targets.is_empty() {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let error = sbi_remote_sfence_vma(
            targets.bits(),
            0,
            invalidation.virt().0,
            invalidation.size(),
        );
        assert_eq!(error, 0, "SBI remote sfence.vma failed");
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = invalidation;
}

fn remote_sfence_vma_asid(asid: Asid, invalidation: PmapInvalidation) {
    let targets = remote_sfence_targets();
    if targets.is_empty() {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let error = sbi_remote_sfence_vma_asid(
            targets.bits(),
            0,
            invalidation.virt().0,
            invalidation.size(),
            asid.0 as usize,
        );
        assert_eq!(error, 0, "SBI remote sfence.vma.asid failed");
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = (asid, invalidation);
}

fn remote_sfence_targets() -> CpuMask {
    remote_sfence_targets_from(<Platform as SmpIf>::online_cpus(), current_cpu_id())
}

fn remote_sfence_targets_from(online: CpuMask, current: CpuId) -> CpuMask {
    CpuMask::from_bits(online.bits() & !CpuMask::single(current).bits())
}

fn send_sbi_ipi(mask: CpuMask) {
    let mask = mask.bits();
    if mask == 0 {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let _ = sbi_send_ipi(mask, 0);
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = mask;
}

fn rv64_fence_all() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence iorw, iorw", "fence.i", options(nostack));
    }
}

fn rv64_fence_i() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence.i", options(nomem, nostack));
    }
}

fn rv64_remote_fence_i() {
    rv64_fence_i();

    #[cfg(target_arch = "riscv64")]
    {
        let targets = remote_sfence_targets();
        if !targets.is_empty() {
            let error = sbi_remote_fence_i(targets.bits(), 0);
            assert_eq!(error, 0, "SBI remote fence.i failed");
        }
    }
}

fn clear_supervisor_software_interrupt() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrci sip, 2", options(nomem, nostack));
    }
}

#[cfg(target_arch = "riscv64")]
fn sbi_console_putchar(byte: u8) {
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") byte as usize => _,
            in("a7") 1usize,
            options(nostack)
        );
    }
}

fn read_sbi_console_bytes(buf: &mut [u8]) -> usize {
    let mut read = 0;
    for byte in buf {
        let Some(next) = sbi_console_getchar() else {
            break;
        };
        *byte = next;
        read += 1;
    }
    read
}

#[cfg(target_arch = "riscv64")]
fn sbi_console_getchar() -> Option<u8> {
    let value: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            lateout("a0") value,
            in("a7") 2usize,
            options(nostack)
        );
    }

    if value < 0 {
        None
    } else {
        Some(value as u8)
    }
}

#[cfg(not(target_arch = "riscv64"))]
fn sbi_console_getchar() -> Option<u8> {
    None
}

#[cfg(target_arch = "riscv64")]
fn sbi_hart_start(hart_id: usize, start_addr: usize, opaque: usize) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_id => error,
            in("a1") start_addr,
            in("a2") opaque,
            in("a6") 0usize,
            in("a7") 0x48534dusize,
            lateout("a1") _,
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
fn sbi_send_ipi(hart_mask: u64, hart_mask_base: usize) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a6") 0usize,
            in("a7") 0x735049usize,
            lateout("a1") _,
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
fn sbi_remote_fence_i(hart_mask: u64, hart_mask_base: usize) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a6") 0usize,
            in("a7") 0x52464e43usize,
            lateout("a1") _,
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
fn sbi_remote_sfence_vma(
    hart_mask: u64,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a2") start_addr,
            in("a3") size,
            in("a6") 1usize,
            in("a7") 0x52464e43usize,
            lateout("a1") _,
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
fn sbi_remote_sfence_vma_asid(
    hart_mask: u64,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
    asid: usize,
) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a2") start_addr,
            in("a3") size,
            in("a4") asid,
            in("a6") 2usize,
            in("a7") 0x52464e43usize,
            lateout("a1") _,
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
fn sbi_shutdown() {
    unsafe {
        core::arch::asm!("ecall", in("a7") 8usize, options(nostack));
    }
}

impl BootStaticBag<IdentityLive> {
    fn publish_boot_info_before_identity_drop(mut self, firmware_arg: usize) -> Self {
        debug_assert_eq!(self.firmware_dtb().addr(), firmware_arg);
        unsafe {
            self.publish_boot_info_from_fdt();
        }
        self
    }

    unsafe fn publish_boot_info_from_fdt(&mut self) {
        let dtb_addr = self.firmware_dtb().addr();
        let memory_regions = unsafe { self.memory_regions_mut() };
        let cmdline = unsafe { self.cmdline_mut() };
        memory_regions.fill(reserved_region());
        cmdline.fill(0);

        let parsed = parse_boot_info_from_fdt(dtb_addr, memory_regions, cmdline);
        let (memory_region_count, initrd, cmdline_len, timebase_frequency_hz, possible_cpu_count) =
            if let Some(parsed) = parsed {
                (
                    parsed.memory_region_count,
                    parsed.initrd,
                    parsed.cmdline_len.min(CMDLINE_CAPACITY),
                    parsed
                        .timebase_frequency_hz
                        .unwrap_or(time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ),
                    parsed.possible_cpu_count,
                )
            } else {
                memory_regions[0] = MemoryRegion {
                    base: PhysAddr(QEMU_VIRT_RAM_BASE),
                    size: QEMU_VIRT_FALLBACK_RAM_SIZE,
                    kind: MemoryRegionKind::Usable,
                };
                (1, None, 0, time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ, 1)
            };
        self.publish_timebase_frequency_hz(timebase_frequency_hz);
        self.publish_possible_cpu_count(possible_cpu_count);
        let memory_region_count =
            reserve_firmware_loader_region(memory_regions, memory_region_count);

        let cmdline = if cmdline_len > 0 {
            Some(core::str::from_utf8_unchecked(&cmdline[..cmdline_len]))
        } else {
            None
        };
        let memory_regions =
            core::slice::from_raw_parts(memory_regions.as_ptr(), memory_region_count);

        *unsafe { self.boot_info_mut() } = BootInfo {
            memory_regions,
            kernel_image: self.kernel_image_phys(),
            initrd,
            cmdline,
        };
    }
}

fn reserve_firmware_loader_region(
    memory_regions: &mut [MemoryRegion],
    memory_region_count: usize,
) -> usize {
    let Some(size) = pmap_topology::QEMU_KERNEL_PHYS_BASE.checked_sub(QEMU_VIRT_RAM_BASE) else {
        return memory_region_count;
    };
    if size == 0 || memory_region_count >= memory_regions.len() {
        return memory_region_count;
    }

    memory_regions[memory_region_count] = MemoryRegion {
        base: PhysAddr(QEMU_VIRT_RAM_BASE),
        size,
        kind: MemoryRegionKind::Reserved,
    };
    memory_region_count + 1
}

#[cfg(test)]
mod tests;
