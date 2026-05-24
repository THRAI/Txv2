//! Signalfd notification meanings.
//!
//! This module owns the per-fd readable code and semantic wake/wait verbs for
//! signalfd. The raw legacy carrier and `WaitSource` construction stay here so
//! `mod.rs` can speak in signalfd readiness terms.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_readable, release_wait_point, wait_until_readable,
};

#[notification_adapter(
    subsystem = "signalfd",
    domain = "readiness",
    reason = "signalfd notification.rs owns the readable wait code and wake verb"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::signalfd::adapter::step_engine::{
        ByteProgress, InterestMask, StepOutcome, WaitSource,
    };
    use crate::signalfd::adapter::wait_routing::{self, Channel, Mask};

    /// A matching signal is pending and readable from this signalfd.
    const SIGNALFD_READABLE: u64 = 0x1;

    pub(crate) struct SignalfdWaitPoint {
        pub(crate) channel: Channel,
        pub(crate) source_id: u64,
        pub(crate) source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_point() -> SignalfdWaitPoint {
        let channel = Channel::new();
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        SignalfdWaitPoint {
            channel,
            source_id,
            source,
        }
    }

    pub(crate) fn release_wait_point(source_id: u64) {
        wait_routing::unregister_source(source_id);
    }

    pub(crate) fn notify_readable(channel: &Channel, source: &Arc<WaitSource>) {
        channel.fire(Mask::from_bits(SIGNALFD_READABLE));
        source.notify_emit(InterestMask::new(SIGNALFD_READABLE));
    }

    pub(crate) fn wait_until_readable(source_id: u64) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, SIGNALFD_READABLE)
    }
}
