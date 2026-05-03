#![no_std]

#[cfg(test)]
extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

mod boot_static;
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
    DmaDirection, DmaIf, InitIf, IpiKind, IrqDispatchTable, IrqHandled, IrqIf, MemoryRegion,
    MemoryRegionKind, PercpuIf, PhysAddr, PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError,
    PmapIf, PmapInvalidation, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot,
    PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SecondaryEntry, SmpIf, TimeIf, VirtAddr,
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
}
impl IrqIf for Platform {
    const MAX_IRQ: u32 = PLIC_MAX_IRQ;

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

fn current_cpu_id() -> CpuId {
    let kernel_tls = read_kernel_tls();
    cpu_id_from_kernel_tls(kernel_tls).unwrap_or_else(|| {
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
    if offset % stride != 0 {
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

    let targets = remote_sfence_targets();
    if targets.is_empty() {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let error = sbi_remote_fence_i(targets.bits(), 0);
        assert_eq!(error, 0, "SBI remote fence.i failed");
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
mod tests {
    use tx_hal::{
        AuxvIf, CacheIf, ConsoleIf, CpuId, CpuMask, DmaAddr, DmaDirection, DmaIf, FaultInfo,
        IpiKind, IrqDispatchTable, IrqHandled, IrqIf, KernelTrapSink, PercpuIf, PhysAddr, SmpIf,
        TrapAction, TrapClass, TrapFrameMut, TrapFrameSnapshot, TrapIf, TrapPreviousMode, VirtAddr,
    };

    use crate::{
        dispatch_trap_frame, enter_irq_context, mark_ipi_ack, percpu_tls_for_cpu,
        remote_sfence_targets_from, trap::classify_rv64_trap, Platform, Rv64TrapFrame,
        RV64_PERCPU_AREAS,
    };

    static RV64_HAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static IRQ_HANDLER_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn wake_irq_handler(irq: u32) -> IrqHandled {
        assert_eq!(irq, 8);
        IRQ_HANDLER_COUNT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        IrqHandled::Wake
    }

    struct RecordingTrapSink;

    impl KernelTrapSink<Platform> for RecordingTrapSink {
        fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
            assert_eq!(view.view().previous_mode, TrapPreviousMode::User);
            assert_eq!(view.view().fault_address, Some(VirtAddr(0xfeed_cafe)));
            assert_eq!(view.view().faulting_instruction, Some(VirtAddr(0x3000)));
            assert_eq!(fault.address, VirtAddr(0xfeed_cafe));
            assert!(fault.write);
            assert!(!fault.instruction);
            assert!(fault.from_user);
            TrapAction::Terminate
        }

        fn on_syscall(mut view: TrapFrameMut<'_>) -> TrapAction {
            assert_eq!(view.view().syscall_number, 64);
            assert_eq!(view.view().syscall_args, [1, 2, 3, 4, 5, 6]);
            view.set_syscall_return(123);
            TrapAction::Resume
        }

        fn on_timer_interrupt(_cpu: CpuId) -> TrapAction {
            assert!(<Platform as IrqIf>::in_irq_context());
            TrapAction::Reschedule
        }

        fn on_external_irq(_cpu: CpuId) -> TrapAction {
            assert!(<Platform as IrqIf>::in_irq_context());
            TrapAction::Resume
        }

        fn on_ipi(_cpu: CpuId) -> TrapAction {
            assert!(<Platform as IrqIf>::in_irq_context());
            TrapAction::Resume
        }

        fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
            assert_eq!(view.view().pc, VirtAddr(0x4040));
            assert_eq!(fault.address, VirtAddr(0x4040));
            assert!(fault.instruction);
            TrapAction::Terminate
        }
    }

    #[test]
    fn rv64_trap_classification_decodes_sync_faults_and_interrupts() {
        assert_eq!(classify_rv64_trap(2), TrapClass::IllegalInstruction);
        assert_eq!(classify_rv64_trap(3), TrapClass::Breakpoint);
        assert_eq!(
            classify_rv64_trap(4),
            TrapClass::AlignmentFault {
                write: false,
                instruction: false,
            }
        );
        assert_eq!(
            classify_rv64_trap(6),
            TrapClass::AlignmentFault {
                write: true,
                instruction: false,
            }
        );
        assert_eq!(
            classify_rv64_trap(0),
            TrapClass::AlignmentFault {
                write: false,
                instruction: true,
            }
        );
        assert_eq!(classify_rv64_trap(8), TrapClass::Syscall);
        assert_eq!(
            classify_rv64_trap(12),
            TrapClass::PageFault {
                write: false,
                instruction: true,
            }
        );
        assert_eq!(
            classify_rv64_trap(13),
            TrapClass::PageFault {
                write: false,
                instruction: false,
            }
        );
        assert_eq!(
            classify_rv64_trap(15),
            TrapClass::PageFault {
                write: true,
                instruction: false,
            }
        );

        let interrupt_bit = 1usize << (usize::BITS as usize - 1);
        assert_eq!(
            classify_rv64_trap(interrupt_bit | 1),
            TrapClass::InterprocessorInterrupt
        );
        assert_eq!(
            classify_rv64_trap(interrupt_bit | 5),
            TrapClass::TimerInterrupt
        );
        assert_eq!(
            Platform::classify_trap(TrapFrameSnapshot {
                scause: interrupt_bit | 9,
                sepc: 0x1000,
                stval: 0,
            }),
            TrapClass::ExternalInterrupt
        );
    }

    #[test]
    fn rv64_trap_classification_distinguishes_unknown_sync_and_interrupt() {
        let interrupt_bit = 1usize << (usize::BITS as usize - 1);

        assert_eq!(classify_rv64_trap(63), TrapClass::UnknownSync);
        assert_eq!(
            classify_rv64_trap(interrupt_bit | 63),
            TrapClass::UnknownInterrupt
        );
    }

    #[test]
    fn trap_class_legacy_names_remain_compatible() {
        assert_eq!(
            TrapClass::InstructionPageFault,
            TrapClass::PageFault {
                write: false,
                instruction: true,
            }
        );
        assert_eq!(
            TrapClass::LoadPageFault,
            TrapClass::PageFault {
                write: false,
                instruction: false,
            }
        );
        assert_eq!(
            TrapClass::StorePageFault,
            TrapClass::PageFault {
                write: true,
                instruction: false,
            }
        );
        assert_eq!(TrapClass::UserEnvCall, TrapClass::Syscall);
        assert_eq!(TrapClass::SupervisorTimer, TrapClass::TimerInterrupt);
        assert_eq!(TrapClass::SupervisorExternal, TrapClass::ExternalInterrupt);
        assert_eq!(TrapClass::Unknown, TrapClass::UnknownSync);
    }

    #[test]
    fn platform_trap_snapshot_projects_portable_fault_fields() {
        let snapshot = TrapFrameSnapshot {
            scause: 15,
            sepc: 0x2000,
            stval: 0xfeed_cafe,
        };

        let portable = Platform::snapshot_trap(snapshot);

        assert_eq!(
            portable.class,
            TrapClass::PageFault {
                write: true,
                instruction: false,
            }
        );
        assert_eq!(portable.pc, VirtAddr(0x2000));
        assert_eq!(portable.fault_address, Some(VirtAddr(0xfeed_cafe)));
    }

    #[test]
    fn percpu_install_sets_kernel_tls_pointer_and_current_cpu() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

        <Platform as PercpuIf>::install_early_percpu(CpuId(2));

        let kernel_tls = <Platform as PercpuIf>::read_kernel_tls() as usize;
        assert_eq!(Some(kernel_tls), percpu_tls_for_cpu(CpuId(2)));
        assert_eq!(<Platform as PercpuIf>::current_cpu_id(), CpuId(2));
        assert_eq!(<Platform as SmpIf>::current_cpu_id(), CpuId(2));

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    #[cfg(not(target_arch = "riscv64"))]
    fn percpu_install_kernel_stack_records_stack_top() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

        <Platform as PercpuIf>::install_early_percpu(CpuId(1));
        unsafe {
            <Platform as PercpuIf>::install_kernel_stack(VirtAddr(0x8000_4000));
        }

        assert_eq!(
            RV64_PERCPU_AREAS[1].kernel_stack_top(),
            VirtAddr(0x8000_4000)
        );

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    fn irq_context_guard_tracks_nested_interrupt_depth() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

        <Platform as PercpuIf>::install_early_percpu(CpuId(3));
        assert!(!<Platform as IrqIf>::in_irq_context());

        {
            let _outer = enter_irq_context();
            assert!(<Platform as IrqIf>::in_irq_context());
            assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 1);

            {
                let _inner = enter_irq_context();
                assert!(<Platform as IrqIf>::in_irq_context());
                assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 2);
            }

            assert!(<Platform as IrqIf>::in_irq_context());
            assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 1);
        }

        assert!(!<Platform as IrqIf>::in_irq_context());
        assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 0);

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    #[cfg(not(target_arch = "riscv64"))]
    fn plic_priority_enable_claim_and_complete_use_current_context() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .reset();

        <Platform as PercpuIf>::install_early_percpu(CpuId(1));
        let context = super::plic_context_for_cpu(CpuId(1));

        <Platform as IrqIf>::set_priority(8, 3);
        assert_eq!(
            super::HOST_PLIC_STATE
                .lock()
                .expect("host plic state")
                .read_u32(super::plic_priority_offset(8)),
            3
        );

        <Platform as IrqIf>::unmask(8);
        let enable_offset = super::plic_enable_word_offset(context, 0);
        assert_ne!(
            super::HOST_PLIC_STATE
                .lock()
                .expect("host plic state")
                .read_u32(enable_offset)
                & (1 << 8),
            0
        );

        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .write_u32(super::plic_claim_complete_offset(context), 8);
        assert_eq!(<Platform as IrqIf>::claim(), 8);

        <Platform as IrqIf>::complete(8);
        assert_eq!(
            super::HOST_PLIC_STATE
                .lock()
                .expect("host plic state")
                .read_u32(super::plic_claim_complete_offset(context)),
            8
        );

        <Platform as IrqIf>::mask(8);
        assert_eq!(
            super::HOST_PLIC_STATE
                .lock()
                .expect("host plic state")
                .read_u32(enable_offset)
                & (1 << 8),
            0
        );

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    #[cfg(not(target_arch = "riscv64"))]
    fn plic_dispatch_table_invokes_handler_and_masks_unhandled_irq() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .reset();
        IRQ_HANDLER_COUNT.store(0, std::sync::atomic::Ordering::Release);

        <Platform as PercpuIf>::install_early_percpu(CpuId(0));
        let context = super::plic_context_for_cpu(CpuId(0));
        let mut table = IrqDispatchTable::new();
        table.entries[8] = Some(wake_irq_handler);
        let table = std::boxed::Box::leak(std::boxed::Box::new(table));
        <Platform as IrqIf>::install_dispatch_table(table);

        assert_eq!(<Platform as IrqIf>::dispatch_irq(8), IrqHandled::Wake);
        assert_eq!(
            IRQ_HANDLER_COUNT.load(std::sync::atomic::Ordering::Acquire),
            1
        );

        <Platform as IrqIf>::unmask(9);
        let enable_offset = super::plic_enable_word_offset(context, 0);
        assert_ne!(
            super::HOST_PLIC_STATE
                .lock()
                .expect("host plic state")
                .read_u32(enable_offset)
                & (1 << 9),
            0
        );

        assert_eq!(<Platform as IrqIf>::dispatch_irq(9), IrqHandled::Done);
        assert_eq!(
            super::HOST_PLIC_STATE
                .lock()
                .expect("host plic state")
                .read_u32(enable_offset)
                & (1 << 9),
            0
        );

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    fn cache_methods_are_callable_on_qemu_coherent_platform() {
        <Platform as CacheIf>::fence_all();
        <Platform as CacheIf>::fence_i_local();
        <Platform as CacheIf>::fence_i_all();
        <Platform as CacheIf>::flush_icache_range(VirtAddr(0x8020_0000), 4096);
        <Platform as CacheIf>::dcache_clean_range(PhysAddr(0x8020_0000), 4096);
        <Platform as CacheIf>::dcache_invalidate_range(PhysAddr(0x8020_0000), 4096);
        <Platform as CacheIf>::dcache_clean_invalidate_range(PhysAddr(0x8020_0000), 4096);
    }

    #[test]
    fn dma_identity_mapping_and_sync_are_qemu_coherent() {
        assert!(<Platform as DmaIf>::DMA_COHERENT);
        assert_eq!(
            <Platform as DmaIf>::phys_to_dma(PhysAddr(0x8020_1000)),
            DmaAddr(0x8020_1000)
        );
        assert_eq!(
            <Platform as DmaIf>::dma_to_phys(DmaAddr(0x8020_2000)),
            PhysAddr(0x8020_2000)
        );

        <Platform as DmaIf>::sync_for_device(PhysAddr(0x8020_3000), 512, DmaDirection::ToDevice);
        <Platform as DmaIf>::sync_for_cpu(PhysAddr(0x8020_3000), 512, DmaDirection::FromDevice);
        <Platform as DmaIf>::sync_for_device(
            PhysAddr(0x8020_3000),
            512,
            DmaDirection::Bidirectional,
        );
        <Platform as DmaIf>::sync_for_cpu(PhysAddr(0x8020_3000), 512, DmaDirection::Bidirectional);
    }

    #[test]
    fn auxv_facts_publish_riscv64_platform_and_hwcap() {
        let facts = <Platform as AuxvIf>::arch_auxv_facts();

        assert_eq!(
            facts.page_size,
            <Platform as tx_hal::PlatformConfig>::PAGE_SIZE
        );
        assert_eq!(facts.hwcap, tx_hal::RISCV_HWCAP_IMAFDC);
        assert_eq!(facts.hwcap2, 0);
        assert_eq!(facts.platform, "riscv64");
    }

    #[test]
    #[cfg(not(target_arch = "riscv64"))]
    fn console_read_bytes_is_nonblocking_when_host_has_no_sbi_input() {
        let mut buf = [0xaa; 4];

        assert_eq!(<Platform as ConsoleIf>::read_bytes(&mut buf), 0);
        assert_eq!(tx_hal::console_read_bytes::<Platform>(&mut buf), 0);
        assert_eq!(buf, [0xaa; 4]);
    }

    #[test]
    fn trap_frame_view_projects_rv64_trap_metadata() {
        let mut frame = test_trap_frame(15, 0x3000, 0xfeed_cafe);
        frame.sstatus &= !(1 << 8);
        frame.sstatus |= 1 << 5;
        frame.x[2] = 0x7000;
        frame.x[4] = 0x1234_5678;

        let view = frame.view();

        assert_eq!(view.pc, VirtAddr(0x3000));
        assert_eq!(view.sp, VirtAddr(0x7000));
        assert_eq!(view.fault_address, Some(VirtAddr(0xfeed_cafe)));
        assert_eq!(view.faulting_instruction, Some(VirtAddr(0x3000)));
        assert_eq!(view.previous_mode, TrapPreviousMode::User);
        assert!(view.interrupts_enabled_before);
        assert_eq!(view.user_tls_register, 0x1234_5678);
    }

    #[test]
    fn trap_frame_view_omits_fault_fields_for_interrupts_and_syscalls() {
        let interrupt_bit = 1usize << (usize::BITS as usize - 1);

        let syscall = test_trap_frame(8, 0x1000, 0xaaaa);
        let syscall_view = syscall.view();
        assert_eq!(syscall_view.fault_address, None);
        assert_eq!(syscall_view.faulting_instruction, Some(VirtAddr(0x1000)));

        let interrupt = test_trap_frame(interrupt_bit | 5, 0x2000, 0xbbbb);
        let interrupt_view = interrupt.view();
        assert_eq!(interrupt_view.fault_address, None);
        assert_eq!(interrupt_view.faulting_instruction, None);
    }

    #[test]
    fn trap_frame_view_projects_syscall_fields() {
        let mut frame = test_trap_frame(8, 0x1000, 0);
        frame.x[10] = 1;
        frame.x[11] = 2;
        frame.x[12] = 3;
        frame.x[13] = 4;
        frame.x[14] = 5;
        frame.x[15] = 6;
        frame.x[17] = 64;

        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

        assert_eq!(action, TrapAction::Resume);
        assert_eq!(frame.x[10], 123);
    }

    #[test]
    fn trap_frame_mutators_write_saved_registers() {
        let mut frame = test_trap_frame(8, 0x1000, 0);

        {
            let mut view = frame.view_mut();
            view.set_pc(VirtAddr(0x1111));
            view.set_sp(VirtAddr(0x2222));
            view.set_syscall_return(7);
            view.set_user_tls_register(0x3333);

            assert_eq!(view.view().pc, VirtAddr(0x1111));
            assert_eq!(view.view().sp, VirtAddr(0x2222));
            assert_eq!(view.view().syscall_args[0], 7);
            assert_eq!(view.view().user_tls_register, 0x3333);
        }

        assert_eq!(frame.sepc, 0x1111);
        assert_eq!(frame.x[2], 0x2222);
        assert_eq!(frame.x[10], 7);
        assert_eq!(frame.x[4], 0x3333);
    }

    #[test]
    fn trap_frame_signal_context_round_trips_user_registers() {
        let mut frame = test_trap_frame(8, 0x1000, 0);
        for (idx, reg) in frame.x.iter_mut().enumerate() {
            *reg = 0x1000 + idx;
        }
        frame.x[0] = 0;
        frame.x[2] = 0x8000;
        frame.sstatus &= !(1 << 8);

        let saved = frame.view_mut().capture_user_context();

        frame.x[1] = 0xaaaa;
        frame.x[2] = 0xbbbb;
        frame.sepc = 0xcccc;
        frame.sstatus |= 1 << 8;

        frame.view_mut().restore_user_context(&saved);

        assert_eq!(frame.x, saved.regs);
        assert_eq!(frame.sepc, 0x1000);
        assert_eq!(frame.x[2], 0x8000);
        assert_eq!(frame.sstatus & (1 << 8), 0);
        assert_ne!(frame.sstatus & (1 << 5), 0);
    }

    #[test]
    fn trap_frame_signal_handler_regs_write_entry_arguments() {
        let mut frame = test_trap_frame(8, 0x4000, 0);

        {
            let mut view = frame.view_mut();
            view.set_signal_handler_regs(tx_hal::SignalHandlerRegs {
                return_pc: VirtAddr(0x7000),
                args: [9, 0x7100, 0x7200],
            });
            view.set_pc(VirtAddr(0x6000));
            view.set_sp(VirtAddr(0x5ff0));
        }

        assert_eq!(frame.sepc, 0x6000);
        assert_eq!(frame.x[1], 0x7000);
        assert_eq!(frame.x[2], 0x5ff0);
        assert_eq!(frame.x[10], 9);
        assert_eq!(frame.x[11], 0x7100);
        assert_eq!(frame.x[12], 0x7200);
    }

    #[test]
    fn trap_frame_rewind_pc_steps_back_one_rv64_instruction() {
        let mut frame = test_trap_frame(8, 0x4004, 0);

        frame.view_mut().rewind_pc(4);

        assert_eq!(frame.sepc, 0x4000);
    }

    #[test]
    fn trap_frame_mutators_encode_syscall_error() {
        let mut frame = test_trap_frame(8, 0x1000, 0);

        frame.view_mut().set_syscall_error(5);

        assert_eq!(frame.x[10], (-5isize) as usize);
    }

    #[test]
    fn trap_frame_prepare_user_return_sets_sret_mode_bits() {
        let mut frame = test_trap_frame(8, 0x1000, 0);

        frame.prepare_user_return();

        assert_eq!(frame.sstatus & (1 << 8), 0);
        assert_ne!(frame.sstatus & (1 << 5), 0);
        assert_eq!(frame.previous_mode(), TrapPreviousMode::User);
    }

    #[test]
    fn trap_dispatch_routes_timer_to_sink_action() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
        <Platform as PercpuIf>::install_early_percpu(CpuId(0));
        let interrupt_bit = 1usize << (usize::BITS as usize - 1);
        let mut frame = test_trap_frame(interrupt_bit | 5, 0x2000, 0);

        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

        assert_eq!(action, TrapAction::Reschedule);
        assert!(!<Platform as IrqIf>::in_irq_context());
        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    fn trap_dispatch_marks_external_and_ipi_as_irq_context() {
        let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
        <Platform as PercpuIf>::install_early_percpu(CpuId(0));
        let interrupt_bit = 1usize << (usize::BITS as usize - 1);

        let mut external = test_trap_frame(interrupt_bit | 9, 0x2000, 0);
        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut external);
        assert_eq!(action, TrapAction::Resume);
        assert!(!<Platform as IrqIf>::in_irq_context());

        let mut ipi = test_trap_frame(interrupt_bit | 1, 0x2000, 0);
        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut ipi);
        assert_eq!(action, TrapAction::Resume);
        assert!(!<Platform as IrqIf>::in_irq_context());

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    fn trap_dispatch_routes_page_fault_with_user_flag() {
        let mut frame = test_trap_frame(15, 0x3000, 0xfeed_cafe);
        frame.sstatus &= !(1 << 8);

        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

        assert_eq!(action, TrapAction::Terminate);
    }

    #[test]
    fn trap_dispatch_routes_sync_fault_to_illegal_or_sync_sink() {
        let mut frame = test_trap_frame(2, 0x4040, 0);

        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

        assert_eq!(action, TrapAction::Terminate);
    }

    #[test]
    fn remote_sfence_targets_exclude_current_hart() {
        let targets = remote_sfence_targets_from(CpuMask::from_bits(0b1111), CpuId(2));

        assert_eq!(targets.bits(), 0b1011);
    }

    #[test]
    fn remote_sfence_targets_are_empty_for_uniprocessor_online_mask() {
        let targets = remote_sfence_targets_from(CpuMask::single(CpuId(0)), CpuId(0));

        assert!(targets.is_empty());
    }

    #[test]
    fn ipi_ack_observation_can_be_cleared_by_mask() {
        <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::from_bits(u64::MAX));
        mark_ipi_ack(CpuId(1));
        mark_ipi_ack(CpuId(3));

        assert_eq!(
            <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule).bits(),
            0b1010
        );

        <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::single(CpuId(1)));

        assert_eq!(
            <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule).bits(),
            0b1000
        );
    }

    fn test_trap_frame(scause: usize, sepc: usize, stval: usize) -> Rv64TrapFrame {
        Rv64TrapFrame {
            x: [0; 32],
            scause,
            sepc,
            stval,
            sstatus: 1 << 8,
        }
    }
}
