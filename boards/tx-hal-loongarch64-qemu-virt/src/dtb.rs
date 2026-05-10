use fdt::nodes::AsNode;
use tx_hal::{MemoryRegion, MemoryRegionKind, PhysAddr, PhysRange};

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

    cmdline.fill(0);
    memory_regions.fill(MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    });

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
        .and_then(|prop| read_timebase_frequency_hz(Some(prop.value)));
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

fn read_timebase_frequency_hz(value: Option<&[u8]>) -> Option<u64> {
    let value = value?;
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
