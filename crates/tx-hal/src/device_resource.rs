//! Immutable boot-time hardware resource facts.
//!
//! Platforms publish [`DeviceResourceGraph`] seeds from boot-owned storage.
//! One-shot resource providers may copy a seed into [`DeviceGraphBuilder`],
//! append records, and call [`DeviceGraphBuilder::freeze`].  No reference is
//! leaked until the complete graph has passed validation.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::{MmioFlags, PhysRange, TxPlatform, VirtRange};
use core::marker::PhantomData;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResourceProviderId(pub &'static str);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PciFunctionId {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeviceLocalId {
    FirmwarePath(&'static str),
    PciFunction(PciFunctionId),
    PlatformKey(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeviceId {
    pub provider: ResourceProviderId,
    pub local: DeviceLocalId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceMatchId {
    FirmwareCompatible(&'static str),
    Pci(PciDeviceMatch),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDeviceMatch {
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: Option<u16>,
    pub subsystem_device: Option<u16>,
    pub class: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceStatus {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResourceRole {
    /// A specification-defined `*-names` entry.
    Named(&'static str),
    /// A specification-defined ordinal for a binding without names.
    Index(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceOriginKind {
    Firmware,
    BusEnumeration,
    PlatformStatic,
    CapabilityRefinement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceOrigin {
    pub provider: ResourceProviderId,
    pub record: &'static str,
    pub kind: ResourceOriginKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioResource {
    pub role: ResourceRole,
    pub phys: PhysRange,
    pub virt: VirtRange,
    pub flags: MmioFlags,
    pub origin: ResourceOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqTrigger {
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqPolarity {
    High,
    Low,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqSharing {
    Exclusive,
    Shared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqResource {
    pub role: ResourceRole,
    pub line: u32,
    pub trigger: IrqTrigger,
    pub polarity: IrqPolarity,
    pub sharing: IrqSharing,
    pub origin: ResourceOrigin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DmaDomainId {
    pub provider: ResourceProviderId,
    pub local: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderSpecifier {
    pub cells: &'static [u32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockRef {
    pub role: ResourceRole,
    pub provider: DeviceId,
    pub spec: ProviderSpecifier,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResetRef {
    pub role: ResourceRole,
    pub provider: DeviceId,
    pub spec: ProviderSpecifier,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SysconRef {
    pub role: ResourceRole,
    pub provider: DeviceId,
    pub spec: ProviderSpecifier,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MdioBusRef {
    pub role: ResourceRole,
    pub provider: DeviceId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyRef {
    pub role: ResourceRole,
    pub provider: DeviceId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmemCellRef {
    pub provider: DeviceId,
    pub offset: u32,
    pub length: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacAddressSource {
    Firmware([u8; 6]),
    Nvmem(NvmemCellRef),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaDomainRef {
    pub role: ResourceRole,
    pub domain: DmaDomainId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceResource {
    Mmio(MmioResource),
    Irq(IrqResource),
    DmaDomain(DmaDomainRef),
    Clock(ClockRef),
    Reset(ResetRef),
    Syscon(SysconRef),
    MdioBus(MdioBusRef),
    Phy(PhyRef),
    MacAddress {
        role: ResourceRole,
        source: MacAddressSource,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Mmio,
    Irq,
    DmaDomain,
    Clock,
    Reset,
    Syscon,
    MdioBus,
    Phy,
    MacAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceCapacityKind {
    Device,
    Resource,
    DmaDomain,
    IrqRoute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaCoherency {
    Coherent,
    NonCoherent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaTranslation {
    Direct { offset: i64 },
    Managed { address_space: u64 },
}

/// DMA allocation limits for one device-visible address domain.
///
/// `segment_boundary` is a power-of-two window size. A segment may not cross
/// a window of that size. `None` means that the domain imposes no additional
/// window boundary. When constraints are intersected, the smaller window is
/// stricter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaConstraints {
    pub dma_address_bits: u8,
    pub min_alignment: usize,
    pub segment_boundary: Option<u64>,
    pub max_segment_len: usize,
    pub max_segments: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaConstraintsError {
    InvalidAddressBits(u8),
    InvalidAlignment(usize),
    InvalidSegmentBoundary(u64),
    ZeroMaxSegmentLen,
    ZeroMaxSegments,
}

impl DmaConstraints {
    pub fn validate(self) -> Result<(), DmaConstraintsError> {
        if self.dma_address_bits == 0 || self.dma_address_bits > u64::BITS as u8 {
            return Err(DmaConstraintsError::InvalidAddressBits(
                self.dma_address_bits,
            ));
        }
        if self.min_alignment == 0 || !self.min_alignment.is_power_of_two() {
            return Err(DmaConstraintsError::InvalidAlignment(self.min_alignment));
        }
        if let Some(boundary) = self.segment_boundary {
            if boundary == 0 || !boundary.is_power_of_two() {
                return Err(DmaConstraintsError::InvalidSegmentBoundary(boundary));
            }
        }
        if self.max_segment_len == 0 {
            return Err(DmaConstraintsError::ZeroMaxSegmentLen);
        }
        if self.max_segments == 0 {
            return Err(DmaConstraintsError::ZeroMaxSegments);
        }
        Ok(())
    }

    /// Compute the strict conjunction of two independently sourced limits.
    pub fn strict_intersection(self, other: Self) -> Result<Self, DmaConstraintsError> {
        self.validate()?;
        other.validate()?;

        let segment_boundary = match (self.segment_boundary, other.segment_boundary) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
            (None, None) => None,
        };
        let intersection = Self {
            dma_address_bits: self.dma_address_bits.min(other.dma_address_bits),
            min_alignment: self.min_alignment.max(other.min_alignment),
            segment_boundary,
            max_segment_len: self.max_segment_len.min(other.max_segment_len),
            max_segments: self.max_segments.min(other.max_segments),
        };
        intersection.validate()?;
        Ok(intersection)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaDomain {
    pub id: DmaDomainId,
    pub translation: DmaTranslation,
    pub constraints: DmaConstraints,
    pub coherency: DmaCoherency,
    pub origin: ResourceOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformDevice {
    pub id: DeviceId,
    pub status: DeviceStatus,
    pub matches: &'static [DeviceMatchId],
    pub resources: &'static [DeviceResource],
    pub origin: ResourceOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceResourceGraph {
    pub platform_mmio: &'static [MmioResource],
    pub devices: &'static [PlatformDevice],
    pub dma_domains: &'static [DmaDomain],
}

pub static EMPTY_DEVICE_RESOURCE_GRAPH: DeviceResourceGraph = DeviceResourceGraph {
    platform_mmio: &[],
    devices: &[],
    dma_domains: &[],
};

pub struct ResourceProviderDescriptor<P: TxPlatform> {
    pub id: ResourceProviderId,
    pub enumerate: fn(
        seed: &'static DeviceResourceGraph,
        out: &mut DeviceGraphBuilder,
    ) -> Result<(), ResourceGraphError>,
    pub _platform: PhantomData<fn() -> P>,
}

#[derive(Debug)]
pub struct DeviceRecordBuilder {
    pub id: DeviceId,
    pub status: DeviceStatus,
    pub matches: Vec<DeviceMatchId>,
    pub resources: Vec<DeviceResource>,
    pub origin: ResourceOrigin,
}

pub struct DeviceGraphBuilder {
    platform_mmio: Vec<MmioResource>,
    devices: Vec<DeviceRecordBuilder>,
    dma_domains: Vec<DmaDomain>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceGraphError {
    DuplicateDeviceId(DeviceId),
    DuplicateResource {
        device: DeviceId,
        kind: ResourceKind,
        role: ResourceRole,
    },
    DuplicateDmaDomain(DmaDomainId),
    DanglingDmaDomain {
        device: DeviceId,
        domain: DmaDomainId,
    },
    DanglingDependency {
        device: DeviceId,
        provider: DeviceId,
    },
    InvalidMmioRange {
        device: Option<DeviceId>,
        role: ResourceRole,
    },
    InvalidIrq {
        device: DeviceId,
        role: ResourceRole,
    },
    InvalidDmaConstraints(DmaDomainId),
    ProviderFailed {
        provider: ResourceProviderId,
        code: u32,
    },
    CapacityExceeded {
        kind: ResourceCapacityKind,
        required: usize,
    },
}

impl DeviceGraphBuilder {
    pub fn from_seed(seed: &'static DeviceResourceGraph) -> Result<Self, ResourceGraphError> {
        validate_graph(seed)?;

        let mut platform_mmio = Vec::new();
        reserve(
            &mut platform_mmio,
            seed.platform_mmio.len(),
            ResourceCapacityKind::Resource,
        )?;
        platform_mmio.extend_from_slice(seed.platform_mmio);

        let mut devices = Vec::new();
        reserve(
            &mut devices,
            seed.devices.len(),
            ResourceCapacityKind::Device,
        )?;
        for device in seed.devices {
            let mut matches = Vec::new();
            reserve(
                &mut matches,
                device.matches.len(),
                ResourceCapacityKind::Resource,
            )?;
            matches.extend_from_slice(device.matches);

            let mut resources = Vec::new();
            reserve(
                &mut resources,
                device.resources.len(),
                ResourceCapacityKind::Resource,
            )?;
            resources.extend_from_slice(device.resources);
            devices.push(DeviceRecordBuilder {
                id: device.id,
                status: device.status,
                matches,
                resources,
                origin: device.origin,
            });
        }

        let mut dma_domains = Vec::new();
        reserve(
            &mut dma_domains,
            seed.dma_domains.len(),
            ResourceCapacityKind::DmaDomain,
        )?;
        dma_domains.extend_from_slice(seed.dma_domains);

        Ok(Self {
            platform_mmio,
            devices,
            dma_domains,
        })
    }

    pub fn push_device(&mut self, device: DeviceRecordBuilder) -> Result<(), ResourceGraphError> {
        if self.devices.iter().any(|current| current.id == device.id) {
            return Err(ResourceGraphError::DuplicateDeviceId(device.id));
        }
        validate_device_record(&device)?;
        reserve(&mut self.devices, 1, ResourceCapacityKind::Device)?;
        self.devices.push(device);
        Ok(())
    }

    pub fn push_dma_domain(&mut self, domain: DmaDomain) -> Result<(), ResourceGraphError> {
        if self
            .dma_domains
            .iter()
            .any(|current| current.id == domain.id)
        {
            return Err(ResourceGraphError::DuplicateDmaDomain(domain.id));
        }
        if domain.constraints.validate().is_err() {
            return Err(ResourceGraphError::InvalidDmaConstraints(domain.id));
        }
        reserve(&mut self.dma_domains, 1, ResourceCapacityKind::DmaDomain)?;
        self.dma_domains.push(domain);
        Ok(())
    }

    pub fn freeze(self) -> Result<&'static DeviceResourceGraph, ResourceGraphError> {
        // Validation must remain before the first Box::leak below. A failed
        // graph owns all of its allocations and drops without publication.
        validate_builder(&self)?;

        let mut devices = Vec::new();
        reserve(
            &mut devices,
            self.devices.len(),
            ResourceCapacityKind::Device,
        )?;

        // Every typed failure is now behind us. Only freeze-owned conversion
        // and publication remains after the first leak.
        let platform_mmio = Box::leak(self.platform_mmio.into_boxed_slice());
        let dma_domains = Box::leak(self.dma_domains.into_boxed_slice());
        for device in self.devices {
            let matches = Box::leak(device.matches.into_boxed_slice());
            let resources = Box::leak(device.resources.into_boxed_slice());
            devices.push(PlatformDevice {
                id: device.id,
                status: device.status,
                matches,
                resources,
                origin: device.origin,
            });
        }
        let devices = Box::leak(devices.into_boxed_slice());

        Ok(Box::leak(Box::new(DeviceResourceGraph {
            platform_mmio,
            devices,
            dma_domains,
        })))
    }
}

fn reserve<T>(
    target: &mut Vec<T>,
    additional: usize,
    kind: ResourceCapacityKind,
) -> Result<(), ResourceGraphError> {
    target
        .try_reserve_exact(additional)
        .map_err(|_| ResourceGraphError::CapacityExceeded {
            kind,
            required: target.len().saturating_add(additional),
        })
}

fn validate_graph(graph: &DeviceResourceGraph) -> Result<(), ResourceGraphError> {
    for mmio in graph.platform_mmio {
        validate_mmio(None, mmio)?;
    }

    for (index, device) in graph.devices.iter().enumerate() {
        if graph.devices[..index]
            .iter()
            .any(|current| current.id == device.id)
        {
            return Err(ResourceGraphError::DuplicateDeviceId(device.id));
        }
        validate_device_resources(device.id, device.resources)?;
    }
    validate_domains(graph.dma_domains)?;
    validate_references(
        graph
            .devices
            .iter()
            .map(|device| (device.id, device.resources)),
        graph.devices.iter().map(|device| device.id),
        graph.dma_domains.iter().map(|domain| domain.id),
    )
}

fn validate_builder(builder: &DeviceGraphBuilder) -> Result<(), ResourceGraphError> {
    for mmio in &builder.platform_mmio {
        validate_mmio(None, mmio)?;
    }

    for (index, device) in builder.devices.iter().enumerate() {
        if builder.devices[..index]
            .iter()
            .any(|current| current.id == device.id)
        {
            return Err(ResourceGraphError::DuplicateDeviceId(device.id));
        }
        validate_device_record(device)?;
    }
    validate_domains(&builder.dma_domains)?;
    validate_references(
        builder
            .devices
            .iter()
            .map(|device| (device.id, device.resources.as_slice())),
        builder.devices.iter().map(|device| device.id),
        builder.dma_domains.iter().map(|domain| domain.id),
    )
}

fn validate_device_record(device: &DeviceRecordBuilder) -> Result<(), ResourceGraphError> {
    validate_device_resources(device.id, &device.resources)
}

fn validate_device_resources(
    device: DeviceId,
    resources: &[DeviceResource],
) -> Result<(), ResourceGraphError> {
    for (index, resource) in resources.iter().enumerate() {
        let (kind, role) = resource_key(resource);
        if resources[..index]
            .iter()
            .any(|current| resource_key(current) == (kind, role))
        {
            return Err(ResourceGraphError::DuplicateResource { device, kind, role });
        }
        match resource {
            DeviceResource::Mmio(mmio) => validate_mmio(Some(device), mmio)?,
            DeviceResource::Irq(irq) if irq.line == 0 => {
                return Err(ResourceGraphError::InvalidIrq {
                    device,
                    role: irq.role,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_mmio(device: Option<DeviceId>, mmio: &MmioResource) -> Result<(), ResourceGraphError> {
    let valid = mmio.phys.size != 0
        && mmio.phys.size == mmio.virt.size
        && mmio.phys.start.0.checked_add(mmio.phys.size).is_some()
        && mmio.virt.start.0.checked_add(mmio.virt.size).is_some();
    if valid {
        Ok(())
    } else {
        Err(ResourceGraphError::InvalidMmioRange {
            device,
            role: mmio.role,
        })
    }
}

fn validate_domains(domains: &[DmaDomain]) -> Result<(), ResourceGraphError> {
    for (index, domain) in domains.iter().enumerate() {
        if domains[..index]
            .iter()
            .any(|current| current.id == domain.id)
        {
            return Err(ResourceGraphError::DuplicateDmaDomain(domain.id));
        }
        if domain.constraints.validate().is_err() {
            return Err(ResourceGraphError::InvalidDmaConstraints(domain.id));
        }
    }
    Ok(())
}

fn validate_references<'a>(
    devices: impl Iterator<Item = (DeviceId, &'a [DeviceResource])> + Clone,
    device_ids: impl Iterator<Item = DeviceId> + Clone,
    domain_ids: impl Iterator<Item = DmaDomainId> + Clone,
) -> Result<(), ResourceGraphError> {
    for (device, resources) in devices {
        for resource in resources {
            if let DeviceResource::DmaDomain(reference) = resource {
                if !domain_ids.clone().any(|domain| domain == reference.domain) {
                    return Err(ResourceGraphError::DanglingDmaDomain {
                        device,
                        domain: reference.domain,
                    });
                }
            }
            if let Some(provider) = dependency_provider(resource) {
                if !device_ids.clone().any(|candidate| candidate == provider) {
                    return Err(ResourceGraphError::DanglingDependency { device, provider });
                }
            }
        }
    }
    Ok(())
}

fn dependency_provider(resource: &DeviceResource) -> Option<DeviceId> {
    match resource {
        DeviceResource::Clock(reference) => Some(reference.provider),
        DeviceResource::Reset(reference) => Some(reference.provider),
        DeviceResource::Syscon(reference) => Some(reference.provider),
        DeviceResource::MdioBus(reference) => Some(reference.provider),
        DeviceResource::Phy(reference) => Some(reference.provider),
        DeviceResource::MacAddress {
            source: MacAddressSource::Nvmem(reference),
            ..
        } => Some(reference.provider),
        _ => None,
    }
}

fn resource_key(resource: &DeviceResource) -> (ResourceKind, ResourceRole) {
    match resource {
        DeviceResource::Mmio(resource) => (ResourceKind::Mmio, resource.role),
        DeviceResource::Irq(resource) => (ResourceKind::Irq, resource.role),
        DeviceResource::DmaDomain(resource) => (ResourceKind::DmaDomain, resource.role),
        DeviceResource::Clock(resource) => (ResourceKind::Clock, resource.role),
        DeviceResource::Reset(resource) => (ResourceKind::Reset, resource.role),
        DeviceResource::Syscon(resource) => (ResourceKind::Syscon, resource.role),
        DeviceResource::MdioBus(resource) => (ResourceKind::MdioBus, resource.role),
        DeviceResource::Phy(resource) => (ResourceKind::Phy, resource.role),
        DeviceResource::MacAddress { role, .. } => (ResourceKind::MacAddress, *role),
    }
}
