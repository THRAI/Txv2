//! AIO notification meanings.
//!
//! This module owns the AIO context wait codes and semantic notify/release
//! verbs for iocb arrival and completion availability.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_points, notify_events_available, notify_iocb_arrived, notify_ring_space_available,
    release_wait_points,
};
pub use readiness::{EVENTS_AVAILABLE_MASK, IOCB_ARRIVED_MASK, RING_SPACE_AVAILABLE_MASK};

#[notification_adapter(
    subsystem = "aio",
    domain = "readiness",
    reason = "aio notification.rs owns iocb-arrived and events-available wait codes"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::aio::adapter::step_engine::{InterestMask, WaitSource};
    use crate::aio::adapter::wait_routing;

    /// An iocb is queued and ready for the worker.
    pub const IOCB_ARRIVED_MASK: u64 = 0x1;
    /// A completion event is queued and ready for io_getevents.
    pub const EVENTS_AVAILABLE_MASK: u64 = 0x1;
    /// The user-visible ring has space after io_getevents advanced head.
    pub const RING_SPACE_AVAILABLE_MASK: u64 = 0x1;

    pub(crate) struct AioWaitPoints {
        pub(crate) iocb_arrived_id: u64,
        pub(crate) iocb_arrived: Arc<WaitSource>,
        pub(crate) events_available_id: u64,
        pub(crate) events_available: Arc<WaitSource>,
        pub(crate) ring_space_available_id: u64,
        pub(crate) ring_space_available: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_points() -> AioWaitPoints {
        let iocb_arrived_id = crate::allocate_notification_source_id();
        let iocb_arrived = wait_routing::new_wait_source(iocb_arrived_id);
        let events_available_id = crate::allocate_notification_source_id();
        let events_available = wait_routing::new_wait_source(events_available_id);
        let ring_space_available_id = crate::allocate_notification_source_id();
        let ring_space_available = wait_routing::new_wait_source(ring_space_available_id);
        AioWaitPoints {
            iocb_arrived_id,
            iocb_arrived,
            events_available_id,
            events_available,
            ring_space_available_id,
            ring_space_available,
        }
    }

    pub(crate) fn notify_iocb_arrived(source: &Arc<WaitSource>) {
        source.notify_emit(InterestMask::new(IOCB_ARRIVED_MASK));
    }

    pub(crate) fn notify_events_available(source: &Arc<WaitSource>) {
        source.notify_emit(InterestMask::new(EVENTS_AVAILABLE_MASK));
    }

    pub(crate) fn notify_ring_space_available(source: &Arc<WaitSource>) {
        source.notify_emit(InterestMask::new(RING_SPACE_AVAILABLE_MASK));
    }

    pub(crate) fn release_wait_points(
        iocb_arrived_id: u64,
        events_available_id: u64,
        ring_space_available_id: u64,
    ) {
        wait_routing::unregister_source(iocb_arrived_id);
        wait_routing::unregister_source(events_available_id);
        wait_routing::unregister_source(ring_space_available_id);
    }
}
