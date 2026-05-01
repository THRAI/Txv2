use tx_hal::{MemoryRegion, MemoryRegionKind, PhysAddr, PhysRange};

use fdt::nodes::AsNode;

type FallibleFdtNode<'a> = fdt::nodes::Node<
    'a,
    (
        fdt::parsing::unaligned::UnalignedParser<'a>,
        fdt::parsing::NoPanic,
    ),
>;
type FallibleFdt<'a> = fdt::Fdt<
    'a,
    (
        fdt::parsing::unaligned::UnalignedParser<'a>,
        fdt::parsing::NoPanic,
    ),
>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DtbBootInfo {
    pub(crate) memory_region_count: usize,
    pub(crate) initrd: Option<PhysRange>,
    pub(crate) cmdline_len: usize,
    pub(crate) timebase_frequency_hz: Option<u64>,
    pub(crate) possible_cpu_count: usize,
}

pub(crate) unsafe fn parse_boot_info_from_fdt(
    dtb_addr: usize,
    memory_regions: &mut [MemoryRegion],
    cmdline: &mut [u8],
) -> Option<DtbBootInfo> {
    if dtb_addr == 0 {
        return None;
    }

    let fdt = unsafe { fdt::Fdt::from_ptr_unaligned_fallible(dtb_addr as *const u8) }.ok()?;
    let root = fdt.root().ok()?;
    let root_address_cells = root
        .cell_sizes()
        .ok()
        .map_or(2, |sizes| sizes.address_cells);
    let timebase_frequency_hz = root
        .as_node()
        .raw_property("timebase-frequency")
        .ok()
        .flatten()
        .and_then(|prop| read_timebase_frequency_hz(prop.value));
    let memory_region_count = copy_memory_regions(&fdt, memory_regions)?;
    let possible_cpu_count = count_cpu_nodes(&fdt).max(1);
    let chosen = fdt.find_node("/chosen").ok().flatten();
    let cmdline_len = copy_bootargs(chosen, cmdline);
    let initrd_start = chosen
        .and_then(|node| node.raw_property("linux,initrd-start").ok().flatten())
        .and_then(|prop| read_cells(prop.value, root_address_cells))
        .map(PhysAddr);
    let initrd_end = chosen
        .and_then(|node| node.raw_property("linux,initrd-end").ok().flatten())
        .and_then(|prop| read_cells(prop.value, root_address_cells))
        .map(PhysAddr);

    let initrd = match (initrd_start, initrd_end) {
        (Some(start), Some(end)) if end.0 > start.0 => Some(PhysRange {
            start,
            size: end.0 - start.0,
        }),
        _ => None,
    };

    Some(DtbBootInfo {
        memory_region_count,
        initrd,
        cmdline_len,
        timebase_frequency_hz,
        possible_cpu_count,
    })
}

fn copy_memory_regions(fdt: &FallibleFdt<'_>, out: &mut [MemoryRegion]) -> Option<usize> {
    let mut count = 0usize;
    for node in fdt.find_all_nodes_with_name("memory").ok()? {
        let node = node.ok()?;
        let Some(reg) = node.reg().ok()? else {
            continue;
        };
        count += copy_reg_regions(reg, &mut out[count..])?;
        if count >= out.len() {
            break;
        }
    }
    Some(count)
}

fn copy_reg_regions(reg: fdt::properties::reg::Reg<'_>, out: &mut [MemoryRegion]) -> Option<usize> {
    let mut count = 0usize;
    for entry in reg.iter::<u64, u64>() {
        let entry = entry.ok()?;
        let base = usize::try_from(entry.address).ok()?;
        let size = usize::try_from(entry.len).ok()?;
        if size > 0 {
            if count >= out.len() {
                break;
            }
            out[count] = MemoryRegion {
                base: PhysAddr(base),
                size,
                kind: MemoryRegionKind::Usable,
            };
            count += 1;
        }
    }
    Some(count)
}

fn read_cells(data: &[u8], cells: usize) -> Option<usize> {
    if cells == 0 || cells > 2 || data.len() < cells * 4 {
        return None;
    }
    let mut value = 0u64;
    for cell in data.get(..cells * 4)?.chunks_exact(4) {
        value = (value << 32) | u32::from_be_bytes(cell.try_into().ok()?) as u64;
    }
    usize::try_from(value).ok()
}

fn read_timebase_frequency_hz(value: &[u8]) -> Option<u64> {
    let frequency_hz = if value.len() == 4 {
        u64::from(u32::from_be_bytes(value.try_into().ok()?))
    } else if value.len() == 8 {
        u64::from_be_bytes(value.try_into().ok()?)
    } else {
        return None;
    };

    if frequency_hz == 0 {
        None
    } else {
        Some(frequency_hz)
    }
}

fn count_cpu_nodes(fdt: &FallibleFdt<'_>) -> usize {
    let Ok(nodes) = fdt.find_all_nodes_with_name("cpu") else {
        return 0;
    };
    nodes.filter_map(Result::ok).count()
}

fn copy_bootargs(chosen: Option<FallibleFdtNode<'_>>, dst: &mut [u8]) -> usize {
    let Some(bootargs) = chosen
        .and_then(|node| node.raw_property("bootargs").ok().flatten())
        .and_then(|prop| prop.as_value::<&str>().ok())
    else {
        return 0;
    };
    let src = bootargs.as_bytes();
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
    len
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::vec::Vec;

    use tx_hal::{
        BootInfoIf, BootPlatformIf, BootProtocol, MemoryRegion, MemoryRegionKind, PhysAddr,
        PhysRange, PlatformInfoIf,
    };

    use super::{parse_boot_info_from_fdt, DtbBootInfo};
    use crate::boot_static::{BootStaticBag, IdentityLive};
    use crate::Platform;

    #[test]
    fn parses_qemu_memory_chosen_cmdline_and_initrd_from_fdt() {
        let fdt = fake_qemu_fdt();
        let mut regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; 4];
        let mut cmdline = [0u8; 64];

        let parsed =
            unsafe { parse_boot_info_from_fdt(fdt.as_ptr() as usize, &mut regions, &mut cmdline) }
                .expect("valid fdt should parse");

        assert_eq!(
            parsed,
            DtbBootInfo {
                memory_region_count: 1,
                initrd: Some(PhysRange {
                    start: PhysAddr(0x8100_0000),
                    size: 0x20_0000,
                }),
                cmdline_len: 12,
                timebase_frequency_hz: Some(10_000_000),
                possible_cpu_count: 4,
            }
        );
        assert_eq!(regions[0].base, PhysAddr(0x8000_0000));
        assert_eq!(regions[0].size, 0x800_0000);
        assert_eq!(regions[0].kind, MemoryRegionKind::Usable);
        assert_eq!(
            core::str::from_utf8(&cmdline[..parsed.cmdline_len]),
            Ok("console=hvc0")
        );
    }

    #[test]
    fn fake_qemu_fdt_is_accepted_by_fdt_crate() {
        let fdt = fake_qemu_fdt();

        fdt::Fdt::new_unaligned(&fdt).expect("fake qemu fdt should be crate-parseable");
    }

    #[test]
    fn parses_multiple_memory_nodes_from_fdt() {
        let fdt = fake_qemu_fdt_with_memory_ranges(&[
            (0x8000_0000, 0x0800_0000),
            (0x9000_0000, 0x0100_0000),
        ]);
        let mut regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; 4];
        let mut cmdline = [0u8; 64];

        let parsed =
            unsafe { parse_boot_info_from_fdt(fdt.as_ptr() as usize, &mut regions, &mut cmdline) }
                .expect("valid fdt should parse");

        assert_eq!(parsed.memory_region_count, 2);
        assert_eq!(regions[0].base, PhysAddr(0x8000_0000));
        assert_eq!(regions[0].size, 0x0800_0000);
        assert_eq!(regions[1].base, PhysAddr(0x9000_0000));
        assert_eq!(regions[1].size, 0x0100_0000);
    }

    #[test]
    fn zero_timebase_frequency_is_treated_as_absent() {
        let fdt = fake_qemu_fdt_with_timebase_frequency(0);
        let mut regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; 4];
        let mut cmdline = [0u8; 64];

        let parsed =
            unsafe { parse_boot_info_from_fdt(fdt.as_ptr() as usize, &mut regions, &mut cmdline) }
                .expect("valid fdt should parse");

        assert_eq!(parsed.timebase_frequency_hz, None);
    }

    #[test]
    fn boot_handoff_publishes_static_boot_info() {
        let fdt = fake_qemu_fdt();
        let dtb_addr = fdt.as_ptr() as usize;

        unsafe {
            BootStaticBag::<IdentityLive>::reset_global_for_test();
        }

        let handoff = Platform::boot_handoff(0, dtb_addr);
        let boot_info = Platform::boot_info();

        assert_eq!(handoff.protocol, BootProtocol::RiscvSbi);
        assert_eq!(boot_info.memory_regions.len(), 2);
        assert_eq!(boot_info.memory_regions[0].base, PhysAddr(0x8000_0000));
        assert_eq!(boot_info.memory_regions[0].kind, MemoryRegionKind::Usable);
        assert_eq!(boot_info.memory_regions[1].base, PhysAddr(0x8000_0000));
        assert_eq!(boot_info.memory_regions[1].size, 0x20_0000);
        assert_eq!(boot_info.memory_regions[1].kind, MemoryRegionKind::Reserved);
        assert_eq!(
            boot_info.initrd,
            Some(PhysRange {
                start: PhysAddr(0x8100_0000),
                size: 0x20_0000,
            })
        );
        assert_eq!(boot_info.cmdline, Some("console=hvc0"));
        assert_eq!(Platform::platform_info().timebase_frequency_hz, 10_000_000);
        assert_eq!(Platform::platform_info().possible_cpu_count, 4);
        assert_eq!(<Platform as tx_hal::TimeIf>::frequency_hz(), 10_000_000);

        unsafe {
            BootStaticBag::<IdentityLive>::reset_global_for_test();
        }
    }

    fn fake_qemu_fdt() -> Vec<u8> {
        fake_qemu_fdt_with_memory_ranges(&[(0x8000_0000, 0x0800_0000)])
    }

    fn fake_qemu_fdt_with_memory_ranges(memory_ranges: &[(u64, u64)]) -> Vec<u8> {
        fake_qemu_fdt_with_memory_ranges_and_timebase(memory_ranges, 10_000_000)
    }

    fn fake_qemu_fdt_with_timebase_frequency(timebase_frequency_hz: u32) -> Vec<u8> {
        fake_qemu_fdt_with_memory_ranges_and_timebase(
            &[(0x8000_0000, 0x0800_0000)],
            timebase_frequency_hz,
        )
    }

    fn fake_qemu_fdt_with_memory_ranges_and_timebase(
        memory_ranges: &[(u64, u64)],
        timebase_frequency_hz: u32,
    ) -> Vec<u8> {
        let mut strings = Vec::new();
        let address_cells = add_string(&mut strings, "#address-cells");
        let size_cells = add_string(&mut strings, "#size-cells");
        let device_type = add_string(&mut strings, "device_type");
        let reg = add_string(&mut strings, "reg");
        let bootargs = add_string(&mut strings, "bootargs");
        let initrd_start = add_string(&mut strings, "linux,initrd-start");
        let initrd_end = add_string(&mut strings, "linux,initrd-end");
        let timebase_frequency = add_string(&mut strings, "timebase-frequency");
        let status = add_string(&mut strings, "status");

        let mut structure = Vec::new();
        begin_node(&mut structure, "");
        prop_u32(&mut structure, address_cells, 2);
        prop_u32(&mut structure, size_cells, 2);
        prop_u32(&mut structure, timebase_frequency, timebase_frequency_hz);

        begin_node(&mut structure, "cpus");
        prop_u32(&mut structure, address_cells, 1);
        prop_u32(&mut structure, size_cells, 0);
        for cpu in 0..4u32 {
            begin_node(&mut structure, &format!("cpu@{cpu}"));
            prop_bytes(&mut structure, device_type, b"cpu\0");
            prop_bytes(&mut structure, status, b"okay\0");
            prop_cells(&mut structure, reg, &[cpu]);
            end_node(&mut structure);
        }
        end_node(&mut structure);

        for (base, size) in memory_ranges {
            begin_node(&mut structure, &format!("memory@{base:x}"));
            prop_bytes(&mut structure, device_type, b"memory\0");
            prop_cells(
                &mut structure,
                reg,
                &[
                    (base >> 32) as u32,
                    *base as u32,
                    (size >> 32) as u32,
                    *size as u32,
                ],
            );
            end_node(&mut structure);
        }

        begin_node(&mut structure, "chosen");
        prop_bytes(&mut structure, bootargs, b"console=hvc0\0");
        prop_cells(&mut structure, initrd_start, &[0, 0x8100_0000]);
        prop_cells(&mut structure, initrd_end, &[0, 0x8120_0000]);
        end_node(&mut structure);

        end_node(&mut structure);
        push_be32(&mut structure, 9);

        let header_len = 40usize;
        let reserve_len = 16usize;
        let off_mem_rsvmap = header_len;
        let off_dt_struct = header_len + reserve_len;
        let off_dt_strings = off_dt_struct + structure.len();
        let total = off_dt_strings + strings.len();

        let mut fdt = Vec::new();
        push_be32(&mut fdt, 0xd00d_feed);
        push_be32(&mut fdt, total as u32);
        push_be32(&mut fdt, off_dt_struct as u32);
        push_be32(&mut fdt, off_dt_strings as u32);
        push_be32(&mut fdt, off_mem_rsvmap as u32);
        push_be32(&mut fdt, 17);
        push_be32(&mut fdt, 16);
        push_be32(&mut fdt, 0);
        push_be32(&mut fdt, strings.len() as u32);
        push_be32(&mut fdt, structure.len() as u32);
        fdt.extend_from_slice(&[0u8; 16]);
        fdt.extend_from_slice(&structure);
        fdt.extend_from_slice(&strings);
        fdt
    }

    fn add_string(strings: &mut Vec<u8>, value: &str) -> u32 {
        let offset = strings.len() as u32;
        strings.extend_from_slice(value.as_bytes());
        strings.push(0);
        offset
    }

    fn begin_node(out: &mut Vec<u8>, name: &str) {
        push_be32(out, 1);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        align4(out);
    }

    fn end_node(out: &mut Vec<u8>) {
        push_be32(out, 2);
    }

    fn prop_u32(out: &mut Vec<u8>, name_offset: u32, value: u32) {
        prop_cells(out, name_offset, &[value]);
    }

    fn prop_cells(out: &mut Vec<u8>, name_offset: u32, cells: &[u32]) {
        let mut data = Vec::new();
        for cell in cells {
            push_be32(&mut data, *cell);
        }
        prop_bytes(out, name_offset, &data);
    }

    fn prop_bytes(out: &mut Vec<u8>, name_offset: u32, data: &[u8]) {
        push_be32(out, 3);
        push_be32(out, data.len() as u32);
        push_be32(out, name_offset);
        out.extend_from_slice(data);
        align4(out);
    }

    fn push_be32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn align4(out: &mut Vec<u8>) {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
}
