#![no_std]

#[cfg(test)]
extern crate std;

mod dtb;
mod pmap;

use core::cell::UnsafeCell;

use dtb::parse_boot_info_from_fdt;
use tx_hal::{
    AllocError, Arch, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf, BootPlatformIf,
    BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, DmaIf, InitIf, IrqIf, MemoryRegion,
    MemoryRegionKind, PercpuIf, PhysAddr, PhysRange, PlatformConfig, PlatformInfo, PlatformInfoIf,
    PmapIf, PowerIf, PtNode, SignalFrameIf, SmpIf, TimeIf, TrapIf, UserAccessIf,
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
    call tx_rv64_qemu_bootstrap_satp
    csrw satp, a0
    sfence.vma

    mv a0, s0
    mv a1, s1
    call rust_entry

3:
    wfi
    j 3b
"#
);

pub struct Platform;

const MAX_MEMORY_REGIONS: usize = 8;
const CMDLINE_CAPACITY: usize = 256;
const QEMU_VIRT_RAM_BASE: usize = 0x8000_0000;
const QEMU_VIRT_FALLBACK_RAM_SIZE: usize = 256 * 1024 * 1024;

struct BootInfoCell(UnsafeCell<BootInfo>);
struct MemoryRegionsCell(UnsafeCell<[MemoryRegion; MAX_MEMORY_REGIONS]>);
struct CmdlineCell(UnsafeCell<[u8; CMDLINE_CAPACITY]>);

unsafe impl Sync for BootInfoCell {}
unsafe impl Sync for MemoryRegionsCell {}
unsafe impl Sync for CmdlineCell {}

static BOOT_INFO: BootInfoCell = BootInfoCell(UnsafeCell::new(BootInfo::empty()));
static MEMORY_REGIONS: MemoryRegionsCell =
    MemoryRegionsCell(UnsafeCell::new([reserved_region(); MAX_MEMORY_REGIONS]));
static CMDLINE: CmdlineCell = CmdlineCell(UnsafeCell::new([0; CMDLINE_CAPACITY]));

static PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: Platform::BOARD,
    spi_sd: None,
};

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "qemu-riscv64-virt";
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvSbi;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        unsafe {
            publish_boot_info(firmware_arg);
        }
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
        unsafe { &*BOOT_INFO.0.get() }
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

const fn reserved_region() -> MemoryRegion {
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    }
}

unsafe fn publish_boot_info(dtb_addr: usize) {
    let memory_regions = &mut *MEMORY_REGIONS.0.get();
    let cmdline = &mut *CMDLINE.0.get();
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

    let cmdline = if cmdline_len > 0 {
        Some(core::str::from_utf8_unchecked(&cmdline[..cmdline_len]))
    } else {
        None
    };
    let memory_regions = core::slice::from_raw_parts(memory_regions.as_ptr(), memory_region_count);

    *BOOT_INFO.0.get() = BootInfo {
        memory_regions,
        kernel_image: kernel_image_range(),
        initrd,
        cmdline,
    };
}

fn kernel_image_range() -> PhysRange {
    #[cfg(target_arch = "riscv64")]
    {
        unsafe extern "C" {
            static __kernel_start: u8;
            static __kernel_end: u8;
        }

        let start = core::ptr::addr_of!(__kernel_start) as usize;
        let end = core::ptr::addr_of!(__kernel_end) as usize;
        PhysRange {
            start: PhysAddr(start),
            size: end.saturating_sub(start),
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        PhysRange::empty()
    }
}
