//! Immutable boot facts recovered from the 2K1000 U-Boot EFI handoff.

use core::sync::atomic::{AtomicU8, Ordering};

use fdt::nodes::AsNode;

use super::la64_irq_trap::{
    console_write_decimal, console_write_hex, console_write_literal,
    la64_detect_timebase_frequency_hz,
};
use super::la64_pmap::{la64_cached_virt, la64_dmw_direct_map, la64_dmw_mapped_phys};
use super::*;

const CMDLINE_CAPACITY: usize = 4096;
const FDT_HEADER_SIZE: usize = 40;
const FDT_MAGIC: u32 = 0xd00d_feed;
const EFI_SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
const EFI_MAX_CONFIG_TABLES: usize = 32;
const DEVICE_TREE_GUID: [u8; 16] = [
    0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0,
];

static STATE: AtomicU8 = AtomicU8::new(0);
static mut CMDLINE: [u8; CMDLINE_CAPACITY] = [0; CMDLINE_CAPACITY];
static MEMORY_REGIONS: [MemoryRegion; 6] = [
    MemoryRegion {
        base: PhysAddr(LA2K1000_RAM0_BASE),
        size: LA2K1000_FDT_SCRATCH_BASE,
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(LA2K1000_FDT_SCRATCH_BASE),
        size: LA2K1000_FDT_SCRATCH_SIZE,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(LA2K1000_FDT_SCRATCH_BASE + LA2K1000_FDT_SCRATCH_SIZE),
        size: LA2K1000_FRAMEBUFFER_BASE - (LA2K1000_FDT_SCRATCH_BASE + LA2K1000_FDT_SCRATCH_SIZE),
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(LA2K1000_FRAMEBUFFER_BASE),
        size: LA2K1000_FRAMEBUFFER_SIZE,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(LA2K1000_BOOTPARAM_BASE),
        size: LA2K1000_BOOTPARAM_SIZE,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(LA2K1000_RAM1_BASE),
        size: LA2K1000_RAM1_SIZE,
        kind: MemoryRegionKind::Usable,
    },
];
static mut BOOT_INFO: BootInfo = BootInfo::empty();
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
    mmio_regions: &MMIO_REGIONS,
    device_resources: &EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiTableHeader {
    signature: u64,
    revision: u32,
    header_size: u32,
    crc32: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiSystemTable {
    hdr: EfiTableHeader,
    fw_vendor: u64,
    fw_revision: u32,
    _pad: u32,
    con_in_handle: u64,
    con_in: u64,
    con_out_handle: u64,
    con_out: u64,
    stderr_handle: u64,
    stderr: u64,
    runtime: u64,
    boottime: u64,
    nr_tables: u64,
    tables: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiConfigurationTable {
    guid: [u8; 16],
    table: u64,
}

#[derive(Clone, Copy)]
struct FdtBlob {
    phys: usize,
    size: usize,
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

pub(crate) fn linked_kernel_image() -> PhysRange {
    #[cfg(target_arch = "loongarch64")]
    {
        let start = (&raw const __kernel_start) as usize & LA64_PHYS_ADDR_MASK;
        let end = (&raw const __kernel_end) as usize & LA64_PHYS_ADDR_MASK;
        PhysRange {
            start: PhysAddr(start),
            size: end.saturating_sub(start),
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        PhysRange {
            start: PhysAddr(LA2K1000_KERNEL_LOAD_BASE),
            size: 2 * 1024 * 1024,
        }
    }
}

pub(crate) fn ensure_static_boot_facts() {
    loop {
        match STATE.load(Ordering::Acquire) {
            2 => return,
            0 => {
                if STATE
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    publish();
                    STATE.store(2, Ordering::Release);
                    return;
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

fn publish() {
    let args = super::boot_args::snapshot();
    let kernel_image = linked_kernel_image();
    let cmdline_len =
        unsafe { copy_cmdline(args.cmdline.0, &mut *core::ptr::addr_of_mut!(CMDLINE)) };
    let fdt = unsafe { find_efi_fdt(args.system_table.0) };
    let initrd = fdt.and_then(|blob| unsafe { parse_fdt_initrd(blob, kernel_image) });
    let timebase = la64_detect_timebase_frequency_hz();
    let cmdline = if cmdline_len == 0 {
        None
    } else {
        let bytes = unsafe {
            core::slice::from_raw_parts(core::ptr::addr_of!(CMDLINE).cast::<u8>(), cmdline_len)
        };
        match core::str::from_utf8(bytes) {
            Ok(value) => Some(value),
            Err(_) => {
                console_write_literal(
                    b"txkernel:loongson-2k1000:h2:bootinfo:cmdline-invalid-utf8\n",
                );
                None
            }
        }
    };
    let kernel_virt = VirtRange {
        start: VirtAddr(la64_cached_virt(kernel_image.start.0)),
        size: kernel_image.size,
    };

    unsafe {
        core::ptr::write(
            core::ptr::addr_of_mut!(BOOT_INFO),
            BootInfo {
                memory_regions: &MEMORY_REGIONS,
                kernel_image,
                initrd,
                cmdline,
            },
        );
        core::ptr::write(
            core::ptr::addr_of_mut!(BOOTSTRAP_PMAP_INFO),
            BootstrapPmapInfo {
                root: PhysAddr(0),
                mapped: la64_dmw_mapped_phys(),
                direct_map_base: VirtAddr(LA64_DMW_CACHED_BASE),
                direct_map: la64_dmw_direct_map(),
                kernel_image: kernel_virt,
                identity: None,
                pt_node_pool: PhysRange::empty(),
                reserved_page_tables: &[],
            },
        );
        (*core::ptr::addr_of_mut!(PLATFORM_INFO)).timebase_frequency_hz = timebase;
    }
    LA64_TIMEBASE_HZ.store(timebase, Ordering::Release);
    LA64_POSSIBLE_CPU_COUNT.store(1, Ordering::Release);

    console_write_literal(b"txkernel:loongson-2k1000:h2:bootinfo:cpu=0x");
    console_write_hex(args.cpu_id.0);
    console_write_literal(b":a0=0x");
    console_write_hex(args.boot_flag);
    console_write_literal(b":a1=0x");
    console_write_hex(args.cmdline.0);
    console_write_literal(b":a2=0x");
    console_write_hex(args.system_table.0);
    console_write_literal(b":a3=0x");
    console_write_hex(args.reserved);
    console_write_literal(b":fdt=0x");
    console_write_hex(fdt.map_or(0, |blob| blob.phys));
    console_write_literal(b":timebase-hz=");
    console_write_decimal(timebase as usize);
    console_write_literal(b":initrd-start=0x");
    console_write_hex(initrd.map_or(0, |range| range.start.0));
    console_write_literal(b":initrd-size=0x");
    console_write_hex(initrd.map_or(0, |range| range.size));
    console_write_literal(b"\n");
}

fn normalize_firmware_address(address: usize) -> Option<usize> {
    if address <= LA64_PHYS_ADDR_MASK {
        return Some(address);
    }
    match address & !LA64_PHYS_ADDR_MASK {
        LA64_DMW_CACHED_BASE | LA64_DMW_UNCACHED_BASE => Some(address & LA64_PHYS_ADDR_MASK),
        _ => None,
    }
}

fn valid_ram_range(start: usize, size: usize) -> bool {
    let Some(end) = start.checked_add(size) else {
        return false;
    };
    let ram0_end = LA2K1000_RAM0_BASE + LA2K1000_RAM0_SIZE;
    let ram1_end = LA2K1000_RAM1_BASE + LA2K1000_RAM1_SIZE;
    (start >= LA2K1000_RAM0_BASE && end <= ram0_end)
        || (start >= LA2K1000_RAM1_BASE && end <= ram1_end)
}

fn boot_ptr<T>(address: usize) -> Option<*const T> {
    let phys = normalize_firmware_address(address)?;
    if !valid_ram_range(phys, core::mem::size_of::<T>()) {
        return None;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        Some(la64_cached_virt(phys) as *const T)
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = phys;
        None
    }
}

unsafe fn read_boot<T: Copy>(address: usize) -> Option<T> {
    Some(unsafe { core::ptr::read_unaligned(boot_ptr::<T>(address)?) })
}

unsafe fn copy_cmdline(address: usize, out: &mut [u8]) -> usize {
    if address == 0 {
        return 0;
    }
    let Some(phys) = normalize_firmware_address(address) else {
        return 0;
    };
    let Some(remaining) = ram_bytes_remaining(phys) else {
        return 0;
    };
    let Some(src) = boot_ptr::<u8>(address) else {
        return 0;
    };
    let mut len = 0;
    while len < out.len().min(remaining) {
        let byte = unsafe { core::ptr::read_volatile(src.add(len)) };
        if byte == 0 {
            return len;
        }
        out[len] = byte;
        len += 1;
    }
    0
}

unsafe fn find_efi_fdt(system_table: usize) -> Option<FdtBlob> {
    if system_table == 0 {
        return None;
    }
    let system = unsafe { read_boot::<EfiSystemTable>(system_table)? };
    if system.hdr.signature != EFI_SYSTEM_TABLE_SIGNATURE {
        return None;
    }
    let tables = usize::try_from(system.tables).ok()?;
    let count = usize::try_from(system.nr_tables)
        .ok()?
        .min(EFI_MAX_CONFIG_TABLES);
    for index in 0..count {
        let address = tables.checked_add(index * core::mem::size_of::<EfiConfigurationTable>())?;
        let entry = unsafe { read_boot::<EfiConfigurationTable>(address)? };
        if entry.guid == DEVICE_TREE_GUID {
            return unsafe { validate_fdt_blob(usize::try_from(entry.table).ok()?) };
        }
    }
    None
}

unsafe fn validate_fdt_blob(address: usize) -> Option<FdtBlob> {
    let phys = normalize_firmware_address(address)?;
    let header = unsafe { read_boot::<[u8; 8]>(address)? };
    let size = fdt_header_total_size(header)?;
    if !valid_ram_range(phys, size) {
        return None;
    }
    Some(FdtBlob { phys, size })
}

fn fdt_header_total_size(header: [u8; 8]) -> Option<usize> {
    if u32::from_be_bytes(header[..4].try_into().ok()?) != FDT_MAGIC {
        return None;
    }
    let size = usize::try_from(u32::from_be_bytes(header[4..].try_into().ok()?)).ok()?;
    (size >= FDT_HEADER_SIZE).then_some(size)
}

unsafe fn parse_fdt_initrd(fdt_blob: FdtBlob, kernel_image: PhysRange) -> Option<PhysRange> {
    let fdt_ptr = boot_ptr::<u8>(fdt_blob.phys)?;
    let fdt = unsafe { fdt::Fdt::from_ptr_unaligned_fallible(fdt_ptr) }.ok()?;
    let root = fdt.root().ok()?;
    let address_cells = root
        .cell_sizes()
        .ok()
        .map_or(2, |sizes| sizes.address_cells);
    let chosen = fdt.find_node("/chosen").ok().flatten()?;
    let start = chosen
        .as_node()
        .raw_property("linux,initrd-start")
        .ok()
        .flatten()
        .and_then(|property| read_cells(property.value, address_cells))?;
    let end = chosen
        .as_node()
        .raw_property("linux,initrd-end")
        .ok()
        .flatten()
        .and_then(|property| read_cells(property.value, address_cells))?;
    let start = normalize_firmware_address(start)?;
    let end = normalize_firmware_address(end)?;
    if end <= start || !valid_ram_range(start, end - start) {
        return None;
    }
    let size = end - start;
    if ranges_overlap(start, size, kernel_image.start.0, kernel_image.size)
        || ranges_overlap(
            start,
            size,
            LA2K1000_FDT_SCRATCH_BASE,
            LA2K1000_FDT_SCRATCH_SIZE,
        )
        || ranges_overlap(
            start,
            size,
            LA2K1000_FRAMEBUFFER_BASE,
            LA2K1000_FRAMEBUFFER_SIZE,
        )
        || ranges_overlap(
            start,
            size,
            LA2K1000_BOOTPARAM_BASE,
            LA2K1000_BOOTPARAM_SIZE,
        )
        || ranges_overlap(start, size, fdt_blob.phys, fdt_blob.size)
    {
        return None;
    }
    Some(PhysRange {
        start: PhysAddr(start),
        size,
    })
}

fn ram_bytes_remaining(start: usize) -> Option<usize> {
    let ram0_end = LA2K1000_RAM0_BASE.checked_add(LA2K1000_RAM0_SIZE)?;
    let ram1_end = LA2K1000_RAM1_BASE.checked_add(LA2K1000_RAM1_SIZE)?;
    if (LA2K1000_RAM0_BASE..ram0_end).contains(&start) {
        Some(ram0_end - start)
    } else if (LA2K1000_RAM1_BASE..ram1_end).contains(&start) {
        Some(ram1_end - start)
    } else {
        None
    }
}

fn ranges_overlap(
    first_start: usize,
    first_size: usize,
    second_start: usize,
    second_size: usize,
) -> bool {
    let (Some(first_end), Some(second_end)) = (
        first_start.checked_add(first_size),
        second_start.checked_add(second_size),
    ) else {
        return true;
    };
    first_start < second_end && second_start < first_end
}

fn read_cells(data: &[u8], cells: usize) -> Option<usize> {
    if cells == 0 || cells > 2 || data.len() < cells * 4 {
        return None;
    }
    let mut value = 0u64;
    for cell in data.get(..cells * 4)?.chunks_exact(4) {
        value = (value << 32) | u64::from(u32::from_be_bytes(cell.try_into().ok()?));
    }
    usize::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_addresses_accept_only_physical_or_known_dmw_tags() {
        assert_eq!(normalize_firmware_address(0x9800_0000), Some(0x9800_0000));
        assert_eq!(
            normalize_firmware_address(LA64_DMW_CACHED_BASE | 0x9800_0000),
            Some(0x9800_0000)
        );
        assert_eq!(
            normalize_firmware_address(LA64_DMW_UNCACHED_BASE | 0x9800_0000),
            Some(0x9800_0000)
        );
        assert_eq!(normalize_firmware_address(0xa000_0000_9800_0000), None);
    }

    #[test]
    fn ram_ranges_must_remain_inside_one_confirmed_bank() {
        assert!(valid_ram_range(0x9800_0000, 0x1000));
        assert!(valid_ram_range(0x0fff_f000, 0x1000));
        assert!(!valid_ram_range(0x0fff_f000, 0x2000));
        assert!(!valid_ram_range(0x1000_0000, 1));
        assert!(!valid_ram_range(usize::MAX, 2));
    }

    #[test]
    fn published_memory_map_reserves_fdt_framebuffer_and_bootparams() {
        assert_eq!(MEMORY_REGIONS.len(), 6);
        assert_eq!(MEMORY_REGIONS[0].size, LA2K1000_FDT_SCRATCH_BASE);
        assert_eq!(MEMORY_REGIONS[1].base.0, LA2K1000_FDT_SCRATCH_BASE);
        assert_eq!(MEMORY_REGIONS[1].kind, MemoryRegionKind::Reserved);
        assert_eq!(MEMORY_REGIONS[3].base.0, LA2K1000_FRAMEBUFFER_BASE);
        assert_eq!(MEMORY_REGIONS[3].kind, MemoryRegionKind::Reserved);
        assert_eq!(MEMORY_REGIONS[4].base.0, LA2K1000_BOOTPARAM_BASE);
        assert_eq!(MEMORY_REGIONS[4].kind, MemoryRegionKind::Reserved);
    }

    #[test]
    fn fdt_header_requires_magic_and_complete_header_size() {
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        header[4..].copy_from_slice(&(FDT_HEADER_SIZE as u32).to_be_bytes());
        assert_eq!(fdt_header_total_size(header), Some(FDT_HEADER_SIZE));
        header[0] = 0;
        assert_eq!(fdt_header_total_size(header), None);
        header[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        header[4..].copy_from_slice(&8u32.to_be_bytes());
        assert_eq!(fdt_header_total_size(header), None);
    }

    #[test]
    fn overlap_checks_include_touching_and_overflow_cases() {
        assert!(ranges_overlap(0x1000, 0x1000, 0x1800, 0x1000));
        assert!(!ranges_overlap(0x1000, 0x1000, 0x2000, 0x1000));
        assert!(ranges_overlap(usize::MAX, 2, 0, 1));
    }
}
