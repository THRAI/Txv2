//! Process notification meanings.
//!
//! This module owns process exit-source readiness codes and wait-point
//! construction so execution paths do not reach for raw wake primitives.

use alloc::sync::Arc;

use tx_platform_adapter::notification_adapter;

pub(crate) use exit_source::{new_exit_wait_point, notify_child_zombified_with_post};

#[notification_adapter(
    subsystem = "process",
    domain = "exit_source",
    reason = "process notification.rs owns child-state exit-source masks and wait-source registration"
)]
mod exit_source {
    use super::Arc;

    use crate::process::adapter::step_engine::Cap;
    use crate::process::adapter::wait_routing::{self, MailboxEvent, TaskMailbox, WaitSource};
    use crate::process::structure::{ProcessIdentity, EXIT_SOURCE_CHILD_ZOMBIFIED};

    pub(crate) struct ProcessExitWaitPoint {
        pub(crate) source_id: u64,
        pub(crate) source: Arc<WaitSource>,
    }

    pub(crate) fn new_exit_wait_point() -> ProcessExitWaitPoint {
        let source_id = crate::allocate_notification_source_id();
        let source = wait_routing::new_wait_source(source_id);
        crate::wait_source::register_wait_source_with_diagnostic_kind(
            source_id,
            source.clone(),
            "process-exit",
        );
        ProcessExitWaitPoint { source_id, source }
    }

    pub(crate) fn notify_child_zombified_with_post<F>(
        parent: &Cap<ProcessIdentity>,
        post: F,
    ) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        parent.fire_exit_source_with_post(child_zombified_mask(), post)
    }

    fn child_zombified_mask() -> u64 {
        EXIT_SOURCE_CHILD_ZOMBIFIED
    }
}
