//! Static provider/driver adapter for enumerated VirtIO PCI network functions.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::marker::PhantomData;

use tx_hal::{
    DeviceGraphBuilder, DeviceId, DeviceLocalId, DeviceMatchId, DeviceRecordBuilder,
    DeviceResource, DeviceResourceGraph, DeviceStatus, DmaDomain, DmaDomainRef, IrqIf, IrqResource,
    MmioRegion, MmioResource, PciFunctionId, PlatformDevice, PlatformInfoIf, ResourceCapacityKind,
    ResourceGraphError, ResourceKind, ResourceOrigin, ResourceOriginKind,
    ResourceProviderDescriptor, ResourceProviderId, ResourceRole, TxPlatform, VirtRange,
};
use tx_subsystems::device::DevT;
use tx_subsystems::device_binding::{BoundDevice, BoundDeviceRegistration, DriverId};
use tx_subsystems::net::NetDeviceRegistration;

use super::binder::{
    DeviceBindError, DeviceBindReservation, DriverMatch, MatchPriority, StaticDriverDescriptor,
};

const PCI_HOST_COMPATIBLE: &str = "pci-host-ecam-generic";
const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_TRANSITIONAL_NET_DEVICE_ID: u16 = 0x1000;
const VIRTIO_MODERN_NET_DEVICE_ID: u16 = 0x1041;
const VIRTIO_NET_MAJOR: u32 = 97;
const PROVIDER_ID: ResourceProviderId = ResourceProviderId("pci-function-enumerator");
const DRIVER_ID: DriverId = DriverId("virtio-pci-net");

#[derive(Clone, Copy)]
struct PciHostResources {
    ecam: MmioResource,
    mmio32: MmioResource,
    dma: DmaDomainRef,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtioPciNetResources {
    ecam: MmioRegion,
    function: PciFunctionId,
    irq: IrqResource,
    dma: DmaDomainRef,
    bar_count: usize,
}

pub const fn resource_provider_descriptor<P: TxPlatform>() -> ResourceProviderDescriptor<P> {
    ResourceProviderDescriptor {
        id: PROVIDER_ID,
        enumerate: enumerate::<P>,
        _platform: PhantomData,
    }
}

pub const fn driver_descriptor<P: TxPlatform>() -> StaticDriverDescriptor<P> {
    StaticDriverDescriptor {
        id: DRIVER_ID,
        match_device,
        prepare: prepare::<P>,
        _platform: PhantomData,
    }
}

fn enumerate<P: TxPlatform>(
    seed: &'static DeviceResourceGraph,
    out: &mut DeviceGraphBuilder,
) -> Result<(), ResourceGraphError> {
    let host = exact_pci_host(seed)?;
    let ecam = mmio_region("pci-ecam", host.ecam);
    let mmio32 = mmio_region("pci-mmio32", host.mmio32);
    let facts = tx_drivers::virtio::pci::enumerate_virtio_net_functions(ecam, mmio32)
        .map_err(|_| provider_error(10))?;

    for fact in facts {
        let irq = <P as IrqIf>::pci_intx_irq(fact.function, fact.interrupt_pin)
            .ok_or_else(|| provider_error(11))?;
        let origin = ResourceOrigin {
            provider: PROVIDER_ID,
            record: "pci-function",
            kind: ResourceOriginKind::BusEnumeration,
        };
        let mut matches = Vec::new();
        matches
            .try_reserve_exact(1)
            .map_err(|_| capacity(ResourceCapacityKind::Resource, 1))?;
        matches.push(DeviceMatchId::Pci(fact.device_match));

        let required = fact.bars.len().saturating_add(3);
        let mut resources = Vec::new();
        resources
            .try_reserve_exact(required)
            .map_err(|_| capacity(ResourceCapacityKind::Resource, required))?;
        let mut config = host.ecam;
        config.role = ResourceRole::Named("config");
        resources.push(DeviceResource::Mmio(config));
        for bar in fact.bars {
            let offset = bar
                .phys
                .start
                .0
                .checked_sub(host.mmio32.phys.start.0)
                .ok_or_else(|| provider_error(12))?;
            if offset
                .checked_add(bar.phys.size)
                .is_none_or(|end| end > host.mmio32.phys.size)
            {
                return Err(provider_error(13));
            }
            resources.push(DeviceResource::Mmio(MmioResource {
                role: ResourceRole::Index(u16::from(bar.index)),
                phys: bar.phys,
                virt: VirtRange {
                    start: tx_hal::VirtAddr(host.mmio32.virt.start.0 + offset),
                    size: bar.phys.size,
                },
                flags: host.mmio32.flags,
                origin,
            }));
        }
        resources.push(DeviceResource::Irq(irq));
        resources.push(DeviceResource::DmaDomain(host.dma));
        out.push_device(DeviceRecordBuilder {
            id: DeviceId {
                provider: PROVIDER_ID,
                local: DeviceLocalId::PciFunction(fact.function),
            },
            status: DeviceStatus::Enabled,
            matches,
            resources,
            origin,
        })?;
    }
    Ok(())
}

fn exact_pci_host(seed: &DeviceResourceGraph) -> Result<PciHostResources, ResourceGraphError> {
    let mut selected = None;
    for device in seed.devices {
        if !device.matches.iter().any(|candidate| {
            matches!(candidate, DeviceMatchId::FirmwareCompatible(value) if *value == PCI_HOST_COMPATIBLE)
        }) {
            continue;
        }
        if selected.replace(device).is_some() {
            return Err(provider_error(1));
        }
    }
    let device = selected.ok_or_else(|| provider_error(2))?;
    Ok(PciHostResources {
        ecam: exact_mmio(device, ResourceRole::Named("ecam")).map_err(|_| provider_error(3))?,
        mmio32: exact_mmio(device, ResourceRole::Named("mmio32")).map_err(|_| provider_error(4))?,
        dma: exact_dma(device).map_err(|_| provider_error(5))?,
    })
}

fn match_device(device: &PlatformDevice) -> DriverMatch {
    if device.matches.iter().any(|candidate| {
        matches!(
            candidate,
            DeviceMatchId::Pci(pci)
                if pci.vendor == VIRTIO_VENDOR_ID
                    && matches!(
                        pci.device,
                        VIRTIO_TRANSITIONAL_NET_DEVICE_ID | VIRTIO_MODERN_NET_DEVICE_ID
                    )
        )
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
        tx_drivers::virtio::VirtioPciNet::<P, 256>::from_function(
            resources.ecam,
            resources.function,
        ),
    ));
    net.init()
        .map_err(|_| DeviceBindError::DriverProbe { code: 2 })?;

    let ordinal = reservation.bound_key().0;
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

fn decode_resources(device: &PlatformDevice) -> Result<VirtioPciNetResources, DeviceBindError> {
    let function = match device.id.local {
        DeviceLocalId::PciFunction(function) => function,
        _ => return Err(DeviceBindError::DriverProbe { code: 1 }),
    };
    let config = exact_mmio(device, ResourceRole::Named("config"))?;
    let irq = exact_irq(device)?;
    let dma = exact_dma(device)?;
    let bar_count = device
        .resources
        .iter()
        .filter(|resource| {
            matches!(resource, DeviceResource::Mmio(mmio) if matches!(mmio.role, ResourceRole::Index(_)))
        })
        .count();
    if bar_count == 0 {
        return Err(DeviceBindError::MissingResource {
            device: device.id,
            kind: ResourceKind::Mmio,
            role: ResourceRole::Index(0),
        });
    }
    Ok(VirtioPciNetResources {
        ecam: mmio_region("pci-config", config),
        function,
        irq,
        dma,
        bar_count,
    })
}

fn exact_mmio(
    device: &PlatformDevice,
    role: ResourceRole,
) -> Result<MmioResource, DeviceBindError> {
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

fn mmio_region(name: &'static str, resource: MmioResource) -> MmioRegion {
    MmioRegion {
        name,
        phys: resource.phys,
        virt: resource.virt,
        flags: resource.flags,
    }
}

fn provider_error(code: u32) -> ResourceGraphError {
    ResourceGraphError::ProviderFailed {
        provider: PROVIDER_ID,
        code,
    }
}

fn capacity(kind: ResourceCapacityKind, required: usize) -> ResourceGraphError {
    ResourceGraphError::CapacityExceeded { kind, required }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_hal::{
        DmaDomainId, IrqPolarity, IrqSharing, IrqTrigger, MmioFlags, PciDeviceMatch, PhysAddr,
        PhysRange, ResourceOrigin, ResourceOriginKind, VirtAddr,
    };

    const PROVIDER: ResourceProviderId = ResourceProviderId("virtio-pci-test");
    const ORIGIN: ResourceOrigin = ResourceOrigin {
        provider: PROVIDER,
        record: "fixture",
        kind: ResourceOriginKind::BusEnumeration,
    };
    const FUNCTION: PciFunctionId = PciFunctionId {
        segment: 0,
        bus: 0,
        device: 5,
        function: 0,
    };
    const DEVICE_ID: DeviceId = DeviceId {
        provider: PROVIDER,
        local: DeviceLocalId::PciFunction(FUNCTION),
    };
    static MATCHES: [DeviceMatchId; 1] = [DeviceMatchId::Pci(PciDeviceMatch {
        vendor: VIRTIO_VENDOR_ID,
        device: VIRTIO_MODERN_NET_DEVICE_ID,
        subsystem_vendor: None,
        subsystem_device: None,
        class: 0x02_00_00,
    })];
    const CONFIG: MmioResource = MmioResource {
        role: ResourceRole::Named("config"),
        phys: PhysRange {
            start: PhysAddr(0x2000_0000),
            size: 0x0800_0000,
        },
        virt: VirtRange {
            start: VirtAddr(0x8000_0000_2000_0000),
            size: 0x0800_0000,
        },
        flags: MmioFlags::READ.union(MmioFlags::WRITE),
        origin: ORIGIN,
    };
    const BAR0: MmioResource = MmioResource {
        role: ResourceRole::Index(0),
        phys: PhysRange {
            start: PhysAddr(0x4000_4000),
            size: 0x4000,
        },
        virt: VirtRange {
            start: VirtAddr(0x8000_0000_4000_4000),
            size: 0x4000,
        },
        flags: MmioFlags::READ.union(MmioFlags::WRITE),
        origin: ORIGIN,
    };
    const IRQ: IrqResource = IrqResource {
        role: ResourceRole::Index(0),
        line: 81,
        trigger: IrqTrigger::Level,
        polarity: IrqPolarity::Low,
        sharing: IrqSharing::Shared,
        origin: ORIGIN,
    };
    const DMA: DmaDomainRef = DmaDomainRef {
        role: ResourceRole::Index(0),
        domain: DmaDomainId {
            provider: PROVIDER,
            local: 0,
        },
    };
    static COMPLETE_RESOURCES: [DeviceResource; 4] = [
        DeviceResource::Mmio(CONFIG),
        DeviceResource::Mmio(BAR0),
        DeviceResource::Irq(IRQ),
        DeviceResource::DmaDomain(DMA),
    ];
    static MISSING_IRQ_RESOURCES: [DeviceResource; 3] = [
        DeviceResource::Mmio(CONFIG),
        DeviceResource::Mmio(BAR0),
        DeviceResource::DmaDomain(DMA),
    ];
    static MISSING_DMA_RESOURCES: [DeviceResource; 3] = [
        DeviceResource::Mmio(CONFIG),
        DeviceResource::Mmio(BAR0),
        DeviceResource::Irq(IRQ),
    ];

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
    fn descriptor_matches_pci_identity_without_using_the_function_address() {
        let first = device(&COMPLETE_RESOURCES);
        let mut relocated = first;
        relocated.id.local = DeviceLocalId::PciFunction(PciFunctionId {
            device: 9,
            ..FUNCTION
        });

        assert_eq!(
            match_device(&first),
            DriverMatch::Supported(MatchPriority(100))
        );
        assert_eq!(match_device(&relocated), match_device(&first));
    }

    #[test]
    fn decoder_keeps_config_bars_irq_and_dma_from_the_same_function_record() {
        let decoded = decode_resources(&device(&COMPLETE_RESOURCES)).unwrap();
        assert_eq!(decoded.ecam.phys, CONFIG.phys);
        assert_eq!(decoded.ecam.virt, CONFIG.virt);
        assert_eq!(decoded.function, FUNCTION);
        assert_eq!(decoded.irq, IRQ);
        assert_eq!(decoded.dma, DMA);
        assert_eq!(decoded.bar_count, 1);
    }

    #[test]
    fn decoder_rejects_a_function_record_without_its_irq() {
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
    fn decoder_rejects_a_function_record_without_its_dma_domain() {
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
