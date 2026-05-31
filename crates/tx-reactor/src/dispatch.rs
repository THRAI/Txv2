//! Reactor dispatch bridge for wake placement and reschedule markers.
//!
//! Scheduler policy chooses a target hart. Dispatch state records the
//! mechanism-level preemption marker and asks an outer runtime to send a
//! reschedule IPI when the target is remote. The outer runtime owns the HAL
//! binding; this module remains platform-independent.

use alloc::vec::Vec;

use crate::{
    preempt::{PreemptMarkers, PreemptionPoint},
    scheduler::{HartId, RunnablePlacement},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WakeDispatchAction {
    pub target_hart: HartId,
    pub wake_remote: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WakeDispatchReport {
    pub placements: usize,
    pub local_reschedules: usize,
    pub remote_ipis: usize,
}

impl WakeDispatchReport {
    pub const fn empty() -> Self {
        Self {
            placements: 0,
            local_reschedules: 0,
            remote_ipis: 0,
        }
    }

    pub fn record(&mut self, action: WakeDispatchAction) {
        self.placements += 1;
        if action.wake_remote {
            self.remote_ipis += 1;
        } else {
            self.local_reschedules += 1;
        }
    }
}

pub trait RescheduleSignal {
    fn send_reschedule_ipi(&mut self, target_hart: HartId) -> bool;
}

#[derive(Default)]
pub struct NoopRescheduleSignal;

impl NoopRescheduleSignal {
    pub const fn new() -> Self {
        Self
    }
}

impl RescheduleSignal for NoopRescheduleSignal {
    fn send_reschedule_ipi(&mut self, _target_hart: HartId) -> bool {
        false
    }
}

#[derive(Debug, Default)]
pub struct DispatchState {
    per_hart: Vec<PreemptionPoint>,
}

impl DispatchState {
    pub fn new() -> Self {
        Self {
            per_hart: Vec::new(),
        }
    }

    pub fn apply_runnable_placement<S>(
        &mut self,
        placement: RunnablePlacement,
        signal: &mut S,
    ) -> WakeDispatchAction
    where
        S: RescheduleSignal,
    {
        self.mark_need_resched(placement.target_hart);
        let wake_remote =
            placement.wake_remote && signal.send_reschedule_ipi(placement.target_hart);

        WakeDispatchAction {
            target_hart: placement.target_hart,
            wake_remote,
        }
    }

    pub fn mark_need_resched(&mut self, hart: HartId) {
        self.ensure_hart(hart);
        self.per_hart[hart.0].mark_need_resched();
    }

    pub fn snapshot_markers(&self, hart: HartId) -> PreemptMarkers {
        self.per_hart
            .get(hart.0)
            .map(PreemptionPoint::snapshot)
            .unwrap_or_else(PreemptMarkers::empty)
    }

    pub fn consume_markers(&self, hart: HartId) -> PreemptMarkers {
        self.per_hart
            .get(hart.0)
            .map(PreemptionPoint::consume)
            .unwrap_or_else(PreemptMarkers::empty)
    }

    fn ensure_hart(&mut self, hart: HartId) {
        while self.per_hart.len() <= hart.0 {
            self.per_hart.push(PreemptionPoint::new());
        }
    }
}
