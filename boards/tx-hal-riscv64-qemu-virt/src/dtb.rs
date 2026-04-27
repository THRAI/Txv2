use tx_hal::{MemoryRegion, MemoryRegionKind, PhysAddr, PhysRange};

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DtbBootInfo {
    pub(crate) memory_region_count: usize,
    pub(crate) initrd: Option<PhysRange>,
    pub(crate) cmdline_len: usize,
}

pub(crate) unsafe fn parse_boot_info_from_fdt(
    dtb_addr: usize,
    memory_regions: &mut [MemoryRegion],
    cmdline: &mut [u8],
) -> Option<DtbBootInfo> {
    if dtb_addr == 0 {
        return None;
    }

    let header = FdtHeader::read(dtb_addr)?;
    if header.magic != FDT_MAGIC {
        return None;
    }

    let struct_block = core::slice::from_raw_parts(
        (dtb_addr + header.off_dt_struct) as *const u8,
        header.size_dt_struct,
    );
    let strings_block = core::slice::from_raw_parts(
        (dtb_addr + header.off_dt_strings) as *const u8,
        header.size_dt_strings,
    );

    let mut cursor = 0usize;
    let mut depth = 0usize;
    let mut root_address_cells = 2usize;
    let mut root_size_cells = 2usize;
    let mut in_memory = false;
    let mut in_chosen = false;
    let mut memory_region_count = 0usize;
    let mut initrd_start = None;
    let mut initrd_end = None;
    let mut cmdline_len = 0usize;

    while cursor + 4 <= struct_block.len() {
        let token = read_be32(struct_block, cursor)?;
        cursor += 4;

        match token {
            FDT_BEGIN_NODE => {
                let name_start = cursor;
                while cursor < struct_block.len() && struct_block[cursor] != 0 {
                    cursor += 1;
                }
                if cursor >= struct_block.len() {
                    return None;
                }
                let name = &struct_block[name_start..cursor];
                cursor = align4(cursor + 1);
                depth += 1;
                in_memory = starts_with(name, b"memory@") || name == b"memory";
                in_chosen = name == b"chosen";
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                in_memory = false;
                in_chosen = false;
            }
            FDT_PROP => {
                if cursor + 8 > struct_block.len() {
                    return None;
                }
                let len = read_be32(struct_block, cursor)? as usize;
                let nameoff = read_be32(struct_block, cursor + 4)? as usize;
                cursor += 8;
                if cursor + len > struct_block.len() {
                    return None;
                }
                let data = &struct_block[cursor..cursor + len];
                cursor = align4(cursor + len);
                let name = string_at(strings_block, nameoff)?;

                if depth == 1 {
                    if name == b"#address-cells" {
                        root_address_cells = read_cell_count(data)?;
                    } else if name == b"#size-cells" {
                        root_size_cells = read_cell_count(data)?;
                    }
                } else if in_memory && name == b"reg" {
                    memory_region_count += parse_reg_regions(
                        data,
                        root_address_cells,
                        root_size_cells,
                        &mut memory_regions[memory_region_count..],
                    )?;
                } else if in_chosen {
                    if name == b"bootargs" {
                        cmdline_len = copy_cstr(data, cmdline);
                    } else if name == b"linux,initrd-start" {
                        initrd_start = read_cells(data, root_address_cells).map(PhysAddr);
                    } else if name == b"linux,initrd-end" {
                        initrd_end = read_cells(data, root_address_cells).map(PhysAddr);
                    }
                }
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }

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
    })
}

#[derive(Clone, Copy)]
struct FdtHeader {
    magic: u32,
    off_dt_struct: usize,
    off_dt_strings: usize,
    size_dt_strings: usize,
    size_dt_struct: usize,
}

impl FdtHeader {
    unsafe fn read(dtb_addr: usize) -> Option<Self> {
        let header = core::slice::from_raw_parts(dtb_addr as *const u8, 40);
        Some(Self {
            magic: read_be32(header, 0)?,
            off_dt_struct: read_be32(header, 8)? as usize,
            off_dt_strings: read_be32(header, 12)? as usize,
            size_dt_strings: read_be32(header, 32)? as usize,
            size_dt_struct: read_be32(header, 36)? as usize,
        })
    }
}

fn parse_reg_regions(
    data: &[u8],
    address_cells: usize,
    size_cells: usize,
    out: &mut [MemoryRegion],
) -> Option<usize> {
    let entry_cells = address_cells.checked_add(size_cells)?;
    let entry_len = entry_cells.checked_mul(4)?;
    if entry_len == 0 || !data.len().is_multiple_of(entry_len) {
        return None;
    }

    let mut count = 0usize;
    let mut offset = 0usize;
    while offset + entry_len <= data.len() && count < out.len() {
        let base = read_cells(&data[offset..], address_cells)?;
        offset += address_cells * 4;
        let size = read_cells(&data[offset..], size_cells)?;
        offset += size_cells * 4;
        if size > 0 {
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

fn read_cell_count(data: &[u8]) -> Option<usize> {
    match read_cells(data, 1)? {
        1 | 2 => Some(read_cells(data, 1)?),
        _ => None,
    }
}

fn read_cells(data: &[u8], cells: usize) -> Option<usize> {
    if cells == 0 || cells > 2 || data.len() < cells * 4 {
        return None;
    }
    let mut value = 0usize;
    for idx in 0..cells {
        value = (value << 32) | read_be32(data, idx * 4)? as usize;
    }
    Some(value)
}

fn read_be32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn string_at(strings: &[u8], offset: usize) -> Option<&[u8]> {
    let mut end = offset;
    while end < strings.len() && strings[end] != 0 {
        end += 1;
    }
    (end < strings.len()).then_some(&strings[offset..end])
}

fn copy_cstr(src: &[u8], dst: &mut [u8]) -> usize {
    let len = src.iter().position(|byte| *byte == 0).unwrap_or(src.len());
    let len = len.min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
    len
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn starts_with(value: &[u8], prefix: &[u8]) -> bool {
    value.len() >= prefix.len() && &value[..prefix.len()] == prefix
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use tx_hal::{
        BootInfoIf, BootPlatformIf, BootProtocol, MemoryRegion, MemoryRegionKind, PhysAddr,
        PhysRange,
    };

    use super::{parse_boot_info_from_fdt, DtbBootInfo};
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
    fn boot_handoff_publishes_static_boot_info() {
        let fdt = fake_qemu_fdt();

        let handoff = Platform::boot_handoff(0, fdt.as_ptr() as usize);
        let boot_info = Platform::boot_info();

        assert_eq!(handoff.protocol, BootProtocol::RiscvSbi);
        assert_eq!(boot_info.memory_regions.len(), 1);
        assert_eq!(boot_info.memory_regions[0].base, PhysAddr(0x8000_0000));
        assert_eq!(
            boot_info.initrd,
            Some(PhysRange {
                start: PhysAddr(0x8100_0000),
                size: 0x20_0000,
            })
        );
        assert_eq!(boot_info.cmdline, Some("console=hvc0"));
    }

    fn fake_qemu_fdt() -> Vec<u8> {
        let mut strings = Vec::new();
        let address_cells = add_string(&mut strings, "#address-cells");
        let size_cells = add_string(&mut strings, "#size-cells");
        let device_type = add_string(&mut strings, "device_type");
        let reg = add_string(&mut strings, "reg");
        let bootargs = add_string(&mut strings, "bootargs");
        let initrd_start = add_string(&mut strings, "linux,initrd-start");
        let initrd_end = add_string(&mut strings, "linux,initrd-end");

        let mut structure = Vec::new();
        begin_node(&mut structure, "");
        prop_u32(&mut structure, address_cells, 2);
        prop_u32(&mut structure, size_cells, 2);

        begin_node(&mut structure, "memory@80000000");
        prop_bytes(&mut structure, device_type, b"memory\0");
        prop_cells(&mut structure, reg, &[0, 0x8000_0000, 0, 0x0800_0000]);
        end_node(&mut structure);

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
