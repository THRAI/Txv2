use tx_hal::{DeviceInfo, DeviceKind, MemoryRegion, MemoryRegionKind, PhysAddr, PhysRange};

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
pub(crate) enum DtbDeviceError {
    LegacyDeviceCapacityExceeded { capacity: usize, required: usize },
    LegacyMmioCapacityExceeded { capacity: usize, required: usize },
    LegacyMmioNameCapacityExceeded { kind: DeviceKind, required: usize },
    PlatformMmioCapacityExceeded { capacity: usize, required: usize },
    PlatformDeviceCapacityExceeded { capacity: usize, required: usize },
    DeviceMatchCapacityExceeded { capacity: usize, required: usize },
    DeviceResourceCapacityExceeded { capacity: usize, required: usize },
    StringArenaCapacityExceeded { capacity: usize, required: usize },
    MalformedNodeName,
    MalformedCompatible,
    InvalidIrq(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DtbDeviceClass {
    PlatformMmio(Option<DeviceKind>),
    PlatformDevice(DeviceKind),
}

impl DtbDeviceClass {
    const fn legacy_kind(self) -> Option<DeviceKind> {
        match self {
            Self::PlatformMmio(kind) => kind,
            Self::PlatformDevice(kind) => Some(kind),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DtbDeviceFact<'a> {
    pub(crate) class: DtbDeviceClass,
    pub(crate) node_name: &'a str,
    pub(crate) unit_address: Option<&'a str>,
    pub(crate) compatible: &'a [u8],
    pub(crate) mmio: PhysRange,
    pub(crate) irq: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DtbBootInfo {
    pub(crate) memory_region_count: usize,
    pub(crate) initrd: Option<PhysRange>,
    pub(crate) cmdline_len: usize,
    pub(crate) timebase_frequency_hz: Option<u64>,
    pub(crate) possible_cpu_count: usize,
    /// Bit per hart id for CPUs that can run S-mode. Linux-style DT
    /// properties provide the generic filter; SoC topology normalization
    /// removes firmware-described harts that the hardware cannot run as
    /// supervisor application CPUs. 0 = no data, caller falls back to
    /// counting.
    pub(crate) startable_harts: u64,
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
    // Real trees (QEMU dumpdtb, VisionFive 2) place timebase-frequency on
    // /cpus; some synthetic trees carry it on the root node. Prefer /cpus,
    // fall back to root.
    let timebase_frequency_hz = fdt
        .find_node("/cpus")
        .ok()
        .flatten()
        .and_then(|node| node.raw_property("timebase-frequency").ok().flatten())
        .and_then(|prop| read_timebase_frequency_hz(prop.value))
        .or_else(|| {
            root.as_node()
                .raw_property("timebase-frequency")
                .ok()
                .flatten()
                .and_then(|prop| read_timebase_frequency_hz(prop.value))
        });
    let memory_region_count = copy_memory_regions(&fdt, memory_regions)?;
    let possible_cpu_count = count_cpu_nodes(&fdt).max(1);
    let startable_harts = collect_startable_harts(&fdt);
    let chosen = fdt.find_node("/chosen").ok().flatten();
    let cmdline_len = copy_bootargs(chosen, cmdline);
    let initrd_start = chosen
        .and_then(|node| node.raw_property("linux,initrd-start").ok().flatten())
        .and_then(|prop| read_initrd_addr(prop.value, root_address_cells))
        .map(PhysAddr);
    let initrd_end = chosen
        .and_then(|node| node.raw_property("linux,initrd-end").ok().flatten())
        .and_then(|prop| read_initrd_addr(prop.value, root_address_cells))
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
        startable_harts,
    })
}

/// Bitmap of hart ids that can run S-mode. The generic Linux rule requires
/// `/cpus` children with `device_type = "cpu"`, status not `disabled`, and an
/// `mmu-type` property. `normalize_startable_harts` then cross-checks those
/// candidates against interrupt-controller topology when the DTB provides it.
fn collect_startable_harts(fdt: &FallibleFdt<'_>) -> u64 {
    let Some(cpus) = fdt.find_node("/cpus").ok().flatten() else {
        return 0;
    };
    let Ok(children) = cpus.children() else {
        return 0;
    };
    let mut mask = 0u64;
    for cpu in children.iter().filter_map(Result::ok) {
        let is_cpu = cpu
            .raw_property("device_type")
            .ok()
            .flatten()
            .is_some_and(|prop| prop.value == b"cpu\0");
        if !is_cpu {
            continue;
        }
        if cpu.raw_property("mmu-type").ok().flatten().is_none() {
            continue;
        }
        let disabled = cpu
            .raw_property("status")
            .ok()
            .flatten()
            .is_some_and(|prop| prop.value == b"disabled\0");
        if disabled {
            continue;
        }
        let Some(hart) = read_u32_property(&cpu, "reg") else {
            continue;
        };
        if (hart as usize) < u64::BITS as usize {
            mask |= 1u64 << hart;
        }
    }
    normalize_startable_harts(fdt, mask)
}

/// Correct incomplete or inaccurate CPU nodes with interrupt topology facts.
///
/// A PLIC platform's `interrupts-extended` property identifies exactly which
/// harts have an S-mode external-interrupt context. If that information is
/// present, a hart advertised by `/cpus` but lacking an S-mode context cannot
/// be used as a supervisor application hart and is removed from the candidate
/// mask. This handles the VF2 U-Boot control FDT that misdescribes its S7
/// monitor core without encoding a board name, a particular hart id, or a
/// contiguous topology. DTBs without usable PLIC context data retain the
/// generic CPU-node result, which also keeps AIA-only platforms working.
fn normalize_startable_harts(fdt: &FallibleFdt<'_>, mask: u64) -> u64 {
    let mut contexts = [None; MAX_PLIC_HARTS];
    let resolved = collect_plic_scontexts(fdt, &mut contexts);
    filter_harts_by_plic_scontexts(mask, &contexts, resolved)
}

fn filter_harts_by_plic_scontexts(
    mask: u64,
    contexts: &[Option<u32>; MAX_PLIC_HARTS],
    resolved: usize,
) -> u64 {
    if resolved == 0 {
        return mask;
    }

    let supervisor_harts = contexts
        .iter()
        .enumerate()
        .filter_map(|(hart, context)| context.map(|_| 1u64 << hart))
        .fold(0u64, |acc, bit| acc | bit);
    mask & supervisor_harts
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

fn read_initrd_addr(data: &[u8], root_address_cells: usize) -> Option<usize> {
    if data.len() == 4 {
        return read_cells(data, 1);
    }
    read_cells(data, root_address_cells)
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
    // Count only /cpus children with device_type = "cpu". Name-based
    // matching overcounts on real trees: VisionFive 2 keeps cpu-map (and
    // similar non-CPU nodes) under /cpus.
    let Some(cpus) = fdt.find_node("/cpus").ok().flatten() else {
        return 0;
    };
    let Ok(children) = cpus.children() else {
        return 0;
    };
    children
        .iter()
        .filter_map(Result::ok)
        .filter(|child| {
            child
                .raw_property("device_type")
                .ok()
                .flatten()
                .is_some_and(|prop| prop.value == b"cpu\0")
        })
        .count()
}

/// Walk `/soc` children and collect the legacy compatibility projection.
/// Unknown nodes are skipped; a missing or unparseable tree yields an empty
/// projection. Capacity exhaustion is a boot error rather than truncation.
#[cfg(test)]
pub(crate) unsafe fn parse_devices_from_fdt(
    dtb_addr: usize,
    out: &mut [DeviceInfo],
) -> Result<usize, DtbDeviceError> {
    unsafe { parse_devices_from_fdt_with(dtb_addr, out, |_| Ok(())) }
}

/// Perform the single firmware device walk used by both the legacy
/// [`DeviceInfo`] projection and the typed resource-seed publisher.
pub(crate) unsafe fn parse_devices_from_fdt_with(
    dtb_addr: usize,
    out: &mut [DeviceInfo],
    mut visit: impl FnMut(DtbDeviceFact<'_>) -> Result<(), DtbDeviceError>,
) -> Result<usize, DtbDeviceError> {
    if dtb_addr == 0 {
        return Ok(0);
    }
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr_unaligned_fallible(dtb_addr as *const u8) }) else {
        return Ok(0);
    };
    let Some(soc) = fdt.find_node("/soc").ok().flatten() else {
        return Ok(0);
    };
    let Ok(children) = soc.children() else {
        return Ok(0);
    };

    let mut count = 0usize;
    for child in children.iter().filter_map(Result::ok) {
        if !device_node_is_available(&child) {
            continue;
        }
        let Some((class, compatible)) = classify_device_fact(&child) else {
            continue;
        };
        let name = child
            .name()
            .map_err(|_| DtbDeviceError::MalformedNodeName)?;
        let irq = read_u32_property(&child, "interrupts");

        if matches!(
            class,
            DtbDeviceClass::PlatformMmio(Some(DeviceKind::ClockController))
        ) {
            let Some(reg) = child.reg().ok().flatten() else {
                continue;
            };
            for entry in reg.iter::<u64, u64>() {
                let entry = entry.ok().ok_or(DtbDeviceError::MalformedNodeName)?;
                let Some(mmio) = phys_range_from_reg(entry.address, entry.len) else {
                    continue;
                };
                emit_device_fact(
                    &child,
                    class,
                    compatible,
                    name.name,
                    name.unit_address,
                    mmio,
                    irq,
                    out,
                    &mut count,
                    &mut visit,
                )?;
            }
        } else if let Some(mmio) = first_reg_range(&child) {
            emit_device_fact(
                &child,
                class,
                compatible,
                name.name,
                name.unit_address,
                mmio,
                irq,
                out,
                &mut count,
                &mut visit,
            )?;
        }
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
fn emit_device_fact<'a>(
    node: &FallibleFdtNode<'a>,
    class: DtbDeviceClass,
    compatible: &'a [u8],
    node_name: &'a str,
    unit_address: Option<&'a str>,
    mmio: PhysRange,
    irq: Option<u32>,
    out: &mut [DeviceInfo],
    count: &mut usize,
    visit: &mut impl FnMut(DtbDeviceFact<'a>) -> Result<(), DtbDeviceError>,
) -> Result<(), DtbDeviceError> {
    visit(DtbDeviceFact {
        class,
        node_name,
        unit_address,
        compatible,
        mmio,
        irq,
    })?;

    if let Some(kind) = class.legacy_kind() {
        if *count == out.len() {
            return Err(DtbDeviceError::LegacyDeviceCapacityExceeded {
                capacity: out.len(),
                required: *count + 1,
            });
        }
        out[*count] = DeviceInfo {
            kind,
            mmio,
            irq,
            reg_shift: read_u32_property(node, "reg-shift").unwrap_or(0) as u8,
            reg_io_width: read_u32_property(node, "reg-io-width").unwrap_or(1) as u8,
        };
        *count += 1;
    }
    Ok(())
}

/// Devicetree status rule: absent, `okay`, and `ok` are available; every
/// other value is disabled/reserved and must not become a runtime capability.
fn device_node_is_available(node: &FallibleFdtNode<'_>) -> bool {
    let Ok(Some(status)) = node.raw_property("status") else {
        return true;
    };
    matches!(status.value, b"okay\0" | b"ok\0")
}

/// Collect firmware-reserved RAM ranges as `Reserved` memory regions.
///
/// Two sources, both honored:
/// 1. `/reserved-memory` child nodes — OpenSBI registers its own home
///    (`mmode_resv*`) here and PMP-protects it. On VF2 that is
///    [0x4000_0000, +512K), the very bottom of DDR: treating it as
///    usable made the frame allocator memset its metadata over the
///    firmware and die with a store ACCESS fault (scause=0x7) on the
///    first write — the 2026-07-02 second on-board trap.
/// 2. The FDT header's memreserve block (older mechanism, cheap to
///    honor).
///
/// QEMU is unaffected: its OpenSBI home at 0x8000_0000 was already
/// excluded by `reserve_firmware_loader_region`, and its dumpdtb has
/// no /reserved-memory node.
pub(crate) unsafe fn parse_reserved_regions_from_fdt(
    dtb_addr: usize,
    out: &mut [MemoryRegion],
) -> usize {
    if dtb_addr == 0 {
        return 0;
    }

    let mut count = 0usize;

    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr_unaligned_fallible(dtb_addr as *const u8) }) else {
        return 0;
    };
    if let Some(reserved) = fdt.find_node("/reserved-memory").ok().flatten() {
        if let Ok(children) = reserved.children() {
            for child in children.iter().filter_map(Result::ok) {
                if count == out.len() {
                    return count;
                }
                let Some(range) = first_reg_range(&child) else {
                    continue;
                };
                out[count] = MemoryRegion {
                    base: range.start,
                    size: range.size,
                    kind: MemoryRegionKind::Reserved,
                };
                count += 1;
            }
        }
    }

    // Header memreserve block: (u64 address, u64 size) big-endian pairs
    // at off_mem_rsvmap, terminated by a zero pair.
    const MAX_MEMRESERVE_ENTRIES: usize = 16;
    let header = dtb_addr as *const u8;
    let read_be32 = |offset: usize| -> u32 {
        unsafe { (header.add(offset) as *const u32).read_unaligned() }.to_be()
    };
    let read_be64 = |offset: usize| -> u64 {
        unsafe { (header.add(offset) as *const u64).read_unaligned() }.to_be()
    };
    let totalsize = read_be32(4) as usize;
    let mut offset = read_be32(16) as usize; // off_mem_rsvmap
    for _ in 0..MAX_MEMRESERVE_ENTRIES {
        if count == out.len() || offset + 16 > totalsize {
            break;
        }
        let base = read_be64(offset);
        let size = read_be64(offset + 8);
        if base == 0 && size == 0 {
            break;
        }
        if let (Ok(base), Ok(size)) = (usize::try_from(base), usize::try_from(size)) {
            out[count] = MemoryRegion {
                base: PhysAddr(base),
                size,
                kind: MemoryRegionKind::Reserved,
            };
            count += 1;
        }
        offset += 16;
    }

    count
}

/// Maximum harts we derive PLIC contexts for (VF2 has 5, QEMU 4).
pub(crate) const MAX_PLIC_HARTS: usize = 8;

/// Derive the PLIC S-mode context per hart from `interrupts-extended`.
///
/// The property is a sequence of (cpu-intc phandle, irq) pairs whose
/// index IS the PLIC context number. The pair with irq 9 (S-mode
/// external) names a hart's supervisor context. This matters on real
/// boards: VF2's hart0 is an S7 monitor core without S-mode, so the
/// QEMU `2*hart+1` formula is wrong there (hart1's S context is 2,
/// not 3). Returns the number of harts resolved; 0 means the caller
/// keeps its QEMU-formula fallback.
pub(crate) unsafe fn parse_plic_scontexts_from_fdt(
    dtb_addr: usize,
    out: &mut [Option<u32>; MAX_PLIC_HARTS],
) -> usize {
    out.fill(None);
    if dtb_addr == 0 {
        return 0;
    }
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr_unaligned_fallible(dtb_addr as *const u8) }) else {
        return 0;
    };
    collect_plic_scontexts(&fdt, out)
}

fn collect_plic_scontexts(fdt: &FallibleFdt<'_>, out: &mut [Option<u32>; MAX_PLIC_HARTS]) -> usize {
    const IRQ_S_EXTERNAL: u32 = 9;

    out.fill(None);

    // Pass 1: map each cpu's interrupt-controller phandle -> hart id.
    let mut intc_harts = [(0u32, 0usize); MAX_PLIC_HARTS];
    let mut intc_count = 0usize;
    let Some(cpus) = fdt.find_node("/cpus").ok().flatten() else {
        return 0;
    };
    let Ok(children) = cpus.children() else {
        return 0;
    };
    for cpu in children.iter().filter_map(Result::ok) {
        if intc_count == intc_harts.len() {
            break;
        }
        let is_cpu = cpu
            .raw_property("device_type")
            .ok()
            .flatten()
            .is_some_and(|prop| prop.value == b"cpu\0");
        if !is_cpu {
            continue;
        }
        let Some(hart) = read_u32_property(&cpu, "reg") else {
            continue;
        };
        let Ok(Some(intc)) = cpu.child("interrupt-controller") else {
            continue;
        };
        let phandle = read_u32_property(&intc, "phandle")
            .or_else(|| read_u32_property(&intc, "linux,phandle"));
        let Some(phandle) = phandle else {
            continue;
        };
        intc_harts[intc_count] = (phandle, hart as usize);
        intc_count += 1;
    }
    if intc_count == 0 {
        return 0;
    }

    // Pass 2: walk the plic's interrupts-extended pairs.
    let Some(soc) = fdt.find_node("/soc").ok().flatten() else {
        return 0;
    };
    let Ok(soc_children) = soc.children() else {
        return 0;
    };
    let Some(plic) = soc_children
        .iter()
        .filter_map(Result::ok)
        .find(|node| classify_device(node) == Some(DeviceKind::IntController))
    else {
        return 0;
    };
    let Some(pairs) = plic.raw_property("interrupts-extended").ok().flatten() else {
        return 0;
    };

    let mut resolved = 0usize;
    for (context, pair) in pairs.value.chunks_exact(8).enumerate() {
        let phandle = u32::from_be_bytes(pair[..4].try_into().unwrap());
        let irq = u32::from_be_bytes(pair[4..].try_into().unwrap());
        if irq != IRQ_S_EXTERNAL {
            continue;
        }
        let Some(&(_, hart)) = intc_harts[..intc_count]
            .iter()
            .find(|(candidate, _)| *candidate == phandle)
        else {
            continue;
        };
        if hart < out.len() && out[hart].is_none() {
            out[hart] = Some(context as u32);
            resolved += 1;
        }
    }
    resolved
}

fn classify_device(node: &FallibleFdtNode<'_>) -> Option<DeviceKind> {
    classify_device_fact(node).and_then(|(class, _)| class.legacy_kind())
}

fn classify_device_fact<'a>(node: &FallibleFdtNode<'a>) -> Option<(DtbDeviceClass, &'a [u8])> {
    let compatible = node.raw_property("compatible").ok().flatten()?;
    for name in compatible
        .value
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if let Some(class) = classify_compatible(name) {
            return Some((class, compatible.value));
        }
    }
    None
}

fn classify_compatible(name: &[u8]) -> Option<DtbDeviceClass> {
    match name {
        b"virtio,mmio" => Some(DtbDeviceClass::PlatformDevice(DeviceKind::VirtioMmio)),
        b"ns16550a" | b"snps,dw-apb-uart" | b"sifive,uart0" => {
            Some(DtbDeviceClass::PlatformMmio(Some(DeviceKind::Uart)))
        }
        b"riscv,plic0" | b"sifive,plic-1.0.0" => Some(DtbDeviceClass::PlatformMmio(Some(
            DeviceKind::IntController,
        ))),
        b"sifive,clint0" | b"riscv,clint0" => Some(DtbDeviceClass::PlatformMmio(None)),
        b"google,goldfish-rtc" => Some(DtbDeviceClass::PlatformMmio(Some(DeviceKind::GoldfishRtc))),
        b"starfive,jh7110-clkgen" => Some(DtbDeviceClass::PlatformMmio(Some(
            DeviceKind::ClockController,
        ))),
        b"starfive,jh7110-ccache" | b"sifive,fu740-c000-ccache" | b"sifive,ccache0" => Some(
            DtbDeviceClass::PlatformMmio(Some(DeviceKind::CacheController)),
        ),
        b"pci-host-ecam-generic" => Some(DtbDeviceClass::PlatformDevice(DeviceKind::PciEcam)),
        // Vendor 5.15 dtb (v1.3b) says jh7110-sdio; mainline says
        // jh7110-mmc. Same DesignWare MSHC either way.
        b"starfive,jh7110-sdio" | b"starfive,jh7110-mmc" | b"snps,dw-mshc" => {
            Some(DtbDeviceClass::PlatformDevice(DeviceKind::SdController))
        }
        b"starfive,dwmac" | b"starfive,jh7110-eqos-5.20" | b"snps,dwmac-5.10a" => {
            Some(DtbDeviceClass::PlatformDevice(DeviceKind::Dwmac))
        }
        _ => None,
    }
}

fn first_reg_range(node: &FallibleFdtNode<'_>) -> Option<PhysRange> {
    let reg = node.reg().ok().flatten()?;
    for entry in reg.iter::<u64, u64>() {
        let entry = entry.ok()?;
        if let Some(range) = phys_range_from_reg(entry.address, entry.len) {
            return Some(range);
        }
    }
    None
}

fn phys_range_from_reg(address: u64, len: u64) -> Option<PhysRange> {
    let base = usize::try_from(address).ok()?;
    let size = usize::try_from(len).ok()?;
    (size > 0).then_some(PhysRange {
        start: PhysAddr(base),
        size,
    })
}

fn read_u32_property(node: &FallibleFdtNode<'_>, name: &str) -> Option<u32> {
    let prop = node.raw_property(name).ok().flatten()?;
    let bytes = prop.value.get(..4)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
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

    use super::{classify_compatible, parse_boot_info_from_fdt, DtbBootInfo, DtbDeviceClass};
    use crate::boot_static::{BootStaticBag, IdentityLive};
    use crate::Platform;

    #[test]
    fn classifies_visionfive2_vendor_eqos_as_dwmac() {
        assert_eq!(
            classify_compatible(b"starfive,jh7110-eqos-5.20"),
            Some(DtbDeviceClass::PlatformDevice(tx_hal::DeviceKind::Dwmac))
        );
    }

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
                // Fake-FDT cpus carry no mmu-type -> no data; consumers
                // fall back to the count-prefix mask.
                startable_harts: 0,
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

    // Real-world DTB fixtures (boards/dtbs/, see README there): the fake
    // FDTs above validate parser mechanics; these validate that the same
    // parser reads trees produced by the actual QEMU 9.2.1 evaluation
    // setup and by the physical VisionFive 2 (V1.3B) board.
    const QEMU_RV64_VIRT_DTB: &[u8] = include_bytes!("../../dtbs/qemu-rv64-virt.dtb");
    const JH7110_VF2_DTB: &[u8] =
        include_bytes!("../../dtbs/jh7110-starfive-visionfive-2-v1.3b.dtb");

    #[test]
    fn parses_real_qemu_virt_dumpdtb_fixture() {
        let mut regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; 4];
        let mut cmdline = [0u8; 256];

        let parsed = unsafe {
            parse_boot_info_from_fdt(
                QEMU_RV64_VIRT_DTB.as_ptr() as usize,
                &mut regions,
                &mut cmdline,
            )
        }
        .expect("real qemu virt dtb should parse");

        assert_eq!(parsed.memory_region_count, 1);
        assert_eq!(regions[0].base, PhysAddr(0x8000_0000));
        assert_eq!(regions[0].size, 0x1000_0000);
        assert_eq!(parsed.possible_cpu_count, 4);
        assert_eq!(parsed.timebase_frequency_hz, Some(10_000_000));
        assert_eq!(parsed.initrd, None);
        // Every QEMU hart carries mmu-type -> all startable.
        assert_eq!(parsed.startable_harts, 0b1111);
    }

    #[test]
    fn parses_real_visionfive2_dtb_fixture() {
        let mut regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; 4];
        let mut cmdline = [0u8; 256];

        let parsed = unsafe {
            parse_boot_info_from_fdt(JH7110_VF2_DTB.as_ptr() as usize, &mut regions, &mut cmdline)
        }
        .expect("visionfive2 dtb should parse");

        assert_eq!(parsed.memory_region_count, 1);
        assert_eq!(regions[0].base, PhysAddr(0x4000_0000));
        // The .dts ships a 4 GiB placeholder; U-Boot fixes the real size up
        // at boot. The fixture asserts the file's value.
        assert_eq!(regions[0].size, 0x1_0000_0000);
        // 1x S7 monitor hart + 4x U74 harts.
        assert_eq!(parsed.possible_cpu_count, 5);
        // JH7110 timebase is 4 MHz (QEMU virt is 10 MHz) — this is the
        // board-vs-QEMU difference the dynamic DTB path must carry.
        assert_eq!(parsed.timebase_frequency_hz, Some(4_000_000));
        // hart0 is the S7 monitor core without mmu-type: it must NOT
        // be startable; U74 harts 1..4 are.
        assert_eq!(parsed.startable_harts, 0b11110);
    }

    #[test]
    fn plic_topology_filters_firmware_misdescribed_supervisor_harts() {
        let vf2 = unsafe {
            fdt::Fdt::from_ptr_unaligned_fallible(JH7110_VF2_DTB.as_ptr())
                .expect("visionfive2 dtb should parse")
        };
        let qemu = unsafe {
            fdt::Fdt::from_ptr_unaligned_fallible(QEMU_RV64_VIRT_DTB.as_ptr())
                .expect("qemu virt dtb should parse")
        };

        // Model the faulty U-Boot control FDT's CPU nodes by supplying all five
        // harts as candidates. The PLIC topology still identifies only the
        // four harts that have supervisor external-interrupt contexts.
        assert_eq!(super::normalize_startable_harts(&vf2, 0b11111), 0b11110);
        // Every QEMU hart has a supervisor PLIC context, so none are removed.
        assert_eq!(super::normalize_startable_harts(&qemu, 0b1111), 0b1111);
    }

    #[test]
    fn plic_topology_filter_falls_back_when_contexts_are_unavailable() {
        let no_contexts = [None; super::MAX_PLIC_HARTS];
        assert_eq!(
            super::filter_harts_by_plic_scontexts(0b1010, &no_contexts, 0),
            0b1010,
        );

        let mut contexts = no_contexts;
        contexts[0] = Some(1);
        contexts[1] = Some(3);
        assert_eq!(
            super::filter_harts_by_plic_scontexts(0b0001, &contexts, 2),
            0b0001,
            "PLIC evidence must not add a CPU rejected by its CPU node",
        );
    }

    #[test]
    fn discovers_devices_from_real_qemu_virt_dtb() {
        let mut devices = [tx_hal::DeviceInfo {
            kind: tx_hal::DeviceKind::Uart,
            mmio: PhysRange {
                start: PhysAddr(0),
                size: 0,
            },
            irq: None,
            reg_shift: 0,
            reg_io_width: 1,
        }; 24];

        let count = unsafe {
            super::parse_devices_from_fdt(QEMU_RV64_VIRT_DTB.as_ptr() as usize, &mut devices)
        }
        .expect("QEMU fixture device projection should fit");
        let devices = &devices[..count];

        let uarts: Vec<_> = devices
            .iter()
            .filter(|d| d.kind == tx_hal::DeviceKind::Uart)
            .collect();
        assert_eq!(uarts.len(), 1);
        assert_eq!(uarts[0].mmio.start, PhysAddr(0x1000_0000));
        assert_eq!(uarts[0].irq, Some(10));
        assert_eq!(uarts[0].reg_io_width, 1);

        let plics: Vec<_> = devices
            .iter()
            .filter(|d| d.kind == tx_hal::DeviceKind::IntController)
            .collect();
        assert_eq!(plics.len(), 1);
        assert_eq!(plics[0].mmio.start, PhysAddr(0x0c00_0000));
        assert_eq!(
            devices
                .iter()
                .filter(|d| d.kind == tx_hal::DeviceKind::CacheController)
                .count(),
            0,
            "QEMU coherent DMA must not gain a synthetic cache controller",
        );

        let rtc = devices
            .iter()
            .find(|d| d.kind == tx_hal::DeviceKind::GoldfishRtc)
            .expect("qemu goldfish rtc discovered");
        assert_eq!(rtc.mmio.start, PhysAddr(0x0010_1000));
        assert_eq!(rtc.mmio.size, 0x1000);
        assert_eq!(rtc.irq, Some(11));
        assert!(crate::goldfish_rtc_available_from_devices(devices));
        assert_eq!(crate::uart_irq_from_devices(devices), 10);
        assert_eq!(crate::goldfish_rtc_irq_from_devices(devices), 11);

        let virtio_count = devices
            .iter()
            .filter(|d| d.kind == tx_hal::DeviceKind::VirtioMmio)
            .count();
        assert_eq!(virtio_count, 8);

        assert_eq!(
            devices
                .iter()
                .filter(|d| d.kind == tx_hal::DeviceKind::PciEcam)
                .count(),
            1
        );
    }

    #[test]
    fn device_projection_capacity_exhaustion_is_not_truncated() {
        let mut devices = [tx_hal::DeviceInfo {
            kind: tx_hal::DeviceKind::Uart,
            mmio: PhysRange::empty(),
            irq: None,
            reg_shift: 0,
            reg_io_width: 1,
        }; 1];

        assert_eq!(
            unsafe {
                super::parse_devices_from_fdt(QEMU_RV64_VIRT_DTB.as_ptr() as usize, &mut devices)
            },
            Err(super::DtbDeviceError::LegacyDeviceCapacityExceeded {
                capacity: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn discovers_devices_from_real_visionfive2_dtb() {
        let mut devices = [tx_hal::DeviceInfo {
            kind: tx_hal::DeviceKind::Uart,
            mmio: PhysRange {
                start: PhysAddr(0),
                size: 0,
            },
            irq: None,
            reg_shift: 0,
            reg_io_width: 1,
        }; 24];

        let count = unsafe {
            super::parse_devices_from_fdt(JH7110_VF2_DTB.as_ptr() as usize, &mut devices)
        }
        .expect("VF2 fixture device projection should fit");
        let devices = &devices[..count];

        // dw-apb UART with 32-bit regs at stride 4 — the exact quirk the
        // parameterised 16550 path must honour on the board.
        let uart0 = devices
            .iter()
            .find(|d| d.kind == tx_hal::DeviceKind::Uart && d.mmio.start == PhysAddr(0x1000_0000))
            .expect("vf2 uart0 discovered");
        assert_eq!(
            devices
                .iter()
                .filter(|d| d.kind == tx_hal::DeviceKind::Uart)
                .count(),
            1,
            "disabled VF2 UART nodes must not become runtime devices",
        );
        assert_eq!(uart0.irq, Some(32));
        assert_eq!(uart0.reg_shift, 2);
        assert_eq!(uart0.reg_io_width, 4);

        // Same PLIC base as QEMU virt (SiFive layout).
        let plic = devices
            .iter()
            .find(|d| d.kind == tx_hal::DeviceKind::IntController)
            .expect("vf2 plic discovered");
        assert_eq!(plic.mmio.start, PhysAddr(0x0c00_0000));

        let cache = devices
            .iter()
            .find(|d| d.kind == tx_hal::DeviceKind::CacheController)
            .expect("vf2 cache controller discovered");
        assert_eq!(cache.mmio.start, PhysAddr(0x0201_0000));
        assert_eq!(cache.mmio.size, 0x4000);

        // Two DesignWare MMC hosts; SD card slot is mmc@16020000, IRQ 75.
        let sd: Vec<_> = devices
            .iter()
            .filter(|d| d.kind == tx_hal::DeviceKind::SdController)
            .collect();
        assert_eq!(sd.len(), 2);
        assert!(sd
            .iter()
            .any(|d| d.mmio.start == PhysAddr(0x1602_0000) && d.irq == Some(75)));

        // No virtio anywhere on real hardware.
        assert_eq!(
            devices
                .iter()
                .filter(|d| d.kind == tx_hal::DeviceKind::VirtioMmio)
                .count(),
            0
        );
        assert_eq!(
            devices
                .iter()
                .filter(|d| d.kind == tx_hal::DeviceKind::GoldfishRtc)
                .count(),
            0,
            "JH7110 RTC must not be routed through the Goldfish backend",
        );
        assert!(!crate::goldfish_rtc_available_from_devices(devices));
        assert_eq!(crate::uart_irq_from_devices(devices), 32);
        assert_eq!(crate::goldfish_rtc_irq_from_devices(devices), 0);
    }

    #[test]
    fn derives_plic_scontexts_from_real_qemu_virt_dtb() {
        let mut contexts = [None; super::MAX_PLIC_HARTS];
        let resolved = unsafe {
            super::parse_plic_scontexts_from_fdt(
                QEMU_RV64_VIRT_DTB.as_ptr() as usize,
                &mut contexts,
            )
        };

        // QEMU virt: every hart has M then S context -> S = 2*hart+1.
        assert_eq!(resolved, 4);
        assert_eq!(contexts[0], Some(1));
        assert_eq!(contexts[1], Some(3));
        assert_eq!(contexts[2], Some(5));
        assert_eq!(contexts[3], Some(7));
        assert_eq!(contexts[4], None);
    }

    #[test]
    fn derives_plic_scontexts_from_real_visionfive2_dtb() {
        let mut contexts = [None; super::MAX_PLIC_HARTS];
        let resolved = unsafe {
            super::parse_plic_scontexts_from_fdt(JH7110_VF2_DTB.as_ptr() as usize, &mut contexts)
        };

        // VF2: hart0 is the S7 monitor core (M-only, no S context); the
        // U74 harts 1..4 land on S contexts 2,4,6,8. The QEMU 2*hart+1
        // formula would be wrong for every one of them.
        assert_eq!(resolved, 4);
        assert_eq!(contexts[0], None);
        assert_eq!(contexts[1], Some(2));
        assert_eq!(contexts[2], Some(4));
        assert_eq!(contexts[3], Some(6));
        assert_eq!(contexts[4], Some(8));
    }

    #[test]
    fn parses_firmware_reserved_regions() {
        // Mirror OpenSBI's on-board shape: a /reserved-memory child for
        // the PMP-protected firmware home, plus one legacy header
        // memreserve entry.
        let mut strings = Vec::new();
        let address_cells = add_string(&mut strings, "#address-cells");
        let size_cells = add_string(&mut strings, "#size-cells");
        let ranges = add_string(&mut strings, "ranges");
        let reg = add_string(&mut strings, "reg");

        let mut structure = Vec::new();
        begin_node(&mut structure, "");
        prop_u32(&mut structure, address_cells, 2);
        prop_u32(&mut structure, size_cells, 2);
        begin_node(&mut structure, "reserved-memory");
        prop_u32(&mut structure, address_cells, 2);
        prop_u32(&mut structure, size_cells, 2);
        prop_bytes(&mut structure, ranges, b"");
        begin_node(&mut structure, "mmode_resv0@40000000");
        prop_cells(&mut structure, reg, &[0, 0x4000_0000, 0, 0x8_0000]);
        end_node(&mut structure);
        end_node(&mut structure);
        end_node(&mut structure);
        push_be32(&mut structure, 9);

        let header_len = 40usize;
        let reserve_len = 32usize; // one entry + zero terminator
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
        // memreserve: [0x9000_0000, +0x1_0000), then the zero pair.
        fdt.extend_from_slice(&0x9000_0000u64.to_be_bytes());
        fdt.extend_from_slice(&0x1_0000u64.to_be_bytes());
        fdt.extend_from_slice(&[0u8; 16]);
        fdt.extend_from_slice(&structure);
        fdt.extend_from_slice(&strings);

        let mut out = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; 8];
        let count =
            unsafe { super::parse_reserved_regions_from_fdt(fdt.as_ptr() as usize, &mut out) };

        assert_eq!(count, 2);
        assert_eq!(out[0].base, PhysAddr(0x4000_0000));
        assert_eq!(out[0].size, 0x8_0000);
        assert_eq!(out[0].kind, MemoryRegionKind::Reserved);
        assert_eq!(out[1].base, PhysAddr(0x9000_0000));
        assert_eq!(out[1].size, 0x1_0000);

        // QEMU's dumpdtb carries neither source -> zero impact there.
        let qemu_count = unsafe {
            super::parse_reserved_regions_from_fdt(QEMU_RV64_VIRT_DTB.as_ptr() as usize, &mut out)
        };
        assert_eq!(qemu_count, 0);
    }

    #[test]
    fn low_memory_regions_extend_direct_map_downward() {
        // VF2 shape: DDR reported from 0x4000_0000. Boot must add the
        // missing low direct-map leaves and lower the published pmap
        // facts; QEMU trees (base 0x8000_0000) leave everything as-is.
        let fdt = fake_qemu_fdt_with_memory_ranges(&[(0x4000_0000, 0x8000_0000)]);
        unsafe {
            BootStaticBag::<IdentityLive>::reset_global_for_test();
        }

        let _handoff = Platform::boot_handoff(0, fdt.as_ptr() as usize);

        let info = <Platform as tx_hal::PmapIf>::bootstrap_pmap_info()
            .expect("bootstrap pmap info published");
        assert_eq!(info.mapped.start, PhysAddr(0x4000_0000));
        assert_eq!(
            info.direct_map.start.0,
            info.direct_map_base.0 + 0x4000_0000
        );
        // Root slot 257 = (DIRECT_MAP_BASE + 0x4000_0000) >> 30 & 0x1ff:
        // expect a 1 GiB leaf PTE for phys 0x4000_0000 with
        // V|R|W|G|A|D = 0xe7 ((phys >> 12) << 10 = 0x1000_0000).
        let root =
            crate::boot_static::BootStaticBag::<crate::boot_static::IdentityDropped>::global_ref()
                .bootstrap_root_ref();
        assert_eq!(root.0[257], 0x1000_0000 | 0xe7);
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
