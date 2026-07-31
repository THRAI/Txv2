//! Subsystem hooks installed into the time service.
//!
//! `tx_time` owns wall-clock state; `tx_services::time` remains its compatibility
//! facade. This module only wires subsystem-local side effects after realtime
//! mutations or VVAR publication.

use tx_services::time::{self, DeadlineRegistrar, TimekeeperIf};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

pub fn ensure_hooks_installed() {
    time::install_realtime_timer_notifier(timerfd_clock_was_set_bridge);
    time::install_vvar_publish_hook(publish_vvar_snapshot_bridge);
}

fn timerfd_clock_was_set_bridge(
    generation: u64,
    timer_registrar: Option<&dyn DeadlineRegistrar>,
    post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
) -> usize {
    // Timerfd uses this as an idempotent revalidation/cancel hint. A delayed
    // older generation must not mutate timerfd state after a newer clock set.
    if generation < time::timekeeper().realtime_generation() {
        return 0;
    }
    crate::timerfd::timerfd_clock_was_set_with_post(generation, timer_registrar, post)
}

fn publish_vvar_snapshot_bridge(snapshot: time::VvarSnapshot) {
    if crate::vdso::vdso_available() {
        crate::vdso::vvar_page().update_from_snapshot(snapshot);
    }
}
