//! io_uring notification meanings.
//!
//! This module owns the SQE-arrived and CQE-available wait codes and semantic
//! notify/release verbs for the io_uring scaffold.

use tx_platform_adapter::notification_adapter;

pub use readiness::CQE_AVAILABLE;
pub(crate) use readiness::{
    new_wait_points, notify_cqe_available_with_post, notify_sqe_arrived, release_wait_points,
};

#[notification_adapter(
    subsystem = "io_uring",
    domain = "readiness",
    reason = "io_uring notification.rs owns SQE-arrived and CQE-available wait codes"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::io_uring::adapter::step_engine::{InterestMask, WaitSource};
    use crate::io_uring::adapter::wait_routing::{self, MailboxEvent, TaskMailbox};

    /// An SQE is queued and ready for the SQPOLL worker.
    const SQE_ARRIVED: u64 = 0x1;
    /// A CQE is queued and available for completion consumers.
    pub const CQE_AVAILABLE: u64 = 0x1;

    pub(crate) struct IoUringWaitPoints {
        sqe_arrived_id: u64,
        sqe_arrived: Arc<WaitSource>,
        cqe_available_id: u64,
        cqe_available: Arc<WaitSource>,
    }

    impl IoUringWaitPoints {
        pub(crate) fn sqe_arrived_endpoint(&self) -> &Arc<WaitSource> {
            &self.sqe_arrived
        }

        pub(crate) fn cqe_available_endpoint(&self) -> &Arc<WaitSource> {
            &self.cqe_available
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>, u64, Arc<WaitSource>) {
            (
                self.sqe_arrived_id,
                self.sqe_arrived,
                self.cqe_available_id,
                self.cqe_available,
            )
        }
    }

    pub(crate) fn new_wait_points() -> IoUringWaitPoints {
        let sqe_arrived_id = crate::allocate_notification_source_id();
        let sqe_arrived = wait_routing::new_wait_source(sqe_arrived_id);
        let cqe_available_id = crate::allocate_notification_source_id();
        let cqe_available = wait_routing::new_wait_source(cqe_available_id);
        IoUringWaitPoints {
            sqe_arrived_id,
            sqe_arrived,
            cqe_available_id,
            cqe_available,
        }
    }

    pub(crate) fn notify_sqe_arrived(source: &Arc<WaitSource>) {
        source.notify_emit(InterestMask::new(SQE_ARRIVED));
    }

    pub(crate) fn notify_cqe_available_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, CQE_AVAILABLE, post);
    }

    pub(crate) fn release_wait_points(sqe_arrived_id: u64, cqe_available_id: u64) {
        wait_routing::unregister_source(sqe_arrived_id);
        wait_routing::unregister_source(cqe_available_id);
    }
}
