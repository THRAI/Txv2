#![no_std]

#[cfg(test)]
extern crate std;

use core::sync::atomic::{AtomicU64, Ordering};

mod boot_static;
mod dtb;
mod pmap;
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
    AllocError, Arch, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf, BootPlatformIf,
    BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask, DmaIf, InitIf, IpiKind,
    IrqIf, MemoryRegion, MemoryRegionKind, PercpuIf, PhysAddr, PlatformConfig, PlatformInfo,
    PlatformInfoIf, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SecondaryEntry,
    SignalFrameIf, SmpIf, TimeIf,
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

7:
    wfi
    j 7b

"#
);

pub struct Platform;

const QEMU_VIRT_RAM_BASE: usize = 0x8000_0000;
const QEMU_VIRT_FALLBACK_RAM_SIZE: usize = 256 * 1024 * 1024;
const MAX_BOOT_CPUS: usize = 4;
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0);
static IPI_ACKED_CPUS: AtomicU64 = AtomicU64::new(0);

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

impl AuxvIf for Platform {}
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
impl SignalFrameIf for Platform {}
impl IrqIf for Platform {}
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
}
impl CacheIf for Platform {}
impl DmaIf for Platform {}
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
    #[cfg(target_arch = "riscv64")]
    {
        let cpu_id: usize;
        unsafe {
            core::arch::asm!("mv {cpu_id}, tp", cpu_id = out(reg) cpu_id, options(nomem, nostack));
        }
        CpuId(cpu_id)
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        CpuId(0)
    }
}

fn install_early_percpu(cpu_id: CpuId) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("mv tp, {cpu_id}", cpu_id = in(reg) cpu_id.0, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = cpu_id;
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
        CpuId, CpuMask, FaultInfo, IpiKind, KernelTrapSink, SmpIf, TrapAction, TrapClass,
        TrapFrameMut, TrapFrameSnapshot, TrapIf, TrapPreviousMode, VirtAddr,
    };

    use crate::{
        dispatch_trap_frame, mark_ipi_ack, remote_sfence_targets_from, trap::classify_rv64_trap,
        Platform, Rv64TrapFrame,
    };

    struct RecordingTrapSink;

    impl KernelTrapSink<Platform> for RecordingTrapSink {
        fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
            assert_eq!(view.view().previous_mode, TrapPreviousMode::User);
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
            TrapAction::Reschedule
        }

        fn on_external_irq(_cpu: CpuId) -> TrapAction {
            TrapAction::Resume
        }

        fn on_ipi(_cpu: CpuId) -> TrapAction {
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
        }

        assert_eq!(frame.sepc, 0x1111);
        assert_eq!(frame.x[2], 0x2222);
        assert_eq!(frame.x[10], 7);
        assert_eq!(frame.x[4], 0x3333);
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
        let interrupt_bit = 1usize << (usize::BITS as usize - 1);
        let mut frame = test_trap_frame(interrupt_bit | 5, 0x2000, 0);

        let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

        assert_eq!(action, TrapAction::Reschedule);
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
