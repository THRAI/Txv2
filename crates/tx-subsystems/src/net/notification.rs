//! Network wait-notification boundary.
//!
//! Socket execution names waits with `WaitToken`; this module is the only
//! network-facing layer that lowers those tokens to reactor yield shapes or
//! turns a yielded shape back into a registered wait future.

use tx_platform_adapter::notification_adapter;

pub(crate) use wait::{registered_wait_for_yield, yield_bytes_on_wait_token, yield_on_wait_token};

#[notification_adapter(
    subsystem = "net",
    domain = "wait_source",
    reason = "network notification.rs owns WaitToken to reactor-yield lowering and wait registration"
)]
mod wait {
    use crate::execution::WaitToken;
    use tx_substrate::step::{ByteProgress, NoProgress, StepOutcome, YieldShape};

    pub(crate) fn yield_on_wait_token<T>(token: WaitToken) -> crate::execution::StepOutcome<T> {
        StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest())
    }

    pub(crate) fn yield_bytes_on_wait_token<T>(
        progress: ByteProgress,
        token: WaitToken,
    ) -> StepOutcome<T, ByteProgress> {
        StepOutcome::yield_on_wait_source(progress, token.source_id(), token.interest())
    }

    pub(crate) fn registered_wait_for_yield(
        shape: YieldShape,
    ) -> Option<crate::wait_source::RegisteredWaitFuture> {
        match shape {
            YieldShape::OnWaitSource { source, interests }
            | YieldShape::OnEdge { source, interests } => {
                crate::wait_source::wait_on_registered_source_id(source.raw(), interests.raw())
            }
            YieldShape::OnAgent { .. } | YieldShape::OnTimer { .. } => None,
        }
    }
}
