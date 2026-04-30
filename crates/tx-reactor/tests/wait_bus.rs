use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::{
    sync::{Arc, Mutex},
    task::Wake,
};

use tx_reactor::wait::{Channel, Mask, WaitOutcome, WaitProtocol};
use tx_reactor::{Reactor, RunStats, TaskStatus};
use tx_substrate::bus::{RawPort, RawQueue};

struct CountWake {
    wakes: Arc<AtomicUsize>,
}

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker(wakes: Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(CountWake { wakes }))
}

#[test]
fn wait_event_rechecks_after_register_before_parking() {
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let checks = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));

    let mut reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let checks = Arc::clone(&checks);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(mask, WaitProtocol::Interruptible, move || {
                    checks.fetch_add(1, Ordering::SeqCst) != 0
                })
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert_eq!(channel.fire(mask), 0);
}

#[test]
fn wait_event_treats_wake_as_retry_signal_only() {
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let condition_ready = Arc::new(AtomicUsize::new(0));
    let condition_checks = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));

    let mut reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let condition_ready = Arc::clone(&condition_ready);
        let condition_checks = Arc::clone(&condition_checks);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(mask, WaitProtocol::Interruptible, move || {
                    condition_checks.fetch_add(1, Ordering::SeqCst);
                    condition_ready.load(Ordering::SeqCst) != 0
                })
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(channel.fire(mask), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    condition_ready.store(1, Ordering::SeqCst);
    assert_eq!(channel.fire(mask), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
    assert!(condition_checks.load(Ordering::SeqCst) >= 5);
}

#[test]
fn dropping_wait_future_unsubscribes_from_channel() {
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    let mut wait = channel.wait(mask);

    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);
    drop(wait);

    assert_eq!(channel.fire(mask), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn raw_port_coalesces_fires_until_subscription_observes_ready() {
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut sub = port.subscribe(0x1, waker);

    assert_eq!(port.fire(0x1), 1);
    assert_eq!(port.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(sub.take_ready());
    assert!(!sub.take_ready());

    assert_eq!(port.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    drop(sub);
    assert_eq!(port.fire(0x1), 0);
}

#[test]
fn raw_queue_fires_new_bits_and_drop_removes_subscription() {
    let queue = RawQueue::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut sub = queue.subscribe(0x1, waker);

    assert_eq!(queue.peek(), 0);
    assert_eq!(queue.fire(0x1), 1);
    assert_eq!(queue.peek(), 0x1);
    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(sub.take_ready());
    assert_eq!(queue.fire(0x1), 0);

    queue.clear(0x1);
    assert_eq!(queue.peek(), 0);
    assert_eq!(queue.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);

    drop(sub);
    queue.clear(0x1);
    assert_eq!(queue.fire(0x1), 0);
}
