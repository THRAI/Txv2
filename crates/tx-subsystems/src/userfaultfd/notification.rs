//! Userfaultfd notification meanings.
//!
//! This module owns the pending-fault readable code and semantic wake/wait
//! verbs for userfaultfd.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_readable, release_wait_point, wait_until_readable,
};

#[notification_adapter(
    subsystem = "userfaultfd",
    domain = "readiness",
    reason = "userfaultfd notification.rs owns the pending-fault readable wait code"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::userfaultfd::adapter::step_engine::{
        ByteProgress, InterestMask, StepOutcome, WaitSource,
    };
    use crate::userfaultfd::adapter::wait_routing::{self, Channel, Mask};

    /// Pending fault message is available to read.
    const UFD_READABLE: u64 = 0x1;

    pub(crate) struct UfdWaitPoint {
        pub(crate) channel: Channel,
        pub(crate) source_id: u64,
        pub(crate) source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_point() -> UfdWaitPoint {
        let channel = Channel::new();
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_channel_with_id(source_id, channel.clone());
        UfdWaitPoint {
            channel,
            source_id,
            source,
        }
    }

    pub(crate) fn release_wait_point(source_id: u64) {
        crate::wait_source::release_wait_channel(source_id);
        wait_routing::unregister_source(source_id);
    }

    pub(crate) fn notify_readable(channel: &Channel, source: &Arc<WaitSource>) {
        channel.fire(Mask::from_bits(UFD_READABLE));
        source.notify_emit(InterestMask::new(UFD_READABLE));
    }

    pub(crate) fn wait_until_readable(source_id: u64) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, UFD_READABLE)
    }
}
