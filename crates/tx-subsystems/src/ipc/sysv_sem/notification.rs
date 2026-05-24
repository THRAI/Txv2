//! SysV semaphore notification meanings.
//!
//! This file owns the semantic wake code for semaphore-array value changes.
//! Execution code calls `notify_changed` / `wait_for_change` instead of
//! constructing raw masks or wait-source yields directly.

pub(crate) use readiness::{new_changed_channel, notify_changed, wait_for_change};

use tx_platform_adapter::notification_adapter;

#[notification_adapter(
    subsystem = "sysv_sem",
    domain = "readiness",
    reason = "SysV sem notification.rs owns semaphore-changed wait code and wake verbs"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::process::adapter::step_engine::{NoProgress, StepOutcome};
    use crate::process::adapter::wait_routing::{self, Channel, Mask, WaitSource};

    pub const SEM_CHANGED: u64 = 1;

    pub(crate) fn new_changed_channel() -> (Channel, u64, Arc<WaitSource>) {
        let changed_channel = Channel::new();
        let changed_source_id = crate::allocate_notification_source_id();
        let changed_source = wait_routing::new_wait_source(changed_source_id);
        (changed_channel, changed_source_id, changed_source)
    }

    pub(crate) fn wait_for_change(source_id: u64) -> StepOutcome<usize, NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, source_id, SEM_CHANGED)
    }

    pub(crate) fn notify_changed(channel: &Channel, source: &Arc<WaitSource>) {
        channel.fire(Mask::from_bits(SEM_CHANGED));
        wait_routing::notify_v3_source(source, SEM_CHANGED);
    }
}
