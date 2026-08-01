use std::cell::Cell;
use tx_reactor::{
    hart_loop::{
        step_hart_loop, step_hart_loop_at, step_hart_loop_at_with_poll_budget,
        HartLoopDeadlineAction, HartLoopDecision, HartLoopRuntime,
    },
    preempt::{PreemptMarkers, PreemptionPoint},
    HartId, HartPollBudget, NoopRescheduleSignal, RescheduleSignal, RunStats, WakeDispatchReport,
};
use tx_services::time::CurrentHartDeadlineTimer;

#[derive(Debug)]
struct FakeHartRuntime {
    timer_wakes: usize,
    wake_dispatch: WakeDispatchReport,
    markers: PreemptMarkers,
    stats: RunStats,
    next_deadline_ns: Option<u64>,
    advanced_to: Option<u64>,
    poll_budget: Option<HartPollBudget>,
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
            poll_budget: None,
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

    fn run_hart_loop_ready<S>(
        &mut self,
        _hart: HartId,
        _signal: &mut S,
        poll_budget: HartPollBudget,
    ) -> RunStats
    where
        S: RescheduleSignal,
    {
        self.poll_budget = Some(poll_budget);
        self.stats
    }
}

struct ChangeAwareDriver {
    next_deadline_ns: Cell<Option<u64>>,
    deadline_changed: Cell<bool>,
}

impl ChangeAwareDriver {
    fn set_next_deadline_ns(&self, deadline_ns: Option<u64>) {
        if self.next_deadline_ns.replace(deadline_ns) != deadline_ns {
            self.deadline_changed.set(true);
        }
    }
}

impl ChangeAwareDriver {
    fn program_current_hart_deadline(&self, timer: &mut impl CurrentHartDeadlineTimer) {
        if !self.deadline_changed.replace(false) {
            return;
        }
        match self.next_deadline_ns.get() {
            Some(deadline_ns) => timer.set_current_hart_deadline_ns(deadline_ns),
            None => timer.cancel_current_hart_deadline(),
        }
    }
}

#[derive(Default)]
struct RecordingDeadlineTimer {
    arms: Vec<u64>,
    cancels: usize,
}

impl CurrentHartDeadlineTimer for RecordingDeadlineTimer {
    fn set_current_hart_deadline_ns(&mut self, deadline_ns: u64) {
        self.arms.push(deadline_ns);
    }

    fn cancel_current_hart_deadline(&mut self) {
        self.cancels += 1;
    }
}

#[derive(Default)]
struct CurrentHartActionRecorder {
    arms: Vec<(HartId, u64)>,
    cancels: Vec<HartId>,
}

impl CurrentHartActionRecorder {
    fn record(&mut self, hart: HartId, action: HartLoopDeadlineAction) {
        match action {
            HartLoopDeadlineAction::Arm { deadline_ns } => self.arms.push((hart, deadline_ns)),
            HartLoopDeadlineAction::Cancel => self.cancels.push(hart),
        }
    }
}

fn need_resched_markers() -> PreemptMarkers {
    let markers = PreemptionPoint::new();
    markers.mark_need_resched();
    markers.consume()
}

#[test]
fn deadline_change_programs_current_hart_only_once_per_transition() {
    let driver = ChangeAwareDriver {
        next_deadline_ns: Cell::new(None),
        deadline_changed: Cell::new(false),
    };
    let mut timer = RecordingDeadlineTimer::default();

    driver.program_current_hart_deadline(&mut timer);
    assert!(timer.arms.is_empty());
    assert_eq!(timer.cancels, 0);

    driver.set_next_deadline_ns(Some(250));
    driver.program_current_hart_deadline(&mut timer);
    driver.program_current_hart_deadline(&mut timer);
    assert_eq!(timer.arms, vec![250]);
    assert_eq!(timer.cancels, 0);

    driver.set_next_deadline_ns(None);
    driver.program_current_hart_deadline(&mut timer);
    assert_eq!(timer.arms, vec![250]);
    assert_eq!(timer.cancels, 1);
}

#[test]
fn idle_no_work_decides_to_idle_and_cancel_deadline() {
    let mut runtime = FakeHartRuntime::default();
    let mut signal = NoopRescheduleSignal::new();

    let step = step_hart_loop_at(&mut runtime, HartId(0), 100, &mut signal);

    assert_eq!(runtime.advanced_to, Some(100));
    assert!(!step.ran_work());
    assert!(!step.consumed_reschedule_marker());
    assert!(!step.observed_timer_wakes());
    assert!(step.should_idle());
    assert_eq!(step.decision, HartLoopDecision::Idle);
    assert_eq!(step.deadline_action, HartLoopDeadlineAction::Cancel);
    assert_eq!(step.next_deadline_ns, None);
}

#[test]
fn rescheduled_work_reports_marker_and_continue_decision() {
    let mut runtime = FakeHartRuntime {
        markers: need_resched_markers(),
        stats: RunStats {
            polled: 1,
            completed: 1,
        },
        ..FakeHartRuntime::default()
    };
    let mut signal = NoopRescheduleSignal::new();

    let step = step_hart_loop_at(&mut runtime, HartId(2), 125, &mut signal);

    assert_eq!(step.hart, HartId(2));
    assert!(step.ran_work());
    assert!(step.consumed_reschedule_marker());
    assert!(!step.should_idle());
    assert_eq!(step.decision, HartLoopDecision::Continue);
    assert_eq!(step.deadline_action, HartLoopDeadlineAction::Cancel);
}

#[test]
fn pending_timer_deadline_idles_with_arm_request() {
    let mut runtime = FakeHartRuntime {
        next_deadline_ns: Some(250),
        ..FakeHartRuntime::default()
    };
    let mut signal = NoopRescheduleSignal::new();

    let step = step_hart_loop_at(&mut runtime, HartId(0), 200, &mut signal);

    assert!(!step.ran_work());
    assert!(step.should_idle());
    assert_eq!(step.next_deadline_ns, Some(250));
    assert_eq!(
        step.deadline_action,
        HartLoopDeadlineAction::Arm { deadline_ns: 250 }
    );
}

#[test]
fn nonzero_hart_arm_action_records_the_passed_hart() {
    let mut runtime = FakeHartRuntime {
        next_deadline_ns: Some(250),
        ..FakeHartRuntime::default()
    };
    let hart = HartId(3);
    let mut signal = NoopRescheduleSignal::new();
    let step = step_hart_loop_at(&mut runtime, hart, 200, &mut signal);
    let mut recorder = CurrentHartActionRecorder::default();

    recorder.record(step.hart, step.deadline_action);

    assert_eq!(recorder.arms, vec![(hart, 250)]);
    assert!(recorder.cancels.is_empty());
}

#[test]
fn expired_timer_work_reports_timer_wake_and_continue_decision() {
    let mut runtime = FakeHartRuntime {
        timer_wakes: 1,
        wake_dispatch: WakeDispatchReport {
            placements: 1,
            local_reschedules: 1,
            remote_ipis: 0,
        },
        markers: need_resched_markers(),
        stats: RunStats {
            polled: 1,
            completed: 0,
        },
        ..FakeHartRuntime::default()
    };
    let mut signal = NoopRescheduleSignal::new();

    let step = step_hart_loop_at(&mut runtime, HartId(0), 300, &mut signal);

    assert!(step.observed_timer_wakes());
    assert_eq!(step.timer_wakes, 1);
    assert_eq!(step.wake_dispatch.placements, 1);
    assert!(step.ran_work());
    assert!(!step.should_idle());
    assert_eq!(step.decision, HartLoopDecision::Continue);
}

#[test]
fn clock_adapter_supplies_step_time() {
    let mut runtime = FakeHartRuntime::default();
    let mut signal = NoopRescheduleSignal::new();
    let mut clock = || 400;

    let step = step_hart_loop(&mut runtime, HartId(1), &mut clock, &mut signal);

    assert_eq!(step.now_ns, 400);
    assert_eq!(runtime.advanced_to, Some(400));
}

#[test]
fn bounded_step_forwards_poll_budget_to_runtime() {
    let mut runtime = FakeHartRuntime::default();
    let mut signal = NoopRescheduleSignal::new();
    let budget = HartPollBudget::up_to(1);

    let _ = step_hart_loop_at_with_poll_budget(&mut runtime, HartId(0), 500, &mut signal, budget);

    assert_eq!(runtime.poll_budget, Some(budget));
}
