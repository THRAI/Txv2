//! Boot-frozen dispatch for tier-2 device interrupt routes.
//!
//! This module deliberately stops at the construction seam.  The one-shot
//! device binder may validate and freeze a table here, but the legacy global
//! IRQ table and the production trap top half do not consume it yet.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cmp::Ordering;

use tx_hal::{DeviceId, DeviceLocalId, IrqHandled, IrqSharing, ResourceRole};
use tx_subsystems::device_binding::{BoundDeviceIndex, BoundDeviceLookupError, DeviceIrqContext};

/// Device-specific top-half invoked for one claimed controller line.
pub type DeviceIrqHandler = fn(context: &'static DeviceIrqContext, claimed_line: u32) -> IrqHandled;

/// One validated device route and its driver-owned handler.
#[derive(Clone, Copy, Debug)]
pub struct IrqDispatchEntry {
    pub context: &'static DeviceIrqContext,
    pub handler: DeviceIrqHandler,
}

/// All handlers committed for one controller line.
#[derive(Debug)]
pub struct IrqDispatchBucket {
    pub handlers: &'static [IrqDispatchEntry],
}

/// Immutable controller-line-indexed device dispatch table.
#[derive(Debug)]
pub struct IrqDispatchTable {
    pub entries: &'static [IrqDispatchBucket],
}

impl IrqDispatchTable {
    /// Return the bucket for `line`; absent and empty buckets are equivalent.
    pub fn bucket(&self, line: u32) -> Option<&IrqDispatchBucket> {
        let index = usize::try_from(line).ok()?;
        self.entries
            .get(index)
            .filter(|bucket| !bucket.handlers.is_empty())
    }

    /// Dispatch one claimed controller line through its deterministic bucket.
    ///
    /// The frozen builder orders handlers by device identity and resource
    /// role. Dispatch preserves that order, invokes every shared handler, and
    /// aggregates without allocation or locking. `DeferredWake` dominates so
    /// controller completion ownership cannot be lost; otherwise the order is
    /// `Wake > Done > NotMine`.
    pub fn dispatch_irq(&self, line: u32) -> IrqHandled {
        let Some(bucket) = self.bucket(line) else {
            return IrqHandled::NotMine;
        };

        let mut aggregate = IrqHandled::NotMine;
        for entry in bucket.handlers {
            let outcome = (entry.handler)(entry.context, line);
            aggregate = merge_irq_outcome(aggregate, outcome);
        }
        aggregate
    }

    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

const fn merge_irq_outcome(current: IrqHandled, next: IrqHandled) -> IrqHandled {
    match (current, next) {
        (IrqHandled::DeferredWake, _) | (_, IrqHandled::DeferredWake) => IrqHandled::DeferredWake,
        (IrqHandled::Wake, _) | (_, IrqHandled::Wake) => IrqHandled::Wake,
        (IrqHandled::Done, _) | (_, IrqHandled::Done) => IrqHandled::Done,
        (IrqHandled::NotMine, IrqHandled::NotMine) => IrqHandled::NotMine,
    }
}

static EMPTY_DISPATCH_TABLE: IrqDispatchTable = IrqDispatchTable { entries: &[] };

/// Typed failure while validating or materializing device IRQ routes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqRouteError {
    AllocationFailed,
    InvalidBoundDevice(BoundDeviceLookupError),
    ContextNotOwned {
        device: DeviceId,
        role: ResourceRole,
    },
    DuplicateRoute {
        device: DeviceId,
        role: ResourceRole,
    },
    ExclusiveConflict {
        line: u32,
        first_device: DeviceId,
        first_role: ResourceRole,
        second_device: DeviceId,
        second_role: ResourceRole,
    },
    LineOutOfRange(u32),
}

/// Private-until-freeze builder used only by the one-shot boot binder.
pub struct KernelIrqTableBuilder {
    bound_devices: &'static BoundDeviceIndex,
    max_irq_exclusive: u32,
    pending: Vec<IrqDispatchEntry>,
}

impl KernelIrqTableBuilder {
    /// Create a builder for controller source lines in `1..max_irq_exclusive`.
    ///
    /// The upper bound has the same exclusive semantics as [`tx_hal::IrqIf::MAX_IRQ`].
    pub const fn new(bound_devices: &'static BoundDeviceIndex, max_irq_exclusive: u32) -> Self {
        Self {
            bound_devices,
            max_irq_exclusive,
            pending: Vec::new(),
        }
    }

    /// Reserve a candidate route without publishing or leaking any storage.
    pub fn reserve_device(
        &mut self,
        context: &'static DeviceIrqContext,
        handler: DeviceIrqHandler,
    ) -> Result<(), IrqRouteError> {
        let line = context.route.resource.line;
        if line == 0 || line >= self.max_irq_exclusive {
            return Err(IrqRouteError::LineOutOfRange(line));
        }
        self.pending
            .try_reserve(1)
            .map_err(|_| IrqRouteError::AllocationFailed)?;
        self.pending.push(IrqDispatchEntry { context, handler });
        Ok(())
    }

    /// Validate every candidate, then leak exactly one immutable table.
    ///
    /// Validation precedes every leak, so an identity, duplicate-route, or
    /// sharing failure leaves no partially published dispatch structure.
    pub fn freeze(mut self) -> Result<&'static IrqDispatchTable, IrqRouteError> {
        canonicalize(&mut self.pending);
        validate_routes(self.bound_devices, &self.pending)?;

        if self.pending.is_empty() {
            return Ok(&EMPTY_DISPATCH_TABLE);
        }

        let max_line = self
            .pending
            .iter()
            .map(|entry| entry.context.route.resource.line)
            .max()
            .expect("non-empty route set has a maximum line");
        let bucket_count = usize::try_from(max_line)
            .ok()
            .and_then(|line| line.checked_add(1))
            .ok_or(IrqRouteError::LineOutOfRange(max_line))?;

        let mut counts = Vec::new();
        counts
            .try_reserve_exact(bucket_count)
            .map_err(|_| IrqRouteError::AllocationFailed)?;
        counts.resize(bucket_count, 0usize);
        for entry in &self.pending {
            counts[entry.context.route.resource.line as usize] += 1;
        }

        let mut staged = Vec::new();
        staged
            .try_reserve_exact(bucket_count)
            .map_err(|_| IrqRouteError::AllocationFailed)?;
        for count in counts {
            let mut bucket = Vec::new();
            bucket
                .try_reserve_exact(count)
                .map_err(|_| IrqRouteError::AllocationFailed)?;
            staged.push(bucket);
        }
        for entry in self.pending {
            staged[entry.context.route.resource.line as usize].push(entry);
        }

        // Reserve the outer immutable shape before leaking any inner slice.
        let mut buckets = Vec::new();
        buckets
            .try_reserve_exact(bucket_count)
            .map_err(|_| IrqRouteError::AllocationFailed)?;
        for handlers in staged {
            buckets.push(IrqDispatchBucket {
                handlers: Box::leak(handlers.into_boxed_slice()),
            });
        }

        Ok(Box::leak(Box::new(IrqDispatchTable {
            entries: Box::leak(buckets.into_boxed_slice()),
        })))
    }
}

fn validate_routes(
    bound_devices: &BoundDeviceIndex,
    entries: &[IrqDispatchEntry],
) -> Result<(), IrqRouteError> {
    for (index, entry) in entries.iter().enumerate() {
        let bound = bound_devices
            .resolve_irq_context(entry.context)
            .map_err(IrqRouteError::InvalidBoundDevice)?;
        if !bound
            .irq_contexts
            .iter()
            .any(|candidate| core::ptr::eq(candidate, entry.context))
        {
            return Err(IrqRouteError::ContextNotOwned {
                device: entry.context.route.device,
                role: entry.context.route.resource.role,
            });
        }

        for previous in &entries[..index] {
            let current_route = entry.context.route;
            let previous_route = previous.context.route;
            if current_route.device == previous_route.device
                && current_route.resource.role == previous_route.resource.role
            {
                return Err(IrqRouteError::DuplicateRoute {
                    device: current_route.device,
                    role: current_route.resource.role,
                });
            }

            if current_route.resource.line == previous_route.resource.line
                && (current_route.resource.sharing != IrqSharing::Shared
                    || previous_route.resource.sharing != IrqSharing::Shared)
            {
                return Err(IrqRouteError::ExclusiveConflict {
                    line: current_route.resource.line,
                    first_device: previous_route.device,
                    first_role: previous_route.resource.role,
                    second_device: current_route.device,
                    second_role: current_route.resource.role,
                });
            }
        }
    }
    Ok(())
}

fn canonicalize(entries: &mut [IrqDispatchEntry]) {
    entries.sort_unstable_by(|left, right| {
        compare_device_id(left.context.route.device, right.context.route.device)
            .then_with(|| {
                compare_role(
                    left.context.route.resource.role,
                    right.context.route.resource.role,
                )
            })
            .then_with(|| {
                left.context
                    .route
                    .resource
                    .line
                    .cmp(&right.context.route.resource.line)
            })
    });
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
        DeviceLocalId, IrqPolarity, IrqResource, IrqTrigger, ResourceOrigin, ResourceOriginKind,
        ResourceProviderId,
    };
    use tx_subsystems::device_binding::{
        BoundDevice, BoundDeviceIndexBuilder, BoundDeviceKey, BoundDeviceRegistration, DriverId,
        IrqRoute,
    };

    const PROVIDER: ResourceProviderId = ResourceProviderId("irq-test");
    const ORIGIN: ResourceOrigin = ResourceOrigin {
        provider: PROVIDER,
        record: "irq fixture",
        kind: ResourceOriginKind::PlatformStatic,
    };
    const TEST_MAX_IRQ: u32 = 64;
    static SHARED_DISPATCH_TRACE: AtomicUsize = AtomicUsize::new(0);
    static DEFERRED_DISPATCH_TRACE: AtomicUsize = AtomicUsize::new(0);

    const fn device_id(local: &'static str) -> DeviceId {
        DeviceId {
            provider: PROVIDER,
            local: DeviceLocalId::PlatformKey(local),
        }
    }

    const fn context(
        device: DeviceId,
        bound: u16,
        role: &'static str,
        line: u32,
        sharing: IrqSharing,
    ) -> DeviceIrqContext {
        DeviceIrqContext {
            route: IrqRoute {
                device,
                resource: IrqResource {
                    role: ResourceRole::Named(role),
                    line,
                    trigger: IrqTrigger::Level,
                    polarity: IrqPolarity::High,
                    sharing,
                    origin: ORIGIN,
                },
            },
            bound: BoundDeviceKey(bound),
        }
    }

    static CONTEXT_A: DeviceIrqContext = context(device_id("a"), 0, "rx", 7, IrqSharing::Shared);
    static CONTEXT_B: DeviceIrqContext = context(device_id("b"), 1, "rx", 7, IrqSharing::Shared);
    static CONTEXT_C: DeviceIrqContext =
        context(device_id("c"), 2, "event", 11, IrqSharing::Exclusive);
    static CONTEXT_A_EXCLUSIVE: DeviceIrqContext =
        context(device_id("a"), 0, "exclusive", 7, IrqSharing::Exclusive);
    static CONTEXT_WRONG_IDENTITY: DeviceIrqContext =
        context(device_id("b"), 0, "wrong", 13, IrqSharing::Exclusive);
    static CONTEXT_NOT_OWNED: DeviceIrqContext =
        context(device_id("a"), 0, "not-owned", 17, IrqSharing::Exclusive);
    static CONTEXT_ZERO: DeviceIrqContext =
        context(device_id("a"), 0, "zero", 0, IrqSharing::Exclusive);
    static CONTEXT_HUGE: DeviceIrqContext =
        context(device_id("a"), 0, "huge", u32::MAX, IrqSharing::Exclusive);

    static DEVICE_A_CONTEXTS: &[DeviceIrqContext] = &[CONTEXT_A, CONTEXT_A_EXCLUSIVE];
    static DEVICE_B_CONTEXTS: &[DeviceIrqContext] = &[CONTEXT_B];
    static DEVICE_C_CONTEXTS: &[DeviceIrqContext] = &[CONTEXT_C];

    static DEVICE_A: BoundDevice = BoundDevice {
        key: BoundDeviceKey(0),
        device_id: device_id("a"),
        driver_id: DriverId("driver-a"),
        registration: BoundDeviceRegistration::Controller,
        irq_contexts: DEVICE_A_CONTEXTS,
    };
    static DEVICE_B: BoundDevice = BoundDevice {
        key: BoundDeviceKey(1),
        device_id: device_id("b"),
        driver_id: DriverId("driver-b"),
        registration: BoundDeviceRegistration::Controller,
        irq_contexts: DEVICE_B_CONTEXTS,
    };
    static DEVICE_C: BoundDevice = BoundDevice {
        key: BoundDeviceKey(2),
        device_id: device_id("c"),
        driver_id: DriverId("driver-c"),
        registration: BoundDeviceRegistration::Controller,
        irq_contexts: DEVICE_C_CONTEXTS,
    };

    fn bound_index(
        devices: &[&'static BoundDevice],
    ) -> &'static tx_subsystems::device_binding::BoundDeviceIndex {
        let mut builder = BoundDeviceIndexBuilder::new();
        for device in devices {
            builder.try_push(device).expect("reserve bound device");
        }
        builder.freeze().expect("freeze bound devices")
    }

    fn handled(_context: &'static DeviceIrqContext, _line: u32) -> IrqHandled {
        IrqHandled::Done
    }

    fn trace_shared(step: usize) {
        let previous = SHARED_DISPATCH_TRACE.load(AtomicOrdering::Relaxed);
        SHARED_DISPATCH_TRACE.store(previous * 10 + step, AtomicOrdering::Relaxed);
    }

    fn shared_a_not_mine(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("a"));
        assert_eq!(line, 7);
        trace_shared(1);
        IrqHandled::NotMine
    }

    fn shared_b_done(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("b"));
        assert_eq!(line, 7);
        trace_shared(2);
        IrqHandled::Done
    }

    fn shared_b_wake(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("b"));
        assert_eq!(line, 7);
        trace_shared(2);
        IrqHandled::Wake
    }

    fn trace_deferred(step: usize) {
        let previous = DEFERRED_DISPATCH_TRACE.load(AtomicOrdering::Relaxed);
        DEFERRED_DISPATCH_TRACE.store(previous * 10 + step, AtomicOrdering::Relaxed);
    }

    fn shared_a_deferred(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("a"));
        assert_eq!(line, 7);
        trace_deferred(1);
        IrqHandled::DeferredWake
    }

    fn shared_a_wake(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("a"));
        assert_eq!(line, 7);
        trace_deferred(1);
        IrqHandled::Wake
    }

    fn shared_b_deferred(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("b"));
        assert_eq!(line, 7);
        trace_deferred(2);
        IrqHandled::DeferredWake
    }

    fn shared_b_wake_for_deferred(context: &'static DeviceIrqContext, line: u32) -> IrqHandled {
        assert_eq!(context.route.device, device_id("b"));
        assert_eq!(line, 7);
        trace_deferred(2);
        IrqHandled::Wake
    }

    fn route_projection(table: &IrqDispatchTable) -> Vec<(u32, DeviceId, ResourceRole)> {
        table
            .entries
            .iter()
            .enumerate()
            .flat_map(|(line, bucket)| {
                bucket.handlers.iter().map(move |entry| {
                    (
                        line as u32,
                        entry.context.route.device,
                        entry.context.route.resource.role,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn zero_routes_freeze_to_empty_table() {
        let index = bound_index(&[]);
        let table = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ)
            .freeze()
            .unwrap();
        assert!(table.is_empty());
        assert!(table.bucket(0).is_none());
    }

    #[test]
    fn dispatch_absent_line_returns_not_mine() {
        let index = bound_index(&[&DEVICE_A]);
        let mut builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        builder
            .reserve_device(&DEVICE_A_CONTEXTS[0], handled)
            .unwrap();
        let table = builder.freeze().unwrap();

        assert_eq!(table.dispatch_irq(6), IrqHandled::NotMine);
        assert_eq!(table.dispatch_irq(TEST_MAX_IRQ), IrqHandled::NotMine);
    }

    #[test]
    fn dispatch_shared_handlers_aggregates_done_and_wake_in_canonical_order() {
        let index = bound_index(&[&DEVICE_A, &DEVICE_B]);

        SHARED_DISPATCH_TRACE.store(0, AtomicOrdering::Relaxed);
        let mut done_builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        done_builder
            .reserve_device(&DEVICE_B_CONTEXTS[0], shared_b_done)
            .unwrap();
        done_builder
            .reserve_device(&DEVICE_A_CONTEXTS[0], shared_a_not_mine)
            .unwrap();
        assert_eq!(
            done_builder.freeze().unwrap().dispatch_irq(7),
            IrqHandled::Done
        );
        assert_eq!(SHARED_DISPATCH_TRACE.load(AtomicOrdering::Relaxed), 12);

        SHARED_DISPATCH_TRACE.store(0, AtomicOrdering::Relaxed);
        let mut wake_builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        wake_builder
            .reserve_device(&DEVICE_B_CONTEXTS[0], shared_b_wake)
            .unwrap();
        wake_builder
            .reserve_device(&DEVICE_A_CONTEXTS[0], shared_a_not_mine)
            .unwrap();
        assert_eq!(
            wake_builder.freeze().unwrap().dispatch_irq(7),
            IrqHandled::Wake
        );
        assert_eq!(SHARED_DISPATCH_TRACE.load(AtomicOrdering::Relaxed), 12);
    }

    #[test]
    fn dispatch_deferred_wake_dominates_before_or_after_wake() {
        let index = bound_index(&[&DEVICE_A, &DEVICE_B]);

        DEFERRED_DISPATCH_TRACE.store(0, AtomicOrdering::Relaxed);
        let mut deferred_first = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        deferred_first
            .reserve_device(&DEVICE_B_CONTEXTS[0], shared_b_wake_for_deferred)
            .unwrap();
        deferred_first
            .reserve_device(&DEVICE_A_CONTEXTS[0], shared_a_deferred)
            .unwrap();
        assert_eq!(
            deferred_first.freeze().unwrap().dispatch_irq(7),
            IrqHandled::DeferredWake
        );
        assert_eq!(DEFERRED_DISPATCH_TRACE.load(AtomicOrdering::Relaxed), 12);

        DEFERRED_DISPATCH_TRACE.store(0, AtomicOrdering::Relaxed);
        let mut deferred_last = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        deferred_last
            .reserve_device(&DEVICE_B_CONTEXTS[0], shared_b_deferred)
            .unwrap();
        deferred_last
            .reserve_device(&DEVICE_A_CONTEXTS[0], shared_a_wake)
            .unwrap();
        assert_eq!(
            deferred_last.freeze().unwrap().dispatch_irq(7),
            IrqHandled::DeferredWake
        );
        assert_eq!(DEFERRED_DISPATCH_TRACE.load(AtomicOrdering::Relaxed), 12);
    }

    #[test]
    fn multiple_lines_and_shared_handlers_are_order_independent() {
        let index = bound_index(&[&DEVICE_C, &DEVICE_A, &DEVICE_B]);

        let mut forward = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        forward
            .reserve_device(&DEVICE_C_CONTEXTS[0], handled)
            .unwrap();
        forward
            .reserve_device(&DEVICE_A_CONTEXTS[0], handled)
            .unwrap();
        forward
            .reserve_device(&DEVICE_B_CONTEXTS[0], handled)
            .unwrap();
        let forward = forward.freeze().unwrap();

        let mut reverse = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        reverse
            .reserve_device(&DEVICE_B_CONTEXTS[0], handled)
            .unwrap();
        reverse
            .reserve_device(&DEVICE_A_CONTEXTS[0], handled)
            .unwrap();
        reverse
            .reserve_device(&DEVICE_C_CONTEXTS[0], handled)
            .unwrap();
        let reverse = reverse.freeze().unwrap();

        assert_eq!(forward.bucket(7).unwrap().handlers.len(), 2);
        assert_eq!(forward.bucket(11).unwrap().handlers.len(), 1);
        assert_eq!(route_projection(forward), route_projection(reverse));
    }

    #[test]
    fn duplicate_device_and_role_is_rejected() {
        let index = bound_index(&[&DEVICE_A]);
        let mut builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        builder
            .reserve_device(&DEVICE_A_CONTEXTS[0], handled)
            .unwrap();
        builder
            .reserve_device(&DEVICE_A_CONTEXTS[0], handled)
            .unwrap();
        assert_eq!(
            builder.freeze().unwrap_err(),
            IrqRouteError::DuplicateRoute {
                device: device_id("a"),
                role: ResourceRole::Named("rx"),
            }
        );
    }

    #[test]
    fn same_line_rejects_any_exclusive_route() {
        let index = bound_index(&[&DEVICE_A, &DEVICE_B]);
        let mut builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        builder
            .reserve_device(&DEVICE_B_CONTEXTS[0], handled)
            .unwrap();
        builder
            .reserve_device(&DEVICE_A_CONTEXTS[1], handled)
            .unwrap();
        assert!(matches!(
            builder.freeze(),
            Err(IrqRouteError::ExclusiveConflict { line: 7, .. })
        ));
    }

    #[test]
    fn route_context_must_resolve_to_the_same_device() {
        let index = bound_index(&[&DEVICE_A, &DEVICE_B]);
        let mut builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        builder
            .reserve_device(&CONTEXT_WRONG_IDENTITY, handled)
            .unwrap();
        assert!(matches!(
            builder.freeze(),
            Err(IrqRouteError::InvalidBoundDevice(
                BoundDeviceLookupError::DeviceMismatch { .. }
            ))
        ));
    }

    #[test]
    fn route_context_must_be_owned_by_the_bound_device() {
        let index = bound_index(&[&DEVICE_A]);
        let mut builder = KernelIrqTableBuilder::new(index, TEST_MAX_IRQ);
        builder.reserve_device(&CONTEXT_NOT_OWNED, handled).unwrap();
        assert_eq!(
            builder.freeze().unwrap_err(),
            IrqRouteError::ContextNotOwned {
                device: device_id("a"),
                role: ResourceRole::Named("not-owned"),
            }
        );
    }

    #[test]
    fn controller_range_is_validated_before_reservation() {
        let index = bound_index(&[&DEVICE_A]);

        let mut valid_last_line = KernelIrqTableBuilder::new(index, 8);
        assert_eq!(
            valid_last_line.reserve_device(&CONTEXT_ZERO, handled),
            Err(IrqRouteError::LineOutOfRange(0))
        );
        valid_last_line
            .reserve_device(&DEVICE_A_CONTEXTS[0], handled)
            .expect("exclusive upper bound keeps limit - 1 valid");
        assert_eq!(
            valid_last_line
                .freeze()
                .unwrap()
                .bucket(7)
                .unwrap()
                .handlers
                .len(),
            1
        );

        let mut at_limit = KernelIrqTableBuilder::new(index, 7);
        assert_eq!(
            at_limit.reserve_device(&DEVICE_A_CONTEXTS[0], handled),
            Err(IrqRouteError::LineOutOfRange(7))
        );
        assert_eq!(
            at_limit.reserve_device(&CONTEXT_HUGE, handled),
            Err(IrqRouteError::LineOutOfRange(u32::MAX))
        );
        assert!(at_limit.freeze().unwrap().is_empty());
    }
}
