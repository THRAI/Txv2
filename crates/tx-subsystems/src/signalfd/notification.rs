//! Signalfd notification meanings.
//!
//! This module owns the per-fd readable code and semantic wake/wait verbs for
//! signalfd. Raw `WaitSource` construction stays here so `mod.rs` can speak in
//! signalfd readiness terms.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_readable_with_post, release_wait_point, wait_until_readable,
};

#[notification_adapter(
    subsystem = "signalfd",
    domain = "readiness",
    reason = "signalfd notification.rs owns the readable wait code and wake verb"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::signalfd::adapter::step_engine::{ByteProgress, StepOutcome, WaitSource};
    use crate::signalfd::adapter::wait_routing::{self, MailboxEvent, TaskMailbox};

    /// A matching signal is pending and readable from this signalfd.
    const SIGNALFD_READABLE: u64 = 0x1;

    pub(crate) struct SignalfdWaitPoint {
        source_id: u64,
        source: Arc<WaitSource>,
    }

    impl SignalfdWaitPoint {
        pub(crate) fn endpoint(&self) -> &Arc<WaitSource> {
            &self.source
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>) {
            (self.source_id, self.source)
        }
    }

    pub(crate) fn new_wait_point() -> SignalfdWaitPoint {
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_source_with_id(source_id, Arc::clone(&source));
        SignalfdWaitPoint { source_id, source }
    }

    pub(crate) fn release_wait_point(source_id: u64) {
        crate::wait_source::release_wait_source(source_id);
        wait_routing::unregister_source(source_id);
    }

    pub(crate) fn notify_readable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_source_with_post(source, SIGNALFD_READABLE, post);
    }

    pub(crate) fn wait_until_readable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<usize, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, SIGNALFD_READABLE)
    }
}
