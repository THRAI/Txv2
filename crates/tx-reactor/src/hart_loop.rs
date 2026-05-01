//! Platform-independent per-hart reactor loop shell.
//!
//! This module is the dispatch seam for stepping a hart's reactor work without
//! owning HAL WFI, interrupt acknowledgement, or userspace trap return.

use crate::{
    dispatch::{RescheduleSignal, WakeDispatchReport},
    preempt::PreemptMarkers,
    runtime::{Reactor, RunStats},
    scheduler::HartId,
};

/// Whether the outer runtime should immediately step again or enter its idle path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HartLoopDecision {
    /// The step made progress; the outer loop should re-observe before idling.
    Continue,
    /// The step found no runnable work, consumed marker, or expired timer wake.
    Idle,
}

impl HartLoopDecision {
    pub const fn should_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// Platform-neutral timer programming request for the outer runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HartLoopDeadlineAction {
    /// Arm the platform timer for an absolute reactor deadline.
    Arm { deadline_ns: u64 },
    /// Cancel the platform timer because the reactor has no pending deadline.
    Cancel,
}

impl HartLoopDeadlineAction {
    pub const fn from_next_deadline(next_deadline_ns: Option<u64>) -> Self {
        match next_deadline_ns {
            Some(deadline_ns) => Self::Arm { deadline_ns },
            None => Self::Cancel,
        }
    }

    pub const fn next_deadline_ns(self) -> Option<u64> {
        match self {
            Self::Arm { deadline_ns } => Some(deadline_ns),
            Self::Cancel => None,
        }
    }
}

/// Result of one bounded platform-independent hart loop step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HartLoopStep {
    pub hart: HartId,
    pub now_ns: u64,
    pub stats: RunStats,
    pub wake_dispatch: WakeDispatchReport,
    pub consumed_markers: PreemptMarkers,
    pub timer_wakes: usize,
    pub next_deadline_ns: Option<u64>,
    pub deadline_action: HartLoopDeadlineAction,
    pub decision: HartLoopDecision,
}

impl HartLoopStep {
    pub const fn new(
        hart: HartId,
        now_ns: u64,
        stats: RunStats,
        wake_dispatch: WakeDispatchReport,
        consumed_markers: PreemptMarkers,
        timer_wakes: usize,
        next_deadline_ns: Option<u64>,
    ) -> Self {
        let deadline_action = HartLoopDeadlineAction::from_next_deadline(next_deadline_ns);
        let decision = if stats.polled > 0
            || wake_dispatch.placements > 0
            || !consumed_markers.is_empty()
            || timer_wakes > 0
        {
            HartLoopDecision::Continue
        } else {
            HartLoopDecision::Idle
        };

        Self {
            hart,
            now_ns,
            stats,
            wake_dispatch,
            consumed_markers,
            timer_wakes,
            next_deadline_ns,
            deadline_action,
            decision,
        }
    }

    pub const fn ran_work(&self) -> bool {
        self.stats.polled > 0
    }

    pub const fn consumed_reschedule_marker(&self) -> bool {
        self.consumed_markers.need_resched()
    }

    pub const fn observed_timer_wakes(&self) -> bool {
        self.timer_wakes > 0
    }

    pub const fn should_idle(&self) -> bool {
        self.decision.should_idle()
    }
}

/// Minimal adapter a reactor-like runtime must provide to drive one hart step.
pub trait HartLoopRuntime {
    fn advance_hart_loop_time(&mut self, now_ns: u64) -> usize;

    fn drain_hart_loop_wakes<S>(
        &mut self,
        current_hart: HartId,
        signal: &mut S,
    ) -> WakeDispatchReport
    where
        S: RescheduleSignal;

    fn consume_hart_loop_markers(&mut self, hart: HartId) -> PreemptMarkers;

    fn run_hart_loop_ready<S>(&mut self, hart: HartId, signal: &mut S) -> RunStats
    where
        S: RescheduleSignal;

    fn hart_loop_next_deadline_ns(&self) -> Option<u64>;
}

/// Minimal clock adapter used by the platform-independent step shell.
pub trait HartLoopClock {
    fn now_ns(&mut self) -> u64;
}

impl<F> HartLoopClock for F
where
    F: FnMut() -> u64,
{
    fn now_ns(&mut self) -> u64 {
        self()
    }
}

/// Step one hart using a clock adapter owned by the caller.
pub fn step_hart_loop<R, C, S>(
    runtime: &mut R,
    hart: HartId,
    clock: &mut C,
    signal: &mut S,
) -> HartLoopStep
where
    R: HartLoopRuntime,
    C: HartLoopClock,
    S: RescheduleSignal,
{
    step_hart_loop_at(runtime, hart, clock.now_ns(), signal)
}

/// Step one hart at a caller-observed absolute nanosecond time.
pub fn step_hart_loop_at<R, S>(
    runtime: &mut R,
    hart: HartId,
    now_ns: u64,
    signal: &mut S,
) -> HartLoopStep
where
    R: HartLoopRuntime,
    S: RescheduleSignal,
{
    let timer_wakes = runtime.advance_hart_loop_time(now_ns);
    let wake_dispatch = runtime.drain_hart_loop_wakes(hart, signal);
    let consumed_markers = runtime.consume_hart_loop_markers(hart);
    let stats = runtime.run_hart_loop_ready(hart, signal);
    let next_deadline_ns = runtime.hart_loop_next_deadline_ns();

    HartLoopStep::new(
        hart,
        now_ns,
        stats,
        wake_dispatch,
        consumed_markers,
        timer_wakes,
        next_deadline_ns,
    )
}

impl HartLoopRuntime for Reactor {
    fn advance_hart_loop_time(&mut self, now_ns: u64) -> usize {
        self.advance_time_to(now_ns)
    }

    fn drain_hart_loop_wakes<S>(
        &mut self,
        current_hart: HartId,
        signal: &mut S,
    ) -> WakeDispatchReport
    where
        S: RescheduleSignal,
    {
        self.drain_wakes_for_hart(current_hart, signal)
    }

    fn consume_hart_loop_markers(&mut self, hart: HartId) -> PreemptMarkers {
        self.consume_dispatch_markers(hart)
    }

    fn run_hart_loop_ready<S>(&mut self, hart: HartId, signal: &mut S) -> RunStats
    where
        S: RescheduleSignal,
    {
        self.run_until_idle_on_hart_with_reschedule(hart, signal)
    }

    fn hart_loop_next_deadline_ns(&self) -> Option<u64> {
        self.next_deadline_ns()
    }
}
