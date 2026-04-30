use std::{cell::Cell, num::NonZeroU32, rc::Rc};

use tx_reactor::{
    completion::{Completion, CountdownCompletion},
    wait::{WaitOutcome, WaitProtocol},
    Reactor, RunStats, TaskStatus,
};

fn nz(count: u32) -> NonZeroU32 {
    NonZeroU32::new(count).expect("test count must be nonzero")
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
    let completion = Rc::new(Completion::new());
    let outcome = Rc::new(Cell::new(None));

    completion.complete();

    let mut reactor = Reactor::new();
    let task = {
        let completion = Rc::clone(&completion);
        let outcome = Rc::clone(&outcome);
        reactor.submit(async move {
            outcome.set(Some(completion.wait(WaitProtocol::Uninterruptible).await));
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(outcome.get(), Some(WaitOutcome::Ready));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert!(!completion.try_consume());
}

#[test]
fn counted_completion_wake_is_rechecked_and_consumed_by_one_waiter() {
    let completion = Rc::new(Completion::new());
    let first_outcome = Rc::new(Cell::new(None));
    let second_outcome = Rc::new(Cell::new(None));
    let mut reactor = Reactor::new();

    let first = {
        let completion = Rc::clone(&completion);
        let first_outcome = Rc::clone(&first_outcome);
        reactor.submit(async move {
            first_outcome.set(Some(completion.wait(WaitProtocol::Interruptible).await));
        })
    };
    let second = {
        let completion = Rc::clone(&completion);
        let second_outcome = Rc::clone(&second_outcome);
        reactor.submit(async move {
            second_outcome.set(Some(completion.wait(WaitProtocol::Interruptible).await));
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
    let ready_count = usize::from(first_outcome.get() == Some(WaitOutcome::Ready))
        + usize::from(second_outcome.get() == Some(WaitOutcome::Ready));
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
    assert_eq!(first_outcome.get(), Some(WaitOutcome::Ready));
    assert_eq!(second_outcome.get(), Some(WaitOutcome::Ready));
    assert_eq!(reactor.task_status(first), Some(TaskStatus::Completed));
    assert_eq!(reactor.task_status(second), Some(TaskStatus::Completed));
}

#[test]
fn counted_completion_wait_propagates_timeout() {
    let mut reactor = Reactor::new();
    let completion = Rc::new(Completion::with_channel(reactor.channel()));
    let outcome = Rc::new(Cell::new(None));
    let deadline_ns = 10;

    let task = {
        let completion = Rc::clone(&completion);
        let outcome = Rc::clone(&outcome);
        reactor.submit(async move {
            outcome.set(Some(
                completion
                    .wait(WaitProtocol::InterruptibleTimeout(deadline_ns))
                    .await,
            ));
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(outcome.get(), None);
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    assert_eq!(reactor.advance_time_to(deadline_ns), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(outcome.get(), Some(WaitOutcome::TimedOut));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert!(!completion.try_consume());
}

#[test]
fn countdown_completion_waits_until_final_arrival() {
    let countdown = Rc::new(CountdownCompletion::new(nz(2)));
    let outcome = Rc::new(Cell::new(None));
    let mut reactor = Reactor::new();

    let task = {
        let countdown = Rc::clone(&countdown);
        let outcome = Rc::clone(&outcome);
        reactor.submit(async move {
            outcome.set(Some(countdown.wait(WaitProtocol::Killable).await));
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
    assert_eq!(outcome.get(), None);

    countdown.arrive();
    assert!(countdown.is_complete());
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(outcome.get(), Some(WaitOutcome::Ready));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
}

#[test]
#[should_panic(expected = "countdown completion underflow")]
fn countdown_arrive_panics_on_underflow() {
    let countdown = CountdownCompletion::new(nz(1));

    countdown.arrive();
    countdown.arrive();
}
