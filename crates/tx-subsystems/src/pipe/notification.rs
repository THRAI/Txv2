//! Pipe notification meanings.
//!
//! This module owns the pipe-side readiness codes and semantic wake verbs.
//! The underlying legacy channel and v3 `WaitSource` primitives still live in
//! `adapter.rs`; pipe execution code calls the names here.

pub(crate) use readiness::{
    new_wait_points, notify_readable, notify_writable, release_wait_points, wait_until_readable,
    wait_until_writable, yield_until_readable,
};
pub use readiness::{PIPE_READABLE, PIPE_WRITABLE};

use tx_platform_adapter::notification_adapter;

#[notification_adapter(
    subsystem = "pipe",
    domain = "readiness",
    reason = "pipe notification.rs owns readable and writable wait codes and wake verbs"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::pipe::adapter::step_engine::{self, ByteProgress, StepOutcome};
    use crate::pipe::adapter::wait_routing::{self, Channel, WaitSource};

    /// Bytes are available to read, or writer close made EOF observable.
    pub const PIPE_READABLE: u64 = 0x1;
    /// Space is available to write, or reader close made EPIPE observable.
    pub const PIPE_WRITABLE: u64 = 0x2;

    pub(crate) struct PipeWaitPoints {
        pub(crate) reader_channel: Channel,
        pub(crate) reader_source_id: u64,
        pub(crate) reader_source: Arc<WaitSource>,
        pub(crate) writer_channel: Channel,
        pub(crate) writer_source_id: u64,
        pub(crate) writer_source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_points() -> PipeWaitPoints {
        let reader_channel = Channel::new();
        let reader_source_id = crate::allocate_notification_source_id();
        let writer_channel = Channel::new();
        let writer_source_id = crate::allocate_notification_source_id();

        let reader_source = wait_routing::new_wait_source(reader_source_id);
        let writer_source = wait_routing::new_wait_source(writer_source_id);
        crate::wait_source::register_wait_channel_with_id(reader_source_id, reader_channel.clone());
        crate::wait_source::register_wait_channel_with_id(writer_source_id, writer_channel.clone());

        PipeWaitPoints {
            reader_channel,
            reader_source_id,
            reader_source,
            writer_channel,
            writer_source_id,
            writer_source,
        }
    }

    pub(crate) fn release_wait_points(reader_source_id: u64, writer_source_id: u64) {
        crate::wait_source::release_wait_channel(reader_source_id);
        crate::wait_source::release_wait_channel(writer_source_id);
        wait_routing::unregister_source(reader_source_id);
        wait_routing::unregister_source(writer_source_id);
    }

    pub(crate) fn notify_readable(channel: &Channel, source: &Arc<WaitSource>) {
        wait_routing::fire_legacy_channel(channel, PIPE_READABLE);
        wait_routing::notify_v3_source(source, PIPE_READABLE);
    }

    pub(crate) fn notify_writable(channel: &Channel, source: &Arc<WaitSource>) {
        wait_routing::fire_legacy_channel(channel, PIPE_WRITABLE);
        wait_routing::notify_v3_source(source, PIPE_WRITABLE);
    }

    pub(crate) fn wait_until_readable(source_id: u64) -> StepOutcome<usize, ByteProgress> {
        step_engine::yield_until_readable(source_id, PIPE_READABLE)
    }

    pub(crate) fn yield_until_readable<T>(source_id: u64) -> StepOutcome<T, ByteProgress> {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, PIPE_READABLE)
    }

    pub(crate) fn wait_until_writable(source_id: u64) -> StepOutcome<usize, ByteProgress> {
        step_engine::yield_until_writable(source_id, PIPE_WRITABLE)
    }
}
