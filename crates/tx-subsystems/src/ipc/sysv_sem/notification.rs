//! SysV semaphore notification meanings.
//!
//! This file owns the semantic wake code for semaphore-array value changes.
//! Execution code calls `notify_changed_with_post` / `wait_for_change` instead of
//! constructing raw masks or wait-source yields directly.

pub(crate) use readiness::{new_changed_source, notify_changed_with_post, wait_for_change};

use tx_platform_adapter::notification_adapter;

#[notification_adapter(
    subsystem = "sysv_sem",
    domain = "readiness",
    reason = "SysV sem notification.rs owns semaphore-changed wait code and wake verbs"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::process::adapter::step_engine::{NoProgress, StepOutcome};
    use crate::process::adapter::wait_routing::{self, MailboxEvent, TaskMailbox, WaitSource};

    pub const SEM_CHANGED: u64 = 1;

    pub(crate) fn new_changed_source() -> (u64, Arc<WaitSource>) {
        let changed_source_id = crate::allocate_notification_source_id();
        let changed_source = wait_routing::new_wait_source(changed_source_id);
        crate::wait_source::register_wait_source_with_id(
            changed_source_id,
            Arc::clone(&changed_source),
        );
        (changed_source_id, changed_source)
    }

    pub(crate) fn wait_for_change(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<usize, NoProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(NoProgress, source_id, SEM_CHANGED)
    }

    pub(crate) fn notify_changed_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_v3_source_with_post(source, SEM_CHANGED, post);
    }
}
