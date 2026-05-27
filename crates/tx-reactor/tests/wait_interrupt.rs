use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tx_reactor::{
    interrupt::AtomicInterruptSummary,
    wait::{Channel, Mask, WaitOutcome, WaitProtocol},
    Reactor, RunStats, TaskStatus,
};

const EVENT: Mask = Mask::from_bits(0x1);

fn run_stats(polled: usize, completed: usize) -> RunStats {
    RunStats { polled, completed }
}

#[test]
fn uninterruptible_ignores_deliverable_and_termination_interrupt_state() {
    let channel = Channel::new();
    let condition_ready = Arc::new(AtomicBool::new(false));
    let interrupts = Arc::new(AtomicInterruptSummary::new());
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let condition_ready = Arc::clone(&condition_ready);
        let interrupts = Arc::clone(&interrupts);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event_with_interrupts(
                    EVENT,
                    WaitProtocol::Uninterruptible,
                    interrupts,
                    move || condition_ready.load(Ordering::SeqCst),
                )
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    interrupts.set_deliverable_signal(true);
    interrupts.set_termination(true);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    condition_ready.store(true, Ordering::SeqCst);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 1));
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
}

#[test]
fn interruptible_returns_interrupted_for_deliverable_signal() {
    let channel = Channel::new();
    let interrupts = Arc::new(AtomicInterruptSummary::new());
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let interrupts = Arc::clone(&interrupts);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event_with_interrupts(EVENT, WaitProtocol::Interruptible, interrupts, || {
                    false
                })
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));

    interrupts.set_deliverable_signal(true);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 1));
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Interrupted)
    );
}

#[test]
fn interruptible_does_not_return_killed_for_termination_only() {
    let channel = Channel::new();
    let condition_ready = Arc::new(AtomicBool::new(false));
    let interrupts = Arc::new(AtomicInterruptSummary::new());
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let condition_ready = Arc::clone(&condition_ready);
        let interrupts = Arc::clone(&interrupts);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event_with_interrupts(
                    EVENT,
                    WaitProtocol::Interruptible,
                    interrupts,
                    move || condition_ready.load(Ordering::SeqCst),
                )
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));

    interrupts.set_termination(true);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    condition_ready.store(true, Ordering::SeqCst);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 1));
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
}

#[test]
fn killable_ignores_deliverable_signal_but_returns_killed_for_termination() {
    let channel = Channel::new();
    let interrupts = Arc::new(AtomicInterruptSummary::new());
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let interrupts = Arc::clone(&interrupts);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event_with_interrupts(EVENT, WaitProtocol::Killable, interrupts, || false)
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));

    interrupts.set_deliverable_signal(true);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    interrupts.set_termination(true);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 1));
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Killed)
    );
}

#[test]
fn readiness_wins_when_condition_is_already_true() {
    let channel = Channel::new();
    let interrupts = Arc::new(AtomicInterruptSummary::new());
    let outcome = Arc::new(Mutex::new(None));

    interrupts.set_deliverable_signal(true);
    interrupts.set_termination(true);

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let interrupts = Arc::clone(&interrupts);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event_with_interrupts(EVENT, WaitProtocol::Killable, interrupts, || true)
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle(), run_stats(1, 1));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
}

#[test]
fn condition_recheck_wins_after_normal_wake_even_with_interrupt_pending() {
    let channel = Channel::new();
    let condition_ready = Arc::new(AtomicBool::new(false));
    let interrupts = Arc::new(AtomicInterruptSummary::new());
    let outcome = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = {
        let channel = channel.clone();
        let condition_ready = Arc::clone(&condition_ready);
        let interrupts = Arc::clone(&interrupts);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event_with_interrupts(
                    EVENT,
                    WaitProtocol::Interruptible,
                    interrupts,
                    move || condition_ready.load(Ordering::SeqCst),
                )
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle(), run_stats(1, 0));

    condition_ready.store(true, Ordering::SeqCst);
    interrupts.set_deliverable_signal(true);
    assert_eq!(channel.fire(EVENT), 1);
    assert_eq!(reactor.run_until_idle(), run_stats(1, 1));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert_eq!(
        *outcome.lock().expect("outcome slot poisoned"),
        Some(WaitOutcome::Ready)
    );
}
