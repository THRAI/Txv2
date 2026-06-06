//! VFS notification meanings.
//!
//! This module owns VFS-uniform read/write wait codes, per-RNode wait point
//! setup/teardown, and composite ppoll wait-source yield construction.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_rnode_wait_points, notify_readable, notify_writable, ppoll_wait, release_rnode_wait_points,
    RNodeWaitPoints,
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
    use crate::vfs::adapter::wait_routing::{self, Channel, WaitSource};

    /// Per-RNode wait-source interest mask: bytes are available to read.
    pub const VFS_READABLE: u64 = 0x1;
    /// Per-RNode wait-source interest mask: space is available to write.
    pub const VFS_WRITABLE: u64 = 0x2;

    pub(crate) struct RNodeWaitPoints {
        pub(crate) read_channel: Channel,
        pub(crate) read_source_id: u64,
        pub(crate) read_source: Arc<WaitSource>,
        pub(crate) write_channel: Channel,
        pub(crate) write_source_id: u64,
        pub(crate) write_source: Arc<WaitSource>,
    }

    pub(crate) fn new_rnode_wait_points() -> RNodeWaitPoints {
        let read_channel = Channel::new();
        let read_source_id = crate::allocate_notification_source_id();
        let read_source = wait_routing::new_wait_source(read_source_id);
        let write_channel = Channel::new();
        let write_source_id = crate::allocate_notification_source_id();
        let write_source = wait_routing::new_wait_source(write_source_id);
        crate::wait_source::register_wait_channel_with_id(read_source_id, read_channel.clone());
        crate::wait_source::register_wait_channel_with_id(write_source_id, write_channel.clone());
        RNodeWaitPoints {
            read_channel,
            read_source_id,
            read_source,
            write_channel,
            write_source_id,
            write_source,
        }
    }

    pub(crate) fn release_rnode_wait_points(read_source_id: u64, write_source_id: u64) {
        crate::wait_source::release_wait_channel(read_source_id);
        crate::wait_source::release_wait_channel(write_source_id);
        wait_routing::unregister_source(read_source_id);
        wait_routing::unregister_source(write_source_id);
    }

    pub(crate) fn notify_readable(channel: &Channel, source: &Arc<WaitSource>, mask: u64) -> usize {
        let released = wait_routing::fire_legacy_channel(channel, mask);
        wait_routing::notify_v3_source(source, mask);
        released
    }

    pub(crate) fn notify_writable(channel: &Channel, source: &Arc<WaitSource>, mask: u64) -> usize {
        let released = wait_routing::fire_legacy_channel(channel, mask);
        wait_routing::notify_v3_source(source, mask);
        released
    }

    pub(crate) fn ppoll_wait(
        source: WaitSourceId,
        interests: InterestMask,
    ) -> StepOutcome<usize, NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, source.raw(), interests.raw())
    }
}
