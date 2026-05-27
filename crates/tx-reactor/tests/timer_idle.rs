use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

use tx_reactor::{
    wait::{Mask, WaitOutcome, WaitProtocol},
    Reactor, RunStats, TaskId,
};

struct ManualClock {
    now_ns: Cell<u64>,
    deadline_ns: Cell<Option<u64>>,
    programmed: Cell<usize>,
    cancelled: Cell<usize>,
}

impl ManualClock {
    fn new(now_ns: u64) -> Self {
        Self {
            now_ns: Cell::new(now_ns),
            deadline_ns: Cell::new(None),
            programmed: Cell::new(0),
            cancelled: Cell::new(0),
        }
    }

    fn set_now_ns(&self, now_ns: u64) {
        self.now_ns.set(now_ns);
    }

    fn now_ns(&self) -> u64 {
        self.now_ns.get()
    }

    fn program_deadline(&self, deadline_ns: Option<u64>) {
        match deadline_ns {
            Some(deadline_ns) => {
                self.deadline_ns.set(Some(deadline_ns));
                self.programmed.set(self.programmed.get() + 1);
            }
            None => {
                self.deadline_ns.set(None);
                self.cancelled.set(self.cancelled.get() + 1);
            }
        }
    }

    fn deadline_ns(&self) -> Option<u64> {
        self.deadline_ns.get()
    }

    fn programmed(&self) -> usize {
        self.programmed.get()
    }

    fn cancelled(&self) -> usize {
        self.cancelled.get()
    }
}

macro_rules! run_with_clock {
    ($reactor:expr, $clock:expr) => {{
        let clock = $clock;
        $reactor.run_until_idle_with_clock(
            || clock.now_ns(),
            |deadline| clock.program_deadline(deadline),
        )
    }};
}

fn submit_timeout_waiter(
    reactor: &Reactor,
    deadline_ns: u64,
    outcome: Arc<Mutex<Option<WaitOutcome>>>,
) -> TaskId {
    let channel = reactor.channel();
    let mask = Mask::from_bits(0x1);
    reactor.submit(async move {
        let wait_outcome = channel
            .wait_event(
                mask,
                WaitProtocol::InterruptibleTimeout(deadline_ns),
                || false,
            )
            .await;
        *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
    })
}

#[test]
fn clock_run_reports_earliest_deadline_and_arms_it() {
    let clock = ManualClock::new(0);
    let reactor = Reactor::new();
    let first_outcome = Arc::new(Mutex::new(None));
    let second_outcome = Arc::new(Mutex::new(None));

    let _first = submit_timeout_waiter(&reactor, 50, Arc::clone(&first_outcome));
    let _second = submit_timeout_waiter(&reactor, 20, Arc::clone(&second_outcome));

    let report = run_with_clock!(&reactor, &clock);

    assert_eq!(
        report.stats(),
        RunStats {
            polled: 2,
            completed: 0
        }
    );
    assert_eq!(report.timer_wakes(), 0);
    assert!(report.is_idle());
    assert_eq!(report.next_deadline_ns(), Some(20));
    assert_eq!(reactor.next_deadline_ns(), Some(20));
    assert_eq!(clock.deadline_ns(), Some(20));
    assert_eq!(clock.programmed(), 1);
    assert_eq!(clock.cancelled(), 0);
    assert_eq!(*first_outcome.lock().expect("first outcome poisoned"), None);
    assert_eq!(
        *second_outcome.lock().expect("second outcome poisoned"),
        None
    );
}

#[test]
fn clock_run_returns_idle_without_spinning_when_no_work_is_ready() {
    let clock = ManualClock::new(0);
    let reactor = Reactor::new();
    let outcome = Arc::new(Mutex::new(None));

    let _task = submit_timeout_waiter(&reactor, 100, Arc::clone(&outcome));
    let first = run_with_clock!(&reactor, &clock);
    let second = run_with_clock!(&reactor, &clock);

    assert_eq!(
        first.stats(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(first.next_deadline_ns(), Some(100));
    assert!(first.is_idle());
    assert_eq!(
        second.stats(),
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(second.timer_wakes(), 0);
    assert_eq!(second.next_deadline_ns(), Some(100));
    assert!(second.is_idle());
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);
    assert_eq!(clock.deadline_ns(), Some(100));
}

#[test]
fn clock_run_drives_expired_timer_wake_and_cancels_when_empty() {
    let clock = ManualClock::new(0);
    let reactor = Reactor::new();
    let outcome = Arc::new(Mutex::new(None));

    let task = submit_timeout_waiter(&reactor, 10, Arc::clone(&outcome));

    assert_eq!(
        run_with_clock!(&reactor, &clock).next_deadline_ns(),
        Some(10)
    );
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    clock.set_now_ns(10);
    let report = run_with_clock!(&reactor, &clock);

    assert_eq!(report.timer_wakes(), 1);
    assert_eq!(
        report.stats(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert!(report.is_idle());
    assert_eq!(report.next_deadline_ns(), None);
    assert_eq!(
        reactor.task_status(task),
        Some(tx_reactor::TaskStatus::Completed)
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::TimedOut)
    );
    assert_eq!(clock.deadline_ns(), None);
    assert_eq!(clock.cancelled(), 1);
}

#[test]
fn host_driven_advance_time_to_behavior_is_preserved() {
    let reactor = Reactor::new();
    let outcome = Arc::new(Mutex::new(None));
    let task = submit_timeout_waiter(&reactor, 5, Arc::clone(&outcome));

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(reactor.advance_time_to(4), 0);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    assert_eq!(reactor.advance_time_to(5), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(
        reactor.task_status(task),
        Some(tx_reactor::TaskStatus::Completed)
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::TimedOut)
    );
}
