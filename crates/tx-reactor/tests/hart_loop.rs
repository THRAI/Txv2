use tx_reactor::{
    hart_loop::{
        step_hart_loop, step_hart_loop_at, HartLoopDeadlineAction, HartLoopDecision,
        HartLoopRuntime,
    },
    preempt::{PreemptMarkers, PreemptionPoint},
    HartId, NoopRescheduleSignal, RescheduleSignal, RunStats, WakeDispatchReport,
};

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

    fn hart_loop_next_deadline_ns(&self) -> Option<u64> {
        self.next_deadline_ns
    }
}

fn need_resched_markers() -> PreemptMarkers {
    let markers = PreemptionPoint::new();
    markers.mark_need_resched();
    markers.consume()
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
