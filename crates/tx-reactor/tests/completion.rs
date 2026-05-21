use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use tx_reactor::{
    completion::{Completion, CountdownCompletion},
    wait::{WaitOutcome, WaitProtocol},
    Reactor, RunStats, TaskStatus,
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

#[test]
fn counted_completion_consumes_available_credits_once() {
    let completion = Completion::new();

    assert!(!completion.try_consume());

    completion.complete();
    assert!(completion.try_consume());
    assert!(!completion.try_consume());

    completion.complete();
    completion.complete();
    assert!(completion.try_consume());
    assert!(completion.try_consume());
    assert!(!completion.try_consume());
}

#[test]
fn counted_wait_consumes_preexisting_credit() {
    let completion = Arc::new(Completion::new());
    let outcome = Arc::new(Mutex::new(None));

    completion.complete();

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

    completion.complete();
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

    completion.complete();
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

    countdown.arrive();
    assert!(!countdown.is_complete());
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(recorded(&outcome), None);

    countdown.arrive();
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

    countdown.arrive();
    countdown.arrive();
}
