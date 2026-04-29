#![no_std]

#[cfg(test)]
extern crate std;

mod boot_static;
mod dtb;
mod pmap;

use boot_static::{
    reserved_region, BootStaticBag, IdentityDropped, IdentityLive, CMDLINE_CAPACITY,
};
use dtb::parse_boot_info_from_fdt;
use pmap::topology as pmap_topology;
use tx_hal::{
    AllocError, Arch, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf, BootPlatformIf,
    BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, DmaIf, InitIf, IrqIf, MemoryRegion,
    MemoryRegionKind, PercpuIf, PhysAddr, PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError,
    PmapIf, PmapInvalidation, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot,
    PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SignalFrameIf, SmpIf, TimeIf, TrapClass,
    TrapFrameSnapshot, TrapIf, UserAccessIf,
};

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
    .globl _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop

    mv s0, a0
    mv s1, a1
    la sp, __tx_boot_stack_top

    la t0, __bss_start
    la t1, __bss_end
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b

2:
    addi sp, sp, -32
    mv a0, s1
    mv a1, sp
    call tx_rv64_qemu_prepare_high_boot
    ld s2, 0(sp)
    ld s3, 8(sp)
    ld s4, 16(sp)
    addi sp, sp, 32

    csrw satp, a0
    sfence.vma

    mv sp, s2
    .option push
    .option norelax
    mv gp, s3
    .option pop

    mv a0, s0
    mv a1, s1
    jr s4

3:
    wfi
    j 3b

    .section .text.trap, "ax"
    .align 2
    .globl tx_rv64_qemu_minimal_trap_vector
tx_rv64_qemu_minimal_trap_vector:
    csrr a0, scause
    csrr a1, sepc
    csrr a2, stval
    call tx_rv64_qemu_trap_panic
4:
    wfi
    j 4b
"#
);

pub struct Platform;

const QEMU_VIRT_RAM_BASE: usize = 0x8000_0000;
const QEMU_VIRT_FALLBACK_RAM_SIZE: usize = 256 * 1024 * 1024;

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "qemu-riscv64-virt";
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
    }
}
impl TrapIf for Platform {
    fn install_minimal_trap_vector() {
        install_rv64_trap_vector();
    }

    fn install_kernel_trap_vector() {
        install_rv64_trap_vector();
    }

    fn classify_trap(snapshot: TrapFrameSnapshot) -> TrapClass {
        classify_rv64_trap(snapshot.scause)
    }
}

fn classify_rv64_trap(scause: usize) -> TrapClass {
    let interrupt_bit = 1usize << (usize::BITS as usize - 1);
    let is_interrupt = scause & interrupt_bit != 0;
    let code = scause & !interrupt_bit;

    match (is_interrupt, code) {
        (false, 2) => TrapClass::IllegalInstruction,
        (false, 3) => TrapClass::Breakpoint,
        (false, 8) => TrapClass::UserEnvCall,
        (false, 12) => TrapClass::InstructionPageFault,
        (false, 13) => TrapClass::LoadPageFault,
        (false, 15) => TrapClass::StorePageFault,
        (true, 5) => TrapClass::SupervisorTimer,
        (true, 9) => TrapClass::SupervisorExternal,
        _ => TrapClass::Unknown,
    }
}
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

fn install_rv64_trap_vector() {
    let vector = BootStaticBag::<IdentityLive>::current_trap_vector_kernel_alias();

    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!(
            "csrw stvec, {vector}",
            vector = in(reg) vector.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = vector;
}

#[cfg(target_arch = "riscv64")]
#[no_mangle]
extern "C" fn tx_rv64_qemu_trap_panic(scause: usize, sepc: usize, stval: usize) -> ! {
    console_write_literal(b"txkernel:qemu-riscv64-virt:trap\nscause=0x");
    console_write_hex(scause);
    console_write_literal(b" sepc=0x");
    console_write_hex(sepc);
    console_write_literal(b" stval=0x");
    console_write_hex(stval);
    console_write_literal(b"\n");

    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "riscv64")]
fn console_write_literal(bytes: &[u8]) {
    for &byte in bytes {
        sbi_console_putchar(byte);
    }
}

#[cfg(target_arch = "riscv64")]
fn console_write_hex(value: usize) {
    for shift in (0..usize::BITS).rev().step_by(4) {
        let digit = ((value >> shift) & 0xf) as u8;
        let byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        sbi_console_putchar(byte);
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
        let (memory_region_count, initrd, cmdline_len) = if let Some(parsed) = parsed {
            (
                parsed.memory_region_count,
                parsed.initrd,
                parsed.cmdline_len.min(CMDLINE_CAPACITY),
            )
        } else {
            memory_regions[0] = MemoryRegion {
                base: PhysAddr(QEMU_VIRT_RAM_BASE),
                size: QEMU_VIRT_FALLBACK_RAM_SIZE,
                kind: MemoryRegionKind::Usable,
            };
            (1, None, 0)
        };
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
    use tx_hal::{TrapClass, TrapFrameSnapshot, TrapIf};

    use crate::{classify_rv64_trap, Platform};

    #[test]
    fn rv64_trap_classification_decodes_sync_faults_and_interrupts() {
        assert_eq!(classify_rv64_trap(2), TrapClass::IllegalInstruction);
        assert_eq!(classify_rv64_trap(12), TrapClass::InstructionPageFault);
        assert_eq!(classify_rv64_trap(13), TrapClass::LoadPageFault);
        assert_eq!(classify_rv64_trap(15), TrapClass::StorePageFault);

        let interrupt_bit = 1usize << (usize::BITS as usize - 1);
        assert_eq!(
            classify_rv64_trap(interrupt_bit | 5),
            TrapClass::SupervisorTimer
        );
        assert_eq!(
            Platform::classify_trap(TrapFrameSnapshot {
                scause: interrupt_bit | 9,
                sepc: 0x1000,
                stval: 0,
            }),
            TrapClass::SupervisorExternal
        );
    }
}
