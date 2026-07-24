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

use tx_reactor::adapter::bus_wire::{
    bus_lifecycle, bus_readiness, DeclaredPort, DeclaredQueue, DeclaredWireError, RawPort,
    RawQueue, WireDeclaration, WireDeclarationError,
};
use tx_reactor::wait::{
    Channel, DeclaredChannel, DeclaredReadinessChannel, Mask, WaitOutcome, WaitProtocol,
};
use tx_reactor::{Reactor, RunStats, TaskStatus};
use tx_substrate::wake::mailbox::TaskMailbox;

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

fn assert_send_sync<T: Send + Sync>() {}

const DECLARED_READY: u16 = 0x1;
const DECLARED_GONE: u16 = 0x2;
const DECLARED_READABLE: u16 = 0x1;
const DECLARED_HUP: u16 = 0x2;

bus_lifecycle! {
    struct DeclaredWaitEvent {
        const READY = DECLARED_READY;
        const GONE = DECLARED_GONE;
    }
}

bus_readiness! {
    struct DeclaredReadiness {
        const READABLE = DECLARED_READABLE;
        const HUP = DECLARED_HUP;
    }
}

#[test]
fn reactor_wait_channel_is_send_sync_for_cross_hart_wakes() {
    assert_send_sync::<Channel>();
    assert_send_sync::<DeclaredChannel<DeclaredWaitEvent>>();
    assert_send_sync::<DeclaredReadinessChannel<DeclaredReadiness>>();
}

#[test]
fn wait_event_rechecks_after_register_before_parking() {
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let checks = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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
fn declared_channel_wait_event_uses_existing_typed_declared_port() {
    let port = DeclaredPort::new(WireDeclaration::<DeclaredWaitEvent>::port(
        "reactor.typed.wait",
    ))
    .expect("declared reactor wait port");
    let channel = DeclaredChannel::from_port(port.clone());
    let ready = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let ready = Arc::clone(&ready);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(
                    DeclaredWaitEvent::READY,
                    WaitProtocol::Interruptible,
                    || ready.load(Ordering::SeqCst) != 0,
                )
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
    assert_eq!(port.fire(DeclaredWaitEvent::GONE), 0);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    ready.store(1, Ordering::SeqCst);
    assert_eq!(port.fire(DeclaredWaitEvent::READY), 1);
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
}

#[test]
fn declared_channel_rejects_undeclared_interest_before_polling() {
    let channel = DeclaredChannel::new(WireDeclaration::<DeclaredWaitEvent>::port(
        "reactor.typed.reject",
    ))
    .expect("declared reactor wait channel");

    assert_eq!(
        channel
            .try_wait(DeclaredWaitEvent::from_bits(0x4))
            .map(|_| ()),
        Err(WireDeclarationError::UndeclaredBits)
    );
    assert_eq!(
        channel.try_fire(DeclaredWaitEvent::from_bits(0x4)),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
}

#[test]
fn declared_channel_empty_interest_matches_raw_wait_behavior() {
    let channel = DeclaredChannel::new(WireDeclaration::<DeclaredWaitEvent>::port(
        "reactor.typed.empty",
    ))
    .expect("declared reactor wait channel");
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    let mut wait = channel.wait(DeclaredWaitEvent::default());

    assert_eq!(
        Pin::new(&mut wait).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn reactor_declared_channel_uses_timer_registry_for_timeouts() {
    let reactor = Reactor::new();
    let channel = reactor
        .declared_channel(WireDeclaration::<DeclaredWaitEvent>::port(
            "reactor.typed.timeout",
        ))
        .expect("timer-backed declared channel");
    let outcome = Arc::new(Mutex::new(None));
    let task = {
        let channel = channel.clone();
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(
                    DeclaredWaitEvent::READY,
                    WaitProtocol::InterruptibleTimeout(10),
                    || false,
                )
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
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_deadline_ns(), Some(10));

    assert_eq!(reactor.advance_time_to(10), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::TimedOut)
    );
    assert_eq!(reactor.next_deadline_ns(), None);
}

#[test]
fn declared_readiness_channel_wait_event_uses_existing_typed_declared_queue() {
    let queue = DeclaredQueue::new(WireDeclaration::<DeclaredReadiness>::queue(
        "reactor.typed.readiness",
    ))
    .expect("declared reactor readiness queue");
    let channel = DeclaredReadinessChannel::from_queue(queue.clone());
    let ready = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let ready = Arc::clone(&ready);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(
                    DeclaredReadiness::READABLE,
                    WaitProtocol::Interruptible,
                    || ready.load(Ordering::SeqCst) != 0,
                )
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
    assert_eq!(queue.fire(DeclaredReadiness::HUP), 0);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    ready.store(1, Ordering::SeqCst);
    assert_eq!(queue.fire(DeclaredReadiness::READABLE), 1);
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
}

#[test]
fn declared_readiness_channel_rejects_undeclared_interest_before_polling() {
    let channel = DeclaredReadinessChannel::new(WireDeclaration::<DeclaredReadiness>::queue(
        "reactor.typed.readiness.reject",
    ))
    .expect("declared reactor readiness channel");

    assert_eq!(
        channel
            .try_wait(DeclaredReadiness::from_bits(0x4))
            .map(|_| ()),
        Err(WireDeclarationError::UndeclaredBits)
    );
    assert_eq!(
        channel.try_fire(DeclaredReadiness::from_bits(0x4)),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
    assert_eq!(
        channel.try_clear(DeclaredReadiness::from_bits(0x4)),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
}

#[test]
fn declared_readiness_channel_empty_interest_matches_raw_wait_behavior() {
    let channel = DeclaredReadinessChannel::new(WireDeclaration::<DeclaredReadiness>::queue(
        "reactor.typed.readiness.empty",
    ))
    .expect("declared reactor readiness channel");
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    let mut wait = channel.wait(DeclaredReadiness::default());

    assert_eq!(
        Pin::new(&mut wait).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn reactor_declared_readiness_channel_uses_timer_registry_for_timeouts() {
    let reactor = Reactor::new();
    let channel = reactor
        .declared_readiness_channel(WireDeclaration::<DeclaredReadiness>::queue(
            "reactor.typed.readiness.timeout",
        ))
        .expect("timer-backed declared readiness channel");
    let outcome = Arc::new(Mutex::new(None));
    let task = {
        let channel = channel.clone();
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(
                    DeclaredReadiness::READABLE,
                    WaitProtocol::InterruptibleTimeout(10),
                    || false,
                )
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
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_deadline_ns(), Some(10));

    assert_eq!(reactor.advance_time_to(10), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::TimedOut)
    );
    assert_eq!(reactor.next_deadline_ns(), None);
}

#[test]
fn raw_port_coalesces_fires_until_subscription_observes_ready() {
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let mailbox = Arc::new(TaskMailbox::new());
    mailbox.register_waker(counting_waker(Arc::clone(&wakes)));
    let gen = mailbox.next_generation();
    let sub = port.subscribe(0x1, Arc::downgrade(&mailbox), gen);

    assert_eq!(port.fire(0x1), 1);
    assert_eq!(port.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(mailbox.poll().is_some());
    assert!(mailbox.poll().is_none());

    assert_eq!(port.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    drop(sub);
    assert_eq!(port.fire(0x1), 0);
}

#[test]
fn raw_queue_fires_new_bits_and_drop_removes_subscription() {
    let queue = RawQueue::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let mailbox = Arc::new(TaskMailbox::new());
    mailbox.register_waker(counting_waker(Arc::clone(&wakes)));
    let gen = mailbox.next_generation();
    let sub = queue.subscribe(0x1, Arc::downgrade(&mailbox), gen);

    assert_eq!(queue.peek(), 0);
    assert_eq!(queue.fire(0x1), 1);
    assert_eq!(queue.peek(), 0x1);
    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(mailbox.poll().is_some());
    assert_eq!(queue.fire(0x1), 0);

    queue.clear(0x1);
    assert_eq!(queue.peek(), 0);
    assert_eq!(queue.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);

    drop(sub);
    queue.clear(0x1);
    assert_eq!(queue.fire(0x1), 0);
}
