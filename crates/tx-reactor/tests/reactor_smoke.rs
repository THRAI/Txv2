use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::sync::{Arc, Mutex};

use tx_reactor::wait::{Channel, Mask, WaitOutcome, WaitProtocol};
use tx_reactor::{
    HartId, InitialSchedMeta, Phase1QueueKind, Phase1Scheduler, Reactor, RescheduleSignal,
    RunStats, SharedReactor, SliceClock, SliceConfig, StopReason, TaskHandle, TaskId, TaskStatus,
    WakeDispatchReport, WakeHint,
};
use tx_substrate::step::{InterestMask, WaitSourceId};
use tx_substrate::wake::mailbox::{
    MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitGeneration,
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

struct PendingThenReady {
    polls: Arc<AtomicUsize>,
    ready_after: usize,
}

impl Future for PendingThenReady {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let poll = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
        if poll >= self.ready_after {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

struct SourceFiredDuringPending {
    polls: Arc<AtomicUsize>,
    first_ready: Arc<AtomicUsize>,
    hint: Option<MailboxSchedulerHint>,
}

impl Future for SourceFiredDuringPending {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let poll = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
        if poll == 1 {
            let mailbox = tx_reactor::current_task_mailbox(0).expect("current task mailbox");
            mailbox.register_waker(cx.waker().clone());
            let generation = mailbox.next_generation();
            let event = MailboxEvent::SourceFired {
                generation,
                source: WaitSourceId::new(7),
                interests: InterestMask::new(0b1),
            };
            let posted = match self.hint {
                Some(hint) => mailbox.post_with_scheduler_hint(event, hint),
                None => mailbox.post(event),
            };
            assert!(posted);
            Poll::Pending
        } else {
            let _ = self
                .first_ready
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
            Poll::Ready(())
        }
    }
}

struct CurrentTaskMailboxPark {
    polls: Arc<AtomicUsize>,
}

impl Future for CurrentTaskMailboxPark {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = tx_reactor::current_task_mailbox(0).expect("current task mailbox");
        mailbox.register_waker(cx.waker().clone());
        while let Some(event) = mailbox.poll() {
            if matches!(event, MailboxEvent::SourceFired { .. }) {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

struct RecordReadyOrder {
    first_ready: Arc<AtomicUsize>,
}

impl Future for RecordReadyOrder {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _ = self
            .first_ready
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst);
        Poll::Ready(())
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

const SAW_MAILBOX: usize = 1 << 0;
const SAW_TIMER_WHEEL: usize = 1 << 1;
const SAW_DELEGATE_REGISTRY: usize = 1 << 2;
const SAW_OTHER_HART_CLEAR: usize = 1 << 3;

struct HartContextProbe {
    hart: usize,
    other_hart: usize,
    seen: Arc<AtomicUsize>,
}

impl Future for HartContextProbe {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut observed = 0;
        if tx_reactor::current_task_mailbox(self.hart).is_some() {
            observed |= SAW_MAILBOX;
        }
        if tx_reactor::current_timer_wheel(self.hart).is_some() {
            observed |= SAW_TIMER_WHEEL;
        }
        if tx_reactor::current_delegate_registry(self.hart).is_some() {
            observed |= SAW_DELEGATE_REGISTRY;
        }
        if tx_reactor::current_task_mailbox(self.other_hart).is_none()
            && tx_reactor::current_timer_wheel(self.other_hart).is_none()
            && tx_reactor::current_delegate_registry(self.other_hart).is_none()
        {
            observed |= SAW_OTHER_HART_CLEAR;
        }
        self.seen.fetch_or(observed, Ordering::SeqCst);
        Poll::Ready(())
    }
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

struct ScriptedSliceClock {
    samples: Vec<u64>,
    idx: usize,
    deadlines: Arc<Mutex<Vec<Option<u64>>>>,
}

impl ScriptedSliceClock {
    fn new(samples: Vec<u64>, deadlines: Arc<Mutex<Vec<Option<u64>>>>) -> Self {
        Self {
            samples,
            idx: 0,
            deadlines,
        }
    }
}

impl SliceClock for ScriptedSliceClock {
    fn now_ns(&mut self) -> u64 {
        let sample = self
            .samples
            .get(self.idx)
            .copied()
            .or_else(|| self.samples.last().copied())
            .unwrap_or(0);
        self.idx = self.idx.saturating_add(1);
        sample
    }

    fn set_deadline_ns(&mut self, deadline_ns: u64) {
        self.deadlines
            .lock()
            .expect("deadline log poisoned")
            .push(Some(deadline_ns));
    }

    fn cancel_deadline(&mut self) {
        self.deadlines
            .lock()
            .expect("deadline log poisoned")
            .push(None);
    }
}

#[test]
fn submitted_ready_task_runs_to_completion() {
    let polls = Arc::new(AtomicUsize::new(0));

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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
    let reactor = Reactor::new();
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
    let reactor = Reactor::new();
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
    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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
fn submitted_task_with_affinity_uses_target_hart_queue() {
    let reactor = Reactor::new();
    let task =
        reactor.submit_task_with_meta(async {}, InitialSchedMeta::fair().with_affinity(0b0100));

    assert_eq!(reactor.next_scheduled_task(HartId(0)), None);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(2))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
}

#[test]
fn runtime_affinity_update_moves_queued_task_and_dispatches_remote_marker() {
    let reactor = Reactor::new();
    let task =
        reactor.submit_task_with_meta(async {}, InitialSchedMeta::fair().with_affinity(0b0011));

    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor
        .set_task_affinity(task, 0b0010, HartId(0), &mut signal)
        .expect("affinity update");
    assert_eq!(reactor.task_affinity(task), Ok(0b0010));

    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(1)]);
    assert!(reactor.dispatch_markers(HartId(1)).need_resched());
    assert_eq!(reactor.next_scheduled_task(HartId(0)), None);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(1))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
}

#[test]
fn submit_from_hart_dispatches_remote_ipi_for_spread_task() {
    let reactor = Reactor::new();
    let first = reactor.submit_task_with_meta(
        async {},
        InitialSchedMeta::fair()
            .with_affinity(0b0011)
            .spread_on_submit(),
    );
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(first.id())
    );

    let mut signal = RecordingRescheduleSignal::default();
    let (second, report) = reactor.submit_task_with_meta_from_hart(
        async {},
        InitialSchedMeta::fair()
            .with_affinity(0b0011)
            .spread_on_submit(),
        HartId(0),
        &mut signal,
    );

    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(1)]);
    assert!(reactor.dispatch_markers(HartId(1)).need_resched());
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(1))
            .map(|(handle, _)| handle.id()),
        Some(second.id())
    );
}

#[test]
fn submit_publish_ack_reports_queue_before_child_poll() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut signal = RecordingRescheduleSignal::default();

    let report = reactor.submit_task_publish_ack(
        CountPolls {
            polls: Arc::clone(&polls),
        },
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
        HartId(0),
        &mut signal,
    );

    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(report.publish.task, report.task.id());
    assert_eq!(report.publish.hart, HartId(0));
    assert_eq!(report.publish.queue, Phase1QueueKind::Preempted);
    assert_eq!(report.publish.queued_turn, 0);
    assert_eq!(
        report.dispatch,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 1,
            remote_ipis: 0,
        }
    );
    assert_eq!(signal.sent, Vec::<HartId>::new());
    assert!(reactor.dispatch_markers(HartId(0)).need_resched());
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(report.task.id())
    );
}

#[test]
fn remote_task_wake_dispatches_reschedule_signal() {
    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = reactor.submit_task_with_meta(
        ExternallyWoken {
            polls: Arc::clone(&polls),
            ready: Arc::clone(&ready),
            waker_slot: Arc::clone(&waker_slot),
        },
        InitialSchedMeta::kernel().with_affinity(0b0100),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(2)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    ready.store(1, Ordering::SeqCst);
    waker_slot
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task waker")
        .wake();

    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.drain_wakes_for_hart(HartId(0), &mut signal);

    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(2)]);
    assert!(reactor.dispatch_markers(HartId(2)).need_resched());
    assert!(reactor.consume_dispatch_markers(HartId(2)).need_resched());
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(2))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(2)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
}

#[test]
fn remote_wake_routes_through_target_hart_inbox() {
    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = reactor.submit_task_with_meta(
        ExternallyWoken {
            polls: Arc::clone(&polls),
            ready: Arc::clone(&ready),
            waker_slot: Arc::clone(&waker_slot),
        },
        InitialSchedMeta::kernel().with_affinity(0b0100),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(2)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    ready.store(1, Ordering::SeqCst);
    waker_slot
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task waker")
        .wake();

    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.drain_wakes_for_hart(HartId(0), &mut signal);
    assert_eq!(report.remote_ipis, 1);
    assert_eq!(signal.sent, vec![HartId(2)]);

    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        None
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 0,
            completed: 0,
        }
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(2)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
}

#[test]
fn rescheduled_hart_consumes_marker_before_draining_runqueue() {
    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    let task = reactor.submit_task_with_meta(
        ExternallyWoken {
            polls: Arc::clone(&polls),
            ready: Arc::clone(&ready),
            waker_slot: Arc::clone(&waker_slot),
        },
        InitialSchedMeta::kernel().with_affinity(0b0100),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(2)).polled, 1);
    ready.store(1, Ordering::SeqCst);
    waker_slot
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task waker")
        .wake();

    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.drain_wakes_for_hart(HartId(0), &mut signal);
    assert_eq!(report.remote_ipis, 1);
    assert!(reactor.dispatch_markers(HartId(2)).need_resched());

    let stats = reactor.run_rescheduled_on_hart_with_reschedule(HartId(2), &mut signal);

    assert_eq!(
        stats,
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    assert!(reactor.consume_dispatch_markers(HartId(2)).is_empty());
}

#[test]
fn pending_task_wake_breaks_polling_idle_before_marker_drain() {
    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
    reactor.submit_task_with_meta(
        ExternallyWoken {
            polls: Arc::clone(&polls),
            ready: Arc::clone(&ready),
            waker_slot: Arc::clone(&waker_slot),
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert!(reactor.is_idle());
    assert!(!reactor.should_leave_polling_idle(HartId(0)));

    ready.store(1, Ordering::SeqCst);
    waker_slot
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task waker")
        .wake();

    assert!(!reactor.dispatch_markers(HartId(0)).need_resched());
    assert!(!reactor.is_idle());
    assert!(reactor.should_leave_polling_idle(HartId(0)));
}

#[test]
fn userspace_preempt_marker_does_not_alias_normal_reschedule() {
    let reactor = Reactor::new();

    reactor.mark_userspace_preempt(HartId(1));

    let markers = reactor.dispatch_markers(HartId(1));
    assert!(!markers.need_resched());
    assert!(!markers.slice_expired());
    assert!(markers.userspace_preempt());

    let consumed = reactor.consume_dispatch_markers(HartId(1));
    assert!(!consumed.need_resched());
    assert!(consumed.userspace_preempt());
    assert!(reactor.consume_dispatch_markers(HartId(1)).is_empty());
}

#[test]
fn userspace_preempt_marker_requeues_pending_poll() {
    let polls = Arc::new(AtomicUsize::new(0));
    let reactor = Reactor::new();
    let task = reactor.submit_task_with_meta(
        PendingThenReady {
            polls: Arc::clone(&polls),
            ready_after: 2,
        },
        InitialSchedMeta::fair().with_affinity(0b0001),
    );

    reactor.mark_userspace_preempt(HartId(0));
    let mut signal = RecordingRescheduleSignal::default();
    let stats = reactor.run_until_idle_on_hart_with_reschedule(HartId(0), &mut signal);

    assert_eq!(
        stats,
        RunStats {
            polled: 2,
            completed: 1,
        },
        "userspace preemption should requeue the first Pending poll so the task \
         can be polled again",
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert!(signal.sent.is_empty());
}

#[test]
fn slice_clock_accounts_consumed_time_and_requeues_expired_slice() {
    let polls = Arc::new(AtomicUsize::new(0));
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let reactor = Reactor::new();
    let task = reactor.submit_task_with_meta(
        PendingThenReady {
            polls: Arc::clone(&polls),
            ready_after: 2,
        },
        InitialSchedMeta::fair().with_affinity(0b0001),
    );

    let mut signal = RecordingRescheduleSignal::default();
    let mut clock = ScriptedSliceClock::new(
        vec![
            1_000,
            1_000 + Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            20_000_000,
            20_000_250,
        ],
        Arc::clone(&deadlines),
    );
    let stats = reactor.run_until_idle_on_hart_with_reschedule_and_slice_clock(
        HartId(0),
        &mut signal,
        &mut clock,
    );

    assert_eq!(
        stats,
        RunStats {
            polled: 2,
            completed: 1,
        },
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    assert_eq!(
        reactor.last_stop_reason(task.id()),
        Some(StopReason::Completed)
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(
        deadlines.lock().expect("deadline log poisoned").as_slice(),
        &[
            Some(1_000 + Phase1Scheduler::NEW_QUEUE_SLICE_NS),
            None,
            Some(20_000_000 + Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS),
            None,
        ],
        "each preemptive poll arms a slice deadline and cancels it after accounting",
    );
}

#[test]
fn shared_reactor_serializes_rescheduled_hart_runqueue_drain() {
    let shared = SharedReactor::empty();
    assert!(!shared.is_initialized());
    assert!(shared.init());
    assert!(shared.is_initialized());
    assert!(!shared.init());

    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_task = Arc::clone(&completed);

    let task = shared
        .with(|reactor| {
            let polls = Arc::clone(&polls);
            let ready = Arc::clone(&ready);
            let waker_slot = Arc::clone(&waker_slot);
            reactor.submit_task_with_meta(
                async move {
                    ExternallyWoken {
                        polls,
                        ready,
                        waker_slot,
                    }
                    .await;
                    completed_task.store(1, Ordering::SeqCst);
                },
                InitialSchedMeta::kernel().with_affinity(0b0100),
            )
        })
        .expect("shared reactor initialized");

    let first = shared
        .with(|reactor| reactor.run_until_idle_on_hart(HartId(2)))
        .expect("shared reactor initialized");
    assert_eq!(first.polled, 1);

    ready.store(1, Ordering::SeqCst);
    waker_slot
        .lock()
        .expect("waker slot poisoned")
        .take()
        .expect("task waker")
        .wake();

    let mut signal = RecordingRescheduleSignal::default();
    let report = shared
        .with(|reactor| reactor.drain_wakes_for_hart(HartId(0), &mut signal))
        .expect("shared reactor initialized");
    assert_eq!(report.remote_ipis, 1);
    assert_eq!(signal.sent, vec![HartId(2)]);

    let stats = shared
        .with(|reactor| reactor.run_rescheduled_on_hart_with_reschedule(HartId(2), &mut signal))
        .expect("shared reactor initialized");

    assert_eq!(stats.polled, 1);
    assert_eq!(stats.completed, 1);
    assert_eq!(
        shared
            .with(|reactor| reactor.task_key_status(task))
            .expect("shared reactor initialized"),
        Some(TaskStatus::Completed)
    );
    assert_eq!(completed.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_reactor_with_hart_creates_stable_local_slot() {
    let shared = SharedReactor::empty();
    assert!(shared.init());

    assert_eq!(
        shared.with_hart(HartId(3), |_shared, local| local.hart()),
        Some(HartId(3))
    );
    assert_eq!(
        shared.with_hart(HartId(3), |_shared, local| local.hart()),
        Some(HartId(3))
    );
}

#[test]
fn shared_reactor_with_hart_runtime_drives_hart_loop_step() {
    let shared = SharedReactor::empty();
    assert!(shared.init());

    let task = shared
        .with(|reactor| reactor.submit_task_with_meta(async {}, InitialSchedMeta::kernel()))
        .expect("shared reactor initialized");

    let mut signal = RecordingRescheduleSignal::default();
    let step = shared
        .with_hart_runtime(HartId(0), |runtime| {
            tx_reactor::hart_loop::step_hart_loop_at(runtime, HartId(0), 1, &mut signal)
        })
        .expect("shared reactor initialized");

    assert_eq!(step.stats.polled, 1);
    assert_eq!(step.stats.completed, 1);
    assert_eq!(
        shared
            .with(|reactor| reactor.task_key_status(task))
            .expect("shared reactor initialized"),
        Some(TaskStatus::Completed)
    );
}

#[test]
fn per_hart_runtime_context_is_visible_only_on_polling_hart() {
    let seen = Arc::new(AtomicUsize::new(0));
    let reactor = Reactor::new();
    let task = reactor.submit_task_with_meta(
        HartContextProbe {
            hart: 2,
            other_hart: 0,
            seen: Arc::clone(&seen),
        },
        InitialSchedMeta::kernel().with_affinity(0b0100),
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(2)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    assert_eq!(
        seen.load(Ordering::SeqCst),
        SAW_MAILBOX | SAW_TIMER_WHEEL | SAW_DELEGATE_REGISTRY | SAW_OTHER_HART_CLEAR
    );
    assert!(tx_reactor::current_task_mailbox(2).is_none());
    assert!(tx_reactor::current_timer_wheel(2).is_none());
    assert!(tx_reactor::current_delegate_registry(2).is_none());
    assert!(tx_reactor::current_task_mailbox(usize::MAX).is_none());
}

#[test]
fn shared_concurrent_poll_path_sets_per_hart_runtime_context() {
    let shared = SharedReactor::empty();
    assert!(shared.init());

    let seen = Arc::new(AtomicUsize::new(0));
    let task = shared
        .with(|reactor| {
            reactor.submit_task_with_meta(
                HartContextProbe {
                    hart: 1,
                    other_hart: 0,
                    seen: Arc::clone(&seen),
                },
                InitialSchedMeta::kernel().with_affinity(0b0010),
            )
        })
        .expect("shared reactor initialized");

    let mut signal = RecordingRescheduleSignal::default();
    let step = shared
        .run_hart_loop_concurrent(HartId(1), 0, &mut signal)
        .expect("shared reactor initialized");

    assert_eq!(
        step.stats,
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(
        shared
            .with(|reactor| reactor.task_key_status(task))
            .expect("shared reactor initialized"),
        Some(TaskStatus::Completed)
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        SAW_MAILBOX | SAW_TIMER_WHEEL | SAW_DELEGATE_REGISTRY | SAW_OTHER_HART_CLEAR
    );
    assert!(tx_reactor::current_task_mailbox(1).is_none());
    assert!(tx_reactor::current_timer_wheel(1).is_none());
    assert!(tx_reactor::current_delegate_registry(1).is_none());
}

#[test]
fn observability_accumulates_per_hart_poll_counts() {
    let reactor = Reactor::new();
    reactor.submit_task_with_meta(async {}, InitialSchedMeta::kernel().with_affinity(0b0001));
    reactor.submit_task_with_meta(async {}, InitialSchedMeta::kernel().with_affinity(0b0100));

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(2)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );

    let observed = reactor.observability();
    assert_eq!(observed.hart(HartId(0)).polled, 1);
    assert_eq!(observed.hart(HartId(0)).completed, 1);
    assert_eq!(observed.hart(HartId(1)).polled, 0);
    assert_eq!(observed.hart(HartId(2)).polled, 1);
    assert_eq!(observed.hart(HartId(2)).completed, 1);
}

#[test]
fn blocked_task_reports_stop_reason_and_waits_for_wake() {
    let polls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let waker_slot = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();
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

    let reactor = Reactor::new();
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
    let reactor = Reactor::new();
    let task = reactor.submit(CountOnce);

    assert_eq!(reactor.run_until_idle().completed, 1);

    assert_eq!(reactor.last_stop_reason(task), Some(StopReason::Completed));
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
    assert_eq!(reactor.next_scheduled_task(HartId(0)), None);
}

#[test]
fn source_fired_pending_commit_keeps_userspace_task_behind_preempted_peer() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let first_ready = Arc::new(AtomicUsize::new(0));

    reactor.submit_task_with_meta(
        SourceFiredDuringPending {
            polls: Arc::clone(&polls),
            first_ready: Arc::clone(&first_ready),
            hint: None,
        },
        InitialSchedMeta::fair().userspace_thread(),
    );
    reactor.submit_task_with_meta(
        RecordReadyOrder {
            first_ready: Arc::clone(&first_ready),
        },
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );

    let stats = reactor.run_until_idle();

    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(first_ready.load(Ordering::SeqCst), 2);
    assert_eq!(stats.completed, 2);
}

#[test]
fn wake_handoff_same_hart_marks_userspace_preempt_without_remote_ipi() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        CurrentTaskMailboxPark {
            polls: Arc::clone(&polls),
        },
        InitialSchedMeta::fair().userspace_thread(),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_status(task.id()), Some(TaskStatus::Parked));

    let mailbox = reactor.task_mailbox(task).expect("task mailbox");
    assert!(mailbox.post_with_scheduler_hint(
        MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        },
        MailboxSchedulerHint::WakeHandoff,
    ));

    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.drain_wakes_for_hart(HartId(0), &mut signal);
    assert_eq!(report.remote_ipis, 0);
    assert!(reactor.dispatch_markers(HartId(0)).userspace_preempt());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
}

#[test]
fn lifecycle_wake_on_waker_hart_dispatches_without_remote_ipi() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        CurrentTaskMailboxPark {
            polls: Arc::clone(&polls),
        },
        InitialSchedMeta::fair()
            .userspace_thread()
            .movable()
            .with_affinity(0b11),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_status(task.id()), Some(TaskStatus::Parked));

    let mailbox = reactor.task_mailbox(task).expect("task mailbox");
    assert!(mailbox.post_with_scheduler_hint(
        MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        },
        MailboxSchedulerHint::LifecycleWake,
    ));

    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.drain_wakes_for_hart(HartId(1), &mut signal);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 1,
            remote_ipis: 0,
        }
    );
    assert!(signal.sent.is_empty());
    assert!(reactor.dispatch_markers(HartId(1)).userspace_preempt());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
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
    let reactor = Reactor::new();
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

// ---------------------------------------------------------------------------
// drive-taskmb: reactor wake path via TaskMailbox
// ---------------------------------------------------------------------------

/// A future that parks on a [`TaskMailbox`] using the reactor's waker.
/// On first poll it registers the reactor's waker with the mailbox,
/// drains any pre-queued events, and returns `Pending` if none match.
/// On subsequent polls it drains the queue and completes when a
/// matching event arrives.
struct MailboxParkFuture {
    mailbox: Arc<TaskMailbox>,
    polls: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
}

impl Future for MailboxParkFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.mailbox.register_waker(cx.waker().clone());

        while let Some(event) = self.mailbox.poll() {
            if matches!(event, MailboxEvent::SourceFired { .. }) {
                self.completed.fetch_add(1, Ordering::SeqCst);
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

/// Verify that posting a [`MailboxEvent`] to a [`TaskMailbox`] that
/// a parked reactor task is waiting on wakes the task through the
/// reactor's task-waker mechanism and the task drains the event on
/// re-poll.
#[test]
fn mailbox_post_wakes_parked_reactor_task() {
    let polls = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let mailbox = Arc::new(TaskMailbox::new());

    let reactor = Reactor::new();
    reactor.submit(MailboxParkFuture {
        mailbox: Arc::clone(&mailbox),
        polls: Arc::clone(&polls),
        completed: Arc::clone(&completed),
    });

    // First poll: task registers waker, polls empty queue, returns Pending.
    let stats = reactor.run_until_idle();
    assert_eq!(stats.polled, 1);
    assert_eq!(stats.completed, 0);
    assert!(reactor.is_idle());

    // Post an event — should wake the parked task via the stored waker.
    let posted = mailbox.post(MailboxEvent::SourceFired {
        generation: WaitGeneration::new(1),
        source: WaitSourceId::new(1),
        interests: InterestMask::new(0b1),
    });
    assert!(posted);

    // Second poll: reactor wakes task, drains the posted event, completes.
    let stats = reactor.run_until_idle();
    assert_eq!(stats.polled, 1);
    assert_eq!(stats.completed, 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(reactor.is_idle());
}
