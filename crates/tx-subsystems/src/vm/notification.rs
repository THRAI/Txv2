//! VM notification meanings.
//!
//! This module owns RangeLock release readiness and wait-source yield
//! construction for VM step paths.

use tx_platform_adapter::notification_adapter;

pub(crate) use range_lock::{
    new_range_lock_wait_point, notify_range_lock_released, range_lock_blocked,
    release_range_lock_wait_point, wait_token_from_shape, yield_wait_token,
};

#[notification_adapter(
    subsystem = "vm",
    domain = "range_lock",
    reason = "vm notification.rs owns RangeLock release masks, channel registration, and wait-source yields"
)]
mod range_lock {
    use crate::execution::WaitToken;
    use crate::vm::RANGE_LOCK_RELEASE_MASK;
    use crate::vm::adapter::step_engine::{NoProgress, StepOutcome, StepProgress, YieldShape};
    use crate::vm::adapter::wait_routing::{self, Channel, Mask, WaitSource};
    use alloc::sync::Arc;

    pub(crate) struct RangeLockWaitPoint {
        pub(crate) channel: Channel,
        pub(crate) source_id: u64,
        pub(crate) source: Arc<WaitSource>,
    }

    pub(crate) fn new_range_lock_wait_point() -> RangeLockWaitPoint {
        let channel = Channel::new();
        let source_id = crate::allocate_notification_source_id();
        crate::wait_source::register_wait_channel_with_id(source_id, channel.clone());
        let source = wait_routing::new_wait_source(source_id);
        RangeLockWaitPoint {
            channel,
            source_id,
            source,
        }
    }

    pub(crate) fn release_range_lock_wait_point(source_id: u64) {
        crate::wait_source::release_wait_channel(source_id);
        wait_routing::unregister_source(source_id);
    }

    pub(crate) fn notify_range_lock_released(channel: &Channel, source: &Arc<WaitSource>) {
        channel.fire(Mask::from_bits(RANGE_LOCK_RELEASE_MASK));
        source.notify_emit(crate::vm::adapter::step_engine::InterestMask::new(
            RANGE_LOCK_RELEASE_MASK,
        ));
    }

    pub(crate) fn range_lock_blocked<O>(source_id: u64) -> StepOutcome<O, NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, source_id, RANGE_LOCK_RELEASE_MASK)
    }

    pub(crate) fn wait_token_from_shape(shape: &YieldShape) -> Option<WaitToken> {
        match shape {
            YieldShape::OnWaitSource { source, interests } => {
                Some(WaitToken::new(source.raw(), interests.raw()))
            }
            _ => None,
        }
    }

    pub(crate) fn yield_wait_token<T, P: StepProgress>(
        progress: P,
        token: WaitToken,
    ) -> StepOutcome<T, P> {
        StepOutcome::yield_on_wait_source(progress, token.source_id(), token.interest())
    }
}
