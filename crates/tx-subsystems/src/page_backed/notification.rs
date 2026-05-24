//! Page-backed notification meanings.
//!
//! This module owns wait-source yield conversion for page-cache backed
//! fetch/write/truncate paths.

use tx_platform_adapter::notification_adapter;

pub(crate) use wait_source::{is_wait_source, wait_source_parts, yield_on_wait_source};

#[notification_adapter(
    subsystem = "page_backed",
    domain = "wait_source",
    reason = "page_backed notification.rs owns wait-source yield relay helpers"
)]
mod wait_source {
    use crate::page_backed::adapter::step_engine::{StepOutcome, StepProgress, YieldShape};

    pub(crate) fn wait_source_parts(shape: &YieldShape) -> Option<(u64, u64)> {
        match shape {
            YieldShape::OnWaitSource { source, interests } => Some((source.raw(), interests.raw())),
            _ => None,
        }
    }

    pub(crate) fn is_wait_source(shape: &YieldShape) -> bool {
        wait_source_parts(shape).is_some()
    }

    pub(crate) fn yield_on_wait_source<T, P: StepProgress>(
        progress: P,
        source: u64,
        interests: u64,
    ) -> StepOutcome<T, P> {
        StepOutcome::yield_on_wait_source(progress, source, interests)
    }
}
