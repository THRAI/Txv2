//! Page-backed notification meanings.
//!
//! This module owns wait-source yield conversion for page-cache backed
//! fetch/write/truncate paths.

use tx_platform_adapter::notification_adapter;

pub(crate) use wait_source::{
    is_wait_source, new_page_ready_wait, notify_page_ready_with_post, page_ready_endpoint,
    page_ready_source_id, reset_page_ready, wait_source_parts, yield_on_page_ready_source,
    yield_on_wait_source, PageReadyNotifier, PageReadyWait,
};

#[notification_adapter(
    subsystem = "page_backed",
    domain = "wait_source",
    reason = "page_backed notification.rs owns wait-source yield relay helpers"
)]
mod wait_source {
    use alloc::sync::Arc;

    use crate::page_backed::adapter::step_engine::{StepOutcome, StepProgress, YieldShape};
    use crate::page_backed::adapter::wait_routing::{
        self, MailboxEvent, RawQueue, TaskMailbox, WaitSource,
    };

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
            let source = wait_routing::new_wait_source(source_id);
            let readiness = wait_routing::new_readiness_queue(source_id);
            Self {
                source_id,
                source,
                readiness,
            }
        }

        pub(crate) fn ready_endpoint(&self) -> &Arc<WaitSource> {
            &self.source
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

    pub(crate) fn page_ready_source_id(wait: &PageReadyWait) -> u64 {
        tx_substrate::wake::WaitEndpoint::source_id(page_ready_endpoint(wait)).raw()
    }

    pub(crate) fn page_ready_endpoint(wait: &PageReadyWait) -> &Arc<WaitSource> {
        wait.ready_endpoint()
    }

    pub(crate) fn notify_page_ready_with_post<F>(notifier: &PageReadyNotifier, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        // Page completion is level readiness, not a one-consumer edge.  Latch
        // it before publishing the owner-aware edge so every task that was
        // handed this source id but installs its mailbox later observes the
        // completed generation.  WaitSource's pending bit alone can only be
        // consumed by one late subscriber and is therefore insufficient for
        // coalesced page faults.
        wait_routing::notify_readiness(&notifier.readiness, PAGE_READY);
        wait_routing::notify_source_with_post(&notifier.source, PAGE_READY, post);
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
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<T, P> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(progress, source_id, PAGE_READY)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use alloc::boxed::Box;
        use core::future::Future;
        use core::task::{Context, Poll, Waker};

        #[test]
        fn page_ready_endpoint_matches_source_id() {
            let wait = new_page_ready_wait();

            assert_eq!(
                tx_substrate::wake::WaitEndpoint::source_id(page_ready_endpoint(&wait)).raw(),
                page_ready_source_id(&wait)
            );
        }

        #[test]
        fn page_ready_wait_is_registered_for_token_driven_async_retry() {
            let wait = new_page_ready_wait();
            let source_id = page_ready_source_id(&wait);
            let mut future = Box::pin(
                crate::wait_source::wait_on_registered_source_id(source_id, PAGE_READY)
                    .expect("page-ready source must resolve from its yielded token"),
            );
            let waker = Waker::noop().clone();
            let mut cx = Context::from_waker(&waker);

            assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

            notify_page_ready_with_post(&wait.notifier(), |mailbox, event| mailbox.post(event));

            assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(_)));
        }

        #[test]
        fn page_ready_completion_wakes_every_late_waiter_until_next_fetch() {
            let wait = new_page_ready_wait();
            let source_id = page_ready_source_id(&wait);
            notify_page_ready_with_post(&wait.notifier(), |mailbox, event| mailbox.post(event));

            let mut first = Box::pin(
                crate::wait_source::wait_on_registered_source_id(source_id, PAGE_READY)
                    .expect("first late page waiter resolves"),
            );
            let mut second = Box::pin(
                crate::wait_source::wait_on_registered_source_id(source_id, PAGE_READY)
                    .expect("second late page waiter resolves"),
            );
            let waker = Waker::noop().clone();
            let mut cx = Context::from_waker(&waker);

            assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(_)));
            assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(_)));

            reset_page_ready(&wait);
            let mut next_generation = Box::pin(
                crate::wait_source::wait_on_registered_source_id(source_id, PAGE_READY)
                    .expect("next page generation resolves"),
            );
            assert!(matches!(
                next_generation.as_mut().poll(&mut cx),
                Poll::Pending
            ));
        }
    }
}
