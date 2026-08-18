//! Static binder adapter for firmware-described VirtIO MMIO network devices.

use alloc::boxed::Box;
use alloc::format;
use core::marker::PhantomData;

use tx_hal::{
    DeviceMatchId, DeviceResource, DmaDomain, DmaDomainRef, IrqResource, MmioRegion,
    PlatformDevice, PlatformInfoIf, ResourceKind, ResourceRole, TxPlatform,
};
use tx_subsystems::device::DevT;
use tx_subsystems::device_binding::{BoundDevice, BoundDeviceRegistration, DriverId};
use tx_subsystems::net::NetDeviceRegistration;

use super::binder::{
    DeviceBindError, DeviceBindReservation, DeviceClass, DriverMatch, MatchPriority,
    StaticDriverDescriptor,
};

const VIRTIO_MMIO_COMPATIBLE: &str = "virtio,mmio";
const VIRTIO_NET_MAJOR: u32 = 97;
const DRIVER_ID: DriverId = DriverId("virtio-mmio-net");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtioMmioNetResources {
    region: MmioRegion,
    irq: IrqResource,
    dma: DmaDomainRef,
}

/// Descriptor value used by a board binary's compile-time device bundle.
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
        matches!(candidate, DeviceMatchId::FirmwareCompatible(value) if *value == VIRTIO_MMIO_COMPATIBLE)
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
    let resources = decode_resources(device)?;
    let dma_domain = resolve_dma_domain::<P>(resources.dma)?;
    tx_drivers::virtio::dma::configure_dma_domain(dma_domain)
        .map_err(|_| DeviceBindError::DriverProbe { code: 3 })?;
    let net = Box::leak(Box::new(
        tx_drivers::virtio::VirtioMmioNet::<P, 256>::from_region(resources.region),
    ));
    match net.init() {
        Ok(()) => {}
        Err(tx_drivers::virtio::net::VirtioNetError::WrongDeviceType(_)) => {
            return Err(DeviceBindError::DriverProbe { code: 1 });
        }
        Err(_) => return Err(DeviceBindError::DriverProbe { code: 2 }),
    }

    let ordinal = reservation.registration_ordinal(DeviceClass::Net);
    let name = Box::leak(format!("eth{ordinal}").into_boxed_str());
    let registration = Box::leak(Box::new(NetDeviceRegistration {
        devt: DevT::new(VIRTIO_NET_MAJOR, u32::from(ordinal)),
        name,
        ops: net,
    }));

    reservation.propose_registration(BoundDeviceRegistration::Net(registration))?;
    reservation.propose_irq(resources.irq, crate::irq::typed_net_rx_irq_handler::<P>)?;
    reservation.propose_activation(activate)
}

fn activate(device: &'static BoundDevice) {
    if let BoundDeviceRegistration::Net(registration) = device.registration {
        registration.ops.enable_interrupts();
    }
}

fn decode_resources(device: &PlatformDevice) -> Result<VirtioMmioNetResources, DeviceBindError> {
    let mmio = exact_mmio(device)?;
    let irq = exact_irq(device)?;
    let dma = exact_dma(device)?;
    let name = match device.id.local {
        tx_hal::DeviceLocalId::FirmwarePath(path) | tx_hal::DeviceLocalId::PlatformKey(path) => {
            path
        }
        tx_hal::DeviceLocalId::PciFunction(_) => VIRTIO_MMIO_COMPATIBLE,
    };
    Ok(VirtioMmioNetResources {
        region: MmioRegion {
            name,
            phys: mmio.phys,
            virt: mmio.virt,
            flags: mmio.flags,
        },
        irq,
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
        .ok_or(DeviceBindError::DriverProbe { code: 4 })
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

fn exact_irq(device: &PlatformDevice) -> Result<IrqResource, DeviceBindError> {
    let role = ResourceRole::Index(0);
    let mut selected = None;
    for resource in device.resources {
        if let DeviceResource::Irq(irq) = resource {
            if irq.role != role {
                continue;
            }
            if selected.replace(*irq).is_some() {
                return Err(DeviceBindError::DuplicateResource {
                    device: device.id,
                    kind: ResourceKind::Irq,
                    role,
                });
            }
        }
    }
    selected.ok_or(DeviceBindError::MissingResource {
        device: device.id,
        kind: ResourceKind::Irq,
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
        DeviceId, DeviceLocalId, DeviceStatus, DmaDomainId, IrqPolarity, IrqSharing, IrqTrigger,
        MmioFlags, MmioResource, PhysAddr, PhysRange, ResourceOrigin, ResourceOriginKind,
        ResourceProviderId, VirtAddr, VirtRange,
    };

    const PROVIDER: ResourceProviderId = ResourceProviderId("virtio-mmio-test");
    const ORIGIN: ResourceOrigin = ResourceOrigin {
        provider: PROVIDER,
        record: "fixture",
        kind: ResourceOriginKind::Firmware,
    };
    const DEVICE_ID: DeviceId = DeviceId {
        provider: PROVIDER,
        local: DeviceLocalId::FirmwarePath("/soc/virtio@10002000"),
    };
    static MATCHES: [DeviceMatchId; 1] = [DeviceMatchId::FirmwareCompatible("virtio,mmio")];
    const MMIO: MmioResource = MmioResource {
        role: ResourceRole::Index(0),
        phys: PhysRange {
            start: PhysAddr(0x1000_2000),
            size: 0x1000,
        },
        virt: VirtRange {
            start: VirtAddr(0xffff_ffc0_1000_2000),
            size: 0x1000,
        },
        flags: MmioFlags::READ.union(MmioFlags::WRITE),
        origin: ORIGIN,
    };
    const IRQ: IrqResource = IrqResource {
        role: ResourceRole::Index(0),
        line: 2,
        trigger: IrqTrigger::Level,
        polarity: IrqPolarity::High,
        sharing: IrqSharing::Exclusive,
        origin: ORIGIN,
    };
    const DMA: DmaDomainRef = DmaDomainRef {
        role: ResourceRole::Index(0),
        domain: DmaDomainId {
            provider: PROVIDER,
            local: 0,
        },
    };
    static COMPLETE_RESOURCES: [DeviceResource; 3] = [
        DeviceResource::Mmio(MMIO),
        DeviceResource::Irq(IRQ),
        DeviceResource::DmaDomain(DMA),
    ];
    static MISSING_IRQ_RESOURCES: [DeviceResource; 2] =
        [DeviceResource::Mmio(MMIO), DeviceResource::DmaDomain(DMA)];
    static MISSING_DMA_RESOURCES: [DeviceResource; 2] =
        [DeviceResource::Mmio(MMIO), DeviceResource::Irq(IRQ)];

    fn device(resources: &'static [DeviceResource]) -> PlatformDevice {
        PlatformDevice {
            id: DEVICE_ID,
            status: DeviceStatus::Enabled,
            matches: &MATCHES,
            resources,
            origin: ORIGIN,
        }
    }

    #[test]
    fn descriptor_matches_firmware_compatible_without_using_slot_address() {
        let first = device(&COMPLETE_RESOURCES);
        let mut relocated = first;
        relocated.id.local = DeviceLocalId::FirmwarePath("/soc/virtio@10008000");

        assert_eq!(
            match_device(&first),
            DriverMatch::Supported(MatchPriority(100))
        );
        assert_eq!(match_device(&relocated), match_device(&first));
    }

    #[test]
    fn decoder_keeps_mmio_and_irq_from_the_same_device_record() {
        let decoded = decode_resources(&device(&COMPLETE_RESOURCES)).unwrap();
        assert_eq!(decoded.region.phys, MMIO.phys);
        assert_eq!(decoded.region.virt, MMIO.virt);
        assert_eq!(decoded.irq, IRQ);
        assert_eq!(decoded.dma, DMA);
    }

    #[test]
    fn decoder_rejects_a_device_record_without_its_irq() {
        assert_eq!(
            decode_resources(&device(&MISSING_IRQ_RESOURCES)).unwrap_err(),
            DeviceBindError::MissingResource {
                device: DEVICE_ID,
                kind: ResourceKind::Irq,
                role: ResourceRole::Index(0),
            }
        );
    }

    #[test]
    fn decoder_rejects_a_device_record_without_its_dma_domain() {
        assert_eq!(
            decode_resources(&device(&MISSING_DMA_RESOURCES)).unwrap_err(),
            DeviceBindError::MissingResource {
                device: DEVICE_ID,
                kind: ResourceKind::DmaDomain,
                role: ResourceRole::Index(0),
            }
        );
    }
}
