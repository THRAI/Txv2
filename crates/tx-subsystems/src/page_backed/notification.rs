//! Page-backed notification meanings.
//!
//! This module owns wait-source yield conversion for page-cache backed
//! fetch/write/truncate paths.

use tx_platform_adapter::notification_adapter;

pub(crate) use wait_source::{
    is_wait_source, new_page_ready_wait, notify_page_ready, page_ready_source_id, reset_page_ready,
    wait_source_parts, yield_on_page_ready_source, yield_on_wait_source, PageReadyNotifier,
    PageReadyWait,
};

#[notification_adapter(
    subsystem = "page_backed",
    domain = "wait_source",
    reason = "page_backed notification.rs owns wait-source yield relay helpers"
)]
mod wait_source {
    use alloc::sync::Arc;

    use crate::page_backed::adapter::step_engine::{StepOutcome, StepProgress, YieldShape};
    use crate::page_backed::adapter::wait_routing::{self, RawQueue, WaitSource};

    const PAGE_READY: u64 = 0x1;

    pub(crate) struct PageReadyWait {
        source_id: u64,
        source: Arc<WaitSource>,
        readiness: RawQueue,
    }

    pub(crate) struct PageReadyNotifier {
        source: Arc<WaitSource>,
        readiness: RawQueue,
    }

    impl PageReadyWait {
        fn new() -> Self {
            let source_id = crate::allocate_notification_source_id();
            Self {
                source_id,
                source: wait_routing::new_wait_source(source_id),
                readiness: wait_routing::new_readiness_queue(source_id),
            }
        }

        pub(crate) const fn source_id(&self) -> u64 {
            self.source_id
        }

        pub(crate) fn notifier(&self) -> PageReadyNotifier {
            PageReadyNotifier {
                source: Arc::clone(&self.source),
                readiness: self.readiness.clone(),
            }
        }
    }

    impl core::fmt::Debug for PageReadyWait {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("PageReadyWait")
                .field("source_id", &self.source_id)
                .finish_non_exhaustive()
        }
    }

    impl Drop for PageReadyWait {
        fn drop(&mut self) {
            wait_routing::unregister_source(self.source_id);
        }
    }

    pub(crate) fn new_page_ready_wait() -> PageReadyWait {
        PageReadyWait::new()
    }

    pub(crate) const fn page_ready_source_id(wait: &PageReadyWait) -> u64 {
        wait.source_id()
    }

    pub(crate) fn notify_page_ready(notifier: &PageReadyNotifier) {
        // The readiness queue keeps PAGE_READY asserted, so a waiter that
        // subscribes after the owner completed cannot miss the edge.
        wait_routing::notify_readiness(&notifier.readiness, PAGE_READY);
        wait_routing::notify_source(&notifier.source, PAGE_READY);
    }

    pub(crate) fn reset_page_ready(wait: &PageReadyWait) {
        wait_routing::clear_readiness(&wait.readiness, PAGE_READY);
    }

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

    pub(crate) fn yield_on_page_ready_source<T, P: StepProgress>(
        progress: P,
        source_id: u64,
    ) -> StepOutcome<T, P> {
        StepOutcome::yield_on_wait_source(progress, source_id, PAGE_READY)
    }
}
