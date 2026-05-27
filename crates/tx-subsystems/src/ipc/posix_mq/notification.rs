//! POSIX message queue notification meanings.
//!
//! POSIX mq currently delegates storage to SysV message queues. This module
//! gives the fd-shaped wrapper semantic names for the inherited read/write
//! readiness notifications.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{notify_message_available, notify_space_available};

#[notification_adapter(
    subsystem = "posix_mq",
    domain = "readiness",
    reason = "posix mq notification.rs owns fd-shaped read/write wake meanings"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::process::adapter::wait_routing::{self, Channel, Mask, WaitSource};

    const MQ_CAN_RECV: u64 = 1;
    const MQ_CAN_SEND: u64 = 1;

    pub(crate) fn notify_message_available(channel: &Channel, source: &Arc<WaitSource>) -> usize {
        channel.fire(Mask::from_bits(MQ_CAN_RECV))
            + wait_routing::notify_v3_source(source, MQ_CAN_RECV)
    }

    pub(crate) fn notify_space_available(channel: &Channel, source: &Arc<WaitSource>) -> usize {
        channel.fire(Mask::from_bits(MQ_CAN_SEND))
            + wait_routing::notify_v3_source(source, MQ_CAN_SEND)
    }
}
