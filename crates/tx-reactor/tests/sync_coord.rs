use std::sync::{Arc, Mutex};

use tx_reactor::{
    sync_coord::{AckOutcome, AckResult, SyncRendezvous, SyncTargetToken},
    wait::WaitOutcome,
    HartId, InitialSchedMeta, Reactor, RescheduleSignal, RunStats, TaskStatus, WakeDispatchReport,
};

fn target(raw: u32) -> SyncTargetToken {
    SyncTargetToken::new(raw)
}

fn ack_direct(rendezvous: &SyncRendezvous, target: SyncTargetToken) -> AckResult {
    rendezvous
        .ack_with_post(target, |mailbox, event| mailbox.post(event))
        .0
}

fn assert_send_sync<T: Send + Sync>() {}

#[derive(Default)]
struct RecordingRescheduleSignal {
    sent: Vec<HartId>,
}

impl RescheduleSignal for RecordingRescheduleSignal {
    fn send_reschedule_ipi(&mut self, target_hart: HartId) -> bool {
        self.sent.push(target_hart);
        true
    }
}

#[test]
fn sync_rendezvous_is_send_sync_for_cross_hart_acks() {
    assert_send_sync::<SyncRendezvous>();
}

#[test]
fn rendezvous_completes_after_all_targets_ack() {
    let rendezvous = SyncRendezvous::new([target(10), target(20)]);

    assert_eq!(rendezvous.target_count(), 2);
    assert_eq!(rendezvous.remaining(), 2);
    assert!(!rendezvous.is_complete());

    assert_eq!(
        ack_direct(&rendezvous, target(10)),
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: false,
            remaining: 1,
        }
    );
    assert_eq!(rendezvous.is_acknowledged(target(10)), Some(true));
    assert_eq!(rendezvous.is_acknowledged(target(20)), Some(false));
    assert!(!rendezvous.is_complete());

    assert_eq!(
        ack_direct(&rendezvous, target(20)),
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: true,
            remaining: 0,
        }
    );
    assert!(rendezvous.is_complete());
}

#[test]
fn duplicate_and_unknown_acks_are_reported_without_changing_completion() {
    let rendezvous = SyncRendezvous::new([target(7)]);

    assert_eq!(
        ack_direct(&rendezvous, target(99)),
        AckResult {
            outcome: AckOutcome::UnknownTarget,
            completed: false,
            remaining: 1,
        }
    );
    assert_eq!(rendezvous.remaining(), 1);

    assert_eq!(
        ack_direct(&rendezvous, target(7)),
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: true,
            remaining: 0,
        }
    );

    assert_eq!(
        ack_direct(&rendezvous, target(7)),
        AckResult {
            outcome: AckOutcome::Duplicate,
            completed: true,
            remaining: 0,
        }
    );
    assert_eq!(
        ack_direct(&rendezvous, target(99)),
        AckResult {
            outcome: AckOutcome::UnknownTarget,
            completed: true,
            remaining: 0,
        }
    );
}

#[test]
fn empty_target_set_is_immediately_complete() {
    let rendezvous = SyncRendezvous::new(core::iter::empty());

    assert_eq!(rendezvous.target_count(), 0);
    assert_eq!(rendezvous.remaining(), 0);
    assert!(rendezvous.is_complete());
    assert_eq!(rendezvous.is_acknowledged(target(1)), None);
    assert_eq!(
        ack_direct(&rendezvous, target(1)),
        AckResult {
            outcome: AckOutcome::UnknownTarget,
            completed: true,
            remaining: 0,
        }
    );
}

#[test]
fn final_ack_with_post_routes_owner_aware() {
    let rendezvous = SyncRendezvous::new([target(1), target(2)]);
    let outcome = Arc::new(Mutex::new(None));
    let reactor = Reactor::new();

    let task = reactor.submit_task_with_meta(
        {
            let rendezvous = rendezvous.clone();
            let outcome = Arc::clone(&outcome);
            async move {
                let wait_outcome = rendezvous.wait().await;
                *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
            }
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mut signal = RecordingRescheduleSignal::default();
    let mut report = WakeDispatchReport::empty();
    let (first, first_woken) = rendezvous.ack_with_post(target(1), |mailbox, event| {
        let (posted, next) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        report.merge(next);
        posted
    });
    assert_eq!(
        first,
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: false,
            remaining: 1,
        }
    );
    assert_eq!(first_woken, 0);
    assert_eq!(report, WakeDispatchReport::empty());
    assert!(signal.sent.is_empty());

    let (second, second_woken) = rendezvous.ack_with_post(target(2), |mailbox, event| {
        let (posted, next) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        report.merge(next);
        posted
    });
    assert_eq!(
        second,
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: true,
            remaining: 0,
        }
    );
    assert_eq!(second_woken, 1);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
}

#[test]
fn waiter_wakes_on_final_ack() {
    let rendezvous = SyncRendezvous::new([target(1), target(2)]);
    let outcome = Arc::new(Mutex::new(None));
    let reactor = Reactor::new();

    let task = {
        let rendezvous = rendezvous.clone();
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = rendezvous.wait().await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0,
        }
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    assert_eq!(
        ack_direct(&rendezvous, target(1)),
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: false,
            remaining: 1,
        }
    );
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 0,
            completed: 0,
        }
    );

    assert_eq!(
        ack_direct(&rendezvous, target(2)),
        AckResult {
            outcome: AckOutcome::Acknowledged,
            completed: true,
            remaining: 0,
        }
    );
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
}
