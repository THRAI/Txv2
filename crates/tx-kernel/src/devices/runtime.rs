//! One-shot publication of the boot-frozen device runtime.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

use super::binder::DeviceBindOutcome;

const DEVICE_RUNTIME_VACANT: u8 = 0;
const DEVICE_RUNTIME_RESERVED: u8 = 1;
const DEVICE_RUNTIME_PUBLISHED: u8 = 2;

static DEVICE_RUNTIME_STATE: AtomicU8 = AtomicU8::new(DEVICE_RUNTIME_VACANT);

struct DeviceRuntimeSlot(UnsafeCell<MaybeUninit<DeviceRuntimeSnapshot>>);

impl DeviceRuntimeSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(MaybeUninit::uninit()))
    }
}

// SAFETY: writers are excluded by DEVICE_RUNTIME_STATE's one-shot reservation.
// A reader only dereferences the cell after an Acquire load observes the
// Release transition to PUBLISHED, after which the value is immutable.
unsafe impl Sync for DeviceRuntimeSlot {}

static DEVICE_RUNTIME: DeviceRuntimeSlot = DeviceRuntimeSlot::new();

/// Immutable boot-time device graph, binding report, index, and IRQ table.
#[derive(Clone, Copy, Debug)]
pub struct DeviceRuntimeSnapshot {
    pub outcome: DeviceBindOutcome,
}

/// Failure to reserve the one-shot device-runtime publication slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceRuntimePublishError {
    AlreadyReservedOrPublished,
}

/// Private reservation for one device-runtime publication.
///
/// Dropping an uncommitted reservation restores the vacant state, allowing a
/// larger boot transaction to roll back before any runtime snapshot is visible.
#[must_use = "a prepared device runtime must be committed or dropped"]
pub struct PreparedDeviceRuntime {
    snapshot: DeviceRuntimeSnapshot,
    committed: bool,
}

impl PreparedDeviceRuntime {
    /// Publish the prepared snapshot exactly once.
    ///
    /// Preparation owns the only writer reservation. This step only copies a
    /// fixed-size value and performs the final Release publication; it has no
    /// allocation or failure path.
    pub fn commit(mut self) {
        unsafe {
            (*DEVICE_RUNTIME.0.get()).write(self.snapshot);
        }
        DEVICE_RUNTIME_STATE.store(DEVICE_RUNTIME_PUBLISHED, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for PreparedDeviceRuntime {
    fn drop(&mut self) {
        if !self.committed {
            let _ = DEVICE_RUNTIME_STATE.compare_exchange(
                DEVICE_RUNTIME_RESERVED,
                DEVICE_RUNTIME_VACANT,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

/// Reserve the one-shot publication slot without making `outcome` visible.
pub fn prepare_device_runtime(
    outcome: DeviceBindOutcome,
) -> Result<PreparedDeviceRuntime, DeviceRuntimePublishError> {
    if DEVICE_RUNTIME_STATE
        .compare_exchange(
            DEVICE_RUNTIME_VACANT,
            DEVICE_RUNTIME_RESERVED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return Err(DeviceRuntimePublishError::AlreadyReservedOrPublished);
    }

    Ok(PreparedDeviceRuntime {
        snapshot: DeviceRuntimeSnapshot { outcome },
        committed: false,
    })
}

/// Return the published device runtime, if boot has committed one.
pub fn device_runtime_snapshot() -> Option<&'static DeviceRuntimeSnapshot> {
    if DEVICE_RUNTIME_STATE.load(Ordering::Acquire) != DEVICE_RUNTIME_PUBLISHED {
        return None;
    }

    unsafe { Some((&*DEVICE_RUNTIME.0.get()).assume_init_ref()) }
}

/// Clear the one-shot slot between host tests.
#[cfg(test)]
pub(crate) fn reset_device_runtime_for_test() {
    DEVICE_RUNTIME_STATE.store(DEVICE_RUNTIME_VACANT, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::binder::DeviceBindReport;
    use crate::irq::device::KernelIrqTableBuilder;
    use tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH;
    use tx_subsystems::device_binding::BoundDeviceIndexBuilder;

    static EMPTY_REPORT: DeviceBindReport = DeviceBindReport {
        bound: &[],
        unsupported: &[],
        failed: &[],
    };

    fn empty_outcome() -> DeviceBindOutcome {
        let bound_devices = BoundDeviceIndexBuilder::new()
            .freeze()
            .expect("empty bound-device index");
        let irq_table = KernelIrqTableBuilder::new(bound_devices, 1)
            .freeze()
            .expect("empty IRQ table");
        DeviceBindOutcome {
            graph: &EMPTY_DEVICE_RESOURCE_GRAPH,
            report: &EMPTY_REPORT,
            bound_devices,
            irq_table,
            activations: &[],
        }
    }

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let lock = crate::test_serialise::KERNEL_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_device_runtime_for_test();
        lock
    }

    #[test]
    fn prepared_runtime_is_private_and_drop_releases_reservation() {
        let _lock = setup();
        let outcome = empty_outcome();

        let prepared = prepare_device_runtime(outcome).expect("first reservation");
        assert!(device_runtime_snapshot().is_none());
        assert_eq!(
            prepare_device_runtime(outcome).err(),
            Some(DeviceRuntimePublishError::AlreadyReservedOrPublished)
        );

        drop(prepared);
        assert!(device_runtime_snapshot().is_none());

        prepare_device_runtime(outcome)
            .expect("dropped reservation is retryable")
            .commit();
        let published = device_runtime_snapshot().expect("committed runtime is visible");
        assert!(core::ptr::eq(published.outcome.graph, outcome.graph));
        assert!(core::ptr::eq(published.outcome.report, outcome.report));
        assert!(core::ptr::eq(
            published.outcome.bound_devices,
            outcome.bound_devices
        ));
        assert!(core::ptr::eq(
            published.outcome.irq_table,
            outcome.irq_table
        ));
    }

    #[test]
    fn published_runtime_rejects_a_second_publication() {
        let _lock = setup();
        let outcome = empty_outcome();

        prepare_device_runtime(outcome)
            .expect("first reservation")
            .commit();

        assert_eq!(
            prepare_device_runtime(outcome).err(),
            Some(DeviceRuntimePublishError::AlreadyReservedOrPublished)
        );
        assert!(device_runtime_snapshot().is_some());
    }
}
