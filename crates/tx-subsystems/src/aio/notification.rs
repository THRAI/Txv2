//! AIO notification meanings.
//!
//! This module owns the AIO context wait codes and semantic notify/release
//! verbs for iocb arrival and completion availability.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_points, notify_events_available_with_post, notify_iocb_arrived, release_wait_points,
};
pub use readiness::{EVENTS_AVAILABLE_MASK, IOCB_ARRIVED_MASK};

#[notification_adapter(
    subsystem = "aio",
    domain = "readiness",
    reason = "aio notification.rs owns iocb-arrived and events-available wait codes"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::aio::adapter::step_engine::{InterestMask, WaitSource};
    use crate::aio::adapter::wait_routing::{self, MailboxEvent, TaskMailbox};

    /// An iocb is queued and ready for the worker.
    pub const IOCB_ARRIVED_MASK: u64 = 0x1;
    /// A completion event is queued and ready for io_getevents.
    pub const EVENTS_AVAILABLE_MASK: u64 = 0x1;

    pub(crate) struct AioWaitPoints {
        iocb_arrived_id: u64,
        iocb_arrived: Arc<WaitSource>,
        events_available_id: u64,
        events_available: Arc<WaitSource>,
    }

    impl AioWaitPoints {
        pub(crate) fn iocb_arrived_endpoint(&self) -> &Arc<WaitSource> {
            &self.iocb_arrived
        }

        pub(crate) fn events_available_endpoint(&self) -> &Arc<WaitSource> {
            &self.events_available
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>, u64, Arc<WaitSource>) {
            (
                self.iocb_arrived_id,
                self.iocb_arrived,
                self.events_available_id,
                self.events_available,
            )
        }
    }

    pub(crate) fn new_wait_points() -> AioWaitPoints {
        let iocb_arrived_id = crate::allocate_notification_source_id();
        let iocb_arrived = wait_routing::new_wait_source(iocb_arrived_id);
        let events_available_id = crate::allocate_notification_source_id();
        let events_available = wait_routing::new_wait_source(events_available_id);
        AioWaitPoints {
            iocb_arrived_id,
            iocb_arrived,
            events_available_id,
            events_available,
        }
    }

    pub(crate) fn notify_iocb_arrived(source: &Arc<WaitSource>) {
        source.notify_emit(InterestMask::new(IOCB_ARRIVED_MASK));
    }

    pub(crate) fn notify_events_available_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, EVENTS_AVAILABLE_MASK, post);
    }

    pub(crate) fn release_wait_points(iocb_arrived_id: u64, events_available_id: u64) {
        wait_routing::unregister_source(iocb_arrived_id);
        wait_routing::unregister_source(events_available_id);
    }
}
