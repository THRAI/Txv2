//! LA64 firmware boot-info discovery.
//!
//! This module owns EFI, fw_cfg, and FDT probing used by the QEMU
//! loongarch64 virt boot path. It fills the static boot buffers that are
//! published by the boot-facts path, but does not publish `BootInfo` itself.

use super::boot_args;
use super::boot_facts::{
    boot_cmdline_ptr, boot_memory_regions_ptr, LA64_BOOT_CMDLINE_CAPACITY,
    LA64_BOOT_MEMORY_REGION_CAPACITY,
};
use super::la64_irq_trap::la64_detect_timebase_frequency_hz;
use super::la64_pmap::{la64_cached_virt, la64_kernel_addr_to_phys, la64_uncached_virt};
use super::*;
use crate::dtb::{parse_boot_info_from_fdt, DtbBootInfo};

pub(crate) fn parse_firmware_boot_info(
    reserved_end: usize,
) -> Option<(usize, Option<PhysRange>, usize, u64, usize)> {
    let args = boot_args::snapshot();
    let system_table_phys = args.system_table_phys.0;
    let legacy_firmware_arg = args.legacy_firmware_arg.0;

    unsafe {
        let mut dtb_memory_regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; LA64_BOOT_MEMORY_REGION_CAPACITY];
        let cmdline =
            core::slice::from_raw_parts_mut(boot_cmdline_ptr(), LA64_BOOT_CMDLINE_CAPACITY);

        if system_table_phys != 0 {
            let efi = parse_efi_boot_info(system_table_phys);
            let parsed_dtb = efi.and_then(|info| info.fdt).and_then(|fdt| {
                parse_dtb_boot_info_with_fallbacks(fdt, &mut dtb_memory_regions, cmdline)
            });
            let dtb_cmdline_len = parsed_dtb.map_or(0, |parsed| parsed.cmdline_len);
            let boot_cmdline_len = copy_cmdline_from_phys(args.cmdline_phys.0, cmdline);
            let cmdline_len = if boot_cmdline_len > 0 {
                boot_cmdline_len
            } else {
                dtb_cmdline_len
            }
            .min(cmdline.len());
            let initrd = efi
                .and_then(|info| info.initrd)
                .or_else(|| parsed_dtb.and_then(|parsed| parsed.initrd));

            if parsed_dtb.is_some() || initrd.is_some() || cmdline_len > 0 {
                let memory_region_count = if let Some(parsed) = parsed_dtb {
                    populate_boot_memory_regions_from_dtb(
                        &dtb_memory_regions,
                        parsed.memory_region_count,
                        reserved_end,
                    )
                } else {
                    populate_fallback_boot_memory_regions(reserved_end)
                };
                let timebase_frequency_hz = parsed_dtb
                    .and_then(|parsed| parsed.timebase_frequency_hz)
                    .unwrap_or_else(la64_detect_timebase_frequency_hz);
                let possible_cpu_count = parsed_dtb
                    .map_or(LA64_DEFAULT_POSSIBLE_CPUS, |parsed| {
                        parsed.possible_cpu_count
                    })
                    .clamp(1, LA64_MAX_BOOT_CPUS);

                return Some((
                    memory_region_count,
                    initrd,
                    cmdline_len,
                    timebase_frequency_hz,
                    possible_cpu_count,
                ));
            }
        }

        if let Some(parsed) = parse_fw_cfg_boot_info(reserved_end, &mut dtb_memory_regions, cmdline)
        {
            return Some(parsed);
        }

        if legacy_firmware_arg != 0 {
            if let Some(parsed) = parse_dtb_boot_info_with_fallbacks(
                legacy_firmware_arg,
                &mut dtb_memory_regions,
                cmdline,
            ) {
                let memory_region_count = populate_boot_memory_regions_from_dtb(
                    &dtb_memory_regions,
                    parsed.memory_region_count,
                    reserved_end,
                );
                let cmdline_len = parsed.cmdline_len.min(cmdline.len());
                let timebase_frequency_hz = parsed
                    .timebase_frequency_hz
                    .unwrap_or_else(la64_detect_timebase_frequency_hz);
                let possible_cpu_count = parsed.possible_cpu_count.clamp(1, LA64_MAX_BOOT_CPUS);

                return Some((
                    memory_region_count,
                    parsed.initrd,
                    cmdline_len,
                    timebase_frequency_hz,
                    possible_cpu_count,
                ));
            }
        }
    }

    None
}

unsafe fn parse_fw_cfg_boot_info(
    reserved_end: usize,
    dtb_memory_regions: &mut [MemoryRegion],
    cmdline: &mut [u8],
) -> Option<(usize, Option<PhysRange>, usize, u64, usize)> {
    if !fw_cfg_available() {
        return None;
    }

    let parsed_dtb =
        parse_dtb_boot_info_with_fallbacks(QEMU_LA64_FDT_BASE, dtb_memory_regions, cmdline);
    let dtb_cmdline_len = parsed_dtb.map_or(0, |parsed| parsed.cmdline_len);
    let fw_cfg_files = fw_cfg_find_boot_files();
    let boot_cmdline_len = fw_cfg_copy_cmdline(cmdline, fw_cfg_files.cmdline);
    let cmdline_len = if boot_cmdline_len > 0 {
        boot_cmdline_len
    } else {
        dtb_cmdline_len
    }
    .min(cmdline.len());
    let initrd = fw_cfg_copy_initrd(fw_cfg_files.initrd);

    if parsed_dtb.is_none() && initrd.is_none() && cmdline_len == 0 {
        return None;
    }

    let memory_region_count = if let Some(parsed) = parsed_dtb {
        populate_boot_memory_regions_from_dtb(
            dtb_memory_regions,
            parsed.memory_region_count,
            reserved_end,
        )
    } else {
        populate_fallback_boot_memory_regions(reserved_end)
    };
    let timebase_frequency_hz = parsed_dtb
        .and_then(|parsed| parsed.timebase_frequency_hz)
        .unwrap_or_else(la64_detect_timebase_frequency_hz);
    let possible_cpu_count = parsed_dtb
        .map_or(LA64_DEFAULT_POSSIBLE_CPUS, |parsed| {
            parsed.possible_cpu_count
        })
        .clamp(1, LA64_MAX_BOOT_CPUS);

    Some((
        memory_region_count,
        initrd.or_else(|| parsed_dtb.and_then(|parsed| parsed.initrd)),
        cmdline_len,
        timebase_frequency_hz,
        possible_cpu_count,
    ))
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_SIGNATURE: u16 = 0x0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_KERNEL_CMDLINE: u16 = 0x0009;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_INITRD_SIZE: u16 = 0x000b;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_INITRD_DATA: u16 = 0x0012;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_CMDLINE_SIZE: u16 = 0x0014;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_FILE_DIR: u16 = 0x0019;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_DATA_OFFSET: usize = 0x00;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_SELECTOR_OFFSET: usize = 0x08;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_MAX_FILE_ENTRIES: usize = 64;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_FILE_NAME_LEN: usize = 56;

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
#[derive(Clone, Copy)]
struct FwCfgFile {
    selector: u16,
    size: usize,
}

#[derive(Clone, Copy)]
struct FwCfgBootFiles {
    cmdline: Option<FwCfgFile>,
    initrd: Option<FwCfgFile>,
}

fn fw_cfg_available() -> bool {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        fw_cfg_select(FW_CFG_SIGNATURE);
        let sig = [
            fw_cfg_read_u8(),
            fw_cfg_read_u8(),
            fw_cfg_read_u8(),
            fw_cfg_read_u8(),
        ];
        sig == *b"QEMU"
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        false
    }
}

fn fw_cfg_copy_cmdline(dst: &mut [u8], file: Option<FwCfgFile>) -> usize {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let (selector, size) = if let Some(file) = file {
            (file.selector, file.size.min(dst.len()))
        } else {
            let size = fw_cfg_read_u32_be(FW_CFG_CMDLINE_SIZE)
                .map(|value| value as usize)
                .unwrap_or(dst.len())
                .min(dst.len());
            (FW_CFG_KERNEL_CMDLINE, size)
        };
        if size == 0 {
            return 0;
        }

        fw_cfg_select(selector);
        let mut len = 0usize;
        while len < size {
            let byte = fw_cfg_read_u8();
            if byte == 0 {
                break;
            }
            if len == 0 {
                dst.fill(0);
            }
            dst[len] = byte;
            len += 1;
        }
        len
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = (dst, file);
        0
    }
}

fn fw_cfg_copy_initrd(file: Option<FwCfgFile>) -> Option<PhysRange> {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let (selector, size) = if let Some(file) = file {
            (file.selector, file.size)
        } else {
            (
                FW_CFG_INITRD_DATA,
                fw_cfg_read_u32_be(FW_CFG_INITRD_SIZE)? as usize,
            )
        };
        if size == 0 || size > LA64_FW_CFG_INITRD_CAPACITY {
            return None;
        }

        let dst = fw_cfg_initrd_buffer_ptr();
        fw_cfg_select(selector);
        for offset in 0..size {
            core::ptr::write_volatile(dst.add(offset), fw_cfg_read_u8());
        }

        Some(PhysRange {
            start: PhysAddr(la64_kernel_addr_to_phys(dst as usize)),
            size,
        })
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = file;
        None
    }
}

fn fw_cfg_find_boot_files() -> FwCfgBootFiles {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        fw_cfg_select(FW_CFG_FILE_DIR);
        let count = fw_cfg_read_stream_u32_be().min(FW_CFG_MAX_FILE_ENTRIES as u32) as usize;
        let mut out = FwCfgBootFiles {
            cmdline: None,
            initrd: None,
        };

        for _ in 0..count {
            let size = fw_cfg_read_stream_u32_be() as usize;
            let selector = fw_cfg_read_stream_u16_be();
            let _reserved = fw_cfg_read_stream_u16_be();
            let mut name = [0u8; FW_CFG_FILE_NAME_LEN];
            for byte in &mut name {
                *byte = fw_cfg_read_u8();
            }

            if out.cmdline.is_none()
                && (name_contains(&name, b"cmdline") || name_contains(&name, b"bootargs"))
            {
                out.cmdline = Some(FwCfgFile { selector, size });
            } else if out.initrd.is_none()
                && (name_contains(&name, b"initrd") || name_contains(&name, b"ramdisk"))
            {
                out.initrd = Some(FwCfgFile { selector, size });
            }
        }

        out
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        FwCfgBootFiles {
            cmdline: None,
            initrd: None,
        }
    }
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
fn name_contains(name: &[u8; FW_CFG_FILE_NAME_LEN], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > name.len() {
        return false;
    }
    name.windows(needle.len()).any(|window| window == needle)
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_u32_be(selector: u16) -> Option<u32> {
    fw_cfg_select(selector);
    Some(fw_cfg_read_stream_u32_be())
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_stream_u32_be() -> u32 {
    u32::from_be_bytes([
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
    ])
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_stream_u16_be() -> u16 {
    u16::from_be_bytes([fw_cfg_read_u8(), fw_cfg_read_u8()])
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_select(selector: u16) {
    let ptr = la64_uncached_virt(QEMU_LA64_FW_CFG_BASE + FW_CFG_SELECTOR_OFFSET) as *mut u16;
    core::ptr::write_volatile(ptr, selector.to_be());
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_u8() -> u8 {
    let ptr = la64_uncached_virt(QEMU_LA64_FW_CFG_BASE + FW_CFG_DATA_OFFSET) as *const u8;
    core::ptr::read_volatile(ptr)
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_initrd_buffer_ptr() -> *mut u8 {
    core::ptr::addr_of_mut!(LA64_FW_CFG_INITRD_BUFFER.bytes) as *mut u8
}

#[derive(Clone, Copy)]
struct EfiBootInfo {
    initrd: Option<PhysRange>,
    fdt: Option<usize>,
}

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

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiInitrd {
    base: u64,
    size: u64,
}

const EFI_SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
const EFI_MAX_CONFIG_TABLES: usize = 16;
const LINUX_EFI_INITRD_MEDIA_GUID: [u8; 16] = [
    0x27, 0xe4, 0x68, 0x55, 0xfc, 0x68, 0x3d, 0x4f, 0xac, 0x74, 0xca, 0x55, 0x52, 0x31, 0xcc, 0x68,
];
const DEVICE_TREE_GUID: [u8; 16] = [
    0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0,
];

unsafe fn parse_efi_boot_info(system_table_phys: usize) -> Option<EfiBootInfo> {
    let systab = read_boot_phys::<EfiSystemTable>(system_table_phys)?;
    if systab.hdr.signature != EFI_SYSTEM_TABLE_SIGNATURE {
        return None;
    }

    let tables_phys = usize::try_from(systab.tables).ok()?;
    let table_count = usize::try_from(systab.nr_tables)
        .ok()?
        .min(EFI_MAX_CONFIG_TABLES);
    let mut out = EfiBootInfo {
        initrd: None,
        fdt: None,
    };

    for index in 0..table_count {
        let entry_phys =
            tables_phys.checked_add(index * core::mem::size_of::<EfiConfigurationTable>())?;
        let entry = read_boot_phys::<EfiConfigurationTable>(entry_phys)?;
        let table_phys = usize::try_from(entry.table).ok()?;
        if entry.guid == LINUX_EFI_INITRD_MEDIA_GUID {
            let initrd = read_boot_phys::<EfiInitrd>(table_phys)?;
            let base = usize::try_from(initrd.base).ok()?;
            let size = usize::try_from(initrd.size).ok()?;
            if size > 0 {
                out.initrd = Some(PhysRange {
                    start: PhysAddr(base),
                    size,
                });
            }
        } else if entry.guid == DEVICE_TREE_GUID && table_phys != 0 {
            out.fdt = Some(table_phys);
        }
    }

    Some(out)
}

unsafe fn read_boot_phys<T: Copy>(phys: usize) -> Option<T> {
    let ptr = boot_phys_to_ptr::<T>(phys)?;
    Some(unsafe { core::ptr::read_unaligned(ptr) })
}

fn boot_phys_to_ptr<T>(phys: usize) -> Option<*const T> {
    let phys = la64_kernel_addr_to_phys(phys);
    if (QEMU_LA64_RAM_END..QEMU_LA64_PCIE_ECAM_BASE).contains(&phys) {
        return None;
    }

    #[cfg(target_arch = "loongarch64")]
    {
        Some(la64_cached_virt(phys) as *const T)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        Some(phys as *const T)
    }
}

unsafe fn copy_cmdline_from_phys(cmdline_phys: usize, dst: &mut [u8]) -> usize {
    let Some(src) = boot_phys_to_ptr::<u8>(cmdline_phys) else {
        return 0;
    };

    let mut len = 0usize;
    while len < dst.len() {
        let byte = unsafe { core::ptr::read_volatile(src.add(len)) };
        if byte == 0 {
            break;
        }
        if len == 0 {
            dst.fill(0);
        }
        dst[len] = byte;
        len += 1;
    }
    len
}

unsafe fn parse_dtb_boot_info_with_fallbacks(
    firmware_arg: usize,
    memory_regions: &mut [MemoryRegion],
    cmdline: &mut [u8],
) -> Option<DtbBootInfo> {
    let parse = |addr: usize, memory: &mut [MemoryRegion], out: &mut [u8]| unsafe {
        parse_boot_info_from_fdt(addr, memory, out)
    };

    // Derive the physical address and both DMW aliases. We always probe
    // the cached-alias first (safe: DMW window bypasses TLB, no fault
    // risk at early boot). Reading a raw physical address — what a QEMU
    // direct-boot loader places in `a1` — causes a kernel-mode TLB fault
    // and Terminate before page tables exist, so we check whether
    // `firmware_arg` is already in a DMW window before trying it directly.
    let phys = la64_kernel_addr_to_phys(firmware_arg);
    let cached_alias = la64_cached_virt(phys);
    let uncached_alias = la64_uncached_virt(phys);

    // 1. Cached DMW alias — always safe.
    if let Some(parsed) = parse(cached_alias, memory_regions, cmdline) {
        return Some(parsed);
    }

    // 2. firmware_arg itself, only if it is a DMW virtual address (i.e.
    //    QEMU / firmware already wrapped the physical in a window tag).
    if firmware_arg != cached_alias && firmware_arg != uncached_alias {
        let fw_tag = firmware_arg >> 48;
        let cached_tag = LA64_DMW_CACHED_BASE >> 48;
        let uncached_tag = LA64_DMW_UNCACHED_BASE >> 48;
        if fw_tag == cached_tag || fw_tag == uncached_tag {
            if let Some(parsed) = parse(firmware_arg, memory_regions, cmdline) {
                return Some(parsed);
            }
        }
    }

    // 3. Uncached DMW alias.
    if uncached_alias != cached_alias {
        return parse(uncached_alias, memory_regions, cmdline);
    }

    None
}

unsafe fn populate_boot_memory_regions_from_dtb(
    dtb_regions: &[MemoryRegion],
    dtb_region_count: usize,
    reserved_end: usize,
) -> usize {
    let out = boot_memory_regions_ptr();
    let mut out_count = 0usize;

    core::ptr::write(
        out.add(out_count),
        MemoryRegion {
            base: PhysAddr(QEMU_LA64_RAM_BASE),
            size: reserved_end.saturating_sub(QEMU_LA64_RAM_BASE),
            kind: MemoryRegionKind::Reserved,
        },
    );
    out_count += 1;

    for region in dtb_regions.iter().take(dtb_region_count) {
        if out_count >= LA64_BOOT_MEMORY_REGION_CAPACITY {
            break;
        }

        if region.kind != MemoryRegionKind::Usable || region.size == 0 {
            continue;
        }

        let start = region.base.0;
        let end = region
            .base
            .0
            .saturating_add(region.size)
            .min(QEMU_LA64_RAM_END);
        if end <= start {
            continue;
        }

        if end <= reserved_end {
            continue;
        }

        let usable_start = start.max(reserved_end);
        if usable_start >= end {
            continue;
        }

        core::ptr::write(
            out.add(out_count),
            MemoryRegion {
                base: PhysAddr(usable_start),
                size: end - usable_start,
                kind: MemoryRegionKind::Usable,
            },
        );
        out_count += 1;
    }

    if out_count == 1
        && out_count < LA64_BOOT_MEMORY_REGION_CAPACITY
        && reserved_end < QEMU_LA64_RAM_END
    {
        core::ptr::write(
            out.add(out_count),
            MemoryRegion {
                base: PhysAddr(reserved_end),
                size: QEMU_LA64_RAM_END - reserved_end,
                kind: MemoryRegionKind::Usable,
            },
        );
        out_count += 1;
    }

    append_la64_highmem_region(out, out_count)
}

/// Append the qemu-virt high-memory region (guest RAM above the MMIO/PCI hole)
/// as a usable region. qemu places memory beyond the low 256 MiB at
/// `QEMU_LA64_HIGHMEM_BASE`; without this the kernel only manages 256 MiB even
/// though `-m 1G` provides 1 GiB. Returns the updated region count.
unsafe fn append_la64_highmem_region(out: *mut MemoryRegion, out_count: usize) -> usize {
    if QEMU_LA64_HIGHMEM_SIZE == 0 || out_count >= LA64_BOOT_MEMORY_REGION_CAPACITY {
        return out_count;
    }
    core::ptr::write(
        out.add(out_count),
        MemoryRegion {
            base: PhysAddr(QEMU_LA64_HIGHMEM_BASE),
            size: QEMU_LA64_HIGHMEM_SIZE,
            kind: MemoryRegionKind::Usable,
        },
    );
    out_count + 1
}

unsafe fn populate_fallback_boot_memory_regions(reserved_end: usize) -> usize {
    let usable_size = QEMU_LA64_RAM_END.saturating_sub(reserved_end);
    let out = boot_memory_regions_ptr();
    unsafe {
        core::ptr::write(
            out,
            MemoryRegion {
                base: PhysAddr(QEMU_LA64_RAM_BASE),
                size: reserved_end - QEMU_LA64_RAM_BASE,
                kind: MemoryRegionKind::Reserved,
            },
        );
        core::ptr::write(
            out.add(1),
            MemoryRegion {
                base: PhysAddr(reserved_end),
                size: usable_size,
                kind: MemoryRegionKind::Usable,
            },
        );
    }
    append_la64_highmem_region(out, 2)
}
