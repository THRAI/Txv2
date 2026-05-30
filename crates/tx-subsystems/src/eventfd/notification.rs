//! Eventfd notification meanings.
//!
//! This module owns the eventfd-side readable/writable codes and semantic
//! wake/wait verbs. Raw wait-source primitives stay behind `adapter.rs`.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_points, notify_readable, notify_writable, wait_until_readable, wait_until_writable,
};
pub use readiness::{EVENTFD_READABLE, EVENTFD_WRITABLE};

#[notification_adapter(
    subsystem = "eventfd",
    domain = "readiness",
    reason = "eventfd notification.rs owns readable and writable wait codes and wake verbs"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::eventfd::adapter::step_engine::{
        self, ByteOutcome, NoProgress, StepOutcome, WaitSource,
    };
    use crate::eventfd::adapter::wait_routing::{self, Channel};

    /// Counter is nonzero and readable.
    pub const EVENTFD_READABLE: u64 = 0x1;
    /// Counter can accept another write without overflowing.
    pub const EVENTFD_WRITABLE: u64 = 0x2;

    pub(crate) struct EventfdWaitPoints {
        pub(crate) reader_channel: Channel,
        pub(crate) reader_source_id: u64,
        pub(crate) reader_source: Arc<WaitSource>,
        pub(crate) writer_channel: Channel,
        pub(crate) writer_source_id: u64,
        pub(crate) writer_source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_points() -> EventfdWaitPoints {
        let reader_channel = Channel::new();
        let writer_channel = Channel::new();
        let reader_source_id = crate::allocate_notification_source_id();
        let writer_source_id = crate::allocate_notification_source_id();
        let reader_source = wait_routing::new_wait_source(reader_source_id);
        let writer_source = wait_routing::new_wait_source(writer_source_id);
        EventfdWaitPoints {
            reader_channel,
            reader_source_id,
            reader_source,
            writer_channel,
            writer_source_id,
            writer_source,
        }
    }

    pub(crate) fn notify_readable(channel: Option<&Channel>, source: Option<&Arc<WaitSource>>) {
        if let Some(channel) = channel {
            wait_routing::fire_legacy_channel(channel, EVENTFD_READABLE);
        }
        if let Some(source) = source {
            wait_routing::notify_v3_source(source, EVENTFD_READABLE);
        }
    }

    pub(crate) fn notify_writable(channel: Option<&Channel>, source: Option<&Arc<WaitSource>>) {
        if let Some(channel) = channel {
            wait_routing::fire_legacy_channel(channel, EVENTFD_WRITABLE);
        }
        if let Some(source) = source {
            wait_routing::notify_v3_source(source, EVENTFD_WRITABLE);
        }
    }

    pub(crate) fn wait_until_readable(source_id: u64) -> ByteOutcome {
        step_engine::yield_until_readable(source_id, EVENTFD_READABLE)
    }

    pub(crate) fn wait_until_writable(source_id: u64) -> StepOutcome<(), NoProgress> {
        step_engine::yield_until_writable(source_id, EVENTFD_WRITABLE)
    }
}
