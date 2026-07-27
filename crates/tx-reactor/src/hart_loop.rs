//! Platform-independent per-hart reactor loop shell.
//!
//! This module is the dispatch seam for stepping a hart's reactor work without
//! owning HAL WFI, interrupt acknowledgement, or userspace trap return.
//!
//! ## PR-7B integration point: DelegateTimeout fires
//!
//! `tx-substrate`'s `DelegateRegistry` and the concrete timer registry now live
//! below the reactor-facing time facade. The reactor-side glue that routes a
//! fired `DelegateTimeout` timer to the registry delegate timeout transition
//! lives behind the time driver boundary. The
//! hart-loop tick handler is the natural caller — drive it after
//! [`HartLoopRuntime::advance_hart_loop_time`] returns.

use crate::{
    dispatch::{RescheduleSignal, WakeDispatchReport},
    preempt::PreemptMarkers,
    runtime::{HartRuntimeView, Reactor, RunStats},
    scheduler::HartId,
};
pub use tx_time::driver::CurrentHartDeadlineAction as HartLoopDeadlineAction;

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

/// Result of one bounded platform-independent hart loop step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HartLoopStep {
    pub hart: HartId,
    pub now_ns: u64,
    pub stats: RunStats,
    pub wake_dispatch: WakeDispatchReport,
    pub consumed_markers: PreemptMarkers,
    pub timer_wakes: usize,
    pub deadline_changed: bool,
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
        deadline_changed: bool,
        next_deadline_ns: Option<u64>,
    ) -> Self {
        let deadline_action = HartLoopDeadlineAction::from_next_deadline(next_deadline_ns);
        let decision = if stats.polled > 0
            || wake_dispatch.placements > 0
            || !consumed_markers.is_empty()
            || timer_wakes > 0
            || deadline_changed
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
            deadline_changed,
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

    fn hart_loop_next_deadline_ns(&self) -> Option<u64>;

    fn advance_hart_loop_time_with_reschedule<S>(
        &mut self,
        hart: HartId,
        now_ns: u64,
        signal: &mut S,
    ) -> (usize, WakeDispatchReport)
    where
        S: RescheduleSignal,
    {
        let _ = hart;
        let _ = signal;
        (
            self.advance_hart_loop_time(now_ns),
            WakeDispatchReport::empty(),
        )
    }

    fn hart_loop_deadline_change_pending(&mut self) -> bool {
        false
    }

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
    let (timer_wakes, mut wake_dispatch) =
        runtime.advance_hart_loop_time_with_reschedule(hart, now_ns, signal);
    wake_dispatch.merge(runtime.drain_hart_loop_wakes(hart, signal));
    let deadline_changed = runtime.hart_loop_deadline_change_pending();
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
        deadline_changed,
        next_deadline_ns,
    )
}

impl HartLoopRuntime for Reactor {
    fn advance_hart_loop_time(&mut self, now_ns: u64) -> usize {
        self.advance_time_to(now_ns)
    }

    fn hart_loop_next_deadline_ns(&self) -> Option<u64> {
        self.next_deadline_ns()
    }

    fn advance_hart_loop_time_with_reschedule<S>(
        &mut self,
        hart: HartId,
        now_ns: u64,
        signal: &mut S,
    ) -> (usize, WakeDispatchReport)
    where
        S: RescheduleSignal,
    {
        self.advance_time_to_from_hart_with_reschedule(now_ns, hart, signal)
    }

    fn hart_loop_deadline_change_pending(&mut self) -> bool {
        self.deadline_change_pending()
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
}

impl HartLoopRuntime for HartRuntimeView<'_> {
    fn advance_hart_loop_time(&mut self, now_ns: u64) -> usize {
        self.advance_time_to(now_ns)
    }

    fn hart_loop_next_deadline_ns(&self) -> Option<u64> {
        self.next_deadline_ns()
    }

    fn advance_hart_loop_time_with_reschedule<S>(
        &mut self,
        hart: HartId,
        now_ns: u64,
        signal: &mut S,
    ) -> (usize, WakeDispatchReport)
    where
        S: RescheduleSignal,
    {
        self.advance_time_to_with_reschedule(now_ns, hart, signal)
    }

    fn hart_loop_deadline_change_pending(&mut self) -> bool {
        self.deadline_change_pending()
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
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 cleanup)
// ---------------------------------------------------------------------------
//
// Reactor-internal step fns. There is no epoch `Guard` argument here —
// these step the hart loop entirely inside the reactor and do not cross
// into substrate-protected payload storage. The `&mut` arguments are
// stored by mutable reference; `StepOp::step` is a single call.

/// `StepOp` wrap of [`step_hart_loop`].
pub struct HartLoopOp<'a, R, C, S>
where
    R: HartLoopRuntime,
    C: HartLoopClock,
    S: RescheduleSignal,
{
    pub runtime: &'a mut R,
    pub hart: HartId,
    pub clock: &'a mut C,
    pub signal: &'a mut S,
}

impl<'a, R, C, S, I> crate::adapter::step_engine::StepOp<I> for HartLoopOp<'a, R, C, S>
where
    R: HartLoopRuntime,
    C: HartLoopClock,
    S: RescheduleSignal,
    I: crate::adapter::step_engine::SubjectIdentity,
{
    type Output = HartLoopStep;
    type Progress = crate::adapter::step_engine::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut crate::adapter::step_engine::ScriptCtx<I>,
    ) -> crate::adapter::step_engine::StepOutcome<Self::Output, Self::Progress> {
        crate::adapter::step_engine::StepOutcome::Done(step_hart_loop(
            self.runtime,
            self.hart,
            self.clock,
            self.signal,
        ))
    }
}

/// `StepOp` wrap of [`step_hart_loop_at`].
pub struct HartLoopAtOp<'a, R, S>
where
    R: HartLoopRuntime,
    S: RescheduleSignal,
{
    pub runtime: &'a mut R,
    pub hart: HartId,
    pub now_ns: u64,
    pub signal: &'a mut S,
}

impl<'a, R, S, I> crate::adapter::step_engine::StepOp<I> for HartLoopAtOp<'a, R, S>
where
    R: HartLoopRuntime,
    S: RescheduleSignal,
    I: crate::adapter::step_engine::SubjectIdentity,
{
    type Output = HartLoopStep;
    type Progress = crate::adapter::step_engine::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut crate::adapter::step_engine::ScriptCtx<I>,
    ) -> crate::adapter::step_engine::StepOutcome<Self::Output, Self::Progress> {
        crate::adapter::step_engine::StepOutcome::Done(step_hart_loop_at(
            self.runtime,
            self.hart,
            self.now_ns,
            self.signal,
        ))
    }
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 cleanup `StepOp` wrap smoke tests for `hart_loop`.
    //!
    //! Each test builds the `*Op` adapter, drives it with `.step(&mut ctx)`,
    //! and pins the `StepOutcome::Done(HartLoopStep)` shape. Semantics
    //! coverage lives in the integration tests under
    //! `tests/hart_loop.rs`.
    use super::*;
    use crate::adapter::step_engine::{
        PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome as V3,
    };
    use crate::dispatch::NoopRescheduleSignal;

    #[derive(Debug)]
    struct FakeHartRuntime {
        timer_wakes: usize,
        wake_dispatch: WakeDispatchReport,
        markers: PreemptMarkers,
        stats: RunStats,
        next_deadline_ns: Option<u64>,
        advanced_to: Option<u64>,
    }

    impl Default for FakeHartRuntime {
        fn default() -> Self {
            Self {
                timer_wakes: 0,
                wake_dispatch: WakeDispatchReport::empty(),
                markers: PreemptMarkers::empty(),
                stats: RunStats::empty(),
                next_deadline_ns: None,
                advanced_to: None,
            }
        }
    }

    impl HartLoopRuntime for FakeHartRuntime {
        fn advance_hart_loop_time(&mut self, now_ns: u64) -> usize {
            self.advanced_to = Some(now_ns);
            self.timer_wakes
        }

        fn hart_loop_next_deadline_ns(&self) -> Option<u64> {
            self.next_deadline_ns
        }

        fn drain_hart_loop_wakes<S>(
            &mut self,
            _current_hart: HartId,
            _signal: &mut S,
        ) -> WakeDispatchReport
        where
            S: RescheduleSignal,
        {
            self.wake_dispatch
        }

        fn consume_hart_loop_markers(&mut self, _hart: HartId) -> PreemptMarkers {
            self.markers
        }

        fn run_hart_loop_ready<S>(&mut self, _hart: HartId, _signal: &mut S) -> RunStats
        where
            S: RescheduleSignal,
        {
            self.stats
        }
    }

    #[test]
    fn hart_loop_at_op_delegates_to_step_hart_loop_at() {
        let mut runtime = FakeHartRuntime::default();
        let mut signal = NoopRescheduleSignal::new();
        let mut op = HartLoopAtOp {
            runtime: &mut runtime,
            hart: HartId(0),
            now_ns: 100,
            signal: &mut signal,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            V3::Done(step) => {
                assert_eq!(step.hart, HartId(0));
                assert_eq!(step.now_ns, 100);
                assert!(step.should_idle());
            }
            other => panic!("expected Done(_), got {other:?}"),
        }
        assert_eq!(runtime.advanced_to, Some(100));
    }

    #[test]
    fn hart_loop_op_delegates_to_step_hart_loop_via_clock() {
        let mut runtime = FakeHartRuntime::default();
        let mut signal = NoopRescheduleSignal::new();
        let mut clock = || 400u64;
        let mut op = HartLoopOp {
            runtime: &mut runtime,
            hart: HartId(1),
            clock: &mut clock,
            signal: &mut signal,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            V3::Done(step) => {
                assert_eq!(step.now_ns, 400);
                assert_eq!(step.hart, HartId(1));
            }
            other => panic!("expected Done(_), got {other:?}"),
        }
        assert_eq!(runtime.advanced_to, Some(400));
    }
}
