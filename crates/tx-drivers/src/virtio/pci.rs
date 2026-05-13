use tx_hal::{MmioRegion, PlatformInfoIf, TxPlatform};
use virtio_drivers::transport::{
    pci::{
        bus::{
            BarInfo, Cam, Command, ConfigurationAccess, DeviceFunction, MemoryBarType, MmioCam,
            PciRoot,
        },
        virtio_device_type, PciTransport,
    },
    DeviceType,
};

use super::dma::TxVirtioHal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioPciError {
    MissingMmioRegion(&'static str),
    NoBlockDevice,
    NoNetDevice,
    BarProbe,
    BarTooLarge,
    BarAddressExhausted,
    Transport,
}

impl VirtioPciError {
    pub const fn as_str(self) -> &'static str {
        match self {
            VirtioPciError::MissingMmioRegion(_) => "missing-mmio-region",
            VirtioPciError::NoBlockDevice => "no-block-device",
            VirtioPciError::BarProbe => "bar-probe",
            VirtioPciError::BarTooLarge => "bar-too-large",
            VirtioPciError::BarAddressExhausted => "bar-address-exhausted",
            VirtioPciError::Transport => "transport",
        }
    }
}

pub fn mmio_region<P: TxPlatform>(name: &'static str) -> Result<MmioRegion, VirtioPciError> {
    <P as PlatformInfoIf>::platform_info()
        .mmio_regions
        .iter()
        .copied()
        .find(|region| region.name == name)
        .ok_or(VirtioPciError::MissingMmioRegion(name))
}

pub fn find_virtio_blk_transport<P: TxPlatform>(
    ecam_region: MmioRegion,
    mmio32_region: MmioRegion,
) -> Result<PciTransport, VirtioPciError> {
    find_virtio_transport::<P>(ecam_region, mmio32_region, DeviceType::Block).map_err(|err| {
        match err {
            VirtioPciError::NoNetDevice => VirtioPciError::NoBlockDevice,
            other => other,
        }
    })
}

pub fn find_virtio_net_transport<P: TxPlatform>(
    ecam_region: MmioRegion,
    mmio32_region: MmioRegion,
) -> Result<PciTransport, VirtioPciError> {
    find_virtio_transport::<P>(ecam_region, mmio32_region, DeviceType::Network)
}

fn find_virtio_transport<P: TxPlatform>(
    ecam_region: MmioRegion,
    mmio32_region: MmioRegion,
    device_type: DeviceType,
) -> Result<PciTransport, VirtioPciError> {
    let cam = unsafe { MmioCam::new(ecam_region.virt.start.0 as *mut u8, Cam::Ecam) };
    let mut root = PciRoot::new(cam);
    let mut allocator = PciMemory32Allocator::new(
        mmio32_region.phys.start.0 as u64,
        mmio32_region.phys.size as u64,
    )?;

    for (device_function, info) in root.enumerate_bus(0) {
        if virtio_device_type(&info) != Some(device_type) {
            continue;
        }
        allocate_bars(&mut root, device_function, &mut allocator)?;
        root.set_command(
            device_function,
            Command::MEMORY_SPACE | Command::BUS_MASTER | Command::IO_SPACE,
        );
        return PciTransport::new::<TxVirtioHal<P>, _>(&mut root, device_function)
            .map_err(|_| VirtioPciError::Transport);
    }

    match device_type {
        DeviceType::Block => Err(VirtioPciError::NoBlockDevice),
        DeviceType::Network => Err(VirtioPciError::NoNetDevice),
        _ => Err(VirtioPciError::Transport),
    }
}

struct PciMemory32Allocator {
    next: u64,
    end: u64,
}

impl PciMemory32Allocator {
    fn new(start: u64, size: u64) -> Result<Self, VirtioPciError> {
        let end = start
            .checked_add(size)
            .ok_or(VirtioPciError::BarAddressExhausted)?;
        if end > u32::MAX as u64 + 1 {
            return Err(VirtioPciError::BarTooLarge);
        }
        Ok(Self { next: start, end })
    }

    fn allocate(&mut self, size: u64) -> Result<u64, VirtioPciError> {
        if size == 0 {
            return Ok(self.next);
        }
        if !size.is_power_of_two() || size > u32::MAX as u64 {
            return Err(VirtioPciError::BarTooLarge);
        }
        let aligned = align_up(self.next, size);
        let end = aligned
            .checked_add(size)
            .ok_or(VirtioPciError::BarAddressExhausted)?;
        if end > self.end {
            return Err(VirtioPciError::BarAddressExhausted);
        }
        self.next = end;
        Ok(aligned)
    }
}

fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

fn allocate_bars(
    root: &mut PciRoot<impl ConfigurationAccess>,
    device_function: DeviceFunction,
    allocator: &mut PciMemory32Allocator,
) -> Result<(), VirtioPciError> {
    let bars = root
        .bars(device_function)
        .map_err(|_| VirtioPciError::BarProbe)?;
    for (bar_index, bar) in bars.into_iter().enumerate() {
        let Some(BarInfo::Memory {
            address_type, size, ..
        }) = bar
        else {
            continue;
        };
        if size == 0 {
            continue;
        }
        let address = allocator.allocate(size)?;
        match address_type {
            MemoryBarType::Width32 => {
                root.set_bar_32(device_function, bar_index as u8, address as u32);
            }
            MemoryBarType::Width64 => {
                root.set_bar_64(device_function, bar_index as u8, address);
            }
            MemoryBarType::Below1MiB => return Err(VirtioPciError::BarTooLarge),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_memory32_allocator_aligns_and_bounds_allocations() {
        let mut allocator = PciMemory32Allocator::new(0x4000_1001, 0xffff).unwrap();
        assert_eq!(allocator.allocate(0x1000).unwrap(), 0x4000_2000);
        assert_eq!(allocator.allocate(0x2000).unwrap(), 0x4000_4000);
        assert_eq!(
            allocator.allocate(0x1_0000),
            Err(VirtioPciError::BarAddressExhausted)
        );
    }
}
