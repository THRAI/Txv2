#![no_std]

#[cfg(test)]
extern crate std;

mod pmap;

use tx_hal::{
    AllocError, Arch, Asid, AuxvIf, BootHandoff, BootInfo, BootInfoIf, BootPlatformIf,
    BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, DmaIf, InitIf, IrqIf, MmioFlags,
    MmioRegion, PercpuIf, PhysAddr, PhysRange, PlatformConfig, PlatformInfo, PlatformInfoIf,
    PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation, PmapReserveKind,
    PmapRoot, PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SignalFrameIf, SmpIf, SpiSdInfo,
    TimeIf, TrapIf, UserAccessIf, VirtAddr, VirtRange,
};

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.trampoline, "ax"
    .equ TX_M1_KERNEL_VIRT_OFFSET, 0xffffffff00000000
    .equ TX_M1_QEMU_RAM_BASE, 0x80000000
    .equ TX_M1_UART0_BASE, 0x10000000
    .equ TX_M1_DIRECT_MAP_RAM_ROOT_SLOT, 258
    .equ TX_M1_DIRECT_MAP_MMIO_ROOT_SLOT, 256
    .equ TX_M1_IDENTITY_ROOT_SLOT, 2
    .equ TX_M1_KERNEL_ROOT_SLOT, 510
    .equ TX_M1_UART_L1_SLOT, 128
    .equ TX_M1_UART_L0_SLOT, 0
    .equ TX_M1_SATP_SV39, 0x8000000000000000
    .equ TX_M1_PTE_V, 0x001
    .equ TX_M1_PTE_R, 0x002
    .equ TX_M1_PTE_W, 0x004
    .equ TX_M1_PTE_X, 0x008
    .equ TX_M1_PTE_G, 0x020
    .equ TX_M1_PTE_A, 0x040
    .equ TX_M1_PTE_D, 0x080
    .equ TX_M1_PTE_IDENTITY, TX_M1_PTE_V | TX_M1_PTE_R | TX_M1_PTE_W | TX_M1_PTE_X | TX_M1_PTE_A | TX_M1_PTE_D
    .equ TX_M1_PTE_DIRECT, TX_M1_PTE_V | TX_M1_PTE_R | TX_M1_PTE_W | TX_M1_PTE_G | TX_M1_PTE_A | TX_M1_PTE_D
    .equ TX_M1_PTE_KERNEL_BOOT, TX_M1_PTE_V | TX_M1_PTE_R | TX_M1_PTE_W | TX_M1_PTE_X | TX_M1_PTE_G | TX_M1_PTE_A | TX_M1_PTE_D

    .globl _start
_start:
    mv s0, a0
    mv s1, a1

    la sp, __tx_boot_stack_top_load

    la t0, __bss_start_load
    la t1, __bss_end_load
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b

2:
    la s2, __m1_boot_root_load

    li t0, TX_M1_QEMU_RAM_BASE
    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_M1_PTE_IDENTITY
    li t2, TX_M1_IDENTITY_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_M1_PTE_DIRECT
    li t2, TX_M1_DIRECT_MAP_RAM_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_M1_PTE_KERNEL_BOOT
    li t2, TX_M1_KERNEL_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    la s3, __m1_boot_mmio_l1_load
    srli t1, s3, 12
    slli t1, t1, 10
    ori t1, t1, TX_M1_PTE_V
    li t2, TX_M1_DIRECT_MAP_MMIO_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    la s4, __m1_boot_uart_l0_load
    srli t1, s4, 12
    slli t1, t1, 10
    ori t1, t1, TX_M1_PTE_V
    li t2, TX_M1_UART_L1_SLOT
    slli t2, t2, 3
    add t3, s3, t2
    sd t1, 0(t3)

    li t0, TX_M1_UART0_BASE
    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_M1_PTE_DIRECT
    li t2, TX_M1_UART_L0_SLOT
    slli t2, t2, 3
    add t3, s4, t2
    sd t1, 0(t3)

    srli t0, s2, 12
    li t1, TX_M1_SATP_SV39
    or a2, t0, t1
    csrw satp, a2
    sfence.vma

    li t0, TX_M1_KERNEL_VIRT_OFFSET
    la sp, __tx_boot_stack_top_load
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

3:
    wfi
    j 3b
"#
);

pub struct Platform;

pub const SPI0_CS0_SD: SpiSdInfo = SpiSdInfo {
    controller: "spi0",
    chip_select: 0,
    mode: 0,
    max_hz: 25_000_000,
    qemu_backing: "target/images/m1dock-sd.img",
};

static PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: Platform::BOARD,
    spi_sd: Some(SPI0_CS0_SD),
    mmio_regions: MMIO_REGIONS,
};

// The mock runs on QEMU virt under OpenSBI. The smoke console stays SBI-backed,
// but the virt UART page is also precovered at its high direct-map alias so
// substrate phase 3 can consume ordinary platform MMIO facts.
static MMIO_REGIONS: &[MmioRegion] = &[MmioRegion {
    name: "uart0",
    phys: PhysRange {
        start: PhysAddr(pmap::QEMU_UART0_BASE),
        size: pmap::QEMU_UART0_SIZE,
    },
    virt: VirtRange {
        start: VirtAddr(pmap::direct_map_virt(pmap::QEMU_UART0_BASE)),
        size: pmap::QEMU_UART0_SIZE,
    },
    flags: MmioFlags::DEVICE_NGNRNE
        .union(MmioFlags::READ)
        .union(MmioFlags::WRITE),
}];

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "sipeed-m1-dock-mock";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = pmap::RV64_PHYS_ADDR_BITS;
    const VIRT_ADDR_BITS: u8 = pmap::SV39_VIRT_ADDR_BITS;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(pmap::DIRECT_MAP_BASE);
    const DIRECT_MAP_SIZE: usize = pmap::DIRECT_MAP_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(pmap::KERNEL_VIRT_BASE);
    const USER_TOP: VirtAddr = VirtAddr(pmap::SV39_USER_TOP);
    const USER_RESERVED_TOP_SIZE: usize = pmap::USER_RESERVED_TOP_SIZE;
    const USER_ALLOC_TOP: VirtAddr = VirtAddr(pmap::SV39_USER_ALLOC_TOP);
    const KERNEL_STACK_SIZE: usize = pmap::BOOT_STACK_SIZE;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 3;
    const ASID_BITS: u8 = pmap::RV64_ASID_BITS;
    const CACHE_LINE_SIZE: usize = pmap::CACHE_LINE_SIZE;
    const DMA_COHERENT: bool = true;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvSbi;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        pmap::ensure_static_boot_facts();
        BootHandoff {
            cpu_id: tx_hal::CpuId(cpu_id),
            firmware_arg: tx_hal::BootArg(firmware_arg),
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
        pmap::boot_info()
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO
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

    fn reserve_kernel_mapping(
        virt: VirtAddr,
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
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        pmap::unmap_kernel_mapping(virt, kind)
    }

    fn protect_kernel_mapping(
        virt: VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        pmap::protect_kernel_mapping(virt, kind, permissions)
    }

    fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
        pmap::shootdown_kernel_mapping(invalidation);
    }

    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        pmap::create_pmap_root()
    }

    fn destroy_pmap_root(root: PmapRoot) {
        pmap::destroy_pmap_root(root);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
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
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        pmap::unmap_mapping(root, virt, kind)
    }

    fn protect_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        pmap::protect_mapping(root, virt, kind, permissions)
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        pmap::shootdown_mapping(asid, invalidation);
    }
}

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
        #[cfg(target_arch = "riscv64")]
        sbi_shutdown();

        loop {
            core::hint::spin_loop();
        }
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
fn sbi_shutdown() {
    unsafe {
        core::arch::asm!("ecall", in("a7") 8usize, options(nostack));
    }
}
