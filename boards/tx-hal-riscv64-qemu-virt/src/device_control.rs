//! Firmware-driven platform control preparation for concrete devices.
//!
//! This is deliberately board-side: generic drivers receive a typed device
//! record and never embed JH7110 clock/reset register addresses. The provider
//! register banks and consumer control IDs are resolved from the live FDT.

use core::ptr::{read_volatile, write_volatile};

use tx_hal::{DeviceLocalId, DeviceMatchId, PhysRange, PlatformDevice, PlatformDevicePrepareError};

use crate::boot_static::{BootStaticBag, IdentityDropped};
use crate::pmap::topology::direct_map_virt;

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

const JH7110_CLK_SYS_END: u32 = 190;
const JH7110_CLK_STG_END: u32 = 219;
const JH7110_CLK_AON_END: u32 = 233;
const CLOCK_GATE_ENABLE: u32 = 1 << 31;
const RESET_POLL_LIMIT: usize = 10_000;

pub(crate) fn prepare_platform_device(
    device: &'static PlatformDevice,
) -> Result<(), PlatformDevicePrepareError> {
    if !device.matches.iter().any(|candidate| {
        matches!(candidate, DeviceMatchId::FirmwareCompatible(value)
            if *value == "starfive,dwmac"
                || *value == "starfive,jh7110-dwmac"
                || *value == "starfive,jh7110-eqos-5.20")
    }) {
        return Ok(());
    }

    let path = match device.id.local {
        DeviceLocalId::FirmwarePath(path) => path,
        DeviceLocalId::PlatformKey(_) | DeviceLocalId::PciFunction(_) => {
            return Err(PlatformDevicePrepareError::MissingFirmwareNode);
        }
    };
    let dtb = BootStaticBag::<IdentityDropped>::global_ref().firmware_dtb_parse_addr();
    let fdt = unsafe { fdt::Fdt::from_ptr_unaligned_fallible(dtb as *const u8) }
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?;
    let node = fdt
        .find_node(path)
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?
        .ok_or(PlatformDevicePrepareError::MissingFirmwareNode)?;

    enable_clocks(&fdt, &node)?;
    deassert_resets(&fdt, &node)
}

fn enable_clocks(
    fdt: &FallibleFdt<'_>,
    device: &FallibleFdtNode<'_>,
) -> Result<(), PlatformDevicePrepareError> {
    let clocks = raw_property(device, "clocks")?;
    let names = raw_property(device, "clock-names")?;
    let mut cursor = 0usize;
    let mut role_index = 0usize;
    while cursor * 4 < clocks.len() {
        let phandle = cell(clocks, cursor)?;
        let provider = find_soc_provider(fdt, phandle)?;
        let spec_cells = read_u32_property(&provider, "#clock-cells")? as usize;
        if spec_cells != 1 {
            return Err(PlatformDevicePrepareError::UnsupportedProvider);
        }
        let id = cell(clocks, cursor + 1)?;
        let role = nul_string(names, role_index)
            .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)?;
        if clock_role_has_gate(role) {
            enable_jh7110_clock(&provider, id)?;
        }
        cursor += 1 + spec_cells;
        role_index += 1;
    }
    Ok(())
}

fn deassert_resets(
    fdt: &FallibleFdt<'_>,
    device: &FallibleFdtNode<'_>,
) -> Result<(), PlatformDevicePrepareError> {
    let resets = raw_property(device, "resets")?;
    let mut cursor = 0usize;
    while cursor * 4 < resets.len() {
        let phandle = cell(resets, cursor)?;
        let provider = find_soc_provider(fdt, phandle)?;
        let spec_cells = read_u32_property(&provider, "#reset-cells")? as usize;
        if spec_cells != 1 {
            return Err(PlatformDevicePrepareError::UnsupportedProvider);
        }
        let id = cell(resets, cursor + 1)?;
        deassert_jh7110_reset(&provider, id)?;
        cursor += 1 + spec_cells;
    }
    Ok(())
}

fn enable_jh7110_clock(
    provider: &FallibleFdtNode<'_>,
    id: u32,
) -> Result<(), PlatformDevicePrepareError> {
    if !has_compatible(provider, b"starfive,jh7110-clkgen") {
        return Err(PlatformDevicePrepareError::UnsupportedProvider);
    }
    let (bank, offset) = jh7110_clock_location(id)?;
    let range = reg_range(provider, bank)?;
    update_register(range, offset, |value| value | CLOCK_GATE_ENABLE)
}

fn deassert_jh7110_reset(
    provider: &FallibleFdtNode<'_>,
    id: u32,
) -> Result<(), PlatformDevicePrepareError> {
    if !has_compatible(provider, b"starfive,jh7110-reset") {
        return Err(PlatformDevicePrepareError::UnsupportedProvider);
    }
    let mask = 1u32 << (id % 32);
    let (bank, assert_offset, status_offset) = jh7110_reset_location(id)?;
    let range = reg_range(provider, bank)?;
    update_register(range, assert_offset, |value| value & !mask)?;
    let status = register_address(range, status_offset)?;
    for _ in 0..RESET_POLL_LIMIT {
        if unsafe { read_volatile(status) } & mask == mask {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(PlatformDevicePrepareError::ControlTimeout)
}

fn jh7110_clock_location(id: u32) -> Result<(usize, usize), PlatformDevicePrepareError> {
    let (bank, local) = if id < JH7110_CLK_SYS_END {
        (0, id)
    } else if id < JH7110_CLK_STG_END {
        (1, id - JH7110_CLK_SYS_END)
    } else if id < JH7110_CLK_AON_END {
        (2, id - JH7110_CLK_STG_END)
    } else {
        return Err(PlatformDevicePrepareError::UnsupportedProvider);
    };
    Ok((
        bank,
        (local as usize)
            .checked_mul(4)
            .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)?,
    ))
}

fn jh7110_reset_location(id: u32) -> Result<(usize, usize, usize), PlatformDevicePrepareError> {
    let group = id / 32;
    match group {
        0..=3 => Ok((0, 0x2f8 + group as usize * 4, 0x308 + group as usize * 4)),
        4 => Ok((1, 0x74, 0x78)),
        5 => Ok((2, 0x38, 0x3c)),
        6 => Ok((3, 0x38, 0x3c)),
        7 => Ok((4, 0x48, 0x4c)),
        _ => Err(PlatformDevicePrepareError::UnsupportedProvider),
    }
}

fn clock_role_has_gate(role: &[u8]) -> bool {
    matches!(
        role,
        b"gtx" | b"tx" | b"ptp_ref" | b"stmmaceth" | b"pclk" | b"gtxc"
    )
}

fn find_soc_provider<'a>(
    fdt: &FallibleFdt<'a>,
    phandle: u32,
) -> Result<FallibleFdtNode<'a>, PlatformDevicePrepareError> {
    let soc = fdt
        .find_node("/soc")
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?
        .ok_or(PlatformDevicePrepareError::MissingFirmwareNode)?;
    let children = soc
        .children()
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?;
    children
        .iter()
        .filter_map(Result::ok)
        .find(|candidate| {
            read_u32_property(candidate, "phandle").ok() == Some(phandle)
                || read_u32_property(candidate, "linux,phandle").ok() == Some(phandle)
        })
        .ok_or(PlatformDevicePrepareError::MissingFirmwareNode)
}

fn reg_range(
    node: &FallibleFdtNode<'_>,
    selected: usize,
) -> Result<PhysRange, PlatformDevicePrepareError> {
    let reg = node
        .reg()
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?
        .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)?;
    let entry = reg
        .iter::<u64, u64>()
        .nth(selected)
        .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)?
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?;
    Ok(PhysRange {
        start: tx_hal::PhysAddr(
            usize::try_from(entry.address)
                .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?,
        ),
        size: usize::try_from(entry.len)
            .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?,
    })
}

fn update_register(
    range: PhysRange,
    offset: usize,
    update: impl FnOnce(u32) -> u32,
) -> Result<(), PlatformDevicePrepareError> {
    let address = register_address(range, offset)?;
    let current = unsafe { read_volatile(address) };
    unsafe { write_volatile(address, update(current)) };
    Ok(())
}

fn register_address(
    range: PhysRange,
    offset: usize,
) -> Result<*mut u32, PlatformDevicePrepareError> {
    if offset.checked_add(4).is_none_or(|end| end > range.size) {
        return Err(PlatformDevicePrepareError::MalformedFirmwareProperty);
    }
    Ok(direct_map_virt(range.start.0 + offset) as *mut u32)
}

fn raw_property<'a>(
    node: &FallibleFdtNode<'a>,
    name: &str,
) -> Result<&'a [u8], PlatformDevicePrepareError> {
    node.raw_property(name)
        .map_err(|_| PlatformDevicePrepareError::MalformedFirmwareProperty)?
        .map(|property| property.value)
        .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)
}

fn read_u32_property(
    node: &FallibleFdtNode<'_>,
    name: &str,
) -> Result<u32, PlatformDevicePrepareError> {
    cell(raw_property(node, name)?, 0)
}

fn cell(bytes: &[u8], index: usize) -> Result<u32, PlatformDevicePrepareError> {
    let start = index
        .checked_mul(4)
        .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)?;
    let value = bytes
        .get(start..start + 4)
        .ok_or(PlatformDevicePrepareError::MalformedFirmwareProperty)?;
    Ok(u32::from_be_bytes(value.try_into().map_err(|_| {
        PlatformDevicePrepareError::MalformedFirmwareProperty
    })?))
}

fn nul_string(bytes: &[u8], selected: usize) -> Option<&[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .nth(selected)
}

fn has_compatible(node: &FallibleFdtNode<'_>, expected: &[u8]) -> bool {
    node.raw_property("compatible")
        .ok()
        .flatten()
        .is_some_and(|property| {
            property
                .value
                .split(|byte| *byte == 0)
                .any(|entry| entry == expected)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_clock_ids_select_sys_and_aon_banks_without_addresses() {
        assert_eq!(jh7110_clock_location(108), Ok((0, 108 * 4)));
        assert_eq!(jh7110_clock_location(221), Ok((2, 2 * 4)));
        assert_eq!(jh7110_clock_location(224), Ok((2, 5 * 4)));
    }

    #[test]
    fn vendor_reset_ids_select_provider_bank_and_status_register() {
        assert_eq!(jh7110_reset_location(160), Ok((2, 0x38, 0x3c)));
        assert_eq!(jh7110_reset_location(161), Ok((2, 0x38, 0x3c)));
        assert_eq!(jh7110_reset_location(66), Ok((0, 0x300, 0x310)));
    }

    #[test]
    fn dwmac_clock_roles_only_gate_real_gate_outputs() {
        assert!(clock_role_has_gate(b"gtx"));
        assert!(clock_role_has_gate(b"stmmaceth"));
        assert!(!clock_role_has_gate(b"rmii_rtx"));
    }
}
