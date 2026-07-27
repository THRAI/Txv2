//! Timerfd notification meanings.
//!
//! This module owns the timerfd-side readable code and semantic wake/wait
//! verbs. Raw `WaitSource` primitives live in `adapter.rs`; timerfd code calls
//! the names here.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_readable_with_post, wait_until_readable, TIMERFD_READABLE,
};

#[notification_adapter(
    subsystem = "timerfd",
    domain = "readiness",
    reason = "timerfd notification.rs owns the readable wait code and wake verb"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::timerfd::adapter::step_engine::{self, ByteOutcome, WaitSource};
    use crate::timerfd::adapter::wait_routing::{self, MailboxEvent, TaskMailbox};

    /// Timer expiration count is available to read.
    pub const TIMERFD_READABLE: u64 = 0x1;

    pub(crate) struct TimerfdWaitPoint {
        source_id: u64,
        source: Arc<WaitSource>,
    }

    impl TimerfdWaitPoint {
        pub(crate) fn endpoint(&self) -> &Arc<WaitSource> {
            &self.source
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>) {
            (self.source_id, self.source)
        }
    }

    pub(crate) fn new_wait_point() -> TimerfdWaitPoint {
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_source_with_id(source_id, Arc::clone(&source));
        TimerfdWaitPoint { source_id, source }
    }

    pub(crate) fn notify_readable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_source_with_post(source, TIMERFD_READABLE, post);
    }

    pub(crate) fn wait_until_readable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> ByteOutcome {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        step_engine::yield_until_readable(source_id, TIMERFD_READABLE)
    }
}
