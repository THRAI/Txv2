//! LA64 boot facts publication.
//!
//! This module turns captured boot arguments and firmware-discovered facts into
//! the stable HAL `BootInfo`, `PlatformInfo`, and `BootstrapPmapInfo` views.

use core::sync::atomic::{AtomicU8, Ordering};

#[cfg(all(target_arch = "loongarch64", feature = "la64-boot-trace"))]
use super::boot_args;
use super::boot_firmware::parse_firmware_boot_info;
use super::la64_irq_trap::{align_up, la64_detect_timebase_frequency_hz};
#[cfg(all(target_arch = "loongarch64", feature = "la64-boot-trace"))]
use super::la64_irq_trap::{console_write_decimal, console_write_hex, console_write_literal};
#[cfg(target_arch = "loongarch64")]
use super::la64_pmap::la64_kernel_addr_to_phys;
use super::la64_pmap::{la64_cached_virt, la64_dmw_direct_map, la64_dmw_mapped_phys};
use super::*;

static BOOT_FACTS_STATE: AtomicU8 = AtomicU8::new(0);
pub(crate) const LA64_BOOT_MEMORY_REGION_CAPACITY: usize = 8;
static mut BOOT_MEMORY_REGIONS: [MemoryRegion; LA64_BOOT_MEMORY_REGION_CAPACITY] = [
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
];

static mut BOOT_INFO: BootInfo = BootInfo::empty();
pub(crate) const LA64_BOOT_CMDLINE_CAPACITY: usize = 16384;
static mut BOOT_CMDLINE: [u8; LA64_BOOT_CMDLINE_CAPACITY] = [0; LA64_BOOT_CMDLINE_CAPACITY];

static mut BOOTSTRAP_PMAP_INFO: BootstrapPmapInfo = BootstrapPmapInfo {
    root: PhysAddr(0),
    mapped: PhysRange::empty(),
    direct_map_base: VirtAddr(0),
    direct_map: VirtRange::empty(),
    kernel_image: VirtRange::empty(),
    identity: None,
    pt_node_pool: PhysRange::empty(),
    reserved_page_tables: &[],
};

static mut PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: Platform::BOARD,
    spi_sd: None,
    mmio_regions: MMIO_REGIONS,
    timebase_frequency_hz: 0,
    possible_cpu_count: LA64_DEFAULT_POSSIBLE_CPUS,
};

const LA64_NO_BOOTSTRAP_PMAP_ROOT: PhysAddr = PhysAddr(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct La64BootstrapMapping {
    pub direct_map_base: VirtAddr,
    pub direct_map: VirtRange,
    pub kernel_image: VirtRange,
    pub dmw_backed: bool,
}

impl La64BootstrapMapping {
    pub(crate) const fn from_kernel_image(kernel_image: PhysRange) -> Self {
        Self {
            direct_map_base: VirtAddr(LA64_DMW_CACHED_BASE),
            direct_map: la64_dmw_direct_map(),
            kernel_image: VirtRange {
                start: VirtAddr(la64_cached_virt(kernel_image.start.0)),
                size: kernel_image.size,
            },
            dmw_backed: true,
        }
    }

    pub(crate) const fn bootstrap_root(self) -> PhysAddr {
        if self.dmw_backed {
            LA64_NO_BOOTSTRAP_PMAP_ROOT
        } else {
            PhysAddr(0)
        }
    }

    pub(crate) const fn direct_map_phys(self) -> PhysRange {
        let _ = self;
        la64_dmw_mapped_phys()
    }

    pub(crate) const fn to_bootstrap_pmap_info(self) -> BootstrapPmapInfo {
        BootstrapPmapInfo {
            root: self.bootstrap_root(),
            mapped: self.direct_map_phys(),
            direct_map_base: self.direct_map_base,
            direct_map: self.direct_map,
            kernel_image: self.kernel_image,
            identity: None,
            pt_node_pool: PhysRange::empty(),
            reserved_page_tables: &[],
        }
    }
}

pub(crate) fn boot_info() -> &'static BootInfo {
    ensure_static_boot_facts();

    unsafe { &*core::ptr::addr_of!(BOOT_INFO) }
}

pub(crate) fn platform_info() -> &'static PlatformInfo {
    ensure_static_boot_facts();

    unsafe { &*core::ptr::addr_of!(PLATFORM_INFO) }
}

pub(crate) fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
    ensure_static_boot_facts();

    unsafe { Some(&*core::ptr::addr_of!(BOOTSTRAP_PMAP_INFO)) }
}

pub(crate) fn platform_timebase_frequency_hz() -> u64 {
    unsafe { (*core::ptr::addr_of!(PLATFORM_INFO)).timebase_frequency_hz }
}

pub(crate) fn boot_memory_regions_ptr() -> *mut MemoryRegion {
    core::ptr::addr_of_mut!(BOOT_MEMORY_REGIONS) as *mut MemoryRegion
}

pub(crate) fn boot_cmdline_ptr() -> *mut u8 {
    core::ptr::addr_of_mut!(BOOT_CMDLINE) as *mut u8
}

pub(crate) fn boot_info_ptr() -> *mut BootInfo {
    core::ptr::addr_of_mut!(BOOT_INFO)
}

pub(crate) fn bootstrap_pmap_info_ptr() -> *mut BootstrapPmapInfo {
    core::ptr::addr_of_mut!(BOOTSTRAP_PMAP_INFO)
}

pub(crate) fn platform_info_ptr() -> *mut PlatformInfo {
    core::ptr::addr_of_mut!(PLATFORM_INFO)
}

pub(crate) fn ensure_static_boot_facts() {
    loop {
        match BOOT_FACTS_STATE.load(Ordering::Acquire) {
            2 => return,
            0 => {
                if BOOT_FACTS_STATE
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    publish_static_boot_facts();
                    BOOT_FACTS_STATE.store(2, Ordering::Release);
                    return;
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

pub(crate) fn publish_static_boot_facts() {
    let kernel_image = linked_kernel_image();
    let reserved_end = align_up(
        kernel_image.end().0,
        <Platform as PlatformConfig>::PAGE_SIZE,
    )
    .min(QEMU_LA64_RAM_END);
    trace_boot_args();
    let parsed_from_firmware = parse_firmware_boot_info(reserved_end);
    let (memory_region_count, initrd, cmdline_len, timebase_frequency_hz, possible_cpu_count) =
        parsed_from_firmware.unwrap_or_else(|| {
            let usable_size = QEMU_LA64_RAM_END.saturating_sub(reserved_end);
            unsafe {
                let regions = boot_memory_regions_ptr();
                core::ptr::write(
                    regions,
                    MemoryRegion {
                        base: PhysAddr(QEMU_LA64_RAM_BASE),
                        size: reserved_end - QEMU_LA64_RAM_BASE,
                        kind: MemoryRegionKind::Reserved,
                    },
                );
                core::ptr::write(
                    regions.add(1),
                    MemoryRegion {
                        base: PhysAddr(reserved_end),
                        size: usable_size,
                        kind: MemoryRegionKind::Usable,
                    },
                );
            }

            (
                2usize,
                None,
                0usize,
                la64_detect_timebase_frequency_hz(),
                LA64_DEFAULT_POSSIBLE_CPUS,
            )
        });

    LA64_TIMEBASE_HZ.store(timebase_frequency_hz, Ordering::Release);
    LA64_POSSIBLE_CPU_COUNT.store(possible_cpu_count, Ordering::Release);

    let bootstrap_mapping = La64BootstrapMapping::from_kernel_image(kernel_image);
    let cmdline = if cmdline_len > 0 {
        unsafe {
            Some(core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                boot_cmdline_ptr() as *const u8,
                cmdline_len,
            )))
        }
    } else {
        None
    };

    unsafe {
        let regions = boot_memory_regions_ptr() as *const MemoryRegion;

        core::ptr::write(
            boot_info_ptr(),
            BootInfo {
                memory_regions: core::slice::from_raw_parts(regions, memory_region_count),
                kernel_image,
                initrd,
                cmdline,
            },
        );

        core::ptr::write(
            bootstrap_pmap_info_ptr(),
            bootstrap_mapping.to_bootstrap_pmap_info(),
        );

        (*platform_info_ptr()).timebase_frequency_hz = timebase_frequency_hz;
        (*platform_info_ptr()).possible_cpu_count = possible_cpu_count;
    }

    write_boot_facts_summary(
        parsed_from_firmware.is_some(),
        memory_region_count,
        timebase_frequency_hz,
        possible_cpu_count,
        initrd,
        cmdline,
    );
}

#[cfg(all(target_arch = "loongarch64", feature = "la64-boot-trace"))]
fn trace_boot_args() {
    let args = boot_args::snapshot();
    let fw = args.legacy_firmware_arg.0;
    let efi_boot = args.efi_boot;
    let cmdline = args.cmdline_phys.0;
    let system_table = args.system_table_phys.0;
    console_write_literal(b"txkernel:qemu-loongarch64-virt:bootarg:a0=0x");
    console_write_hex(efi_boot);
    console_write_literal(b":a1=0x");
    console_write_hex(cmdline);
    console_write_literal(b":a2=0x");
    console_write_hex(system_table);
    console_write_literal(b":fw=0x");
    console_write_hex(fw);
    console_write_literal(b"\n");
}

#[cfg(not(all(target_arch = "loongarch64", feature = "la64-boot-trace")))]
fn trace_boot_args() {}

#[cfg(all(target_arch = "loongarch64", feature = "la64-boot-trace"))]
fn write_boot_facts_summary(
    parsed_from_dtb: bool,
    memory_region_count: usize,
    timebase_frequency_hz: u64,
    possible_cpu_count: usize,
    initrd: Option<PhysRange>,
    cmdline: Option<&str>,
) {
    console_write_literal(b"txkernel:qemu-loongarch64-virt:bootinfo:");
    if parsed_from_dtb {
        console_write_literal(b"dtb");
    } else {
        console_write_literal(b"fallback");
    }
    console_write_literal(b":regions=");
    console_write_decimal(memory_region_count);
    console_write_literal(b":timebase-hz=");
    console_write_decimal(timebase_frequency_hz as usize);
    console_write_literal(b":cpus=");
    console_write_decimal(possible_cpu_count);

    if let Some(range) = initrd {
        console_write_literal(b":initrd=0x");
        console_write_hex(range.start.0);
        console_write_literal(b"+0x");
        console_write_hex(range.size);
    } else {
        console_write_literal(b":initrd=none");
    }

    if let Some(value) = cmdline {
        console_write_literal(b":cmdline=\"");
        Platform::write_bytes(value.as_bytes());
        console_write_literal(b"\"");
    } else {
        console_write_literal(b":cmdline=none");
    }
    console_write_literal(b"\n");
}

#[cfg(not(all(target_arch = "loongarch64", feature = "la64-boot-trace")))]
fn write_boot_facts_summary(
    parsed_from_dtb: bool,
    memory_region_count: usize,
    timebase_frequency_hz: u64,
    possible_cpu_count: usize,
    initrd: Option<PhysRange>,
    cmdline: Option<&str>,
) {
    let _ = (
        parsed_from_dtb,
        memory_region_count,
        timebase_frequency_hz,
        possible_cpu_count,
        initrd,
        cmdline,
    );
}

pub(crate) fn linked_kernel_image() -> PhysRange {
    let start = linked_kernel_start();
    let end = linked_kernel_end();

    PhysRange {
        start: PhysAddr(start),
        size: end.saturating_sub(start),
    }
}

#[cfg(test)]
pub(crate) fn bootstrap_mapping_for_kernel_image(kernel_image: PhysRange) -> La64BootstrapMapping {
    La64BootstrapMapping::from_kernel_image(kernel_image)
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn linked_kernel_start() -> usize {
    la64_kernel_addr_to_phys(core::ptr::addr_of!(__kernel_start) as usize)
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn linked_kernel_start() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn linked_kernel_end() -> usize {
    la64_kernel_addr_to_phys(core::ptr::addr_of!(__kernel_end) as usize)
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn linked_kernel_end() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE + 128 * 1024
}
