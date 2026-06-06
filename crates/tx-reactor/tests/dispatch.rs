use tx_reactor::{
    DispatchState, HartId, RescheduleSignal, RunnablePlacement, WakeDispatchAction,
    WakeDispatchReport,
};

#[derive(Default)]
struct RecordingSignal {
    sent: Vec<HartId>,
}

impl RescheduleSignal for RecordingSignal {
    fn send_reschedule_ipi(&mut self, target_hart: HartId) -> bool {
        self.sent.push(target_hart);
        true
    }
}

#[test]
fn remote_wake_marks_target_need_resched_and_sends_ipi() {
    let mut dispatch = DispatchState::new();
    let mut signal = RecordingSignal::default();

    let action = dispatch.apply_runnable_placement(
        RunnablePlacement {
            target_hart: HartId(2),
            wake_remote: true,
        },
        &mut signal,
    );

    assert_eq!(
        action,
        WakeDispatchAction {
            target_hart: HartId(2),
            wake_remote: true,
        }
    );
    assert_eq!(signal.sent, vec![HartId(2)]);
    assert!(!dispatch.snapshot_markers(HartId(0)).need_resched());
    assert!(dispatch.snapshot_markers(HartId(2)).need_resched());
    assert!(dispatch.consume_markers(HartId(2)).need_resched());
    assert!(dispatch.consume_markers(HartId(2)).is_empty());
}

#[test]
fn local_wake_marks_need_resched_without_ipi() {
    let mut dispatch = DispatchState::new();
    let mut signal = RecordingSignal::default();
    let mut report = WakeDispatchReport::empty();

    let action = dispatch.apply_runnable_placement(
        RunnablePlacement {
            target_hart: HartId(0),
            wake_remote: false,
        },
        &mut signal,
    );
    report.record(action);

    assert!(signal.sent.is_empty());
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 1,
            remote_ipis: 0,
        }
    );
    assert!(dispatch.snapshot_markers(HartId(0)).need_resched());
}
