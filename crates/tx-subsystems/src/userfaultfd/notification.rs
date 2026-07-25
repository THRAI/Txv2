//! Userfaultfd notification meanings.
//!
//! This module owns the pending-fault readable code and semantic wake/wait
//! verbs for userfaultfd.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_readable_with_post, release_wait_point, wait_until_readable,
};

#[notification_adapter(
    subsystem = "userfaultfd",
    domain = "readiness",
    reason = "userfaultfd notification.rs owns the pending-fault readable wait code"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::userfaultfd::adapter::step_engine::{ByteProgress, StepOutcome, WaitSource};
    use crate::userfaultfd::adapter::wait_routing::{
        self, MailboxEvent, MailboxSchedulerHint, TaskMailbox,
    };

    /// Pending fault message is available to read.
    const UFD_READABLE: u64 = 0x1;

    pub(crate) struct UfdWaitPoint {
        source_id: u64,
        source: Arc<WaitSource>,
    }

    impl UfdWaitPoint {
        pub(crate) fn endpoint(&self) -> &Arc<WaitSource> {
            &self.source
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>) {
            (self.source_id, self.source)
        }
    }

    pub(crate) fn new_wait_point() -> UfdWaitPoint {
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_source_with_id(source_id, Arc::clone(&source));
        UfdWaitPoint { source_id, source }
    }

    pub(crate) fn release_wait_point(source_id: u64) {
        crate::wait_source::release_wait_source(source_id);
        wait_routing::unregister_source(source_id);
    }

    pub(crate) fn notify_readable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        wait_routing::notify_source_with_post(source, UFD_READABLE, post);
    }

    pub(crate) fn wait_until_readable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<usize, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, UFD_READABLE)
    }
}
