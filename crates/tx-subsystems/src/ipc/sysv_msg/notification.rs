//! SysV message queue notification meanings.
//!
//! This is the subsystem-local semantic wrapper around the vanilla
//! wait-channel / wait-source primitives. Message queue execution code calls
//! these verbs instead of constructing masks or raw wait yields directly.

pub(crate) use readiness::{
    abort_removed, new_wait_channels, notify_message_available, notify_space_available,
    wait_for_message, wait_for_send_space,
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
    use crate::process::adapter::wait_routing::{self, Channel, Mask, WaitSource};

    pub const MSG_CAN_SEND: u64 = 1;
    pub const MSG_CAN_RECV: u64 = 1;

    pub(crate) fn new_wait_channels(
    ) -> (Channel, Channel, u64, u64, Arc<WaitSource>, Arc<WaitSource>) {
        let send_channel = Channel::new();
        let recv_channel = Channel::new();
        let send_source_id = crate::allocate_notification_source_id();
        let recv_source_id = crate::allocate_notification_source_id();
        let send_source = wait_routing::new_wait_source(send_source_id);
        let recv_source = wait_routing::new_wait_source(recv_source_id);
        (
            send_channel,
            recv_channel,
            send_source_id,
            recv_source_id,
            send_source,
            recv_source,
        )
    }

    pub(crate) fn wait_for_send_space(source_id: u64) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, MSG_CAN_SEND)
    }

    pub(crate) fn wait_for_message(source_id: u64) -> StepOutcome<(i64, Vec<u8>), NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, source_id, MSG_CAN_RECV)
    }

    pub(crate) fn notify_space_available(channel: &Channel, source: &Arc<WaitSource>) {
        channel.fire(Mask::from_bits(MSG_CAN_SEND));
        wait_routing::notify_v3_source(source, MSG_CAN_SEND);
    }

    pub(crate) fn notify_message_available(channel: &Channel, source: &Arc<WaitSource>) {
        channel.fire(Mask::from_bits(MSG_CAN_RECV));
        wait_routing::notify_v3_source(source, MSG_CAN_RECV);
    }

    pub(crate) fn abort_removed(
        send_channel: &Channel,
        send_source: &Arc<WaitSource>,
        recv_channel: &Channel,
        recv_source: &Arc<WaitSource>,
    ) {
        notify_space_available(send_channel, send_source);
        notify_message_available(recv_channel, recv_source);
    }
}
