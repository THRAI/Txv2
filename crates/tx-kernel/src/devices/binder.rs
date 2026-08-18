//! Static resource-provider and driver binding transaction.
//!
//! This is the typed device execution seam. It builds the final resource graph,
//! prepares a private set of prospective bindings, validates the complete set,
//! and only then freezes a report. The production entry point retains
//! rollback-safe registry reservations until the complete runtime is ready,
//! publishes once, and only then arms devices and unmasks their typed interrupt
//! routes. Legacy discovery remains only for transports not migrated here yet.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::marker::PhantomData;

use tx_hal::{
    DeviceGraphBuilder, DeviceId, DeviceLocalId, DeviceResource, DeviceResourceGraph, DeviceStatus,
    IrqResource, IrqSharing, PlatformDevice, ResourceGraphError, ResourceKind,
    ResourceProviderDescriptor, ResourceRole, TxPlatform,
};
use tx_substrate::step::Errno;
use tx_subsystems::device::{prepare_block_devices, DevT};
use tx_subsystems::device_binding::{
    BoundDevice, BoundDeviceIndex, BoundDeviceIndexBuilder, BoundDeviceIndexError, BoundDeviceKey,
    BoundDeviceRegistration, DeviceIrqContext, DriverId, IrqRoute,
};
use tx_subsystems::net::device::prepare_net_devices;

use crate::irq::device::{
    DeviceIrqHandler, IrqDispatchTable, IrqRouteError, KernelIrqTableBuilder,
};

use super::runtime::{prepare_device_runtime, DeviceRuntimePublishError, PreparedDeviceRuntime};

/// Driver-match precedence; larger values win.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MatchPriority(pub u16);

/// Result of matching one descriptor against one immutable device record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverMatch {
    Unsupported,
    Supported(MatchPriority),
}

/// One statically linked driver descriptor specialized for `P`.
pub struct StaticDriverDescriptor<P: TxPlatform> {
    pub id: DriverId,
    pub match_device: fn(&PlatformDevice) -> DriverMatch,
    pub prepare: fn(
        device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<P>,
    ) -> Result<(), DeviceBindError>,
    pub _platform: PhantomData<fn() -> P>,
}

/// Compile-time board composition seam for providers and drivers.
pub trait StaticDeviceBundle<P: TxPlatform>: 'static {
    fn resource_providers() -> &'static [ResourceProviderDescriptor<P>];
    fn drivers() -> &'static [StaticDriverDescriptor<P>];
}

/// Compatibility composition used by existing single-parameter `CoreInit<P>`
/// references and host tests. Board binaries name their own local bundle type.
pub struct EmptyDeviceBundle;

impl<P: TxPlatform> StaticDeviceBundle<P> for EmptyDeviceBundle {
    fn resource_providers() -> &'static [ResourceProviderDescriptor<P>] {
        &[]
    }

    fn drivers() -> &'static [StaticDriverDescriptor<P>] {
        &[]
    }
}

/// Device registry class used by capacity diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceClass {
    Controller,
    Char,
    Block,
    Net,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DeviceClassOrdinals {
    controller: u16,
    char_device: u16,
    block: u16,
    net: u16,
}

impl DeviceClassOrdinals {
    const fn get(self, class: DeviceClass) -> u16 {
        match class {
            DeviceClass::Controller => self.controller,
            DeviceClass::Char => self.char_device,
            DeviceClass::Block => self.block,
            DeviceClass::Net => self.net,
        }
    }

    fn advance(&mut self, registration: BoundDeviceRegistration) -> Result<(), DeviceBindError> {
        let class = match registration {
            BoundDeviceRegistration::Controller => DeviceClass::Controller,
            BoundDeviceRegistration::Char(_) => DeviceClass::Char,
            BoundDeviceRegistration::Block(_) => DeviceClass::Block,
            BoundDeviceRegistration::Net(_) => DeviceClass::Net,
        };
        let slot = match class {
            DeviceClass::Controller => &mut self.controller,
            DeviceClass::Char => &mut self.char_device,
            DeviceClass::Block => &mut self.block,
            DeviceClass::Net => &mut self.net,
        };
        *slot = slot
            .checked_add(1)
            .ok_or(DeviceBindError::RegistryCapacity {
                class,
                required: usize::from(*slot).saturating_add(1),
            })?;
        Ok(())
    }
}

/// Typed candidate or global binding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceBindError {
    AmbiguousDriver {
        device: DeviceId,
        priority: MatchPriority,
    },
    MissingResource {
        device: DeviceId,
        kind: ResourceKind,
        role: ResourceRole,
    },
    DuplicateResource {
        device: DeviceId,
        kind: ResourceKind,
        role: ResourceRole,
    },
    IncompatibleResource {
        device: DeviceId,
        kind: ResourceKind,
        role: ResourceRole,
    },
    Irq(IrqRouteError),
    DriverProbe {
        code: u32,
    },
    RegistryCapacity {
        class: DeviceClass,
        required: usize,
    },
    DuplicateRegistrationDevice(DeviceId),
    DuplicateActivation(DeviceId),
    MissingRegistration(DeviceId),
    MissingActivation(DeviceId),
    DuplicateDevt(DevT),
    BoundIndex(BoundDeviceIndexError),
    RegistryPrepare {
        class: DeviceClass,
        error: Errno,
    },
    RuntimePublication(DeviceRuntimePublishError),
    AllocationFailed,
}

/// One enabled device that matched a driver but failed candidate preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceBindFailure {
    pub device: DeviceId,
    pub driver: Option<DriverId>,
    pub error: DeviceBindError,
}

/// Frozen result of one successful global binding transaction.
#[derive(Debug)]
pub struct DeviceBindReport {
    pub bound: &'static [&'static BoundDevice],
    pub unsupported: &'static [DeviceId],
    pub failed: &'static [DeviceBindFailure],
}

/// Infallible, driver-supplied activation retained until after publication.
#[derive(Clone, Copy)]
pub struct DeviceActivation {
    pub device: &'static BoundDevice,
    arm: fn(&'static BoundDevice),
}

impl DeviceActivation {
    fn activate(self) {
        (self.arm)(self.device);
    }
}

impl core::fmt::Debug for DeviceActivation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceActivation")
            .field("device", &self.device.device_id)
            .finish_non_exhaustive()
    }
}

/// Final graph plus its corresponding binding report.
#[derive(Clone, Copy, Debug)]
pub struct DeviceBindOutcome {
    pub graph: &'static DeviceResourceGraph,
    pub report: &'static DeviceBindReport,
    pub bound_devices: &'static BoundDeviceIndex,
    pub irq_table: &'static IrqDispatchTable,
    pub activations: &'static [DeviceActivation],
}

/// Fatal graph-construction or global binding error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceBindTransactionError {
    Graph(ResourceGraphError),
    Bind(DeviceBindError),
}

impl From<ResourceGraphError> for DeviceBindTransactionError {
    fn from(error: ResourceGraphError) -> Self {
        Self::Graph(error)
    }
}

impl From<DeviceBindError> for DeviceBindTransactionError {
    fn from(error: DeviceBindError) -> Self {
        Self::Bind(error)
    }
}

/// Opaque, rollback-by-drop reservation passed to a selected driver.
pub struct DeviceBindReservation<P: TxPlatform> {
    device: &'static PlatformDevice,
    key: BoundDeviceKey,
    driver: DriverId,
    class_ordinals: DeviceClassOrdinals,
    registration: Option<BoundDeviceRegistration>,
    irq_proposals: Vec<PendingIrq>,
    activation: Option<fn(&'static BoundDevice)>,
    _platform: PhantomData<fn() -> P>,
}

impl<P: TxPlatform> DeviceBindReservation<P> {
    fn new(
        device: &'static PlatformDevice,
        key: BoundDeviceKey,
        driver: DriverId,
        class_ordinals: DeviceClassOrdinals,
    ) -> Self {
        Self {
            device,
            key,
            driver,
            class_ordinals,
            registration: None,
            irq_proposals: Vec::new(),
            activation: None,
            _platform: PhantomData,
        }
    }

    pub const fn device_id(&self) -> DeviceId {
        self.device.id
    }

    pub const fn bound_key(&self) -> BoundDeviceKey {
        self.key
    }

    /// Return the next stable ordinal within one registration class.
    ///
    /// This is deliberately independent of [`Self::bound_key`], whose index
    /// spans controllers, block devices, character devices, and network
    /// devices together. A block device binding before the first NIC must not
    /// turn that NIC into `eth1`.
    pub const fn registration_ordinal(&self, class: DeviceClass) -> u16 {
        self.class_ordinals.get(class)
    }

    /// Propose one static class registration. A second proposal is a typed
    /// candidate error and does not mutate the first proposal.
    pub fn propose_registration(
        &mut self,
        registration: BoundDeviceRegistration,
    ) -> Result<(), DeviceBindError> {
        if self.registration.is_some() {
            return Err(DeviceBindError::DuplicateRegistrationDevice(self.device.id));
        }
        self.registration = Some(registration);
        Ok(())
    }

    /// Retain an IRQ proposal only when it is the exact typed resource decoded
    /// from this device's immutable graph record.
    pub fn propose_irq(
        &mut self,
        resource: IrqResource,
        handler: DeviceIrqHandler,
    ) -> Result<(), DeviceBindError> {
        let exact = self
            .device
            .resources
            .iter()
            .any(|candidate| matches!(candidate, DeviceResource::Irq(irq) if *irq == resource));
        if !exact {
            let same_role = self.device.resources.iter().any(
                |candidate| matches!(candidate, DeviceResource::Irq(irq) if irq.role == resource.role),
            );
            return Err(if same_role {
                DeviceBindError::IncompatibleResource {
                    device: self.device.id,
                    kind: ResourceKind::Irq,
                    role: resource.role,
                }
            } else {
                DeviceBindError::MissingResource {
                    device: self.device.id,
                    kind: ResourceKind::Irq,
                    role: resource.role,
                }
            });
        }
        if self
            .irq_proposals
            .iter()
            .any(|current| current.resource.role == resource.role)
        {
            return Err(DeviceBindError::DuplicateResource {
                device: self.device.id,
                kind: ResourceKind::Irq,
                role: resource.role,
            });
        }
        self.irq_proposals
            .try_reserve(1)
            .map_err(|_| DeviceBindError::AllocationFailed)?;
        self.irq_proposals.push(PendingIrq { resource, handler });
        Ok(())
    }

    /// Retain the driver's infallible hardware-arm step. It runs only after
    /// the class registries and immutable runtime snapshot are visible.
    pub fn propose_activation(
        &mut self,
        activation: fn(&'static BoundDevice),
    ) -> Result<(), DeviceBindError> {
        if self.activation.is_some() {
            return Err(DeviceBindError::DuplicateActivation(self.device.id));
        }
        self.activation = Some(activation);
        Ok(())
    }

    fn into_pending(mut self) -> Result<PendingBinding, DeviceBindError> {
        self.irq_proposals
            .sort_unstable_by(|left, right| compare_irq_resource(&left.resource, &right.resource));
        let registration = self
            .registration
            .ok_or(DeviceBindError::MissingRegistration(self.device.id))?;
        if !self.irq_proposals.is_empty() && self.activation.is_none() {
            return Err(DeviceBindError::MissingActivation(self.device.id));
        }
        Ok(PendingBinding {
            device: self.device,
            key: self.key,
            driver: self.driver,
            registration,
            irq_proposals: self.irq_proposals,
            activation: self.activation,
        })
    }
}

#[derive(Clone, Copy)]
struct PendingIrq {
    resource: IrqResource,
    handler: DeviceIrqHandler,
}

struct PendingBinding {
    device: &'static PlatformDevice,
    key: BoundDeviceKey,
    driver: DriverId,
    registration: BoundDeviceRegistration,
    irq_proposals: Vec<PendingIrq>,
    activation: Option<fn(&'static BoundDevice)>,
}

/// Run all selected providers once, bind enabled devices, and freeze a report.
///
/// The returned objects are private host witnesses for the future one-shot
/// production binder. This function performs no class-registry publication,
/// IRQ-table installation, device arming, or unmasking.
pub fn bind_static_devices<P, B>() -> Result<DeviceBindOutcome, DeviceBindTransactionError>
where
    P: TxPlatform + 'static,
    B: StaticDeviceBundle<P>,
{
    let seed = P::platform_info().device_resources;
    let mut graph_builder = DeviceGraphBuilder::from_seed(seed)?;
    for provider in B::resource_providers() {
        (provider.enumerate)(seed, &mut graph_builder)?;
    }
    let graph = graph_builder.freeze()?;

    let mut devices: Vec<&'static PlatformDevice> = Vec::new();
    devices
        .try_reserve_exact(graph.devices.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    devices.extend(graph.devices.iter());
    devices.sort_unstable_by(|left, right| compare_device_id(left.id, right.id));

    let mut pending = Vec::new();
    let mut unsupported = Vec::new();
    let mut failed = Vec::new();
    let mut class_ordinals = DeviceClassOrdinals::default();
    pending
        .try_reserve_exact(devices.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    unsupported
        .try_reserve_exact(devices.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    failed
        .try_reserve_exact(devices.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;

    for device in devices {
        if device.status != DeviceStatus::Enabled {
            continue;
        }
        let Some((driver, _priority)) = select_driver::<P>(device, B::drivers())? else {
            unsupported.push(device.id);
            continue;
        };

        let key = u16::try_from(pending.len())
            .map(BoundDeviceKey)
            .map_err(|_| DeviceBindError::RegistryCapacity {
                class: DeviceClass::Controller,
                required: pending.len().saturating_add(1),
            })?;
        let mut reservation =
            DeviceBindReservation::<P>::new(device, key, driver.id, class_ordinals);
        match (driver.prepare)(device, &mut reservation) {
            Ok(()) => match reservation.into_pending() {
                Ok(binding) => {
                    class_ordinals.advance(binding.registration)?;
                    pending.push(binding);
                }
                Err(error) => failed.push(DeviceBindFailure {
                    device: device.id,
                    driver: Some(driver.id),
                    error,
                }),
            },
            Err(DeviceBindError::AllocationFailed) => {
                return Err(DeviceBindError::AllocationFailed.into());
            }
            Err(error) => failed.push(DeviceBindFailure {
                device: device.id,
                driver: Some(driver.id),
                error,
            }),
        }
    }

    validate_pending(&pending)?;
    freeze_outcome::<P>(graph, pending, unsupported, failed)
}

/// Bind and publish the statically selected device bundle as one boot
/// transaction.
///
/// Empty class proposals deliberately leave their legacy registries vacant
/// during the migration. A non-empty typed class owns that registry; all
/// fallible preparation completes before the first commit. The runtime
/// snapshot is the final common publication gate. Hardware activation and IRQ
/// unmasking happen strictly afterward.
pub fn publish_static_devices<P, B>() -> Result<DeviceBindOutcome, DeviceBindTransactionError>
where
    P: TxPlatform + 'static,
    B: StaticDeviceBundle<P>,
{
    let outcome = bind_static_devices::<P, B>()?;

    let mut block_regs = Vec::new();
    let mut net_regs = Vec::new();
    block_regs
        .try_reserve_exact(outcome.report.bound.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    net_regs
        .try_reserve_exact(outcome.report.bound.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    for bound in outcome.report.bound {
        match bound.registration {
            BoundDeviceRegistration::Block(registration) => block_regs.push(registration),
            BoundDeviceRegistration::Net(registration) => net_regs.push(registration),
            BoundDeviceRegistration::Char(_) | BoundDeviceRegistration::Controller => {}
        }
    }

    let prepared_block = if block_regs.is_empty() {
        None
    } else {
        Some(prepare_block_devices(&block_regs).map_err(|error| {
            DeviceBindError::RegistryPrepare {
                class: DeviceClass::Block,
                error,
            }
        })?)
    };
    let prepared_net = if net_regs.is_empty() {
        None
    } else {
        Some(
            prepare_net_devices(&net_regs).map_err(|error| DeviceBindError::RegistryPrepare {
                class: DeviceClass::Net,
                error,
            })?,
        )
    };
    let prepared_runtime: PreparedDeviceRuntime =
        prepare_device_runtime(outcome).map_err(DeviceBindError::RuntimePublication)?;

    if let Some(prepared) = prepared_block {
        prepared.commit();
    }
    if let Some(prepared) = prepared_net {
        prepared.commit();
    }
    prepared_runtime.commit();

    for activation in outcome.activations {
        activation.activate();
    }
    for (line, bucket) in outcome.irq_table.entries.iter().enumerate().skip(1) {
        if bucket.handlers.is_empty() {
            continue;
        }
        let line = u32::try_from(line).expect("validated IRQ table line fits u32");
        P::set_priority(line, 1);
        P::unmask(line);
    }

    Ok(outcome)
}

fn select_driver<'a, P: TxPlatform>(
    device: &PlatformDevice,
    drivers: &'a [StaticDriverDescriptor<P>],
) -> Result<Option<(&'a StaticDriverDescriptor<P>, MatchPriority)>, DeviceBindError> {
    let mut winner = None;
    let mut ambiguous = false;
    for driver in drivers {
        let DriverMatch::Supported(priority) = (driver.match_device)(device) else {
            continue;
        };
        match winner {
            None => {
                winner = Some((driver, priority));
                ambiguous = false;
            }
            Some((_, current)) if priority > current => {
                winner = Some((driver, priority));
                ambiguous = false;
            }
            Some((_, current)) if priority == current => ambiguous = true,
            Some(_) => {}
        }
    }
    if let Some((_, priority)) = winner {
        if ambiguous {
            return Err(DeviceBindError::AmbiguousDriver {
                device: device.id,
                priority,
            });
        }
    }
    Ok(winner)
}

fn validate_pending(pending: &[PendingBinding]) -> Result<(), DeviceBindError> {
    for (index, binding) in pending.iter().enumerate() {
        for previous in &pending[..index] {
            if previous.device.id == binding.device.id {
                return Err(DeviceBindError::DuplicateRegistrationDevice(
                    binding.device.id,
                ));
            }
            if let (Some(left), Some(right)) = (
                registration_devt(previous.registration),
                registration_devt(binding.registration),
            ) {
                if left == right {
                    return Err(DeviceBindError::DuplicateDevt(right));
                }
            }
        }

        for (irq_index, irq) in binding.irq_proposals.iter().enumerate() {
            for previous_irq in &binding.irq_proposals[..irq_index] {
                validate_irq_pair(
                    binding.device.id,
                    previous_irq.resource,
                    binding.device.id,
                    irq.resource,
                )?;
            }
            for previous in &pending[..index] {
                for previous_irq in &previous.irq_proposals {
                    validate_irq_pair(
                        previous.device.id,
                        previous_irq.resource,
                        binding.device.id,
                        irq.resource,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn validate_irq_pair(
    first_device: DeviceId,
    first: IrqResource,
    second_device: DeviceId,
    second: IrqResource,
) -> Result<(), DeviceBindError> {
    if first_device == second_device && first.role == second.role {
        return Err(DeviceBindError::Irq(IrqRouteError::DuplicateRoute {
            device: second_device,
            role: second.role,
        }));
    }
    if first.line == second.line
        && (first.sharing != IrqSharing::Shared || second.sharing != IrqSharing::Shared)
    {
        return Err(DeviceBindError::Irq(IrqRouteError::ExclusiveConflict {
            line: second.line,
            first_device,
            first_role: first.role,
            second_device,
            second_role: second.role,
        }));
    }
    Ok(())
}

fn freeze_outcome<P: TxPlatform>(
    graph: &'static DeviceResourceGraph,
    pending: Vec<PendingBinding>,
    unsupported: Vec<DeviceId>,
    failed: Vec<DeviceBindFailure>,
) -> Result<DeviceBindOutcome, DeviceBindTransactionError> {
    // Stage every fallible allocation before the first BoundDevice/report leak.
    let mut staged_contexts = Vec::new();
    staged_contexts
        .try_reserve_exact(pending.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    for binding in &pending {
        let mut contexts = Vec::new();
        contexts
            .try_reserve_exact(binding.irq_proposals.len())
            .map_err(|_| DeviceBindError::AllocationFailed)?;
        for proposal in &binding.irq_proposals {
            contexts.push(DeviceIrqContext {
                route: IrqRoute {
                    device: binding.device.id,
                    resource: proposal.resource,
                },
                bound: binding.key,
            });
        }
        staged_contexts.push(contexts);
    }

    let mut bound = Vec::new();
    bound
        .try_reserve_exact(pending.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    let mut activations = Vec::new();
    activations
        .try_reserve_exact(pending.len())
        .map_err(|_| DeviceBindError::AllocationFailed)?;
    let mut index_builder =
        BoundDeviceIndexBuilder::try_with_capacity(pending.len(), pending.len())
            .map_err(DeviceBindError::BoundIndex)?;

    // All typed/fallible work is now complete. Freeze only immutable values.
    let mut irq_entries = Vec::new();
    let irq_count = pending
        .iter()
        .map(|binding| binding.irq_proposals.len())
        .sum();
    irq_entries
        .try_reserve_exact(irq_count)
        .map_err(|_| DeviceBindError::AllocationFailed)?;

    for (binding, contexts) in pending.into_iter().zip(staged_contexts) {
        let contexts = Box::leak(contexts.into_boxed_slice());
        for (context, proposal) in contexts.iter().zip(&binding.irq_proposals) {
            irq_entries.push((context as &'static DeviceIrqContext, proposal.handler));
        }
        let bound_device = Box::leak(Box::new(BoundDevice {
            key: binding.key,
            device_id: binding.device.id,
            driver_id: binding.driver,
            registration: binding.registration,
            irq_contexts: contexts,
        })) as &'static BoundDevice;
        index_builder
            .try_push(bound_device)
            .map_err(DeviceBindError::BoundIndex)?;
        if let Some(arm) = binding.activation {
            activations.push(DeviceActivation {
                device: bound_device,
                arm,
            });
        }
        bound.push(bound_device);
    }
    let report = Box::leak(Box::new(DeviceBindReport {
        bound: Box::leak(bound.into_boxed_slice()),
        unsupported: Box::leak(unsupported.into_boxed_slice()),
        failed: Box::leak(failed.into_boxed_slice()),
    }));

    let bound_devices = index_builder
        .freeze()
        .map_err(DeviceBindError::BoundIndex)?;

    let mut irq_builder = KernelIrqTableBuilder::new(bound_devices, P::MAX_IRQ);
    for (context, handler) in irq_entries {
        irq_builder
            .reserve_device(context, handler)
            .map_err(DeviceBindError::Irq)?;
    }
    let irq_table = irq_builder.freeze().map_err(DeviceBindError::Irq)?;

    Ok(DeviceBindOutcome {
        graph,
        report,
        bound_devices,
        irq_table,
        activations: Box::leak(activations.into_boxed_slice()),
    })
}

const fn registration_devt(registration: BoundDeviceRegistration) -> Option<DevT> {
    match registration {
        BoundDeviceRegistration::Char(binding) => Some(binding.devt),
        BoundDeviceRegistration::Block(registration) => Some(registration.devt),
        BoundDeviceRegistration::Net(registration) => Some(registration.devt),
        BoundDeviceRegistration::Controller => None,
    }
}

fn compare_irq_resource(left: &IrqResource, right: &IrqResource) -> Ordering {
    compare_role(left.role, right.role).then_with(|| left.line.cmp(&right.line))
}

fn compare_device_id(left: DeviceId, right: DeviceId) -> Ordering {
    left.provider
        .0
        .cmp(right.provider.0)
        .then_with(|| compare_local_id(left.local, right.local))
}

fn compare_local_id(left: DeviceLocalId, right: DeviceLocalId) -> Ordering {
    match (left, right) {
        (DeviceLocalId::FirmwarePath(left), DeviceLocalId::FirmwarePath(right))
        | (DeviceLocalId::PlatformKey(left), DeviceLocalId::PlatformKey(right)) => left.cmp(right),
        (DeviceLocalId::PciFunction(left), DeviceLocalId::PciFunction(right)) => left
            .segment
            .cmp(&right.segment)
            .then_with(|| left.bus.cmp(&right.bus))
            .then_with(|| left.device.cmp(&right.device))
            .then_with(|| left.function.cmp(&right.function)),
        (left, right) => local_id_rank(left).cmp(&local_id_rank(right)),
    }
}

const fn local_id_rank(id: DeviceLocalId) -> u8 {
    match id {
        DeviceLocalId::FirmwarePath(_) => 0,
        DeviceLocalId::PciFunction(_) => 1,
        DeviceLocalId::PlatformKey(_) => 2,
    }
}

fn compare_role(left: ResourceRole, right: ResourceRole) -> Ordering {
    match (left, right) {
        (ResourceRole::Named(left), ResourceRole::Named(right)) => left.cmp(right),
        (ResourceRole::Index(left), ResourceRole::Index(right)) => left.cmp(&right),
        (ResourceRole::Named(_), ResourceRole::Index(_)) => Ordering::Less,
        (ResourceRole::Index(_), ResourceRole::Named(_)) => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tx_hal::{
        DeviceLocalId, DeviceMatchId, DeviceRecordBuilder, IrqHandled, IrqPolarity, IrqTrigger,
        ResourceCapacityKind, ResourceOrigin, ResourceOriginKind, ResourceProviderId,
    };
    use tx_subsystems::adapter::step_engine::{ByteProgress, StepOutcome};
    use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps};
    use tx_subsystems::execution::Guard;

    use crate::irq::tests::IrqTestPlatform;
    use crate::test_serialise::KERNEL_TEST_LOCK;

    const PROVIDER_A: ResourceProviderId = ResourceProviderId("synthetic-a");
    const PROVIDER_B: ResourceProviderId = ResourceProviderId("synthetic-b");
    const PROVIDER_U: ResourceProviderId = ResourceProviderId("synthetic-unsupported");
    const MATCH_OK: &str = "synthetic,ok";
    const MATCH_FAIL: &str = "synthetic,probe-fail";
    const MATCH_UNKNOWN: &str = "synthetic,unknown";

    static PROVIDER_A_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PROVIDER_B_CALLS: AtomicUsize = AtomicUsize::new(0);
    static ACTIVATION_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TYPED_IRQ_CALLS: AtomicUsize = AtomicUsize::new(0);

    const fn origin(provider: ResourceProviderId, record: &'static str) -> ResourceOrigin {
        ResourceOrigin {
            provider,
            record,
            kind: ResourceOriginKind::PlatformStatic,
        }
    }

    const fn device_id(provider: ResourceProviderId, local: &'static str) -> DeviceId {
        DeviceId {
            provider,
            local: DeviceLocalId::PlatformKey(local),
        }
    }

    const fn irq(provider: ResourceProviderId, line: u32) -> IrqResource {
        IrqResource {
            role: ResourceRole::Named("event"),
            line,
            trigger: IrqTrigger::Level,
            polarity: IrqPolarity::High,
            sharing: IrqSharing::Exclusive,
            origin: origin(provider, "synthetic irq"),
        }
    }

    fn push_device(
        out: &mut DeviceGraphBuilder,
        provider: ResourceProviderId,
        local: &'static str,
        compatible: &'static str,
        line: u32,
    ) -> Result<(), ResourceGraphError> {
        let mut matches = Vec::new();
        matches
            .try_reserve_exact(1)
            .map_err(|_| ResourceGraphError::CapacityExceeded {
                kind: ResourceCapacityKind::Resource,
                required: 1,
            })?;
        matches.push(DeviceMatchId::FirmwareCompatible(compatible));
        let mut resources = Vec::new();
        resources
            .try_reserve_exact(1)
            .map_err(|_| ResourceGraphError::CapacityExceeded {
                kind: ResourceCapacityKind::Resource,
                required: 1,
            })?;
        resources.push(DeviceResource::Irq(irq(provider, line)));
        out.push_device(DeviceRecordBuilder {
            id: device_id(provider, local),
            status: DeviceStatus::Enabled,
            matches,
            resources,
            origin: origin(provider, local),
        })
    }

    fn enumerate_a(
        _seed: &'static DeviceResourceGraph,
        out: &mut DeviceGraphBuilder,
    ) -> Result<(), ResourceGraphError> {
        PROVIDER_A_CALLS.fetch_add(1, AtomicOrdering::AcqRel);
        push_device(out, PROVIDER_A, "a", MATCH_OK, 17)
    }

    fn enumerate_b(
        _seed: &'static DeviceResourceGraph,
        out: &mut DeviceGraphBuilder,
    ) -> Result<(), ResourceGraphError> {
        PROVIDER_B_CALLS.fetch_add(1, AtomicOrdering::AcqRel);
        push_device(out, PROVIDER_B, "b", MATCH_OK, 18)
    }

    fn enumerate_unsupported(
        _seed: &'static DeviceResourceGraph,
        out: &mut DeviceGraphBuilder,
    ) -> Result<(), ResourceGraphError> {
        push_device(out, PROVIDER_U, "unsupported", MATCH_UNKNOWN, 19)
    }

    fn enumerate_failed_then_healthy(
        _seed: &'static DeviceResourceGraph,
        out: &mut DeviceGraphBuilder,
    ) -> Result<(), ResourceGraphError> {
        push_device(out, PROVIDER_A, "failed", MATCH_FAIL, 20)?;
        push_device(out, PROVIDER_A, "healthy", MATCH_OK, 21)
    }

    fn matches_ok(device: &PlatformDevice) -> DriverMatch {
        if device.matches.iter().any(|candidate| {
            matches!(candidate, DeviceMatchId::FirmwareCompatible(value) if *value == MATCH_OK)
        }) {
            DriverMatch::Supported(MatchPriority(10))
        } else {
            DriverMatch::Unsupported
        }
    }

    fn matches_ok_or_fail(device: &PlatformDevice) -> DriverMatch {
        if device.matches.iter().any(|candidate| {
            matches!(candidate, DeviceMatchId::FirmwareCompatible(value) if *value == MATCH_OK || *value == MATCH_FAIL)
        }) {
            DriverMatch::Supported(MatchPriority(10))
        } else {
            DriverMatch::Unsupported
        }
    }

    fn prepare_controller(
        device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<IrqTestPlatform>,
    ) -> Result<(), DeviceBindError> {
        reservation.propose_registration(BoundDeviceRegistration::Controller)?;
        for resource in device.resources {
            if let DeviceResource::Irq(irq) = resource {
                reservation.propose_irq(*irq, handled)?;
            }
        }
        reservation.propose_activation(activate_controller)?;
        Ok(())
    }

    fn handled(_context: &'static DeviceIrqContext, _line: u32) -> IrqHandled {
        TYPED_IRQ_CALLS.fetch_add(1, AtomicOrdering::AcqRel);
        IrqHandled::Done
    }

    fn activate_controller(_device: &'static BoundDevice) {
        assert!(crate::devices::runtime::device_runtime_snapshot().is_some());
        ACTIVATION_CALLS.fetch_add(1, AtomicOrdering::AcqRel);
    }

    fn prepare_maybe_fail(
        device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<IrqTestPlatform>,
    ) -> Result<(), DeviceBindError> {
        if device.matches.iter().any(|candidate| {
            matches!(candidate, DeviceMatchId::FirmwareCompatible(value) if *value == MATCH_FAIL)
        }) {
            return Err(DeviceBindError::DriverProbe { code: 7 });
        }
        prepare_controller(device, reservation)
    }

    struct DummyChar;

    impl CharDeviceOps for DummyChar {
        fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
            unreachable!("synthetic registration is never published")
        }

        fn write(&self, _bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
            unreachable!("synthetic registration is never published")
        }
    }

    static DUMMY_CHAR: DummyChar = DummyChar;
    static DUPLICATE_CHAR_REGISTRATION: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(240, 0),
        name: "synthetic-char",
        ops: &DUMMY_CHAR,
    };

    struct DummyNet;

    impl tx_subsystems::net::NetDeviceOps for DummyNet {
        fn receive(&self) -> Option<tx_subsystems::net::RxFrame> {
            None
        }

        fn transmit(
            &self,
            _frame: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_subsystems::execution::StepOutcome<()> {
            tx_subsystems::execution::StepOutcome::Done(())
        }

        fn mac_addr(&self) -> tx_subsystems::net::EthernetAddress {
            tx_subsystems::net::EthernetAddress::new([0x02, 0, 0, 0, 0, 0x42])
        }

        fn mtu(&self) -> u16 {
            1500
        }
    }

    static DUMMY_NET: DummyNet = DummyNet;
    static NET_REGISTRATION: tx_subsystems::net::NetDeviceRegistration =
        tx_subsystems::net::NetDeviceRegistration {
            devt: DevT::new(241, 0),
            name: "synthetic-net",
            ops: &DUMMY_NET,
        };

    fn prepare_duplicate_devt(
        _device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<IrqTestPlatform>,
    ) -> Result<(), DeviceBindError> {
        reservation
            .propose_registration(BoundDeviceRegistration::Char(&DUPLICATE_CHAR_REGISTRATION))
    }

    fn prepare_without_registration(
        _device: &'static PlatformDevice,
        _reservation: &mut DeviceBindReservation<IrqTestPlatform>,
    ) -> Result<(), DeviceBindError> {
        Ok(())
    }

    fn prepare_without_activation(
        device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<IrqTestPlatform>,
    ) -> Result<(), DeviceBindError> {
        reservation.propose_registration(BoundDeviceRegistration::Controller)?;
        for resource in device.resources {
            if let DeviceResource::Irq(irq) = resource {
                reservation.propose_irq(*irq, handled)?;
            }
        }
        Ok(())
    }

    fn prepare_net(
        device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<IrqTestPlatform>,
    ) -> Result<(), DeviceBindError> {
        reservation.propose_registration(BoundDeviceRegistration::Net(&NET_REGISTRATION))?;
        for resource in device.resources {
            if let DeviceResource::Irq(irq) = resource {
                reservation.propose_irq(*irq, handled)?;
            }
        }
        reservation.propose_activation(activate_controller)
    }

    const PROVIDER_A_DESCRIPTOR: ResourceProviderDescriptor<IrqTestPlatform> =
        ResourceProviderDescriptor {
            id: PROVIDER_A,
            enumerate: enumerate_a,
            _platform: PhantomData,
        };
    const PROVIDER_B_DESCRIPTOR: ResourceProviderDescriptor<IrqTestPlatform> =
        ResourceProviderDescriptor {
            id: PROVIDER_B,
            enumerate: enumerate_b,
            _platform: PhantomData,
        };
    const UNSUPPORTED_PROVIDER_DESCRIPTOR: ResourceProviderDescriptor<IrqTestPlatform> =
        ResourceProviderDescriptor {
            id: PROVIDER_U,
            enumerate: enumerate_unsupported,
            _platform: PhantomData,
        };
    const FAILED_PROVIDER_DESCRIPTOR: ResourceProviderDescriptor<IrqTestPlatform> =
        ResourceProviderDescriptor {
            id: PROVIDER_A,
            enumerate: enumerate_failed_then_healthy,
            _platform: PhantomData,
        };

    const CONTROLLER_DRIVER: StaticDriverDescriptor<IrqTestPlatform> = StaticDriverDescriptor {
        id: DriverId("synthetic-controller"),
        match_device: matches_ok,
        prepare: prepare_controller,
        _platform: PhantomData,
    };
    const FALLIBLE_DRIVER: StaticDriverDescriptor<IrqTestPlatform> = StaticDriverDescriptor {
        id: DriverId("synthetic-fallible"),
        match_device: matches_ok_or_fail,
        prepare: prepare_maybe_fail,
        _platform: PhantomData,
    };
    const DUPLICATE_DEVT_DRIVER: StaticDriverDescriptor<IrqTestPlatform> = StaticDriverDescriptor {
        id: DriverId("synthetic-duplicate-devt"),
        match_device: matches_ok,
        prepare: prepare_duplicate_devt,
        _platform: PhantomData,
    };
    const AMBIGUOUS_DRIVER: StaticDriverDescriptor<IrqTestPlatform> = StaticDriverDescriptor {
        id: DriverId("synthetic-ambiguous"),
        match_device: matches_ok,
        prepare: prepare_controller,
        _platform: PhantomData,
    };
    const MISSING_REGISTRATION_DRIVER: StaticDriverDescriptor<IrqTestPlatform> =
        StaticDriverDescriptor {
            id: DriverId("synthetic-missing-registration"),
            match_device: matches_ok,
            prepare: prepare_without_registration,
            _platform: PhantomData,
        };
    const MISSING_ACTIVATION_DRIVER: StaticDriverDescriptor<IrqTestPlatform> =
        StaticDriverDescriptor {
            id: DriverId("synthetic-missing-activation"),
            match_device: matches_ok,
            prepare: prepare_without_activation,
            _platform: PhantomData,
        };
    const NET_DRIVER: StaticDriverDescriptor<IrqTestPlatform> = StaticDriverDescriptor {
        id: DriverId("synthetic-net"),
        match_device: matches_ok,
        prepare: prepare_net,
        _platform: PhantomData,
    };

    struct EmptyBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for EmptyBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[]
        }
    }

    struct AlternateEmptyBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for AlternateEmptyBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[]
        }
    }

    struct ForwardBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for ForwardBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_A_DESCRIPTOR, PROVIDER_B_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[CONTROLLER_DRIVER]
        }
    }

    struct ReverseBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for ReverseBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_B_DESCRIPTOR, PROVIDER_A_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[CONTROLLER_DRIVER]
        }
    }

    struct UnsupportedBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for UnsupportedBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[UNSUPPORTED_PROVIDER_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[CONTROLLER_DRIVER]
        }
    }

    struct FailedFirstBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for FailedFirstBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[FAILED_PROVIDER_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[FALLIBLE_DRIVER]
        }
    }

    struct AmbiguousBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for AmbiguousBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_A_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[CONTROLLER_DRIVER, AMBIGUOUS_DRIVER]
        }
    }

    struct DuplicateDevtBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for DuplicateDevtBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_A_DESCRIPTOR, PROVIDER_B_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[DUPLICATE_DEVT_DRIVER]
        }
    }

    struct MissingRegistrationBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for MissingRegistrationBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_A_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[MISSING_REGISTRATION_DRIVER]
        }
    }

    struct MissingActivationBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for MissingActivationBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_A_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[MISSING_ACTIVATION_DRIVER]
        }
    }

    struct NetBundle;
    impl StaticDeviceBundle<IrqTestPlatform> for NetBundle {
        fn resource_providers() -> &'static [ResourceProviderDescriptor<IrqTestPlatform>] {
            &[PROVIDER_A_DESCRIPTOR]
        }
        fn drivers() -> &'static [StaticDriverDescriptor<IrqTestPlatform>] {
            &[NET_DRIVER]
        }
    }

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let lock = KERNEL_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        PROVIDER_A_CALLS.store(0, AtomicOrdering::Release);
        PROVIDER_B_CALLS.store(0, AtomicOrdering::Release);
        ACTIVATION_CALLS.store(0, AtomicOrdering::Release);
        TYPED_IRQ_CALLS.store(0, AtomicOrdering::Release);
        crate::devices::runtime::reset_device_runtime_for_test();
        tx_subsystems::net::device::reset_net_registry_for_test();
        lock
    }

    fn bound_ids(outcome: DeviceBindOutcome) -> Vec<DeviceId> {
        outcome
            .report
            .bound
            .iter()
            .map(|bound| bound.device_id)
            .collect()
    }

    fn instantiate_core<D: StaticDeviceBundle<IrqTestPlatform>>(
    ) -> crate::init::CoreInit<IrqTestPlatform, D> {
        crate::init::CoreInit::new()
    }

    #[test]
    fn core_init_accepts_distinct_compile_time_bundle_types() {
        let _lock = setup();
        let first = instantiate_core::<EmptyBundle>();
        let second = instantiate_core::<AlternateEmptyBundle>();
        assert_eq!(core::mem::size_of_val(&first), 0);
        assert_eq!(core::mem::size_of_val(&second), 0);
        assert_ne!(
            core::any::type_name_of_val(&first),
            core::any::type_name_of_val(&second)
        );
    }

    #[test]
    fn registration_ordinals_are_scoped_by_device_class() {
        let mut ordinals = DeviceClassOrdinals::default();
        ordinals
            .advance(BoundDeviceRegistration::Controller)
            .unwrap();
        ordinals
            .advance(BoundDeviceRegistration::Net(&NET_REGISTRATION))
            .unwrap();

        assert_eq!(ordinals.get(DeviceClass::Controller), 1);
        assert_eq!(ordinals.get(DeviceClass::Net), 1);
        assert_eq!(ordinals.get(DeviceClass::Block), 0);
        assert_eq!(ordinals.get(DeviceClass::Char), 0);
    }

    #[test]
    fn zero_provider_and_driver_bundle_is_valid() {
        let _lock = setup();
        let outcome = bind_static_devices::<IrqTestPlatform, EmptyBundle>().unwrap();
        assert!(outcome.graph.devices.is_empty());
        assert!(outcome.report.bound.is_empty());
        assert!(outcome.bound_devices.is_empty());
        assert!(outcome.irq_table.is_empty());
        assert!(outcome.report.unsupported.is_empty());
        assert!(outcome.report.failed.is_empty());
    }

    #[test]
    fn production_publication_exposes_runtime_before_activation_and_typed_dispatch() {
        let _lock = setup();
        let outcome = publish_static_devices::<IrqTestPlatform, ForwardBundle>().unwrap();
        let runtime =
            crate::devices::runtime::device_runtime_snapshot().expect("published device runtime");

        assert!(core::ptr::eq(runtime.outcome.irq_table, outcome.irq_table));
        assert_eq!(ACTIVATION_CALLS.load(AtomicOrdering::Acquire), 2);
        assert_eq!(
            crate::irq::dispatch_external_irq::<IrqTestPlatform>(17),
            IrqHandled::Done
        );
        assert_eq!(TYPED_IRQ_CALLS.load(AtomicOrdering::Acquire), 1);
    }

    #[test]
    fn production_publication_commits_nonempty_typed_net_registry() {
        let _lock = setup();
        publish_static_devices::<IrqTestPlatform, NetBundle>().unwrap();

        let registrations = tx_subsystems::net::net_device_snapshot();
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].name, "synthetic-net");
        assert_eq!(ACTIVATION_CALLS.load(AtomicOrdering::Acquire), 1);
    }

    #[test]
    fn failed_common_runtime_prepare_rolls_back_class_registry_reservation() {
        let _lock = setup();
        publish_static_devices::<IrqTestPlatform, EmptyBundle>().unwrap();

        assert_eq!(
            publish_static_devices::<IrqTestPlatform, NetBundle>().unwrap_err(),
            DeviceBindTransactionError::Bind(DeviceBindError::RuntimePublication(
                DeviceRuntimePublishError::AlreadyReservedOrPublished,
            ))
        );
        assert!(tx_subsystems::net::net_device_snapshot().is_empty());
    }

    #[test]
    fn an_additional_provider_needs_no_generic_kernel_arch_branch() {
        let _lock = setup();
        let outcome = bind_static_devices::<IrqTestPlatform, ForwardBundle>().unwrap();
        assert_eq!(PROVIDER_A_CALLS.load(AtomicOrdering::Acquire), 1);
        assert_eq!(PROVIDER_B_CALLS.load(AtomicOrdering::Acquire), 1);
        assert_eq!(outcome.report.bound.len(), 2);
        assert_eq!(
            outcome.report.bound[0].irq_contexts[0].route.resource.line,
            17
        );
        assert_eq!(
            outcome.report.bound[1].irq_contexts[0].route.resource.line,
            18
        );
        for bound in outcome.report.bound {
            assert_eq!(bound.irq_contexts[0].route.device, bound.device_id);
            assert_eq!(bound.irq_contexts[0].bound, bound.key);
        }
        assert_eq!(outcome.bound_devices.len(), 2);
        assert!(outcome.irq_table.bucket(17).is_some());
        assert!(outcome.irq_table.bucket(18).is_some());
    }

    #[test]
    fn provider_order_does_not_change_bound_order() {
        let _lock = setup();
        let forward = bind_static_devices::<IrqTestPlatform, ForwardBundle>().unwrap();
        let reverse = bind_static_devices::<IrqTestPlatform, ReverseBundle>().unwrap();
        assert_eq!(bound_ids(forward), bound_ids(reverse));
    }

    #[test]
    fn unsupported_enabled_device_is_a_valid_report_entry() {
        let _lock = setup();
        let outcome = bind_static_devices::<IrqTestPlatform, UnsupportedBundle>().unwrap();
        assert!(outcome.report.bound.is_empty());
        assert!(outcome.report.failed.is_empty());
        assert_eq!(
            outcome.report.unsupported,
            &[device_id(PROVIDER_U, "unsupported")]
        );
    }

    #[test]
    fn failed_candidate_does_not_hide_later_healthy_device() {
        let _lock = setup();
        let outcome = bind_static_devices::<IrqTestPlatform, FailedFirstBundle>().unwrap();
        assert_eq!(outcome.report.bound.len(), 1);
        assert_eq!(
            outcome.report.bound[0].device_id,
            device_id(PROVIDER_A, "healthy")
        );
        assert_eq!(
            outcome.report.failed,
            &[DeviceBindFailure {
                device: device_id(PROVIDER_A, "failed"),
                driver: Some(DriverId("synthetic-fallible")),
                error: DeviceBindError::DriverProbe { code: 7 },
            }]
        );
    }

    #[test]
    fn equal_highest_driver_match_aborts_transaction() {
        let _lock = setup();
        assert_eq!(
            bind_static_devices::<IrqTestPlatform, AmbiguousBundle>().unwrap_err(),
            DeviceBindTransactionError::Bind(DeviceBindError::AmbiguousDriver {
                device: device_id(PROVIDER_A, "a"),
                priority: MatchPriority(10),
            })
        );
    }

    #[test]
    fn duplicate_devt_aborts_global_transaction() {
        let _lock = setup();
        assert_eq!(
            bind_static_devices::<IrqTestPlatform, DuplicateDevtBundle>().unwrap_err(),
            DeviceBindTransactionError::Bind(DeviceBindError::DuplicateDevt(DevT::new(240, 0)))
        );
    }

    #[test]
    fn driver_must_explicitly_propose_a_registration_kind() {
        let _lock = setup();
        let outcome = bind_static_devices::<IrqTestPlatform, MissingRegistrationBundle>().unwrap();
        assert!(outcome.report.bound.is_empty());
        assert_eq!(
            outcome.report.failed,
            &[DeviceBindFailure {
                device: device_id(PROVIDER_A, "a"),
                driver: Some(DriverId("synthetic-missing-registration")),
                error: DeviceBindError::MissingRegistration(device_id(PROVIDER_A, "a")),
            }]
        );
    }

    #[test]
    fn irq_driver_must_explicitly_propose_post_publish_activation() {
        let _lock = setup();
        let outcome = bind_static_devices::<IrqTestPlatform, MissingActivationBundle>().unwrap();
        assert!(outcome.report.bound.is_empty());
        assert_eq!(
            outcome.report.failed,
            &[DeviceBindFailure {
                device: device_id(PROVIDER_A, "a"),
                driver: Some(DriverId("synthetic-missing-activation")),
                error: DeviceBindError::MissingActivation(device_id(PROVIDER_A, "a")),
            }]
        );
    }
}
