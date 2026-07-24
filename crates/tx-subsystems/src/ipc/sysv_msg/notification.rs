//! SysV message queue notification meanings.
//!
//! This is the subsystem-local semantic wrapper around the vanilla
//! wait-source primitives. Message queue execution code calls these verbs
//! instead of constructing masks or raw wait yields directly.

pub(crate) use readiness::{
    abort_removed_with_post, new_wait_sources, notify_message_available_with_post,
    notify_space_available_with_post, release_wait_sources, wait_for_message, wait_for_send_space,
};

use tx_platform_adapter::notification_adapter;

#[notification_adapter(
    subsystem = "sysv_msg",
    domain = "readiness",
    reason = "SysV msg notification.rs owns send-space and message-available wait codes"
)]
mod readiness {
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use crate::adapter::step_engine::ByteProgress;
    use crate::process::adapter::step_engine::{NoProgress, StepOutcome};
    use crate::process::adapter::wait_routing::{self, MailboxEvent, TaskMailbox, WaitSource};

    pub const MSG_CAN_SEND: u64 = 1;
    pub const MSG_CAN_RECV: u64 = 1;

    pub(crate) fn new_wait_sources() -> (u64, u64, Arc<WaitSource>, Arc<WaitSource>) {
        let send_source_id = crate::allocate_notification_source_id();
        let recv_source_id = crate::allocate_notification_source_id();
        let send_source = wait_routing::new_wait_source(send_source_id);
        let recv_source = wait_routing::new_wait_source(recv_source_id);
        crate::wait_source::register_wait_source_with_id(send_source_id, Arc::clone(&send_source));
        crate::wait_source::register_wait_source_with_id(recv_source_id, Arc::clone(&recv_source));
        (send_source_id, recv_source_id, send_source, recv_source)
    }

    pub(crate) fn release_wait_sources(send_source_id: u64, recv_source_id: u64) {
        crate::wait_source::release_wait_source(send_source_id);
        crate::wait_source::release_wait_source(recv_source_id);
        wait_routing::unregister_source(send_source_id);
        wait_routing::unregister_source(recv_source_id);
    }

    pub(crate) fn wait_for_send_space(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<usize, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, MSG_CAN_SEND)
    }

    pub(crate) fn wait_for_message(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<(i64, Vec<u8>), NoProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(NoProgress, source_id, MSG_CAN_RECV)
    }

    pub(crate) fn notify_space_available_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, MSG_CAN_SEND, post);
    }

    pub(crate) fn notify_message_available_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, MSG_CAN_RECV, post);
    }

    pub(crate) fn abort_removed_with_post<F>(
        send_source: &Arc<WaitSource>,
        recv_source: &Arc<WaitSource>,
        mut post: F,
    ) where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        notify_space_available_with_post(send_source, |mailbox, event| post(mailbox, event));
        notify_message_available_with_post(recv_source, |mailbox, event| post(mailbox, event));
    }
}
