//! Static binder adapter for firmware-described Synopsys DWMAC network devices.

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
    DeviceBindError, DeviceBindReservation, DriverMatch, MatchPriority, StaticDriverDescriptor,
};

const STARFIVE_DWMAC_COMPATIBLES: [&str; 3] = [
    "starfive,dwmac",
    "starfive,jh7110-dwmac",
    "starfive,jh7110-eqos-5.20",
];
const DWMAC_NET_MAJOR: u32 = 98;
const DRIVER_ID: DriverId = DriverId("starfive-dwmac-net");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DwmacNetResources {
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
        matches!(candidate, DeviceMatchId::FirmwareCompatible(value)
            if STARFIVE_DWMAC_COMPATIBLES.contains(value))
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
    tx_hal::console_write_str::<P>("txkernel:dwmac:candidate\n");
    let resources = decode_resources(device).map_err(|error| {
        tx_hal::console_write_str::<P>("txkernel:dwmac:resources:");
        tx_hal::console_write_str::<P>(resource_error_label(error));
        tx_hal::console_write_str::<P>("\n");
        error
    })?;
    if let Err(error) = <P as PlatformInfoIf>::prepare_platform_device(device) {
        tx_hal::console_write_str::<P>("txkernel:dwmac:platform-prepare:");
        tx_hal::console_write_str::<P>(error.label());
        tx_hal::console_write_str::<P>("\n");
        return Err(DeviceBindError::DriverProbe { code: 3 });
    }
    let dma_domain = resolve_dma_domain::<P>(resources.dma)?;
    let net = Box::leak(Box::new(tx_drivers::dwmac::DwmacNet::<P>::new(
        resources.region,
        dma_domain,
    )));
    if let Err(error) = net.init() {
        tx_hal::console_write_str::<P>("txkernel:dwmac:probe:");
        tx_hal::console_write_str::<P>(error.label());
        tx_hal::console_write_str::<P>("\n");
        return Err(DeviceBindError::DriverProbe { code: 1 });
    }
    tx_hal::console_write_str::<P>("txkernel:dwmac:probe:ok\n");

    let ordinal = reservation.bound_key().0;
    let name = Box::leak(format!("eth{ordinal}").into_boxed_str());
    let registration = Box::leak(Box::new(NetDeviceRegistration {
        devt: DevT::new(DWMAC_NET_MAJOR, u32::from(ordinal)),
        name,
        ops: net,
    }));

    reservation.propose_registration(BoundDeviceRegistration::Net(registration))?;
    reservation.propose_irq(resources.irq, crate::irq::typed_net_rx_irq_handler::<P>)?;
    reservation.propose_activation(activate)
}

fn resource_error_label(error: DeviceBindError) -> &'static str {
    match error {
        DeviceBindError::MissingResource {
            kind: ResourceKind::Mmio,
            ..
        } => "missing-mmio",
        DeviceBindError::MissingResource {
            kind: ResourceKind::Irq,
            ..
        } => "missing-irq",
        DeviceBindError::MissingResource {
            kind: ResourceKind::DmaDomain,
            ..
        } => "missing-dma",
        DeviceBindError::DuplicateResource {
            kind: ResourceKind::Mmio,
            ..
        } => "duplicate-mmio",
        DeviceBindError::DuplicateResource {
            kind: ResourceKind::Irq,
            ..
        } => "duplicate-irq",
        DeviceBindError::DuplicateResource {
            kind: ResourceKind::DmaDomain,
            ..
        } => "duplicate-dma",
        _ => "invalid-resource-set",
    }
}

fn activate(device: &'static BoundDevice) {
    if let BoundDeviceRegistration::Net(registration) = device.registration {
        registration.ops.enable_interrupts();
    }
}

fn decode_resources(device: &PlatformDevice) -> Result<DwmacNetResources, DeviceBindError> {
    let mmio = exact_mmio(device)?;
    let irq = exact_irq(device)?;
    let dma = exact_dma(device)?;
    let name = match device.id.local {
        tx_hal::DeviceLocalId::FirmwarePath(path) | tx_hal::DeviceLocalId::PlatformKey(path) => {
            path
        }
        tx_hal::DeviceLocalId::PciFunction(_) => DRIVER_ID.0,
    };
    Ok(DwmacNetResources {
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

    const PROVIDER: ResourceProviderId = ResourceProviderId("dwmac-test");
    const ORIGIN: ResourceOrigin = ResourceOrigin {
        provider: PROVIDER,
        record: "fixture",
        kind: ResourceOriginKind::Firmware,
    };
    const DEVICE_ID: DeviceId = DeviceId {
        provider: PROVIDER,
        local: DeviceLocalId::FirmwarePath("/soc/ethernet@16030000"),
    };
    static MATCHES: [DeviceMatchId; 2] = [
        DeviceMatchId::FirmwareCompatible("starfive,dwmac"),
        DeviceMatchId::FirmwareCompatible("snps,dwmac-5.10a"),
    ];
    static VENDOR_MATCHES: [DeviceMatchId; 1] = [DeviceMatchId::FirmwareCompatible(
        "starfive,jh7110-eqos-5.20",
    )];
    const MMIO: tx_hal::MmioResource = MmioResource {
        role: ResourceRole::Index(0),
        phys: PhysRange {
            start: PhysAddr(0x1603_0000),
            size: 0x1_0000,
        },
        virt: VirtRange {
            start: VirtAddr(0xffff_ffc0_1603_0000),
            size: 0x1_0000,
        },
        flags: MmioFlags::READ.union(MmioFlags::WRITE),
        origin: ORIGIN,
    };
    const IRQ: IrqResource = IrqResource {
        role: ResourceRole::Index(0),
        line: 7,
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
    static RESOURCES: [DeviceResource; 3] = [
        DeviceResource::Mmio(MMIO),
        DeviceResource::Irq(IRQ),
        DeviceResource::DmaDomain(DMA),
    ];

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
    fn matches_starfive_compatible_without_using_mmio_address() {
        let first = device();
        let mut relocated = first;
        relocated.id.local = DeviceLocalId::FirmwarePath("/soc/ethernet@26030000");
        assert_eq!(
            match_device(&first),
            DriverMatch::Supported(MatchPriority(100))
        );
        assert_eq!(match_device(&relocated), match_device(&first));
    }

    #[test]
    fn matches_visionfive2_vendor_eqos_compatible() {
        let mut vendor = device();
        vendor.matches = &VENDOR_MATCHES;

        assert_eq!(
            match_device(&vendor),
            DriverMatch::Supported(MatchPriority(100))
        );
    }

    #[test]
    fn decoder_keeps_all_resources_from_one_device_record() {
        let decoded = decode_resources(&device()).unwrap();
        assert_eq!(decoded.region.phys, MMIO.phys);
        assert_eq!(decoded.irq, IRQ);
        assert_eq!(decoded.dma, DMA);
    }
}
