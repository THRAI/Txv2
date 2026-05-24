//! Futex notification meanings.
//!
//! This module owns futex wait-point construction and semantic bucket/exact
//! wake verbs. Futex keys and waiter accounting remain in `mod.rs`.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{new_wait_point, notify_bucket, notify_exact};

#[notification_adapter(
    subsystem = "futex",
    domain = "readiness",
    reason = "futex notification.rs owns bucket and exact waiter wake publication"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::futex::adapter::step_engine::InterestMask;
    use crate::futex::adapter::wait_routing::{self, Channel, WaitSource};

    pub(crate) struct FutexWaitPoint {
        pub(crate) channel: Channel,
        pub(crate) source_id: u64,
        pub(crate) wait_source: Arc<WaitSource>,
    }

    pub(crate) fn new_wait_point() -> FutexWaitPoint {
        let channel = Channel::new();
        let source_id = crate::allocate_notification_source_id();
        let wait_source = wait_routing::new_wait_source(source_id);
        FutexWaitPoint {
            channel,
            source_id,
            wait_source,
        }
    }

    pub(crate) fn notify_exact(channel: &Channel, source: &Arc<WaitSource>, mask: u64) {
        wait_routing::fire_legacy_channel(channel, mask);
        wait_routing::notify_v3_source(source, mask);
    }

    pub(crate) fn notify_bucket(channel: &Channel, source: &Arc<WaitSource>, mask: u64) -> u32 {
        wait_routing::fire_legacy_channel(channel, mask);
        source.notify(InterestMask::new(mask)) as u32
    }
}
