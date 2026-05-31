//! TTY notification meanings.
//!
//! This module owns TTY readiness codes, wait-point setup/teardown, and
//! wait-source yield conversion helpers shared by TTY execution steps.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_readable, release_wait_point, wait_source_parts, yield_on_wait_source,
    yield_readable_for_tty, yield_writable_for_tty,
};
pub use readiness::{TTY_DEFERRED_SIGNAL, TTY_READABLE, TTY_WRITABLE};

#[notification_adapter(
    subsystem = "tty",
    domain = "readiness",
    reason = "tty notification.rs owns readable/writable readiness and wait-source yields"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::tty::adapter::step_engine::{ByteProgress, StepOutcome, StepProgress, YieldShape};
    use crate::tty::adapter::wait_routing::{self, Channel, WaitSource};

    /// Level bit for `TtyIdentity::input_readable`.
    pub const TTY_READABLE: u64 = 0x1;
    /// Level bit for `TtyIdentity::output_writable`.
    pub const TTY_WRITABLE: u64 = 0x1;
    /// Edge bit for a deferred foreground-pgrp signal event.
    pub const TTY_DEFERRED_SIGNAL: u64 = 0x1;

    pub(crate) struct TtyWaitPoint {
        pub(crate) channel: Channel,
        pub(crate) source_id: u64,
        pub(crate) source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_point() -> TtyWaitPoint {
        let channel = Channel::new();
        let source_id = crate::allocate_notification_source_id();
        crate::wait_source::register_wait_channel_with_id(source_id, channel.clone());
        let source = wait_routing::new_wait_source(source_id);
        TtyWaitPoint {
            channel,
            source_id,
            source,
        }
    }

    pub(crate) fn release_wait_point(source_id: u64) {
        crate::wait_source::release_wait_source(source_id);
        wait_routing::unregister_source(source_id);
    }

    pub(crate) fn notify_readable(channel: &Channel, source: &Arc<WaitSource>) {
        wait_routing::fire_legacy_channel(channel, TTY_READABLE);
        wait_routing::notify_v3_source(source, TTY_READABLE);
    }

    pub(crate) fn wait_source_parts(shape: &YieldShape) -> Option<(u64, u64)> {
        match shape {
            YieldShape::OnWaitSource { source, interests } => Some((source.raw(), interests.raw())),
            _ => None,
        }
    }

    pub(crate) fn yield_on_wait_source<T, P: StepProgress>(
        progress: P,
        carrier: u64,
        interest: u64,
    ) -> StepOutcome<T, P> {
        StepOutcome::yield_on_wait_source(progress, carrier, interest)
    }

    pub(crate) fn yield_writable_for_tty(tty_raw: u64) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, tty_raw, TTY_WRITABLE)
    }

    pub(crate) fn yield_readable_for_tty<T>(tty_raw: u64) -> StepOutcome<T, ByteProgress> {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, tty_raw, TTY_READABLE)
    }
}
