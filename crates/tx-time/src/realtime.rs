//! Realtime mutation and publication capability.

use tx_substrate::wake::{MailboxEvent, TaskMailbox};

use crate::clock::ClockRead;
use crate::deadline::DeadlineRegistrar;
use crate::types::TimeError;

/// Policy for writing realtime changes back to persistent clock hardware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeSetPolicy {
    Disabled,
    BestEffort,
    Required,
}

/// Result details for a realtime mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealtimeSetReport {
    pub realtime_ns: u64,
    pub generation: u64,
    pub persistent_written: bool,
}

/// Realtime control capability for privileged mutation paths.
pub trait RealtimeControl: ClockRead {
    fn set_realtime_ns(
        &self,
        ns: u64,
        policy: RealtimeSetPolicy,
    ) -> Result<RealtimeSetReport, TimeError>;

    fn set_realtime_ns_with_timerfd_post<F>(
        &self,
        ns: u64,
        policy: RealtimeSetPolicy,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<RealtimeSetReport, TimeError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool;

    fn seed_realtime_from_persistent(&self) -> Result<RealtimeSetReport, TimeError>;

    fn realtime_generation(&self) -> u64;

    fn realtime_offset_ns(&self) -> i128;
}

/// VVAR/vDSO publication capability.
pub trait VvarPublisher {
    fn publish_vvar(&self);
}
