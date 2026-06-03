//! Timerfd notification meanings.
//!
//! This module owns the timerfd-side readable code and semantic wake/wait
//! verbs. The raw legacy channel and v3 `WaitSource` primitives still live in
//! `adapter.rs`; timerfd code calls the names here.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{new_wait_point, notify_readable, wait_until_readable};

#[notification_adapter(
    subsystem = "timerfd",
    domain = "readiness",
    reason = "timerfd notification.rs owns the readable wait code and wake verb"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::timerfd::adapter::step_engine::{self, ByteOutcome, WaitSource};
    use crate::timerfd::adapter::wait_routing::{self, Channel};

    /// Timer expiration count is available to read.
    pub const TIMERFD_READABLE: u64 = 0x1;

    pub(crate) struct TimerfdWaitPoint {
        pub(crate) channel: Channel,
        pub(crate) source_id: u64,
        pub(crate) source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_point() -> TimerfdWaitPoint {
        let channel = Channel::new();
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_channel_with_id(source_id, channel.clone());
        TimerfdWaitPoint {
            channel,
            source_id,
            source,
        }
    }

    pub(crate) fn notify_readable(channel: Option<&Channel>, source: Option<&Arc<WaitSource>>) {
        if let Some(channel) = channel {
            wait_routing::fire_legacy_channel(channel, TIMERFD_READABLE);
        }
        if let Some(source) = source {
            wait_routing::notify_v3_source(source, TIMERFD_READABLE);
        }
    }

    pub(crate) fn wait_until_readable(source_id: u64) -> ByteOutcome {
        step_engine::yield_until_readable(source_id, TIMERFD_READABLE)
    }
}
