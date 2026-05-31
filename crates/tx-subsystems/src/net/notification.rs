//! Network notification meanings.
//!
//! This module owns socket wait-source yield construction and small
//! wait-mask adapters for net execution/facade/delegate code.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{empty_mask, wait_on_yield_shape, yield_bytes_on_token, yield_on_token};

#[notification_adapter(
    subsystem = "net",
    domain = "readiness",
    reason = "net notification.rs owns socket wait-source yields and wait-mask adapters"
)]
mod readiness {
    use crate::adapter::step_engine::{ByteProgress, NoProgress, StepOutcome, YieldShape};
    use crate::adapter::wait_routing::Mask;
    use crate::execution::WaitToken;

    pub(crate) fn yield_on_token<T>(token: WaitToken) -> StepOutcome<T, NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest())
    }

    pub(crate) fn yield_bytes_on_token<T>(
        progress: ByteProgress,
        token: WaitToken,
    ) -> StepOutcome<T, ByteProgress> {
        StepOutcome::yield_on_wait_source(progress, token.source_id(), token.interest())
    }

    pub(crate) fn wait_on_yield_shape(
        shape: YieldShape,
    ) -> Option<crate::wait_source::RegisteredWaitFuture> {
        match shape {
            YieldShape::OnWaitSource { source, interests }
            | YieldShape::OnEdge { source, interests } => {
                crate::wait_source::wait_on_source(source.raw(), interests.raw())
            }
            YieldShape::OnAgent { .. } | YieldShape::OnTimer { .. } => None,
        }
    }

    pub(crate) fn empty_mask() -> Mask {
        Mask::from_bits(0)
    }
}
