use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use tx_reactor::{
    completion::{Completion, CountdownCompletion},
    wait::{WaitOutcome, WaitProtocol},
    HartId, InitialSchedMeta, Reactor, RescheduleSignal, RunStats, TaskStatus, WakeDispatchReport,
};

fn nz(count: u32) -> NonZeroU32 {
    NonZeroU32::new(count).expect("test count must be nonzero")
}

fn record(outcome: &Arc<Mutex<Option<WaitOutcome>>>, value: WaitOutcome) {
    *outcome.lock().expect("outcome lock poisoned") = Some(value);
}

fn recorded(outcome: &Arc<Mutex<Option<WaitOutcome>>>) -> Option<WaitOutcome> {
    *outcome.lock().expect("outcome lock poisoned")
}

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
fn counted_completion_consumes_available_credits_once() {
    let completion = Completion::new();

    assert!(!completion.try_consume());

    completion.complete_with_post(|mailbox, event| mailbox.post(event));
    assert!(completion.try_consume());
    assert!(!completion.try_consume());

    completion.complete_with_post(|mailbox, event| mailbox.post(event));
    completion.complete_with_post(|mailbox, event| mailbox.post(event));
    assert!(completion.try_consume());
    assert!(completion.try_consume());
    assert!(!completion.try_consume());
}

#[test]
fn counted_wait_consumes_preexisting_credit() {
    let completion = Arc::new(Completion::new());
    let outcome = Arc::new(Mutex::new(None));

    completion.complete_with_post(|mailbox, event| mailbox.post(event));

    let reactor = Reactor::new();
    let task = {
        let completion = Arc::clone(&completion);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            record(
                &outcome,
                completion.wait(WaitProtocol::Uninterruptible).await,
            );
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(recorded(&outcome), Some(WaitOutcome::Ready));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert!(!completion.try_consume());
}

#[test]
fn counted_completion_wake_is_rechecked_and_consumed_by_one_waiter() {
    let completion = Arc::new(Completion::new());
    let first_outcome = Arc::new(Mutex::new(None));
    let second_outcome = Arc::new(Mutex::new(None));
    let reactor = Reactor::new();

    let first = {
        let completion = Arc::clone(&completion);
        let first_outcome = Arc::clone(&first_outcome);
        reactor.submit(async move {
            record(
                &first_outcome,
                completion.wait(WaitProtocol::Interruptible).await,
            );
        })
    };
    let second = {
        let completion = Arc::clone(&completion);
        let second_outcome = Arc::clone(&second_outcome);
        reactor.submit(async move {
            record(
                &second_outcome,
                completion.wait(WaitProtocol::Interruptible).await,
            );
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 2,
            completed: 0
        }
    );
    assert_eq!(reactor.task_status(first), Some(TaskStatus::Parked));
    assert_eq!(reactor.task_status(second), Some(TaskStatus::Parked));

    completion.complete_with_post(|mailbox, event| mailbox.post(event));
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 2,
            completed: 1
        }
    );
    let ready_count = usize::from(recorded(&first_outcome) == Some(WaitOutcome::Ready))
        + usize::from(recorded(&second_outcome) == Some(WaitOutcome::Ready));
    assert_eq!(ready_count, 1);
    assert!(!completion.try_consume());

    completion.complete_with_post(|mailbox, event| mailbox.post(event));
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(recorded(&first_outcome), Some(WaitOutcome::Ready));
    assert_eq!(recorded(&second_outcome), Some(WaitOutcome::Ready));
    assert_eq!(reactor.task_status(first), Some(TaskStatus::Completed));
    assert_eq!(reactor.task_status(second), Some(TaskStatus::Completed));
}

#[test]
fn counted_completion_complete_with_post_routes_owner_aware() {
    let reactor = Reactor::new();
    let completion = Arc::new(Completion::new());
    let outcome = Arc::new(Mutex::new(None));
    let task = reactor.submit_task_with_meta(
        {
            let completion = Arc::clone(&completion);
            let outcome = Arc::clone(&outcome);
            async move {
                record(&outcome, completion.wait(WaitProtocol::Interruptible).await);
            }
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mut signal = RecordingRescheduleSignal::default();
    let mut report = WakeDispatchReport::empty();
    let woken = completion.complete_with_post(|mailbox, event| {
        let (posted, next) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        report.merge(next);
        posted
    });

    assert_eq!(woken, 1);
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
    assert_eq!(recorded(&outcome), Some(WaitOutcome::Ready));
}

#[test]
fn counted_completion_wait_propagates_timeout() {
    let reactor = Reactor::new();
    let completion = Arc::new(Completion::with_channel(reactor.channel()));
    let outcome = Arc::new(Mutex::new(None));
    let deadline_ns = 10;

    let task = {
        let completion = Arc::clone(&completion);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            record(
                &outcome,
                completion
                    .wait(WaitProtocol::InterruptibleTimeout(deadline_ns))
                    .await,
            );
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(recorded(&outcome), None);
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    assert_eq!(reactor.advance_time_to(deadline_ns), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(recorded(&outcome), Some(WaitOutcome::TimedOut));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert!(!completion.try_consume());
}

#[test]
fn countdown_completion_arrive_with_post_routes_owner_aware_on_final_arrival() {
    let reactor = Reactor::new();
    let countdown = Arc::new(CountdownCompletion::new(nz(2)));
    let outcome = Arc::new(Mutex::new(None));
    let task = reactor.submit_task_with_meta(
        {
            let countdown = Arc::clone(&countdown);
            let outcome = Arc::clone(&outcome);
            async move {
                record(&outcome, countdown.wait(WaitProtocol::Killable).await);
            }
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mut signal = RecordingRescheduleSignal::default();
    let mut report = WakeDispatchReport::empty();
    let first_woken = countdown.arrive_with_post(|mailbox, event| {
        let (posted, next) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        report.merge(next);
        posted
    });
    assert_eq!(first_woken, 0);
    assert_eq!(report, WakeDispatchReport::empty());
    assert!(signal.sent.is_empty());

    let second_woken = countdown.arrive_with_post(|mailbox, event| {
        let (posted, next) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        report.merge(next);
        posted
    });
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
    assert_eq!(recorded(&outcome), Some(WaitOutcome::Ready));
}

#[test]
fn countdown_completion_waits_until_final_arrival() {
    let countdown = Arc::new(CountdownCompletion::new(nz(2)));
    let outcome = Arc::new(Mutex::new(None));
    let reactor = Reactor::new();

    let task = {
        let countdown = Arc::clone(&countdown);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            record(&outcome, countdown.wait(WaitProtocol::Killable).await);
        })
    };

    assert!(!countdown.is_complete());
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    countdown.arrive_with_post(|mailbox, event| mailbox.post(event));
    assert!(!countdown.is_complete());
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(recorded(&outcome), None);

    countdown.arrive_with_post(|mailbox, event| mailbox.post(event));
    assert!(countdown.is_complete());
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(recorded(&outcome), Some(WaitOutcome::Ready));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
}

#[test]
#[should_panic(expected = "countdown completion underflow")]
fn countdown_arrive_panics_on_underflow() {
    let countdown = CountdownCompletion::new(nz(1));

    countdown.arrive_with_post(|mailbox, event| mailbox.post(event));
    countdown.arrive_with_post(|mailbox, event| mailbox.post(event));
}
