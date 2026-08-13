//! Static binder adapter for firmware-described Loongson 2K1000 AHCI.

use alloc::boxed::Box;
use core::marker::PhantomData;

use tx_hal::{
    DeviceMatchId, DeviceResource, DmaDomain, DmaDomainRef, MmioRegion, PlatformDevice,
    PlatformInfoIf, ResourceKind, ResourceRole, TxPlatform,
};
use tx_subsystems::device::{BlockDeviceRegistration, DevT};
use tx_subsystems::device_binding::{BoundDeviceRegistration, DriverId};

use super::binder::{
    DeviceBindError, DeviceBindReservation, DriverMatch, MatchPriority, StaticDriverDescriptor,
};

const AHCI_COMPATIBLES: [&str; 2] = ["loongson,ls-ahci", "snps,spear-ahci"];
const SCSI_DISK_MAJOR: u32 = 8;
const DRIVER_ID: DriverId = DriverId("loongson-2k1000-ahci-block");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AhciBlockResources {
    region: MmioRegion,
    dma: DmaDomainRef,
}

/// Descriptor value used by the 2K1000 board binary's device bundle.
pub const fn driver_descriptor<P: TxPlatform>() -> StaticDriverDescriptor<P> {
    StaticDriverDescriptor {
        id: DRIVER_ID,
        match_device,
        prepare: prepare::<P>,
        _platform: PhantomData,
    }
}

fn match_device(device: &PlatformDevice) -> DriverMatch {
    if device.matches.iter().any(|candidate| {
        matches!(candidate, DeviceMatchId::FirmwareCompatible(value)
            if AHCI_COMPATIBLES.contains(value))
    }) {
        DriverMatch::Supported(MatchPriority(100))
    } else {
        DriverMatch::Unsupported
    }
}

fn prepare<P: TxPlatform + 'static>(
    device: &'static PlatformDevice,
    reservation: &mut DeviceBindReservation<P>,
) -> Result<(), DeviceBindError> {
    tx_hal::console_write_str::<P>("txkernel:ahci:candidate\n");
    let resources = decode_resources(device).map_err(|error| {
        tx_hal::console_write_str::<P>("txkernel:ahci:resources:");
        tx_hal::console_write_str::<P>(resource_error_label(error));
        tx_hal::console_write_str::<P>("\n");
        error
    })?;
    if let Err(error) = <P as PlatformInfoIf>::prepare_platform_device(device) {
        tx_hal::console_write_str::<P>("txkernel:ahci:platform-prepare:");
        tx_hal::console_write_str::<P>(error.label());
        tx_hal::console_write_str::<P>("\n");
        return Err(DeviceBindError::DriverProbe { code: 3 });
    }

    let dma_domain = resolve_dma_domain::<P>(resources.dma)?;
    let block = Box::new(tx_drivers::ahci::AhciBlock::<P>::new(
        resources.region,
        dma_domain,
    ));
    let identity = match block.init() {
        Ok(identity) => identity,
        Err(error) => {
            tx_hal::console_write_str::<P>("txkernel:ahci:probe:");
            tx_hal::console_write_str::<P>(error.label());
            tx_hal::console_write_str::<P>("\n");
            return Err(DeviceBindError::DriverProbe { code: 1 });
        }
    };
    let block = Box::leak(block);
    let probe = alloc::format!(
        "txkernel:ahci:probe:ok:blocks={}:block-size={}\n",
        identity.total_blocks,
        identity.logical_block_size
    );
    tx_hal::console_write_str::<P>(&probe);

    let registration = Box::leak(Box::new(BlockDeviceRegistration {
        devt: DevT::new(SCSI_DISK_MAJOR, 0),
        name: "sda",
        ops: block,
    }));
    reservation.propose_registration(BoundDeviceRegistration::Block(registration))
}

fn resource_error_label(error: DeviceBindError) -> &'static str {
    match error {
        DeviceBindError::MissingResource {
            kind: ResourceKind::Mmio,
            ..
        } => "missing-mmio",
        DeviceBindError::MissingResource {
            kind: ResourceKind::DmaDomain,
            ..
        } => "missing-dma",
        DeviceBindError::DuplicateResource {
            kind: ResourceKind::Mmio,
            ..
        } => "duplicate-mmio",
        DeviceBindError::DuplicateResource {
            kind: ResourceKind::DmaDomain,
            ..
        } => "duplicate-dma",
        _ => "invalid-resource-set",
    }
}

fn decode_resources(device: &PlatformDevice) -> Result<AhciBlockResources, DeviceBindError> {
    let mmio = exact_mmio(device)?;
    let dma = exact_dma(device)?;
    let name = match device.id.local {
        tx_hal::DeviceLocalId::FirmwarePath(path) | tx_hal::DeviceLocalId::PlatformKey(path) => {
            path
        }
        tx_hal::DeviceLocalId::PciFunction(_) => DRIVER_ID.0,
    };
    Ok(AhciBlockResources {
        region: MmioRegion {
            name,
            phys: mmio.phys,
            virt: mmio.virt,
            flags: mmio.flags,
        },
        dma,
    })
}

fn resolve_dma_domain<P: TxPlatform>(
    reference: DmaDomainRef,
) -> Result<&'static DmaDomain, DeviceBindError> {
    <P as PlatformInfoIf>::platform_info()
        .device_resources
        .dma_domains
        .iter()
        .find(|domain| domain.id == reference.domain)
        .ok_or(DeviceBindError::DriverProbe { code: 2 })
}

fn exact_mmio(device: &PlatformDevice) -> Result<tx_hal::MmioResource, DeviceBindError> {
    let role = ResourceRole::Index(0);
    let mut selected = None;
    for resource in device.resources {
        if let DeviceResource::Mmio(mmio) = resource {
            if mmio.role != role {
                continue;
            }
            if selected.replace(*mmio).is_some() {
                return Err(DeviceBindError::DuplicateResource {
                    device: device.id,
                    kind: ResourceKind::Mmio,
                    role,
                });
            }
        }
    }
    selected.ok_or(DeviceBindError::MissingResource {
        device: device.id,
        kind: ResourceKind::Mmio,
        role,
    })
}

fn exact_dma(device: &PlatformDevice) -> Result<DmaDomainRef, DeviceBindError> {
    let role = ResourceRole::Index(0);
    let mut selected = None;
    for resource in device.resources {
        if let DeviceResource::DmaDomain(domain) = resource {
            if domain.role != role {
                continue;
            }
            if selected.replace(*domain).is_some() {
                return Err(DeviceBindError::DuplicateResource {
                    device: device.id,
                    kind: ResourceKind::DmaDomain,
                    role,
                });
            }
        }
    }
    selected.ok_or(DeviceBindError::MissingResource {
        device: device.id,
        kind: ResourceKind::DmaDomain,
        role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_hal::{
        DeviceId, DeviceLocalId, DeviceStatus, DmaDomainId, MmioFlags, MmioResource, PhysAddr,
        PhysRange, ResourceOrigin, ResourceOriginKind, ResourceProviderId, VirtAddr, VirtRange,
    };

    const PROVIDER: ResourceProviderId = ResourceProviderId("ahci-test");
    const ORIGIN: ResourceOrigin = ResourceOrigin {
        provider: PROVIDER,
        record: "fixture",
        kind: ResourceOriginKind::Firmware,
    };
    const DEVICE_ID: DeviceId = DeviceId {
        provider: PROVIDER,
        local: DeviceLocalId::FirmwarePath("/soc/sata@400e0000"),
    };
    static MATCHES: [DeviceMatchId; 1] = [DeviceMatchId::FirmwareCompatible("loongson,ls-ahci")];
    static SPEAR_MATCHES: [DeviceMatchId; 1] =
        [DeviceMatchId::FirmwareCompatible("snps,spear-ahci")];
    const MMIO: tx_hal::MmioResource = MmioResource {
        role: ResourceRole::Index(0),
        phys: PhysRange {
            start: PhysAddr(0x400e_0000),
            size: 0x1_0000,
        },
        virt: VirtRange {
            start: VirtAddr(0x9000_0000_400e_0000),
            size: 0x1_0000,
        },
        flags: MmioFlags::READ.union(MmioFlags::WRITE),
        origin: ORIGIN,
    };
    const DMA: DmaDomainRef = DmaDomainRef {
        role: ResourceRole::Index(0),
        domain: DmaDomainId {
            provider: PROVIDER,
            local: 0,
        },
    };
    static RESOURCES: [DeviceResource; 2] =
        [DeviceResource::Mmio(MMIO), DeviceResource::DmaDomain(DMA)];

    fn device() -> PlatformDevice {
        PlatformDevice {
            id: DEVICE_ID,
            status: DeviceStatus::Enabled,
            matches: &MATCHES,
            resources: &RESOURCES,
            origin: ORIGIN,
        }
    }

    #[test]
    fn matches_both_board_compatibles_without_using_mmio_address() {
        let first = device();
        let mut relocated = first;
        relocated.id.local = DeviceLocalId::FirmwarePath("/soc/sata@500e0000");
        relocated.matches = &SPEAR_MATCHES;

        assert_eq!(
            match_device(&first),
            DriverMatch::Supported(MatchPriority(100))
        );
        assert_eq!(match_device(&relocated), match_device(&first));
    }

    #[test]
    fn decoder_requires_only_mmio_and_dma_for_polling_mode() {
        let decoded = decode_resources(&device()).unwrap();
        assert_eq!(decoded.region.phys, MMIO.phys);
        assert_eq!(decoded.dma, DMA);
    }
}
