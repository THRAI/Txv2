use tx_hal::*;

const PROVIDER: ResourceProviderId = ResourceProviderId("test-provider");
const ORIGIN: ResourceOrigin = ResourceOrigin {
    provider: PROVIDER,
    record: "fixture",
    kind: ResourceOriginKind::Firmware,
};
const MMIO_FLAGS: MmioFlags = MmioFlags::DEVICE_NGNRNE
    .union(MmioFlags::READ)
    .union(MmioFlags::WRITE);

fn device_id(path: &'static str) -> DeviceId {
    DeviceId {
        provider: PROVIDER,
        local: DeviceLocalId::FirmwarePath(path),
    }
}

fn domain_id(local: u32) -> DmaDomainId {
    DmaDomainId {
        provider: PROVIDER,
        local,
    }
}

fn constraints() -> DmaConstraints {
    DmaConstraints {
        dma_address_bits: 48,
        min_alignment: 64,
        segment_boundary: Some(64 * 1024),
        max_segment_len: 32 * 1024,
        max_segments: 32,
    }
}

fn domain(local: u32) -> DmaDomain {
    DmaDomain {
        id: domain_id(local),
        translation: DmaTranslation::Direct { offset: 0 },
        constraints: constraints(),
        coherency: DmaCoherency::Coherent,
        origin: ORIGIN,
    }
}

fn mmio(role: ResourceRole, base: usize) -> DeviceResource {
    DeviceResource::Mmio(MmioResource {
        role,
        phys: PhysRange {
            start: PhysAddr(base),
            size: 0x1000,
        },
        virt: VirtRange {
            start: VirtAddr(0xffff_8000_0000_0000usize + base),
            size: 0x1000,
        },
        flags: MMIO_FLAGS,
        origin: ORIGIN,
    })
}

fn irq(role: ResourceRole, line: u32) -> DeviceResource {
    DeviceResource::Irq(IrqResource {
        role,
        line,
        trigger: IrqTrigger::Level,
        polarity: IrqPolarity::High,
        sharing: IrqSharing::Exclusive,
        origin: ORIGIN,
    })
}

fn record(path: &'static str, base: usize, dma: Option<DmaDomainId>) -> DeviceRecordBuilder {
    let mut resources = vec![
        mmio(ResourceRole::Named("registers"), base),
        irq(ResourceRole::Named("rx-tx"), 9),
    ];
    if let Some(domain) = dma {
        resources.push(DeviceResource::DmaDomain(DmaDomainRef {
            role: ResourceRole::Named("data"),
            domain,
        }));
    }
    DeviceRecordBuilder {
        id: device_id(path),
        status: DeviceStatus::Enabled,
        matches: vec![DeviceMatchId::FirmwareCompatible("vendor,device")],
        resources,
        origin: ORIGIN,
    }
}

#[test]
fn empty_graph_freezes_without_synthetic_devices() {
    let graph = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH)
        .unwrap()
        .freeze()
        .unwrap();
    assert!(graph.platform_mmio.is_empty());
    assert!(graph.devices.is_empty());
    assert!(graph.dma_domains.is_empty());
}

#[test]
fn from_seed_validates_platform_mmio_before_copying() {
    static INVALID_PLATFORM_MMIO: [MmioResource; 1] = [MmioResource {
        role: ResourceRole::Named("platform-window"),
        phys: PhysRange {
            start: PhysAddr(usize::MAX - 0x7ff),
            size: 0x1000,
        },
        virt: VirtRange {
            start: VirtAddr(0xffff_8000_0000_0000),
            size: 0x1000,
        },
        flags: MMIO_FLAGS,
        origin: ORIGIN,
    }];
    static INVALID_SEED: DeviceResourceGraph = DeviceResourceGraph {
        platform_mmio: &INVALID_PLATFORM_MMIO,
        devices: &[],
        dma_domains: &[],
    };

    let err = match DeviceGraphBuilder::from_seed(&INVALID_SEED) {
        Ok(_) => panic!("overflowing platform MMIO seed must be rejected"),
        Err(err) => err,
    };
    assert_eq!(
        err,
        ResourceGraphError::InvalidMmioRange {
            device: None,
            role: ResourceRole::Named("platform-window"),
        }
    );
}

#[test]
fn multi_device_graph_keeps_stable_ids_when_input_is_reordered() {
    let mut forward = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    forward.push_dma_domain(domain(1)).unwrap();
    forward
        .push_device(record("/soc/net@1000", 0x1000, Some(domain_id(1))))
        .unwrap();
    forward
        .push_device(record("/soc/net@2000", 0x2000, Some(domain_id(1))))
        .unwrap();
    let forward = forward.freeze().unwrap();

    let mut reverse = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    reverse.push_dma_domain(domain(1)).unwrap();
    reverse
        .push_device(record("/soc/net@2000", 0x2000, Some(domain_id(1))))
        .unwrap();
    reverse
        .push_device(record("/soc/net@1000", 0x1000, Some(domain_id(1))))
        .unwrap();
    let reverse = reverse.freeze().unwrap();

    assert_eq!(forward.devices.len(), 2);
    assert_eq!(reverse.devices.len(), 2);
    for expected in [device_id("/soc/net@1000"), device_id("/soc/net@2000")] {
        assert!(forward.devices.iter().any(|device| device.id == expected));
        assert!(reverse.devices.iter().any(|device| device.id == expected));
    }
}

#[test]
fn duplicate_device_is_rejected() {
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    builder
        .push_device(record("/soc/net@1000", 0x1000, None))
        .unwrap();
    let err = builder
        .push_device(record("/soc/net@1000", 0x2000, None))
        .unwrap_err();
    assert_eq!(
        err,
        ResourceGraphError::DuplicateDeviceId(device_id("/soc/net@1000"))
    );
}

#[test]
fn duplicate_resource_role_is_rejected() {
    let device = device_id("/soc/net@1000");
    let mut duplicate = record("/soc/net@1000", 0x1000, None);
    duplicate
        .resources
        .push(mmio(ResourceRole::Named("registers"), 0x2000));
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    let err = builder.push_device(duplicate).unwrap_err();
    assert_eq!(
        err,
        ResourceGraphError::DuplicateResource {
            device,
            kind: ResourceKind::Mmio,
            role: ResourceRole::Named("registers"),
        }
    );
}

#[test]
fn duplicate_dma_domain_is_rejected() {
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    builder.push_dma_domain(domain(1)).unwrap();
    assert_eq!(
        builder.push_dma_domain(domain(1)).unwrap_err(),
        ResourceGraphError::DuplicateDmaDomain(domain_id(1))
    );
}

#[test]
fn dangling_dma_domain_is_rejected_at_freeze() {
    let device = device_id("/soc/net@1000");
    let missing = domain_id(7);
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    builder
        .push_device(record("/soc/net@1000", 0x1000, Some(missing)))
        .unwrap();
    assert_eq!(
        builder.freeze().unwrap_err(),
        ResourceGraphError::DanglingDmaDomain {
            device,
            domain: missing,
        }
    );
}

#[test]
fn dangling_dependency_is_rejected_at_freeze() {
    let consumer = device_id("/soc/net@1000");
    let missing = device_id("/soc/clock@9000");
    let mut device = record("/soc/net@1000", 0x1000, None);
    device.resources.push(DeviceResource::Clock(ClockRef {
        role: ResourceRole::Named("core"),
        provider: missing,
        spec: ProviderSpecifier { cells: &[1] },
    }));
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    builder.push_device(device).unwrap();
    assert_eq!(
        builder.freeze().unwrap_err(),
        ResourceGraphError::DanglingDependency {
            device: consumer,
            provider: missing,
        }
    );
}

#[test]
fn invalid_mmio_irq_and_dma_constraints_are_typed_errors() {
    let id = device_id("/soc/net@1000");

    let mut bad_mmio = record("/soc/net@1000", 0x1000, None);
    bad_mmio.resources[0] = DeviceResource::Mmio(MmioResource {
        role: ResourceRole::Named("registers"),
        phys: PhysRange {
            start: PhysAddr(0x1000),
            size: 0,
        },
        virt: VirtRange {
            start: VirtAddr(0xffff_8000_0000_1000),
            size: 0,
        },
        flags: MMIO_FLAGS,
        origin: ORIGIN,
    });
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    assert_eq!(
        builder.push_device(bad_mmio).unwrap_err(),
        ResourceGraphError::InvalidMmioRange {
            device: Some(id),
            role: ResourceRole::Named("registers"),
        }
    );

    let mut bad_irq = record("/soc/net@1000", 0x1000, None);
    bad_irq.resources[1] = irq(ResourceRole::Named("rx-tx"), 0);
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    assert_eq!(
        builder.push_device(bad_irq).unwrap_err(),
        ResourceGraphError::InvalidIrq {
            device: id,
            role: ResourceRole::Named("rx-tx"),
        }
    );

    let mut bad_domain = domain(4);
    bad_domain.constraints.min_alignment = 3;
    let mut builder = DeviceGraphBuilder::from_seed(&EMPTY_DEVICE_RESOURCE_GRAPH).unwrap();
    assert_eq!(
        builder.push_dma_domain(bad_domain).unwrap_err(),
        ResourceGraphError::InvalidDmaConstraints(domain_id(4))
    );
}

#[test]
fn strict_dma_intersection_uses_the_stricter_limit() {
    let left = DmaConstraints {
        dma_address_bits: 48,
        min_alignment: 64,
        segment_boundary: Some(64 * 1024),
        max_segment_len: 32 * 1024,
        max_segments: 32,
    };
    let right = DmaConstraints {
        dma_address_bits: 32,
        min_alignment: 4096,
        segment_boundary: Some(4096),
        max_segment_len: 8192,
        max_segments: 8,
    };
    assert_eq!(
        left.strict_intersection(right).unwrap(),
        DmaConstraints {
            dma_address_bits: 32,
            min_alignment: 4096,
            segment_boundary: Some(4096),
            max_segment_len: 8192,
            max_segments: 8,
        }
    );

    let no_boundary = DmaConstraints {
        segment_boundary: None,
        ..left
    };
    assert_eq!(
        no_boundary
            .strict_intersection(right)
            .unwrap()
            .segment_boundary,
        Some(4096)
    );
}

#[test]
fn strict_dma_intersection_rejects_invalid_window_sizes() {
    let invalid = DmaConstraints {
        segment_boundary: Some(6000),
        ..constraints()
    };
    assert_eq!(
        constraints().strict_intersection(invalid),
        Err(DmaConstraintsError::InvalidSegmentBoundary(6000))
    );
}

#[test]
fn platform_info_can_publish_the_empty_device_resource_seed() {
    let info = PlatformInfo {
        board: "test-platform",
        spi_sd: None,
        mmio_regions: &[],
        device_resources: &EMPTY_DEVICE_RESOURCE_GRAPH,
        timebase_frequency_hz: 1,
        possible_cpu_count: 1,
    };

    assert!(core::ptr::eq(
        info.device_resources,
        &EMPTY_DEVICE_RESOURCE_GRAPH
    ));
    assert!(info.device_resources.platform_mmio.is_empty());
    assert!(info.device_resources.devices.is_empty());
    assert!(info.device_resources.dma_domains.is_empty());
}
