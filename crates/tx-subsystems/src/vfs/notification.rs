//! VFS notification meanings.
//!
//! This module owns VFS-uniform read/write wait codes, per-RNode wait point
//! setup/teardown, and composite ppoll wait-source yield construction.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    RNodeWaitPoints, new_rnode_wait_points, notify_readable_with_post, notify_writable_with_post,
    ppoll_wait, release_rnode_wait_points,
};
pub use readiness::{VFS_READABLE, VFS_WRITABLE};

#[notification_adapter(
    subsystem = "vfs",
    domain = "readiness",
    reason = "vfs notification.rs owns RNode read/write readiness codes and wait-source yields"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::vfs::adapter::step_engine::{InterestMask, NoProgress, StepOutcome, WaitSourceId};
    use crate::vfs::adapter::wait_routing::{
        self, MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource,
    };

    /// Per-RNode wait-source interest mask: bytes are available to read.
    pub const VFS_READABLE: u64 = 0x1;
    /// Per-RNode wait-source interest mask: space is available to write.
    pub const VFS_WRITABLE: u64 = 0x2;

    pub(crate) struct RNodeWaitPoints {
        pub(crate) read_source_id: u64,
        pub(crate) read_source: Arc<WaitSource>,
        pub(crate) write_source_id: u64,
        pub(crate) write_source: Arc<WaitSource>,
    }

    pub(crate) fn new_rnode_wait_points() -> RNodeWaitPoints {
        let read_source_id = crate::allocate_notification_source_id();
        let read_source = wait_routing::new_wait_source(read_source_id);
        let write_source_id = crate::allocate_notification_source_id();
        let write_source = wait_routing::new_wait_source(write_source_id);
        crate::wait_source::register_wait_source_with_id(read_source_id, Arc::clone(&read_source));
        crate::wait_source::register_wait_source_with_id(
            write_source_id,
            Arc::clone(&write_source),
        );
        RNodeWaitPoints {
            read_source_id,
            read_source,
            write_source_id,
            write_source,
        }
    }

    pub(crate) fn release_rnode_wait_points(read_source_id: u64, write_source_id: u64) {
        crate::wait_source::release_wait_source(read_source_id);
        crate::wait_source::release_wait_source(write_source_id);
        wait_routing::unregister_source(read_source_id);
        wait_routing::unregister_source(write_source_id);
    }

    pub(crate) fn notify_readable_with_post<F>(source: &Arc<WaitSource>, mask: u64, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, mask, post);
    }

    pub(crate) fn notify_writable_with_post<F>(source: &Arc<WaitSource>, mask: u64, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, mask, post);
    }

    pub(crate) fn ppoll_wait(
        source: WaitSourceId,
        interests: InterestMask,
    ) -> StepOutcome<usize, NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, source.raw(), interests.raw())
    }
}
