use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::sync::{Arc, Mutex};

use tx_reactor::wait::{Channel, Mask, WaitOutcome, WaitProtocol};
use tx_reactor::{
    HartId, InitialSchedMeta, Phase1Scheduler, Reactor, RunStats, SliceConfig, StopReason,
    TaskHandle, TaskId, TaskStatus, WakeHint,
};

static PENDING_POLLS: AtomicUsize = AtomicUsize::new(0);

struct CountOnce;

impl Future for CountOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(())
    }
}

struct CountPolls {
    polls: Arc<AtomicUsize>,
}

impl Future for CountPolls {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
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
    let polls = Arc::new(AtomicUsize::new(0));

    let mut reactor = Reactor::new();
    let task_id = reactor.submit(CountPolls {
        polls: Arc::clone(&polls),
    });

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
    assert_eq!(polls.load(Ordering::SeqCst), 1);
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

#[test]
fn wait_event_rechecks_condition_after_spurious_wake() {
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let condition_ready = Arc::new(AtomicUsize::new(0));
    let waiter_done = Arc::new(AtomicUsize::new(0));

    let mut reactor = Reactor::new();
    let waiter = {
        let channel = channel.clone();
        let condition_ready = Arc::clone(&condition_ready);
        let waiter_done = Arc::clone(&waiter_done);
        reactor.submit(async move {
            let outcome = channel
                .wait_event(mask, WaitProtocol::Interruptible, move || {
                    condition_ready.load(Ordering::SeqCst) != 0
                })
                .await;
            assert_eq!(outcome, WaitOutcome::Ready);
            waiter_done.store(1, Ordering::SeqCst);
        })
    };

    reactor.submit({
        let channel = channel.clone();
        async move {
            assert_eq!(channel.fire(mask), 1);
        }
    });

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 3,
            completed: 1
        }
    );
    assert_eq!(waiter_done.load(Ordering::SeqCst), 0);
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Parked));

    condition_ready.store(1, Ordering::SeqCst);
    reactor.submit({
        let channel = channel.clone();
        async move {
            assert_eq!(channel.fire(mask), 1);
        }
    });

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 2,
            completed: 2
        }
    );
    assert_eq!(waiter_done.load(Ordering::SeqCst), 1);
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Completed));
}

#[test]
fn wait_event_timeout_completes_only_after_deadline_is_driven() {
    let mut reactor = Reactor::new();
    let channel = reactor.channel();
    let mask = Mask::from_bits(0x1);
    let outcome = Arc::new(Mutex::new(None));
    let deadline_ns = 10;

    let waiter = {
        let channel = channel.clone();
        let outcome = Arc::clone(&outcome);
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
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);
    assert_eq!(reactor.advance_time_to(deadline_ns - 1), 0);
    assert_eq!(reactor.run_until_idle().polled, 0);
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);

    assert_eq!(reactor.advance_time_to(deadline_ns), 1);
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
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Completed));
}

#[test]
fn wait_event_ready_before_timeout_unregisters_timer() {
    let mut reactor = Reactor::new();
    let channel = reactor.channel();
    let mask = Mask::from_bits(0x1);
    let condition_ready = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));
    let deadline_ns = 20;

    let waiter = {
        let channel = channel.clone();
        let condition_ready = Arc::clone(&condition_ready);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            let wait_outcome = channel
                .wait_event(
                    mask,
                    WaitProtocol::InterruptibleTimeout(deadline_ns),
                    move || condition_ready.load(Ordering::SeqCst) != 0,
                )
                .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(wait_outcome);
        })
    };

    assert_eq!(reactor.run_until_idle().polled, 1);
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
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Completed));
    assert_eq!(reactor.advance_time_to(deadline_ns), 0);
}

#[test]
fn wait_event_spurious_wake_reparks_before_timeout() {
    let mut reactor = Reactor::new();
    let channel = reactor.channel();
    let mask = Mask::from_bits(0x1);
    let outcome = Arc::new(Mutex::new(None));
    let deadline_ns = 30;

    let waiter = {
        let channel = channel.clone();
        let outcome = Arc::clone(&outcome);
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
    };

    assert_eq!(reactor.run_until_idle().polled, 1);
    assert_eq!(channel.fire(mask), 1);
    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(*outcome.lock().expect("outcome slot poisoned"), None);
    assert_eq!(reactor.task_status(waiter), Some(TaskStatus::Parked));

    assert_eq!(reactor.advance_time_to(deadline_ns), 1);
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
}

#[test]
fn submitted_task_uses_scheduler_backed_runnable_path() {
    let polls = Arc::new(AtomicUsize::new(0));

    let mut reactor = Reactor::new();
    let task_id = reactor.submit(CountPolls {
        polls: Arc::clone(&polls),
    });

    assert_eq!(task_id, TaskId(0));
    assert!(!reactor.is_idle());
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(task, slice)| (task.id(), slice)),
        Some((task_id, SliceConfig::Cooperative))
    );

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);
}

#[test]
fn blocked_task_reports_stop_reason_and_waits_for_wake() {
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
    assert_eq!(reactor.last_stop_reason(task), Some(StopReason::Blocked));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_scheduled_task(HartId(0)), None);

    ready.store(1, Ordering::SeqCst);
    waker_slot
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task waker")
        .wake();
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(task, slice)| (task.id(), slice)),
        Some((task, SliceConfig::Cooperative))
    );
}

#[test]
fn duplicate_wakes_coalesce_into_one_scheduler_notification() {
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

    let waker = waker_slot
        .lock()
        .expect("waker slot poisoned")
        .as_ref()
        .expect("task waker")
        .clone();
    waker.wake_by_ref();
    waker.wake_by_ref();

    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(task, _)| task.id()),
        Some(task)
    );
    assert_eq!(reactor.run_until_idle().polled, 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Parked));
}

#[test]
fn completed_task_reports_stop_reason() {
    let mut reactor = Reactor::new();
    let task = reactor.submit(CountOnce);

    assert_eq!(reactor.run_until_idle().completed, 1);

    assert_eq!(reactor.last_stop_reason(task), Some(StopReason::Completed));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert_eq!(reactor.next_scheduled_task(HartId(0)), None);
}

#[test]
fn phase1_scheduler_prioritizes_kernel_then_new_then_preempted() {
    let mut scheduler = Phase1Scheduler::new();
    let fair_new = TaskId(0);
    let fair_preempted = TaskId(1);
    let kernel = TaskId(2);

    scheduler.task_submitted(
        fair_preempted,
        TaskHandle::new(fair_preempted),
        InitialSchedMeta::fair(),
    );
    assert_eq!(
        scheduler.pick_next(HartId(0)),
        Some((
            TaskHandle::new(fair_preempted),
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS
            }
        ))
    );
    scheduler.task_stopped(
        fair_preempted,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    scheduler.task_submitted(
        fair_new,
        TaskHandle::new(fair_new),
        InitialSchedMeta::fair(),
    );
    scheduler.task_submitted(kernel, TaskHandle::new(kernel), InitialSchedMeta::kernel());

    assert_eq!(
        scheduler.pick_next(HartId(0)),
        Some((TaskHandle::new(kernel), SliceConfig::Cooperative))
    );
    assert_eq!(
        scheduler.pick_next(HartId(0)),
        Some((
            TaskHandle::new(fair_new),
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS
            }
        ))
    );
    assert_eq!(
        scheduler.pick_next(HartId(0)),
        Some((
            TaskHandle::new(fair_preempted),
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS
            }
        ))
    );
}

#[test]
fn phase1_scheduler_preserves_remaining_budget_after_blocked_wake() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(7);

    scheduler.task_submitted(task, TaskHandle::new(task), InitialSchedMeta::fair());
    assert_eq!(
        scheduler.pick_next(HartId(0)).map(|(_, slice)| slice),
        Some(SliceConfig::Preemptive {
            slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        })
    );

    scheduler.task_stopped(task, StopReason::Blocked, 250_000, HartId(0));
    scheduler.task_runnable(task, WakeHint::Normal);

    assert_eq!(
        scheduler.pick_next(HartId(0)),
        Some((
            TaskHandle::new(task),
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS - 250_000
            }
        ))
    );
}

#[test]
fn next_deadline_ns_reports_earliest_and_clears_after_resolution() {
    let mut reactor = Reactor::new();
    let first = reactor.channel();
    let second = reactor.channel();
    let mask = Mask::from_bits(0x1);
    let first_ready = Arc::new(AtomicUsize::new(0));
    let first_outcome = Arc::new(Mutex::new(None));
    let second_outcome = Arc::new(Mutex::new(None));

    let first_task = {
        let first = first.clone();
        let first_ready = Arc::clone(&first_ready);
        let first_outcome = Arc::clone(&first_outcome);
        reactor.submit(async move {
            let outcome = first
                .wait_event(mask, WaitProtocol::InterruptibleTimeout(50), move || {
                    first_ready.load(Ordering::SeqCst) != 0
                })
                .await;
            *first_outcome.lock().expect("first outcome poisoned") = Some(outcome);
        })
    };
    let second_task = {
        let second = second.clone();
        let second_outcome = Arc::clone(&second_outcome);
        reactor.submit(async move {
            let outcome = second
                .wait_event(mask, WaitProtocol::InterruptibleTimeout(20), || false)
                .await;
            *second_outcome.lock().expect("second outcome poisoned") = Some(outcome);
        })
    };

    assert_eq!(reactor.run_until_idle().polled, 2);
    assert_eq!(reactor.next_deadline_ns(), Some(20));

    assert_eq!(reactor.advance_time_to(20), 1);
    assert_eq!(reactor.run_until_idle().completed, 1);
    assert_eq!(
        reactor.task_status(second_task),
        Some(TaskStatus::Completed)
    );
    assert_eq!(reactor.next_deadline_ns(), Some(50));

    first_ready.store(1, Ordering::SeqCst);
    assert_eq!(first.fire(mask), 1);
    assert_eq!(reactor.run_until_idle().completed, 1);
    assert_eq!(reactor.task_status(first_task), Some(TaskStatus::Completed));
    assert_eq!(reactor.next_deadline_ns(), None);
}
