//! POSIX message queue notification meanings.
//!
//! POSIX mq currently delegates storage to SysV message queues. This module
//! gives the fd-shaped wrapper semantic names for the inherited read/write
//! readiness notifications.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{notify_message_available_with_post, notify_space_available_with_post};

#[notification_adapter(
    subsystem = "posix_mq",
    domain = "readiness",
    reason = "posix mq notification.rs owns fd-shaped read/write wake meanings"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::process::adapter::wait_routing::{self, MailboxEvent, TaskMailbox, WaitSource};

    const MQ_CAN_RECV: u64 = 1;
    const MQ_CAN_SEND: u64 = 1;

    pub(crate) fn notify_message_available_with_post<F>(source: &Arc<WaitSource>, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, MQ_CAN_RECV, post)
    }

    pub(crate) fn notify_space_available_with_post<F>(source: &Arc<WaitSource>, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, MQ_CAN_SEND, post)
    }
}
