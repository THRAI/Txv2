use core::task::Waker;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Wake,
};

use tx_substrate::bus::{
    RawPort, RawQueue, RawSubscriptionError, RawSubscriptionState, RawWireError,
};

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
fn raw_queue_subscription_reports_unsubscribed_after_explicit_unsubscribe() {
    let queue = RawQueue::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let replacement_wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = queue.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(subscription.state(), RawSubscriptionState::Subscribed);
    assert!(subscription.is_subscribed());
    assert_eq!(queue.subscriber_count(), 1);

    assert!(subscription.unsubscribe());
    assert_eq!(subscription.state(), RawSubscriptionState::Unsubscribed);
    assert!(!subscription.is_subscribed());
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(
        subscription.try_take_ready(),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert_eq!(
        subscription.try_update(0x2, counting_waker(Arc::clone(&replacement_wakes))),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert!(!subscription.unsubscribe());

    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn raw_port_subscription_reports_unsubscribed_after_explicit_unsubscribe() {
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let replacement_wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(subscription.state(), RawSubscriptionState::Subscribed);
    assert!(subscription.is_subscribed());
    assert_eq!(port.subscriber_count(), 1);

    assert!(subscription.unsubscribe());
    assert_eq!(subscription.state(), RawSubscriptionState::Unsubscribed);
    assert!(!subscription.is_subscribed());
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(
        subscription.try_take_ready(),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert_eq!(
        subscription.try_update(0x2, counting_waker(Arc::clone(&replacement_wakes))),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert!(!subscription.unsubscribe());

    assert_eq!(port.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn raw_queue_terminal_mask_wakes_drains_and_reports_terminal_state() {
    let queue = RawQueue::new();
    let first_wakes = Arc::new(AtomicUsize::new(0));
    let second_wakes = Arc::new(AtomicUsize::new(0));
    let late_wakes = Arc::new(AtomicUsize::new(0));
    let mut first = queue.subscribe(0x1, counting_waker(Arc::clone(&first_wakes)));
    let mut second = queue.subscribe(0x4, counting_waker(Arc::clone(&second_wakes)));

    assert_eq!(queue.subscriber_count(), 2);
    assert_eq!(queue.terminate(0x8), 2);

    assert!(queue.is_terminal());
    assert_eq!(queue.peek(), 0x8);
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(first_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(second_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(first.state(), RawSubscriptionState::Terminal);
    assert_eq!(second.state(), RawSubscriptionState::Terminal);
    assert_eq!(first.try_take_ready(), Err(RawSubscriptionError::Terminal));
    assert!(first.take_ready());
    assert_eq!(
        second.try_update(0x8, counting_waker(Arc::clone(&late_wakes))),
        Err(RawSubscriptionError::Terminal)
    );

    assert_eq!(queue.try_fire(0x1), Err(RawWireError::Terminal));
    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(queue.try_clear(0x8), Err(RawWireError::Terminal));
    queue.clear(0x8);
    assert_eq!(queue.peek(), 0x8);

    let mut late = queue.subscribe(0x1, counting_waker(Arc::clone(&late_wakes)));
    assert_eq!(late.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        queue
            .try_subscribe(0x1, counting_waker(Arc::clone(&late_wakes)))
            .err(),
        Some(RawWireError::Terminal)
    );
    assert!(late.take_ready());
    assert_eq!(late_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(queue.terminate(0x8), 0);
}

#[test]
fn raw_port_terminal_event_wakes_drains_and_reports_terminal_state() {
    let port = RawPort::new();
    let first_wakes = Arc::new(AtomicUsize::new(0));
    let second_wakes = Arc::new(AtomicUsize::new(0));
    let late_wakes = Arc::new(AtomicUsize::new(0));
    let mut first = port.subscribe(0x1, counting_waker(Arc::clone(&first_wakes)));
    let mut second = port.subscribe(0x4, counting_waker(Arc::clone(&second_wakes)));

    assert_eq!(port.subscriber_count(), 2);
    assert_eq!(port.terminate(0x80), 2);

    assert!(port.is_terminal());
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(first_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(second_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(first.state(), RawSubscriptionState::Terminal);
    assert_eq!(second.state(), RawSubscriptionState::Terminal);
    assert_eq!(first.try_take_ready(), Err(RawSubscriptionError::Terminal));
    assert!(first.take_ready());
    assert_eq!(
        second.try_update(0x80, counting_waker(Arc::clone(&late_wakes))),
        Err(RawSubscriptionError::Terminal)
    );

    assert_eq!(port.try_fire(0x1), Err(RawWireError::Terminal));
    assert_eq!(port.fire(0x1), 0);

    let mut late = port.subscribe(0x1, counting_waker(Arc::clone(&late_wakes)));
    assert_eq!(late.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        port.try_subscribe(0x1, counting_waker(Arc::clone(&late_wakes)))
            .err(),
        Some(RawWireError::Terminal)
    );
    assert!(late.take_ready());
    assert_eq!(late_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port.terminate(0x80), 0);
}

#[test]
fn raw_port_terminal_without_gone_event_drains_silently() {
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(port.terminate(0), 0);

    assert!(port.is_terminal());
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(subscription.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        subscription.try_take_ready(),
        Err(RawSubscriptionError::Terminal)
    );
    assert!(subscription.take_ready());
}
