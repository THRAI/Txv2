#![no_std]

#[cfg(test)]
extern crate std;

mod boot_static;
mod dtb;
mod pmap;
mod time;
mod trap;

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
    PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SignalFrameIf, SmpIf, TimeIf, UserAccessIf,
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

7:
    wfi
    j 7b

"#
);

pub struct Platform;

const QEMU_VIRT_RAM_BASE: usize = 0x8000_0000;
const QEMU_VIRT_FALLBACK_RAM_SIZE: usize = 256 * 1024 * 1024;

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
impl UserAccessIf for Platform {}
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

    fn frequency_hz() -> u64 {
        Self::platform_info().timebase_frequency_hz
    }
}
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
        let (memory_region_count, initrd, cmdline_len, timebase_frequency_hz) =
            if let Some(parsed) = parsed {
                (
                    parsed.memory_region_count,
                    parsed.initrd,
                    parsed.cmdline_len.min(CMDLINE_CAPACITY),
                    parsed
                        .timebase_frequency_hz
                        .unwrap_or(time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ),
                )
            } else {
                memory_regions[0] = MemoryRegion {
                    base: PhysAddr(QEMU_VIRT_RAM_BASE),
                    size: QEMU_VIRT_FALLBACK_RAM_SIZE,
                    kind: MemoryRegionKind::Usable,
                };
                (1, None, 0, time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ)
            };
        self.publish_timebase_frequency_hz(timebase_frequency_hz);
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
    use tx_hal::{TrapClass, TrapFrameSnapshot, TrapIf, VirtAddr};

    use crate::{trap::classify_rv64_trap, Platform};

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
}
