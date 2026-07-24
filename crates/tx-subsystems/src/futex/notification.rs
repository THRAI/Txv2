//! Futex notification meanings.
//!
//! This module owns futex wait-point construction and semantic bucket/exact
//! wake verbs. Futex keys and waiter accounting remain in `mod.rs`.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_point, notify_bucket_with_hint, notify_exact_limit_with_post, yielded_wait_source,
};

#[notification_adapter(
    subsystem = "futex",
    domain = "readiness",
    reason = "futex notification.rs owns bucket and exact waiter wake publication"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::futex::adapter::step_engine::{InterestMask, StepOutcome, WaitSourceId, YieldShape};
    use crate::futex::adapter::wait_routing::{
        self, MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource,
    };

    pub(crate) struct FutexWaitPoint {
        source_id: u64,
        wait_source: Arc<WaitSource>,
    }

    impl FutexWaitPoint {
        pub(crate) fn endpoint(&self) -> &Arc<WaitSource> {
            &self.wait_source
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>) {
            (self.source_id, self.wait_source)
        }
    }

    pub(crate) fn new_wait_point() -> FutexWaitPoint {
        let source_id = crate::allocate_notification_source_id();
        let wait_source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_source_with_id(source_id, wait_source.clone());
        FutexWaitPoint {
            source_id,
            wait_source,
        }
    }

    pub(crate) fn notify_exact_limit_with_post<F>(
        source: &Arc<WaitSource>,
        mask: u64,
        limit: usize,
        hint: MailboxSchedulerHint,
        post: F,
    ) -> u32
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        wait_routing::notify_v3_source_limit_emit_with_post(source, mask, limit, hint, post) as u32
    }

    pub(crate) fn notify_bucket_with_hint(
        source: &Arc<WaitSource>,
        mask: u64,
        hint: MailboxSchedulerHint,
    ) -> u32 {
        source.notify_with_hint(InterestMask::new(mask), hint) as u32
    }

    pub(crate) fn yielded_wait_source<T, P>(outcome: &StepOutcome<T, P>) -> Option<WaitSourceId> {
        match outcome {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => Some(*source),
            StepOutcome::Yield { .. }
            | StepOutcome::Done(_)
            | StepOutcome::Continue { .. }
            | StepOutcome::Err(_) => None,
        }
    }
}
