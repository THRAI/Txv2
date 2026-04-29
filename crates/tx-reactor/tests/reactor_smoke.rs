use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::sync::{Arc, Mutex};

use tx_reactor::wait::{Channel, Mask, WaitOutcome};
use tx_reactor::{Reactor, RunStats, TaskId, TaskStatus};

static READY_POLLS: AtomicUsize = AtomicUsize::new(0);
static PENDING_POLLS: AtomicUsize = AtomicUsize::new(0);

struct CountOnce;

impl Future for CountOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        READY_POLLS.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(())
    }
}

struct ParkForever;

impl Future for ParkForever {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        PENDING_POLLS.fetch_add(1, Ordering::SeqCst);
        Poll::Pending
    }
}

struct ExternallyWoken {
    polls: Arc<AtomicUsize>,
    ready: Arc<AtomicUsize>,
    waker_slot: Arc<Mutex<Option<Waker>>>,
}

impl Future for ExternallyWoken {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.ready.load(Ordering::SeqCst) != 0 {
            Poll::Ready(())
        } else {
            *self.waker_slot.lock().expect("waker slot poisoned") = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[test]
fn submitted_ready_task_runs_to_completion() {
    READY_POLLS.store(0, Ordering::SeqCst);

    let mut reactor = Reactor::new();
    let task_id = reactor.submit(CountOnce);

    assert_eq!(task_id, TaskId(0));
    assert!(!reactor.is_idle());

    let stats = reactor.run_until_idle();

    assert_eq!(
        stats,
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(READY_POLLS.load(Ordering::SeqCst), 1);
    assert!(reactor.is_idle());
}

#[test]
fn pending_task_is_parked_after_one_poll() {
    PENDING_POLLS.store(0, Ordering::SeqCst);

    let mut reactor = Reactor::new();
    let task_id = reactor.submit(ParkForever);

    assert_eq!(task_id, TaskId(0));

    let first = reactor.run_until_idle();
    let second = reactor.run_until_idle();

    assert_eq!(
        first,
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(
        second,
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(PENDING_POLLS.load(Ordering::SeqCst), 1);
    assert!(reactor.is_idle());
}

#[test]
fn task_waker_marks_only_its_task_runnable() {
    let polls_a = Arc::new(AtomicUsize::new(0));
    let ready_a = Arc::new(AtomicUsize::new(0));
    let waker_a = Arc::new(Mutex::new(None));
    let polls_b = Arc::new(AtomicUsize::new(0));
    let ready_b = Arc::new(AtomicUsize::new(0));
    let waker_b = Arc::new(Mutex::new(None));

    let mut reactor = Reactor::new();
    let task_a = reactor.submit(ExternallyWoken {
        polls: Arc::clone(&polls_a),
        ready: Arc::clone(&ready_a),
        waker_slot: Arc::clone(&waker_a),
    });
    let task_b = reactor.submit(ExternallyWoken {
        polls: Arc::clone(&polls_b),
        ready: Arc::clone(&ready_b),
        waker_slot: Arc::clone(&waker_b),
    });

    assert_eq!(reactor.run_until_idle().polled, 2);
    assert_eq!(reactor.task_status(task_a), Some(TaskStatus::Parked));
    assert_eq!(reactor.task_status(task_b), Some(TaskStatus::Parked));

    ready_a.store(1, Ordering::SeqCst);
    waker_a
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task A waker")
        .wake();
    assert!(!reactor.is_idle());

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(polls_a.load(Ordering::SeqCst), 2);
    assert_eq!(polls_b.load(Ordering::SeqCst), 1);
    assert_eq!(reactor.task_status(task_a), Some(TaskStatus::Completed));
    assert_eq!(reactor.task_status(task_b), Some(TaskStatus::Parked));
}

#[test]
fn repeated_wakes_enqueue_task_once_before_next_poll() {
    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));

    let mut reactor = Reactor::new();
    let task = reactor.submit(ExternallyWoken {
        polls: Arc::clone(&polls),
        ready: Arc::clone(&ready),
        waker_slot: Arc::clone(&waker_slot),
    });

    assert_eq!(reactor.run_until_idle().polled, 1);
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    let waker = waker_slot
        .lock()
        .expect("waker slot poisoned")
        .as_ref()
        .expect("task waker")
        .clone();
    waker.wake_by_ref();
    waker.wake_by_ref();

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
}

#[test]
fn wait_channel_wakes_registered_task_from_another_task() {
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let waiter_done = Arc::new(AtomicUsize::new(0));
    let publisher_done = Arc::new(AtomicUsize::new(0));

    let mut reactor = Reactor::new();
    let waiter = {
        let channel = channel.clone();
        let waiter_done = Arc::clone(&waiter_done);
        reactor.submit(async move {
            assert_eq!(channel.wait(mask).await, WaitOutcome::Ready);
            waiter_done.store(1, Ordering::SeqCst);
        })
    };
    let publisher = {
        let channel = channel.clone();
        let publisher_done = Arc::clone(&publisher_done);
        reactor.submit(async move {
            assert_eq!(channel.fire(mask), 1);
            publisher_done.store(1, Ordering::SeqCst);
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 3,
            completed: 2
        }
    );
    assert_eq!(waiter_done.load(Ordering::SeqCst), 1);
    assert_eq!(publisher_done.load(Ordering::SeqCst), 1);
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Completed));
    assert_eq!(reactor.task_status(publisher), Some(TaskStatus::Completed));
}

#[test]
fn wait_channel_preserves_matching_wake_across_later_nonmatching_fire() {
    let channel = Channel::new();
    let waited_mask = Mask::from_bits(0x1);
    let other_mask = Mask::from_bits(0x2);
    let waiter_done = Arc::new(AtomicUsize::new(0));

    let mut reactor = Reactor::new();
    let waiter = {
        let channel = channel.clone();
        let waiter_done = Arc::clone(&waiter_done);
        reactor.submit(async move {
            assert_eq!(channel.wait(waited_mask).await, WaitOutcome::Ready);
            waiter_done.store(1, Ordering::SeqCst);
        })
    };
    reactor.submit({
        let channel = channel.clone();
        async move {
            assert_eq!(channel.fire(waited_mask), 1);
        }
    });
    reactor.submit({
        let channel = channel.clone();
        async move {
            assert_eq!(channel.fire(other_mask), 0);
        }
    });

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 4,
            completed: 3
        }
    );
    assert_eq!(waiter_done.load(Ordering::SeqCst), 1);
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Completed));
}
