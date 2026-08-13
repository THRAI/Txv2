//! Boot-frozen device bindings and hardware-identity lookup.
//!
//! These values describe tier-2 devices whose resources were selected during
//! the one-shot boot binder.  They are immutable after publication and are not
//! zone entities: a `BoundDeviceKey` is only an O(1) slot key into the frozen
//! index, while `DeviceId` remains the authoritative hardware identity.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tx_hal::{DeviceId, IrqResource};

use crate::device::{BlockDeviceRegistration, CharDeviceBinding};
use crate::net::device::NetDeviceRegistration;

/// Stable identity of a statically linked driver descriptor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DriverId(pub &'static str);

/// O(1) slot key into a [`BoundDeviceIndex`].
///
/// The key is an internal boot-binding value, not a hardware identity and not
/// a namespace projection.  Every lookup that starts from a route rechecks the
/// slot's [`DeviceId`] before returning the bound device.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BoundDeviceKey(pub u16);

/// Class-specific registration published by one successful tier-2 binding.
#[derive(Clone, Copy)]
pub enum BoundDeviceRegistration {
    Char(&'static CharDeviceBinding),
    Block(&'static BlockDeviceRegistration),
    Net(&'static NetDeviceRegistration),
    Controller,
}

impl core::fmt::Debug for BoundDeviceRegistration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Char(binding) => f.debug_tuple("Char").field(&binding.devt).finish(),
            Self::Block(registration) => f.debug_tuple("Block").field(&registration.devt).finish(),
            Self::Net(registration) => f.debug_tuple("Net").field(&registration.devt).finish(),
            Self::Controller => f.write_str("Controller"),
        }
    }
}

/// One device-local interrupt route retained from resource decoding onward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqRoute {
    pub device: DeviceId,
    pub resource: IrqResource,
}

/// Immutable context carried by a device IRQ dispatch entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceIrqContext {
    pub route: IrqRoute,
    pub bound: BoundDeviceKey,
}

/// A successful tier-2 binding.
pub struct BoundDevice {
    pub key: BoundDeviceKey,
    pub device_id: DeviceId,
    pub driver_id: DriverId,
    pub registration: BoundDeviceRegistration,
    pub irq_contexts: &'static [DeviceIrqContext],
}

impl core::fmt::Debug for BoundDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundDevice")
            .field("key", &self.key)
            .field("device_id", &self.device_id)
            .field("driver_id", &self.driver_id)
            .field("registration", &self.registration)
            .field("irq_contexts", &self.irq_contexts)
            .finish()
    }
}

/// Validation or allocation failure while freezing the bound-device index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundDeviceIndexError {
    AllocationFailed,
    DeviceCapacityExceeded {
        capacity: usize,
        required: usize,
    },
    SlotCapacityExceeded {
        slots: usize,
        key: BoundDeviceKey,
    },
    DuplicateKey(BoundDeviceKey),
    DuplicateDeviceId(DeviceId),
    IrqContextKeyMismatch {
        device: DeviceId,
        expected: BoundDeviceKey,
        actual: BoundDeviceKey,
    },
    IrqContextDeviceMismatch {
        bound: BoundDeviceKey,
        expected: DeviceId,
        actual: DeviceId,
    },
}

/// Typed result of resolving an IRQ context through the frozen index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundDeviceLookupError {
    UnknownKey(BoundDeviceKey),
    DeviceMismatch {
        key: BoundDeviceKey,
        expected: DeviceId,
        actual: DeviceId,
    },
}

/// Private-until-freeze builder used by the one-shot binder.
#[derive(Default)]
pub struct BoundDeviceIndexBuilder {
    devices: Vec<&'static BoundDevice>,
    pre_reserved: Option<PreReservedIndexLayout>,
}

struct PreReservedIndexLayout {
    device_capacity: usize,
    slots: Vec<Option<&'static BoundDevice>>,
    index_storage: Vec<BoundDeviceIndex>,
}

impl BoundDeviceIndexBuilder {
    pub const fn new() -> Self {
        Self {
            devices: Vec::new(),
            pre_reserved: None,
        }
    }

    /// Allocate the complete device and slot layout before any bound device is
    /// leaked into the builder.
    ///
    /// Within the declared capacities, [`Self::try_push`] and [`Self::freeze`]
    /// perform no further allocation. `slot_count` is the exclusive upper bound
    /// for every accepted [`BoundDeviceKey`].
    pub fn try_with_capacity(
        device_capacity: usize,
        slot_count: usize,
    ) -> Result<Self, BoundDeviceIndexError> {
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(device_capacity)
            .map_err(|_| BoundDeviceIndexError::AllocationFailed)?;

        let mut slots = Vec::new();
        slots
            .try_reserve_exact(slot_count)
            .map_err(|_| BoundDeviceIndexError::AllocationFailed)?;
        slots.resize(slot_count, None);

        let mut index_storage = Vec::new();
        index_storage
            .try_reserve_exact(1)
            .map_err(|_| BoundDeviceIndexError::AllocationFailed)?;

        Ok(Self {
            devices,
            pre_reserved: Some(PreReservedIndexLayout {
                device_capacity,
                slots,
                index_storage,
            }),
        })
    }

    /// Add one already-prepared binding without publishing it.
    pub fn try_push(&mut self, device: &'static BoundDevice) -> Result<(), BoundDeviceIndexError> {
        if let Some(layout) = &self.pre_reserved {
            let required = self.devices.len().saturating_add(1);
            if required > layout.device_capacity {
                return Err(BoundDeviceIndexError::DeviceCapacityExceeded {
                    capacity: layout.device_capacity,
                    required,
                });
            }
            if usize::from(device.key.0) >= layout.slots.len() {
                return Err(BoundDeviceIndexError::SlotCapacityExceeded {
                    slots: layout.slots.len(),
                    key: device.key,
                });
            }
        } else {
            self.devices
                .try_reserve(1)
                .map_err(|_| BoundDeviceIndexError::AllocationFailed)?;
        }
        self.devices.push(device);
        Ok(())
    }

    /// Validate every key, identity, and IRQ context before leaking one final
    /// immutable index.  A failed freeze leaks and publishes nothing.
    pub fn freeze(mut self) -> Result<&'static BoundDeviceIndex, BoundDeviceIndexError> {
        validate_devices(&self.devices)?;

        if let Some(mut layout) = self.pre_reserved.take() {
            let len = self.devices.len();
            for device in self.devices {
                layout.slots[usize::from(device.key.0)] = Some(device);
            }
            let slots = Vec::leak(layout.slots);
            layout.index_storage.push(BoundDeviceIndex { slots, len });
            return Ok(&Vec::leak(layout.index_storage)[0]);
        }

        let slot_count = self
            .devices
            .iter()
            .map(|device| usize::from(device.key.0) + 1)
            .max()
            .unwrap_or(0);
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(slot_count)
            .map_err(|_| BoundDeviceIndexError::AllocationFailed)?;
        slots.resize(slot_count, None);
        for device in self.devices {
            slots[usize::from(device.key.0)] = Some(device);
        }

        let slots = Box::leak(slots.into_boxed_slice());
        Ok(Box::leak(Box::new(BoundDeviceIndex {
            slots,
            len: slots.iter().flatten().count(),
        })))
    }
}

fn validate_devices(devices: &[&'static BoundDevice]) -> Result<(), BoundDeviceIndexError> {
    for (idx, device) in devices.iter().copied().enumerate() {
        for previous in devices[..idx].iter().copied() {
            if previous.key == device.key {
                return Err(BoundDeviceIndexError::DuplicateKey(device.key));
            }
            if previous.device_id == device.device_id {
                return Err(BoundDeviceIndexError::DuplicateDeviceId(device.device_id));
            }
        }

        for context in device.irq_contexts {
            if context.bound != device.key {
                return Err(BoundDeviceIndexError::IrqContextKeyMismatch {
                    device: device.device_id,
                    expected: device.key,
                    actual: context.bound,
                });
            }
            if context.route.device != device.device_id {
                return Err(BoundDeviceIndexError::IrqContextDeviceMismatch {
                    bound: device.key,
                    expected: device.device_id,
                    actual: context.route.device,
                });
            }
        }
    }
    Ok(())
}

/// Immutable O(1) lookup table published by the boot binder.
#[derive(Debug)]
pub struct BoundDeviceIndex {
    slots: &'static [Option<&'static BoundDevice>],
    len: usize,
}

impl BoundDeviceIndex {
    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, key: BoundDeviceKey) -> Option<&'static BoundDevice> {
        self.slots.get(usize::from(key.0)).copied().flatten()
    }

    /// Resolve `key` and verify that it still denotes the route's hardware
    /// identity.  Callers must not trust a slot key without this comparison.
    pub fn resolve(
        &self,
        key: BoundDeviceKey,
        expected: DeviceId,
    ) -> Result<&'static BoundDevice, BoundDeviceLookupError> {
        let device = self
            .get(key)
            .ok_or(BoundDeviceLookupError::UnknownKey(key))?;
        if device.device_id != expected {
            return Err(BoundDeviceLookupError::DeviceMismatch {
                key,
                expected,
                actual: device.device_id,
            });
        }
        Ok(device)
    }

    /// Resolve the immutable bound device named by an IRQ context.  This is
    /// the preferred hardware-dispatch lookup because the expected identity is
    /// taken directly from the route rather than supplied independently.
    pub fn resolve_irq_context(
        &self,
        context: &DeviceIrqContext,
    ) -> Result<&'static BoundDevice, BoundDeviceLookupError> {
        self.resolve(context.bound, context.route.device)
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static BoundDevice> + '_ {
        self.slots.iter().filter_map(|entry| *entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_hal::{
        DeviceLocalId, IrqPolarity, IrqSharing, IrqTrigger, ResourceOrigin, ResourceOriginKind,
        ResourceProviderId, ResourceRole,
    };

    const PROVIDER: ResourceProviderId = ResourceProviderId("test-provider");
    const ORIGIN: ResourceOrigin = ResourceOrigin {
        provider: PROVIDER,
        record: "fixture",
        kind: ResourceOriginKind::PlatformStatic,
    };

    const fn device_id(local: &'static str) -> DeviceId {
        DeviceId {
            provider: PROVIDER,
            local: DeviceLocalId::PlatformKey(local),
        }
    }

    const fn irq_context(device: DeviceId, key: BoundDeviceKey, line: u32) -> DeviceIrqContext {
        DeviceIrqContext {
            route: IrqRoute {
                device,
                resource: IrqResource {
                    role: ResourceRole::Named("rx-tx"),
                    line,
                    trigger: IrqTrigger::Level,
                    polarity: IrqPolarity::High,
                    sharing: IrqSharing::Exclusive,
                    origin: ORIGIN,
                },
            },
            bound: key,
        }
    }

    static DEVICE_A_IRQS: &[DeviceIrqContext] =
        &[irq_context(device_id("a"), BoundDeviceKey(1), 17)];
    static DEVICE_B_IRQS: &[DeviceIrqContext] =
        &[irq_context(device_id("b"), BoundDeviceKey(0), 19)];
    static DEVICE_A: BoundDevice = BoundDevice {
        key: BoundDeviceKey(1),
        device_id: device_id("a"),
        driver_id: DriverId("driver-a"),
        registration: BoundDeviceRegistration::Controller,
        irq_contexts: DEVICE_A_IRQS,
    };
    static DEVICE_B: BoundDevice = BoundDevice {
        key: BoundDeviceKey(0),
        device_id: device_id("b"),
        driver_id: DriverId("driver-b"),
        registration: BoundDeviceRegistration::Controller,
        irq_contexts: DEVICE_B_IRQS,
    };

    fn freeze(devices: &[&'static BoundDevice]) -> &'static BoundDeviceIndex {
        let mut builder = BoundDeviceIndexBuilder::new();
        for device in devices {
            builder.try_push(device).expect("builder push");
        }
        builder.freeze().expect("index freeze")
    }

    #[test]
    fn empty_index_is_valid() {
        let index = BoundDeviceIndexBuilder::new()
            .freeze()
            .expect("empty index");
        assert!(index.is_empty());
        assert_eq!(index.iter().count(), 0);
        assert_eq!(
            index
                .resolve(BoundDeviceKey(0), device_id("missing"))
                .unwrap_err(),
            BoundDeviceLookupError::UnknownKey(BoundDeviceKey(0))
        );
    }

    #[test]
    fn multi_device_lookup_is_independent_of_input_order() {
        let forward = freeze(&[&DEVICE_A, &DEVICE_B]);
        let reversed = freeze(&[&DEVICE_B, &DEVICE_A]);

        for index in [forward, reversed] {
            assert_eq!(index.len(), 2);
            assert!(core::ptr::eq(
                index
                    .resolve(BoundDeviceKey(0), DEVICE_B.device_id)
                    .expect("device b"),
                &DEVICE_B
            ));
            assert!(core::ptr::eq(
                index
                    .resolve(BoundDeviceKey(1), DEVICE_A.device_id)
                    .expect("device a"),
                &DEVICE_A
            ));
            assert!(core::ptr::eq(
                index
                    .resolve_irq_context(&DEVICE_A_IRQS[0])
                    .expect("device a irq context"),
                &DEVICE_A
            ));
        }
    }

    #[test]
    fn pre_reserved_layout_freezes_with_declared_device_and_slot_capacity() {
        let mut builder =
            BoundDeviceIndexBuilder::try_with_capacity(2, 2).expect("pre-reserved layout");
        builder.try_push(&DEVICE_A).expect("device a");
        builder.try_push(&DEVICE_B).expect("device b");

        let index = builder.freeze().expect("pre-reserved index freeze");
        assert_eq!(index.len(), 2);
        assert!(core::ptr::eq(
            index.get(BoundDeviceKey(0)).expect("device b"),
            &DEVICE_B
        ));
        assert!(core::ptr::eq(
            index.get(BoundDeviceKey(1)).expect("device a"),
            &DEVICE_A
        ));
    }

    #[test]
    fn pre_reserved_capacity_errors_do_not_consume_builder_capacity() {
        static OUT_OF_LAYOUT: BoundDevice = BoundDevice {
            key: BoundDeviceKey(2),
            device_id: device_id("out-of-layout"),
            driver_id: DriverId("driver-out-of-layout"),
            registration: BoundDeviceRegistration::Controller,
            irq_contexts: &[],
        };
        let mut builder =
            BoundDeviceIndexBuilder::try_with_capacity(1, 2).expect("pre-reserved layout");

        assert_eq!(
            builder.try_push(&OUT_OF_LAYOUT),
            Err(BoundDeviceIndexError::SlotCapacityExceeded {
                slots: 2,
                key: BoundDeviceKey(2),
            })
        );
        builder
            .try_push(&DEVICE_B)
            .expect("retry with in-layout device");
        assert_eq!(
            builder.try_push(&DEVICE_A),
            Err(BoundDeviceIndexError::DeviceCapacityExceeded {
                capacity: 1,
                required: 2,
            })
        );

        let index = builder.freeze().expect("freeze after rejected pushes");
        assert_eq!(index.len(), 1);
        assert!(core::ptr::eq(
            index.get(BoundDeviceKey(0)).expect("device b"),
            &DEVICE_B
        ));
    }

    #[test]
    fn lookup_rechecks_device_identity() {
        let index = freeze(&[&DEVICE_B]);
        assert_eq!(
            index
                .resolve(BoundDeviceKey(0), DEVICE_A.device_id)
                .unwrap_err(),
            BoundDeviceLookupError::DeviceMismatch {
                key: BoundDeviceKey(0),
                expected: DEVICE_A.device_id,
                actual: DEVICE_B.device_id,
            }
        );
    }

    #[test]
    fn duplicate_key_is_rejected_before_freeze() {
        static DUPLICATE_KEY: BoundDevice = BoundDevice {
            key: BoundDeviceKey(0),
            device_id: device_id("duplicate-key"),
            driver_id: DriverId("driver-duplicate-key"),
            registration: BoundDeviceRegistration::Controller,
            irq_contexts: &[],
        };
        let mut builder = BoundDeviceIndexBuilder::new();
        builder.try_push(&DEVICE_B).expect("first push");
        builder.try_push(&DUPLICATE_KEY).expect("second push");
        assert_eq!(
            builder.freeze().unwrap_err(),
            BoundDeviceIndexError::DuplicateKey(BoundDeviceKey(0))
        );
    }

    #[test]
    fn duplicate_device_id_is_rejected_before_freeze() {
        static DUPLICATE_ID: BoundDevice = BoundDevice {
            key: BoundDeviceKey(2),
            device_id: device_id("b"),
            driver_id: DriverId("driver-duplicate-id"),
            registration: BoundDeviceRegistration::Controller,
            irq_contexts: &[],
        };
        let mut builder = BoundDeviceIndexBuilder::new();
        builder.try_push(&DEVICE_B).expect("first push");
        builder.try_push(&DUPLICATE_ID).expect("second push");
        assert_eq!(
            builder.freeze().unwrap_err(),
            BoundDeviceIndexError::DuplicateDeviceId(DEVICE_B.device_id)
        );
    }

    #[test]
    fn irq_context_must_match_bound_key_and_device() {
        static WRONG_KEY_IRQS: &[DeviceIrqContext] =
            &[irq_context(device_id("wrong-key"), BoundDeviceKey(4), 21)];
        static WRONG_KEY: BoundDevice = BoundDevice {
            key: BoundDeviceKey(3),
            device_id: device_id("wrong-key"),
            driver_id: DriverId("driver-wrong-key"),
            registration: BoundDeviceRegistration::Controller,
            irq_contexts: WRONG_KEY_IRQS,
        };
        let mut builder = BoundDeviceIndexBuilder::new();
        builder.try_push(&WRONG_KEY).expect("push wrong key");
        assert_eq!(
            builder.freeze().unwrap_err(),
            BoundDeviceIndexError::IrqContextKeyMismatch {
                device: WRONG_KEY.device_id,
                expected: BoundDeviceKey(3),
                actual: BoundDeviceKey(4),
            }
        );

        static WRONG_DEVICE_IRQS: &[DeviceIrqContext] =
            &[irq_context(device_id("other"), BoundDeviceKey(5), 23)];
        static WRONG_DEVICE: BoundDevice = BoundDevice {
            key: BoundDeviceKey(5),
            device_id: device_id("wrong-device"),
            driver_id: DriverId("driver-wrong-device"),
            registration: BoundDeviceRegistration::Controller,
            irq_contexts: WRONG_DEVICE_IRQS,
        };
        let mut builder = BoundDeviceIndexBuilder::new();
        builder.try_push(&WRONG_DEVICE).expect("push wrong device");
        assert_eq!(
            builder.freeze().unwrap_err(),
            BoundDeviceIndexError::IrqContextDeviceMismatch {
                bound: BoundDeviceKey(5),
                expected: WRONG_DEVICE.device_id,
                actual: device_id("other"),
            }
        );
    }
}
