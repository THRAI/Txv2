use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::sync::{Arc, Barrier, Mutex};

use tx_reactor::adapter::step_engine::{
    AbortReason, AgentCancelPolicy, DelegateRegistry, DelegateReply, DelegateRequest,
    DelegateTokenId, TokenDropPolicy, TransitionOutcome,
};
use tx_reactor::wait::{Channel, Mask, WaitOutcome, WaitProtocol};
use tx_reactor::{
    current_deadline_registrar, current_task_mailbox, yield_now, HartId, HartPollBudget,
    InitialSchedMeta, Phase1QueueKind, Phase1Scheduler, Reactor, RescheduleSignal, RunStats,
    SharedReactor, SignalRouting, SliceClock, SliceConfig, StopReason, TaskHandle, TaskId,
    TaskStatus, WakeDispatchReport, WakeHint,
};
use tx_services::time::{
    CurrentHartDeadlineTimer, DeadlineNs, DeadlineRegistrar, DeadlineRegistrarHandle,
    DeviceTimerCallback, TimerGuard, TimerRole, TimerTarget,
};
use tx_substrate::bus::{RawQueue, RawQueueSubscription};
use tx_substrate::step::{InterestMask, WaitSourceId};
use tx_substrate::wake::mailbox::{
    MailboxEvent, MailboxPollAction, MailboxSchedulerHint, TaskMailbox, WaitGeneration,
};
use tx_substrate::wake::{register_source, unregister_source, SubscriberId, WaitSource};

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

struct SignalDeliveredPark {
    polls: Arc<AtomicUsize>,
    expected_signum: u32,
}

impl Future for SignalDeliveredPark {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = tx_reactor::current_task_mailbox(0).expect("current task mailbox");
        mailbox.register_waker(cx.waker().clone());
        while let Some(event) = mailbox.poll() {
            if let MailboxEvent::SignalDelivered { signum, routing } = event {
                assert_eq!(signum, self.expected_signum);
                assert_eq!(routing, SignalRouting::ProcessDirected);
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

struct DelegateTimeoutPark {
    hart: usize,
    deadline_ns: u64,
    polls: Arc<AtomicUsize>,
    token: Option<DelegateTokenId>,
    registry: Option<Arc<DelegateRegistry>>,
    registrar: Option<DeadlineRegistrarHandle>,
    mailbox: Option<Arc<Mutex<Option<Arc<TaskMailbox>>>>>,
    timer: Option<TimerGuard>,
}

#[derive(Clone, Copy)]
enum DelegateTerminalEvent {
    Replied,
    Aborted(AbortReason),
}

struct DelegateOwnerAwarePark {
    hart: usize,
    polls: Arc<AtomicUsize>,
    token_slot: Arc<Mutex<Option<DelegateTokenId>>>,
    registry_slot: Arc<Mutex<Option<Arc<DelegateRegistry>>>>,
    expected: DelegateTerminalEvent,
}

impl Future for DelegateOwnerAwarePark {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = tx_reactor::current_task_mailbox(self.hart).expect("current task mailbox");

        if self
            .token_slot
            .lock()
            .expect("token slot poisoned")
            .is_none()
        {
            let registry =
                tx_reactor::current_delegate_registry(self.hart).expect("delegate registry");
            let guard = registry.install_request(
                DelegateRequest::Placeholder,
                0,
                AgentCancelPolicy::BestEffort,
                TokenDropPolicy::Abandon,
                Arc::downgrade(&mailbox),
            );
            let token = guard.forget();
            *self.token_slot.lock().expect("token slot poisoned") = Some(token);
            *self.registry_slot.lock().expect("registry slot poisoned") = Some(registry);
            return Poll::Pending;
        }

        let token = self
            .token_slot
            .lock()
            .expect("token slot poisoned")
            .expect("token installed");
        while let Some(event) = mailbox.poll() {
            match (self.expected, event) {
                (DelegateTerminalEvent::Replied, MailboxEvent::AgentReplied { token_id })
                    if token_id == token =>
                {
                    let registry = self
                        .registry_slot
                        .lock()
                        .expect("registry slot poisoned")
                        .as_ref()
                        .expect("registry installed")
                        .clone();
                    assert!(
                        registry.take_reply(token).is_some(),
                        "AgentReplied must publish after installing reply payload",
                    );
                    return Poll::Ready(());
                }
                (
                    DelegateTerminalEvent::Aborted(expected),
                    MailboxEvent::Abort { token_id, reason },
                ) if token_id == token && reason == expected => {
                    return Poll::Ready(());
                }
                _ => {}
            }
        }

        Poll::Pending
    }
}

fn count_device_timer_fire(counter: u64) {
    // The submitting test and its parked future retain the Arc until this callback fires.
    unsafe {
        (&*(counter as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
    }
}

struct DeviceWaitSourceTimerPark {
    hart: usize,
    source: Arc<WaitSource>,
    interests: InterestMask,
    deadline_ns: u64,
    polls: Arc<AtomicUsize>,
    callback_fires: Arc<AtomicUsize>,
    registrar: Option<DeadlineRegistrarHandle>,
    mailbox: Option<Arc<Mutex<Option<Arc<TaskMailbox>>>>>,
    subscriber: Option<SubscriberId>,
    timer: Option<TimerGuard>,
}

struct DeviceRawQueueTimerPark {
    hart: usize,
    queue: RawQueue,
    interests: u64,
    deadline_ns: u64,
    polls: Arc<AtomicUsize>,
    callback_fires: Arc<AtomicUsize>,
    registrar: Option<DeadlineRegistrarHandle>,
    mailbox: Option<Arc<Mutex<Option<Arc<TaskMailbox>>>>>,
    subscriber: Option<RawQueueSubscription>,
    timer: Option<TimerGuard>,
}

struct MixedProducerOwnerAwarePark {
    hart: usize,
    source: Arc<WaitSource>,
    interests: InterestMask,
    deadline_ns: u64,
    polls: Arc<AtomicUsize>,
    observed_stage: Arc<AtomicUsize>,
    token_slot: Arc<Mutex<Option<DelegateTokenId>>>,
    registry_slot: Arc<Mutex<Option<Arc<DelegateRegistry>>>>,
    initialized: bool,
    stage: usize,
    subscriber: Option<SubscriberId>,
    timer: Option<TimerGuard>,
    delegate_token: Option<DelegateTokenId>,
}

impl Future for MixedProducerOwnerAwarePark {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = tx_reactor::current_task_mailbox(self.hart).expect("current task mailbox");

        if !self.initialized {
            let generation = mailbox.next_generation();
            self.subscriber = Some(self.source.register(
                Arc::downgrade(&mailbox),
                generation,
                self.interests,
            ));

            let registrar =
                tx_reactor::current_deadline_registrar(self.hart).expect("deadline registrar");
            self.timer = Some(
                registrar
                    .register_deadline(
                        DeadlineNs::new(self.deadline_ns),
                        TimerRole::DeadlineAbort,
                        TimerTarget::TaskMailbox(Arc::downgrade(&mailbox)),
                    )
                    .expect("deadline registration"),
            );

            let registry =
                tx_reactor::current_delegate_registry(self.hart).expect("delegate registry");
            let guard = registry.install_request(
                DelegateRequest::Placeholder,
                0,
                AgentCancelPolicy::BestEffort,
                TokenDropPolicy::Abandon,
                Arc::downgrade(&mailbox),
            );
            let token = guard.forget();
            self.delegate_token = Some(token);
            *self.token_slot.lock().expect("token slot poisoned") = Some(token);
            *self.registry_slot.lock().expect("registry slot poisoned") = Some(registry);
            self.initialized = true;
            return Poll::Pending;
        }

        match self.stage {
            0 => {
                let source_id = self.source.id();
                let interests = self.interests;
                if mailbox
                    .poll_select(|event| match event {
                        MailboxEvent::SourceFired {
                            source,
                            interests: event_interests,
                            ..
                        } if *source == source_id
                            && event_interests.raw() & interests.raw() == interests.raw() =>
                        {
                            MailboxPollAction::Take
                        }
                        _ => MailboxPollAction::Keep,
                    })
                    .is_some()
                {
                    if let Some(subscriber) = self.subscriber.take() {
                        self.source.unregister(subscriber);
                    }
                    self.stage = 1;
                    self.observed_stage.store(1, Ordering::SeqCst);
                    return Poll::Pending;
                }
            }
            1 => {
                if mailbox
                    .poll_select(|event| match event {
                        MailboxEvent::TimerFired { .. } => MailboxPollAction::Take,
                        _ => MailboxPollAction::Keep,
                    })
                    .is_some()
                {
                    self.timer = None;
                    self.stage = 2;
                    self.observed_stage.store(2, Ordering::SeqCst);
                    return Poll::Pending;
                }
            }
            2 => {
                let expected = self.delegate_token;
                if let Some(MailboxEvent::AgentReplied { token_id }) =
                    mailbox.poll_select(|event| match event {
                        MailboxEvent::AgentReplied { token_id } if Some(*token_id) == expected => {
                            MailboxPollAction::Take
                        }
                        _ => MailboxPollAction::Keep,
                    })
                {
                    let registry = self
                        .registry_slot
                        .lock()
                        .expect("registry slot poisoned")
                        .as_ref()
                        .expect("registry installed")
                        .clone();
                    assert!(
                        registry.take_reply(token_id).is_some(),
                        "AgentReplied must publish after installing reply payload",
                    );
                    self.stage = 3;
                    self.observed_stage.store(3, Ordering::SeqCst);
                    return Poll::Ready(());
                }
            }
            _ => {}
        }

        Poll::Pending
    }
}

impl Future for DeviceRawQueueTimerPark {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = match &self.mailbox {
            Some(slot) => slot
                .lock()
                .expect("mailbox slot poisoned")
                .clone()
                .expect("task mailbox installed before polling"),
            None => tx_reactor::current_task_mailbox(self.hart).expect("current task mailbox"),
        };

        if self.subscriber.is_none() {
            let generation = mailbox.next_generation();
            self.subscriber = Some(self.queue.subscribe(
                self.interests,
                Arc::downgrade(&mailbox),
                generation,
            ));
            let registrar = self
                .registrar
                .clone()
                .or_else(|| tx_reactor::current_deadline_registrar(self.hart))
                .expect("deadline registrar");
            self.timer = Some(
                registrar
                    .register_deadline(
                        DeadlineNs::new(self.deadline_ns),
                        TimerRole::DeviceEvent,
                        TimerTarget::DeviceCallback(
                            DeviceTimerCallback::new(
                                count_device_timer_fire,
                                Arc::as_ptr(&self.callback_fires) as usize as u64,
                            )
                            .with_raw_queue_wake(self.queue.clone(), self.interests),
                        ),
                    )
                    .expect("device timer registration"),
            );
            return Poll::Pending;
        }

        while let Some(event) = mailbox.poll() {
            if let MailboxEvent::SourceFired { interests, .. } = event {
                if interests.raw() & self.interests == self.interests {
                    if let Some(mut subscriber) = self.subscriber.take() {
                        subscriber.unsubscribe();
                    }
                    self.timer = None;
                    return Poll::Ready(());
                }
            }
        }

        Poll::Pending
    }
}

impl Future for DeviceWaitSourceTimerPark {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = match &self.mailbox {
            Some(slot) => slot
                .lock()
                .expect("mailbox slot poisoned")
                .clone()
                .expect("task mailbox installed before polling"),
            None => tx_reactor::current_task_mailbox(self.hart).expect("current task mailbox"),
        };

        if self.subscriber.is_none() {
            let generation = mailbox.next_generation();
            self.subscriber = Some(self.source.register(
                Arc::downgrade(&mailbox),
                generation,
                self.interests,
            ));
            let registrar = self
                .registrar
                .clone()
                .or_else(|| tx_reactor::current_deadline_registrar(self.hart))
                .expect("deadline registrar");
            self.timer = Some(
                registrar
                    .register_deadline(
                        DeadlineNs::new(self.deadline_ns),
                        TimerRole::DeviceEvent,
                        TimerTarget::DeviceCallback(
                            DeviceTimerCallback::new(
                                count_device_timer_fire,
                                Arc::as_ptr(&self.callback_fires) as usize as u64,
                            )
                            .with_wait_source_wake(self.source.id(), self.interests),
                        ),
                    )
                    .expect("device timer registration"),
            );
            return Poll::Pending;
        }

        while let Some(event) = mailbox.poll() {
            if let MailboxEvent::SourceFired {
                source, interests, ..
            } = event
            {
                if source == self.source.id()
                    && interests.raw() & self.interests.raw() == self.interests.raw()
                {
                    if let Some(subscriber) = self.subscriber.take() {
                        self.source.unregister(subscriber);
                    }
                    self.timer = None;
                    return Poll::Ready(());
                }
            }
        }

        Poll::Pending
    }
}

impl Future for DelegateTimeoutPark {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mailbox = match &self.mailbox {
            Some(slot) => slot
                .lock()
                .expect("mailbox slot poisoned")
                .clone()
                .expect("task mailbox installed before polling"),
            None => tx_reactor::current_task_mailbox(self.hart).expect("current task mailbox"),
        };
        mailbox.register_waker(cx.waker().clone());

        if self.token.is_none() {
            let registry = self
                .registry
                .clone()
                .or_else(|| tx_reactor::current_delegate_registry(self.hart))
                .expect("delegate registry");
            let registrar = self
                .registrar
                .clone()
                .or_else(|| tx_reactor::current_deadline_registrar(self.hart))
                .expect("deadline registrar");
            let guard = registry.install_request(
                DelegateRequest::Placeholder,
                0,
                AgentCancelPolicy::BestEffort,
                TokenDropPolicy::Abandon,
                Arc::downgrade(&mailbox),
            );
            let token = guard.forget();
            self.timer = Some(
                registrar
                    .register_deadline(
                        DeadlineNs::new(self.deadline_ns),
                        TimerRole::DelegateTimeout,
                        TimerTarget::DelegateToken(token),
                    )
                    .expect("delegate timeout registration"),
            );
            self.token = Some(token);
            return Poll::Pending;
        }

        while let Some(event) = mailbox.poll() {
            if let MailboxEvent::Abort { token_id, reason } = event {
                if Some(token_id) == self.token {
                    assert_eq!(reason, AbortReason::TimedOut);
                    self.timer = None;
                    return Poll::Ready(());
                }
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
const SAW_DEADLINE_REGISTRAR: usize = 1 << 1;
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
        if tx_reactor::current_deadline_registrar(self.hart).is_some() {
            observed |= SAW_DEADLINE_REGISTRAR;
        }
        if tx_reactor::current_delegate_registry(self.hart).is_some() {
            observed |= SAW_DELEGATE_REGISTRY;
        }
        if tx_reactor::current_task_mailbox(self.other_hart).is_none()
            && tx_reactor::current_deadline_registrar(self.other_hart).is_none()
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
}

impl CurrentHartDeadlineTimer for ScriptedSliceClock {
    fn set_current_hart_deadline_ns(&mut self, deadline_ns: u64) {
        self.deadlines
            .lock()
            .expect("deadline log poisoned")
            .push(Some(deadline_ns));
    }

    fn cancel_current_hart_deadline(&mut self) {
        self.deadlines
            .lock()
            .expect("deadline log poisoned")
            .push(None);
    }
}

struct BarrierSliceClock {
    samples: Vec<u64>,
    idx: usize,
    deadlines: Arc<Mutex<Vec<Option<u64>>>>,
    update_started: Arc<Barrier>,
    update_registered: Arc<Barrier>,
}

impl BarrierSliceClock {
    fn new(
        samples: Vec<u64>,
        deadlines: Arc<Mutex<Vec<Option<u64>>>>,
        update_started: Arc<Barrier>,
        update_registered: Arc<Barrier>,
    ) -> Self {
        Self {
            samples,
            idx: 0,
            deadlines,
            update_started,
            update_registered,
        }
    }
}

impl SliceClock for BarrierSliceClock {
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
}

impl CurrentHartDeadlineTimer for BarrierSliceClock {
    fn set_current_hart_deadline_ns(&mut self, deadline_ns: u64) {
        self.deadlines
            .lock()
            .expect("deadline log poisoned")
            .push(Some(deadline_ns));
    }

    fn cancel_current_hart_deadline(&mut self) {
        self.deadlines
            .lock()
            .expect("deadline log poisoned")
            .push(None);
        self.update_started.wait();
        self.update_registered.wait();
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
fn owner_aware_post_routes_source_fired_without_captured_waker_drain() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        CurrentTaskMailboxPark {
            polls: Arc::clone(&polls),
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mailbox = reactor.task_mailbox(task).expect("task mailbox");
    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.post_mailbox_event_from_hart(
        Arc::downgrade(&mailbox),
        MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        },
        HartId(1),
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
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert!(reactor.dispatch_markers(HartId(0)).need_resched());
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
}

#[test]
fn signal_delivered_routes_through_owner_aware_post() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        SignalDeliveredPark {
            polls: Arc::clone(&polls),
            expected_signum: 15,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mailbox = reactor.task_mailbox(task).expect("task mailbox");
    let mut signal = RecordingRescheduleSignal::default();
    let report = reactor.post_signal_delivered_from_hart(
        Arc::downgrade(&mailbox),
        15,
        SignalRouting::ProcessDirected,
        HartId(1),
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
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn signal_delivery_promotes_already_runnable_userspace_task() {
    let reactor = Reactor::new();
    let aux = reactor.submit_task_with_meta(
        CountOnce,
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );
    let worker = reactor.submit_task_with_meta(
        CountOnce,
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );
    let mailbox = reactor.task_mailbox(worker).expect("worker mailbox");
    let mut signal = RecordingRescheduleSignal::default();

    let report = reactor.post_signal_delivered_from_hart(
        Arc::downgrade(&mailbox),
        15,
        SignalRouting::ThreadDirected { tid: 75 },
        HartId(0),
        &mut signal,
    );

    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 1,
            remote_ipis: 0,
        }
    );
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(worker.id())
    );
    assert_eq!(reactor.task_key_status(aux), Some(TaskStatus::Runnable));
    assert_eq!(reactor.task_key_status(worker), Some(TaskStatus::Runnable));
}

#[test]
fn wait_channel_fire_routes_through_owner_aware_post() {
    let reactor = Reactor::new();
    let channel = Channel::new();
    let mask = Mask::from_bits(0x1);
    let completed = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        {
            let channel = channel.clone();
            let completed = Arc::clone(&completed);
            async move {
                assert_eq!(channel.wait(mask).await, WaitOutcome::Ready);
                completed.store(1, Ordering::SeqCst);
            }
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mut signal = RecordingRescheduleSignal::default();
    let mut report = WakeDispatchReport::empty();
    let woke = channel.fire_with_post(mask, |mailbox, event| {
        let (posted, next) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        report.merge(next);
        posted
    });

    assert_eq!(woke, 1);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(completed.load(Ordering::SeqCst), 1);
}

#[test]
fn delegate_timeout_routes_through_owner_aware_timer_tick() {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let mailbox = Arc::new(Mutex::new(None));
    let task = reactor.submit_task_with_meta(
        DelegateTimeoutPark {
            hart: 0,
            deadline_ns: 50,
            polls: Arc::clone(&polls),
            token: None,
            registry: Some(reactor.delegate_registry_handle()),
            registrar: Some(reactor.deadline_registrar_handle()),
            mailbox: Some(Arc::clone(&mailbox)),
            timer: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    *mailbox.lock().expect("mailbox slot poisoned") =
        Some(reactor.task_mailbox(task).expect("task mailbox"));

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));

    let mut signal = RecordingRescheduleSignal::default();
    let (fired, report) =
        reactor.advance_time_to_from_hart_with_reschedule(50, HartId(1), &mut signal);

    assert_eq!(fired, 1);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

fn run_delegate_owner_aware_transition(
    expected: DelegateTerminalEvent,
    trigger: impl FnOnce(
        &Reactor,
        &DelegateRegistry,
        DelegateTokenId,
        &mut RecordingRescheduleSignal,
    ) -> (TransitionOutcome, WakeDispatchReport),
) {
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let token_slot = Arc::new(Mutex::new(None));
    let registry_slot = Arc::new(Mutex::new(None));
    let task = reactor.submit_task_with_meta(
        DelegateOwnerAwarePark {
            hart: 0,
            polls: Arc::clone(&polls),
            token_slot: Arc::clone(&token_slot),
            registry_slot: Arc::clone(&registry_slot),
            expected,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    let token = token_slot
        .lock()
        .expect("token slot poisoned")
        .expect("token installed");
    let registry = registry_slot
        .lock()
        .expect("registry slot poisoned")
        .as_ref()
        .expect("registry installed")
        .clone();

    let mut signal = RecordingRescheduleSignal::default();
    let (outcome, report) = trigger(&reactor, &registry, token, &mut signal);

    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
}

#[test]
fn delegate_reply_routes_through_owner_aware_post() {
    run_delegate_owner_aware_transition(
        DelegateTerminalEvent::Replied,
        |reactor, registry, token, signal| {
            reactor.mark_delegate_replied_from_hart_with_reschedule(
                registry,
                token,
                DelegateReply::placeholder(),
                HartId(1),
                signal,
            )
        },
    );
}

#[test]
fn delegate_cancel_routes_through_owner_aware_post() {
    run_delegate_owner_aware_transition(
        DelegateTerminalEvent::Aborted(AbortReason::Canceled),
        |reactor, registry, token, signal| {
            reactor.mark_delegate_canceled_from_hart_with_reschedule(
                registry,
                token,
                HartId(1),
                signal,
            )
        },
    );
}

#[test]
fn delegate_agent_died_routes_through_owner_aware_post() {
    run_delegate_owner_aware_transition(
        DelegateTerminalEvent::Aborted(AbortReason::AgentDied),
        |reactor, registry, token, signal| {
            reactor.mark_delegate_agent_died_from_hart_with_reschedule(
                registry,
                token,
                HartId(1),
                signal,
            )
        },
    );
}

#[test]
fn device_callback_wait_source_routes_through_owner_aware_timer_tick() {
    let reactor = Reactor::new();
    let source = Arc::new(WaitSource::new(WaitSourceId::new(0xD0E0)));
    register_source(Arc::clone(&source));
    let polls = Arc::new(AtomicUsize::new(0));
    let callback_fires = Arc::new(AtomicUsize::new(0));
    let mailbox = Arc::new(Mutex::new(None));
    let task = reactor.submit_task_with_meta(
        DeviceWaitSourceTimerPark {
            hart: 0,
            source: Arc::clone(&source),
            interests: InterestMask::new(0b1000),
            deadline_ns: 70,
            polls: Arc::clone(&polls),
            callback_fires: Arc::clone(&callback_fires),
            registrar: Some(reactor.deadline_registrar_handle()),
            mailbox: Some(Arc::clone(&mailbox)),
            subscriber: None,
            timer: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    *mailbox.lock().expect("mailbox slot poisoned") =
        Some(reactor.task_mailbox(task).expect("task mailbox"));

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_deadline_ns(), Some(70));

    let mut signal = RecordingRescheduleSignal::default();
    let (fired, report) =
        reactor.advance_time_to_from_hart_with_reschedule(70, HartId(1), &mut signal);

    assert_eq!(fired, 1);
    assert_eq!(callback_fires.load(Ordering::SeqCst), 1);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));

    unregister_source(source.id());
}

#[test]
fn device_callback_raw_queue_routes_through_owner_aware_timer_tick() {
    let reactor = Reactor::new();
    let queue = RawQueue::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let callback_fires = Arc::new(AtomicUsize::new(0));
    let mailbox = Arc::new(Mutex::new(None));
    let task = reactor.submit_task_with_meta(
        DeviceRawQueueTimerPark {
            hart: 0,
            queue,
            interests: 0b1000,
            deadline_ns: 75,
            polls: Arc::clone(&polls),
            callback_fires: Arc::clone(&callback_fires),
            registrar: Some(reactor.deadline_registrar_handle()),
            mailbox: Some(Arc::clone(&mailbox)),
            subscriber: None,
            timer: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    *mailbox.lock().expect("mailbox slot poisoned") =
        Some(reactor.task_mailbox(task).expect("task mailbox"));

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_deadline_ns(), Some(75));

    let mut signal = RecordingRescheduleSignal::default();
    let (fired, report) =
        reactor.advance_time_to_from_hart_with_reschedule(75, HartId(1), &mut signal);

    assert_eq!(fired, 1);
    assert_eq!(callback_fires.load(Ordering::SeqCst), 1);
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
}

#[test]
fn mixed_producer_wakes_repeatedly_route_current_owner() {
    let reactor = Reactor::new();
    let source = Arc::new(WaitSource::new(WaitSourceId::new(0xA11CE)));
    let interests = InterestMask::new(0b0010);
    let polls = Arc::new(AtomicUsize::new(0));
    let observed_stage = Arc::new(AtomicUsize::new(0));
    let token_slot = Arc::new(Mutex::new(None));
    let registry_slot = Arc::new(Mutex::new(None));

    let task = reactor.submit_task_with_meta(
        MixedProducerOwnerAwarePark {
            hart: 0,
            source: Arc::clone(&source),
            interests,
            deadline_ns: 90,
            polls: Arc::clone(&polls),
            observed_stage: Arc::clone(&observed_stage),
            token_slot: Arc::clone(&token_slot),
            registry_slot: Arc::clone(&registry_slot),
            initialized: false,
            stage: 0,
            subscriber: None,
            timer: None,
            delegate_token: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(0)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(source.subscriber_count(), 1);
    assert_eq!(reactor.next_deadline_ns(), Some(90));

    let mut signal = RecordingRescheduleSignal::default();
    let mut source_report = WakeDispatchReport::empty();
    let source_wakes = source.notify_with_owner_post(
        interests,
        MailboxSchedulerHint::Normal,
        |mailbox, event, hint| {
            let (posted, report) = reactor.post_mailbox_ref_event_with_hint_from_hart(
                mailbox,
                event,
                hint,
                HartId(1),
                &mut signal,
            );
            source_report.merge(report);
            posted
        },
    );
    assert_eq!(source_wakes, 1);
    assert_eq!(
        source_report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 0,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(source.subscriber_count(), 0);
    assert_eq!(observed_stage.load(Ordering::SeqCst), 1);

    signal.sent.clear();
    let (timer_fired, timer_report) =
        reactor.advance_time_to_from_hart_with_reschedule(90, HartId(1), &mut signal);
    assert_eq!(timer_fired, 1);
    assert_eq!(
        timer_report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 0,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(observed_stage.load(Ordering::SeqCst), 2);

    let token = token_slot
        .lock()
        .expect("token slot poisoned")
        .expect("delegate token installed");
    let registry = registry_slot
        .lock()
        .expect("registry slot poisoned")
        .as_ref()
        .expect("registry installed")
        .clone();

    signal.sent.clear();
    let (outcome, delegate_report) = reactor.mark_delegate_replied_from_hart_with_reschedule(
        &registry,
        token,
        DelegateReply::placeholder(),
        HartId(1),
        &mut signal,
    );
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(
        delegate_report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    assert_eq!(polls.load(Ordering::SeqCst), 4);
    assert_eq!(observed_stage.load(Ordering::SeqCst), 3);
}

#[test]
fn broad_owner_aware_producer_stress_routes_remote_wakes() {
    let reactor = Reactor::new();
    let channel = Channel::new();
    let channel_completed = Arc::new(AtomicUsize::new(0));
    let source_polls = Arc::new(AtomicUsize::new(0));
    let signal_polls = Arc::new(AtomicUsize::new(0));
    let delegate_polls = Arc::new(AtomicUsize::new(0));
    let device_source_polls = Arc::new(AtomicUsize::new(0));
    let device_raw_polls = Arc::new(AtomicUsize::new(0));
    let callback_fires = Arc::new(AtomicUsize::new(0));
    let device_source = Arc::new(WaitSource::new(WaitSourceId::new(0xD0E1)));
    register_source(Arc::clone(&device_source));

    let source_task = reactor.submit_task_with_meta(
        CurrentTaskMailboxPark {
            polls: Arc::clone(&source_polls),
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    let signal_task = reactor.submit_task_with_meta(
        SignalDeliveredPark {
            polls: Arc::clone(&signal_polls),
            expected_signum: 12,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    let channel_task = reactor.submit_task_with_meta(
        {
            let channel = channel.clone();
            let channel_completed = Arc::clone(&channel_completed);
            async move {
                assert_eq!(
                    channel.wait(Mask::from_bits(0x20)).await,
                    WaitOutcome::Ready
                );
                channel_completed.store(1, Ordering::SeqCst);
            }
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    let delegate_task = reactor.submit_task_with_meta(
        DelegateTimeoutPark {
            hart: 0,
            deadline_ns: 100,
            polls: Arc::clone(&delegate_polls),
            token: None,
            registry: None,
            registrar: None,
            mailbox: None,
            timer: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    let device_source_task = reactor.submit_task_with_meta(
        DeviceWaitSourceTimerPark {
            hart: 0,
            source: Arc::clone(&device_source),
            interests: InterestMask::new(0b1000),
            deadline_ns: 110,
            polls: Arc::clone(&device_source_polls),
            callback_fires: Arc::clone(&callback_fires),
            registrar: None,
            mailbox: None,
            subscriber: None,
            timer: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );
    let device_raw_task = reactor.submit_task_with_meta(
        DeviceRawQueueTimerPark {
            hart: 0,
            queue: RawQueue::new(),
            interests: 0b1000,
            deadline_ns: 120,
            polls: Arc::clone(&device_raw_polls),
            callback_fires: Arc::clone(&callback_fires),
            registrar: None,
            mailbox: None,
            subscriber: None,
            timer: None,
        },
        InitialSchedMeta::kernel().with_affinity(0b0001),
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 6,
            completed: 0,
        }
    );
    for task in [
        source_task,
        signal_task,
        channel_task,
        delegate_task,
        device_source_task,
        device_raw_task,
    ] {
        assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    }
    assert_eq!(reactor.next_deadline_ns(), Some(100));

    let mut signal = RecordingRescheduleSignal::default();

    let source_mailbox = reactor.task_mailbox(source_task).expect("source mailbox");
    let source_report = reactor.post_mailbox_event_from_hart(
        Arc::downgrade(&source_mailbox),
        MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(0xA001),
            interests: InterestMask::new(0b1),
        },
        HartId(1),
        &mut signal,
    );
    assert_remote_wake(&reactor, source_task, &mut signal, source_report);

    let signal_mailbox = reactor.task_mailbox(signal_task).expect("signal mailbox");
    let signal_report = reactor.post_signal_delivered_from_hart(
        Arc::downgrade(&signal_mailbox),
        12,
        SignalRouting::ProcessDirected,
        HartId(1),
        &mut signal,
    );
    assert_remote_wake(&reactor, signal_task, &mut signal, signal_report);

    let mut channel_report = WakeDispatchReport::empty();
    let channel_wakes = channel.fire_with_post(Mask::from_bits(0x20), |mailbox, event| {
        let (posted, report) =
            reactor.post_mailbox_ref_event_from_hart(mailbox, event, HartId(1), &mut signal);
        channel_report.merge(report);
        posted
    });
    assert_eq!(channel_wakes, 1);
    assert_remote_wake(&reactor, channel_task, &mut signal, channel_report);
    assert_eq!(channel_completed.load(Ordering::SeqCst), 1);

    let (delegate_fired, delegate_report) =
        reactor.advance_time_to_from_hart_with_reschedule(100, HartId(1), &mut signal);
    assert_eq!(delegate_fired, 1);
    assert_remote_wake(&reactor, delegate_task, &mut signal, delegate_report);

    let (device_source_fired, device_source_report) =
        reactor.advance_time_to_from_hart_with_reschedule(110, HartId(1), &mut signal);
    assert_eq!(device_source_fired, 1);
    assert_remote_wake(
        &reactor,
        device_source_task,
        &mut signal,
        device_source_report,
    );

    let (device_raw_fired, device_raw_report) =
        reactor.advance_time_to_from_hart_with_reschedule(120, HartId(1), &mut signal);
    assert_eq!(device_raw_fired, 1);
    assert_remote_wake(&reactor, device_raw_task, &mut signal, device_raw_report);

    assert_eq!(callback_fires.load(Ordering::SeqCst), 2);
    assert_eq!(source_polls.load(Ordering::SeqCst), 2);
    assert_eq!(signal_polls.load(Ordering::SeqCst), 2);
    assert_eq!(delegate_polls.load(Ordering::SeqCst), 2);
    assert_eq!(device_source_polls.load(Ordering::SeqCst), 2);
    assert_eq!(device_raw_polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.next_deadline_ns(), None);

    unregister_source(device_source.id());
}

fn assert_remote_wake(
    reactor: &Reactor,
    task: tx_reactor::TaskKey,
    signal: &mut RecordingRescheduleSignal,
    report: WakeDispatchReport,
) {
    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(0)]);
    assert_eq!(
        reactor
            .next_scheduled_task(HartId(0))
            .map(|(handle, _)| handle.id()),
        Some(task.id())
    );
    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(0)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    signal.sent.clear();
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
fn runnable_work_on_another_hart_does_not_keep_this_hart_spinning() {
    let reactor = Reactor::new();
    reactor.submit_task_with_meta(async {}, InitialSchedMeta::kernel().with_affinity(0b0100));

    reactor.begin_polling_idle(HartId(0));
    assert!(!reactor.should_leave_polling_idle(HartId(0)));
    assert!(reactor.should_leave_polling_idle(HartId(2)));
    reactor.end_polling_idle(HartId(0));
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
fn concurrent_hart_loop_returns_at_poll_budget_boundary() {
    let shared = SharedReactor::empty();
    assert!(shared.init());
    let stage = Arc::new(AtomicUsize::new(0));
    let task = shared
        .with(|reactor| {
            let stage = Arc::clone(&stage);
            reactor.submit_task_with_meta(
                async move {
                    stage.store(1, Ordering::SeqCst);
                    yield_now().await;
                    stage.store(2, Ordering::SeqCst);
                },
                InitialSchedMeta::kernel().with_affinity(0b0001),
            )
        })
        .expect("initialized reactor");
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let mut clock = ScriptedSliceClock::new(vec![0; 8], deadlines);
    let mut signal = RecordingRescheduleSignal::default();
    let one_poll = HartPollBudget::up_to(1);

    let first = shared
        .run_hart_loop_concurrent_with_slice_clock_and_poll_budget(
            HartId(0),
            0,
            &mut signal,
            &mut clock,
            one_poll,
        )
        .expect("initialized reactor");
    assert_eq!(
        first.stats,
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(stage.load(Ordering::SeqCst), 1);
    shared
        .with(|reactor| assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Runnable)))
        .expect("initialized reactor");

    let second = shared
        .run_hart_loop_concurrent_with_slice_clock_and_poll_budget(
            HartId(0),
            0,
            &mut signal,
            &mut clock,
            one_poll,
        )
        .expect("initialized reactor");
    assert_eq!(
        second.stats,
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(stage.load(Ordering::SeqCst), 2);
    shared
        .with(|reactor| assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed)))
        .expect("initialized reactor");
}

#[test]
fn cooperative_poll_preserves_existing_domain_deadline() {
    let shared = SharedReactor::empty();
    assert!(shared.init());
    let domain_deadline_ns = 1_000_000;
    let _domain_deadline = shared
        .with(|reactor| {
            reactor
                .deadline_registrar_handle()
                .register_deadline(
                    DeadlineNs::new(domain_deadline_ns),
                    TimerRole::DelegateTimeout,
                    TimerTarget::DelegateToken(DelegateTokenId::new(1)),
                )
                .expect("domain deadline registration")
        })
        .expect("initialized reactor");
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let mut clock = ScriptedSliceClock::new(vec![1_000, 1_001], Arc::clone(&deadlines));

    shared
        .with(|reactor| reactor.program_current_hart_deadline(&mut clock))
        .expect("initialized reactor");
    shared
        .with(|reactor| {
            reactor
                .submit_task_with_meta(async {}, InitialSchedMeta::kernel().with_affinity(0b0001))
        })
        .expect("initialized reactor");

    let mut signal = RecordingRescheduleSignal::default();
    let step = shared
        .run_hart_loop_concurrent_with_slice_clock_and_poll_budget(
            HartId(0),
            1_000,
            &mut signal,
            &mut clock,
            HartPollBudget::up_to(1),
        )
        .expect("initialized reactor");

    assert_eq!(
        step.stats,
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(
        deadlines.lock().expect("deadline log poisoned").as_slice(),
        &[Some(domain_deadline_ns)],
        "a cooperative poll must neither replace nor cancel the domain deadline",
    );
}

#[test]
fn slice_clock_restores_unchanged_domain_deadline_before_polling_idle() {
    let shared = SharedReactor::empty();
    assert!(shared.init());
    let domain_deadline_ns = 1_000_000;
    let _domain_deadline = shared
        .with(|reactor| {
            reactor
                .deadline_registrar_handle()
                .register_deadline(
                    DeadlineNs::new(domain_deadline_ns),
                    TimerRole::DelegateTimeout,
                    TimerTarget::DelegateToken(DelegateTokenId::new(1)),
                )
                .expect("domain deadline registration")
        })
        .expect("initialized reactor");
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let mut clock = ScriptedSliceClock::new(vec![1_000, 1_001, 1_001], Arc::clone(&deadlines));

    shared
        .with(|reactor| reactor.program_current_hart_deadline(&mut clock))
        .expect("initialized reactor");
    assert_eq!(
        deadlines.lock().expect("deadline log poisoned").as_slice(),
        &[Some(domain_deadline_ns)],
    );

    let task = shared
        .with(|reactor| {
            reactor
                .submit_task_with_meta(ParkForever, InitialSchedMeta::fair().with_affinity(0b0001))
        })
        .expect("initialized reactor");
    let mut signal = RecordingRescheduleSignal::default();
    let step =
        shared.run_hart_loop_concurrent_with_slice_clock(HartId(0), 1_000, &mut signal, &mut clock);

    assert_eq!(
        step.expect("initialized reactor").stats,
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    shared
        .with(|reactor| {
            assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
            assert!(reactor.is_idle());
            reactor.begin_polling_idle(HartId(0));
            assert!(reactor.is_polling_idle(HartId(0)));
            assert!(!reactor.should_leave_polling_idle(HartId(0)));
            reactor.end_polling_idle(HartId(0));
        })
        .expect("initialized reactor");
    assert_eq!(
        deadlines.lock().expect("deadline log poisoned").as_slice(),
        &[
            Some(domain_deadline_ns),
            Some(1_000 + Phase1Scheduler::NEW_QUEUE_SLICE_NS),
            None,
            Some(domain_deadline_ns),
        ],
        "an unchanged domain deadline must replace the cancelled poll-slice arm before idle",
    );
}

#[test]
fn slice_clock_restore_keeps_concurrently_registered_earlier_domain_deadline() {
    let shared = SharedReactor::empty();
    assert!(shared.init());
    let old_deadline_ns = 1_000_000;
    let new_deadline_ns = 500_000;
    let registrar = shared
        .with(|reactor| reactor.deadline_registrar_handle())
        .expect("initialized reactor");
    let _old_deadline = registrar
        .register_deadline(
            DeadlineNs::new(old_deadline_ns),
            TimerRole::DelegateTimeout,
            TimerTarget::DelegateToken(DelegateTokenId::new(1)),
        )
        .expect("old domain deadline registration");
    let update_started = Arc::new(Barrier::new(2));
    let update_registered = Arc::new(Barrier::new(2));
    let updater = {
        let registrar = registrar.clone();
        let update_started = Arc::clone(&update_started);
        let update_registered = Arc::clone(&update_registered);
        std::thread::spawn(move || {
            update_started.wait();
            let _ = registrar
                .register_deadline(
                    DeadlineNs::new(new_deadline_ns),
                    TimerRole::DelegateTimeout,
                    TimerTarget::DelegateToken(DelegateTokenId::new(2)),
                )
                .expect("new domain deadline registration")
                .forget();
            update_registered.wait();
        })
    };
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let mut clock = BarrierSliceClock::new(
        vec![1_000, 1_001, 1_001],
        Arc::clone(&deadlines),
        update_started,
        update_registered,
    );

    shared
        .with(|reactor| reactor.program_current_hart_deadline(&mut clock))
        .expect("initialized reactor");
    let task = shared
        .with(|reactor| {
            reactor
                .submit_task_with_meta(ParkForever, InitialSchedMeta::fair().with_affinity(0b0001))
        })
        .expect("initialized reactor");
    let mut signal = RecordingRescheduleSignal::default();

    let step = shared
        .run_hart_loop_concurrent_with_slice_clock(HartId(0), 1_000, &mut signal, &mut clock)
        .expect("initialized reactor");
    updater.join().expect("deadline updater panicked");

    assert_eq!(
        step.stats,
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    shared
        .with(|reactor| {
            assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
            assert_eq!(reactor.next_deadline_ns(), Some(new_deadline_ns));
        })
        .expect("initialized reactor");
    assert_eq!(
        deadlines.lock().expect("deadline log poisoned").as_slice(),
        &[
            Some(old_deadline_ns),
            Some(1_000 + Phase1Scheduler::NEW_QUEUE_SLICE_NS),
            None,
            Some(new_deadline_ns),
        ],
        "the restore must arm the deadline registered during slice cancellation",
    );
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
fn concurrent_hart_loop_advances_timer_during_self_yield_storm() {
    let shared = SharedReactor::empty();
    assert!(shared.init());
    let timer_done = Arc::new(AtomicUsize::new(0));
    let spinner_polls = Arc::new(AtomicUsize::new(0));
    let deadlines = Arc::new(Mutex::new(Vec::new()));

    shared
        .with(|reactor| {
            reactor.submit_task_with_meta(
                {
                    let timer_done = Arc::clone(&timer_done);
                    let channel = reactor.channel();
                    async move {
                        let _ = channel
                            .wait_event(
                                Mask::from_bits(0),
                                WaitProtocol::InterruptibleTimeout(10),
                                || false,
                            )
                            .await;
                        timer_done.store(1, Ordering::SeqCst);
                    }
                },
                InitialSchedMeta::fair().userspace_thread(),
            )
        })
        .expect("shared reactor initialized");
    shared
        .with(|reactor| {
            reactor.submit_task_with_meta(
                {
                    let spinner_polls = Arc::clone(&spinner_polls);
                    async move {
                        for _ in 0..4 {
                            spinner_polls.fetch_add(1, Ordering::SeqCst);
                            yield_now().await;
                        }
                    }
                },
                InitialSchedMeta::fair().userspace_thread(),
            )
        })
        .expect("shared reactor initialized");

    let mut signal = RecordingRescheduleSignal::default();
    let mut clock = ScriptedSliceClock::new(vec![0, 0, 11, 11, 11, 11, 11, 11], deadlines);
    let step = shared
        .run_hart_loop_concurrent_with_slice_clock(HartId(0), 0, &mut signal, &mut clock)
        .expect("reactor is initialized");

    assert!(
        step.observed_timer_wakes(),
        "timer should fire inside the hot concurrent poll loop"
    );
    assert_eq!(timer_done.load(Ordering::SeqCst), 1);
    assert!(spinner_polls.load(Ordering::SeqCst) >= 1);
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
            hart: 11,
            other_hart: 0,
            seen: Arc::clone(&seen),
        },
        InitialSchedMeta::kernel().with_affinity(1 << 11),
    );

    assert_eq!(
        reactor.run_until_idle_on_hart(HartId(11)),
        RunStats {
            polled: 1,
            completed: 1,
        }
    );
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
    assert_eq!(
        seen.load(Ordering::SeqCst),
        SAW_MAILBOX | SAW_DEADLINE_REGISTRAR | SAW_DELEGATE_REGISTRY | SAW_OTHER_HART_CLEAR
    );
    assert!(tx_reactor::current_task_mailbox(11).is_none());
    assert!(tx_reactor::current_deadline_registrar(11).is_none());
    assert!(tx_reactor::current_delegate_registry(11).is_none());
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
        SAW_MAILBOX | SAW_DEADLINE_REGISTRAR | SAW_DELEGATE_REGISTRY | SAW_OTHER_HART_CLEAR
    );
    assert!(tx_reactor::current_task_mailbox(1).is_none());
    assert!(tx_reactor::current_deadline_registrar(1).is_none());
    assert!(tx_reactor::current_delegate_registry(1).is_none());
}

#[test]
fn shared_concurrent_idle_hart_steals_only_preempted_movable_work() {
    let shared = SharedReactor::empty();
    assert!(shared.init());
    let movable_polls = Arc::new(AtomicUsize::new(0));
    let pinned_polls = Arc::new(AtomicUsize::new(0));
    let new_polls = Arc::new(AtomicUsize::new(0));

    shared
        .with(|reactor| {
            reactor.submit_task_with_meta(
                CountPolls {
                    polls: Arc::clone(&movable_polls),
                },
                InitialSchedMeta::fair()
                    .with_affinity(0b0011)
                    .movable()
                    .preempted_on_submit(),
            );
            reactor.submit_task_with_meta(
                CountPolls {
                    polls: Arc::clone(&pinned_polls),
                },
                InitialSchedMeta::fair()
                    .with_affinity(0b0011)
                    .pinned()
                    .preempted_on_submit(),
            );
        })
        .expect("shared reactor initialized");

    let mut signal = RecordingRescheduleSignal::default();
    let step = shared
        .run_hart_loop_concurrent(HartId(1), 0, &mut signal)
        .expect("shared reactor initialized");
    assert_eq!(step.stats.polled, 1);
    assert_eq!(step.stats.completed, 1);
    assert_eq!(movable_polls.load(Ordering::SeqCst), 1);
    assert_eq!(pinned_polls.load(Ordering::SeqCst), 0);

    let step = shared
        .run_hart_loop_concurrent(HartId(0), 0, &mut signal)
        .expect("shared reactor initialized");
    assert_eq!(step.stats.polled, 1);
    assert_eq!(step.stats.completed, 1);
    assert_eq!(pinned_polls.load(Ordering::SeqCst), 1);

    shared
        .with(|reactor| {
            reactor.submit_task_with_meta(
                CountPolls {
                    polls: Arc::clone(&new_polls),
                },
                InitialSchedMeta::fair().with_affinity(0b0011).movable(),
            );
        })
        .expect("shared reactor initialized");

    // A never-polled task remains in New on its initial owner.  The idle peer
    // must not pull it before that first per-hart context installation.
    let step = shared
        .run_hart_loop_concurrent(HartId(1), 0, &mut signal)
        .expect("shared reactor initialized");
    assert_eq!(step.stats.polled, 0);
    assert_eq!(new_polls.load(Ordering::SeqCst), 0);

    let step = shared
        .run_hart_loop_concurrent(HartId(0), 0, &mut signal)
        .expect("shared reactor initialized");
    assert_eq!(step.stats.polled, 1);
    assert_eq!(step.stats.completed, 1);
    assert_eq!(new_polls.load(Ordering::SeqCst), 1);
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
    let first = scheduler.pick_next(HartId(0));
    assert_eq!(
        first,
        Some((
            TaskHandle::new(fair_preempted),
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS
            }
        ))
    );
    assert!(scheduler.mark_dispatching_polling(fair_preempted, HartId(0)));
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
    let first = scheduler.pick_next(HartId(0));
    assert_eq!(
        first.map(|(_, slice)| slice),
        Some(SliceConfig::Preemptive {
            slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        })
    );
    assert!(scheduler.mark_dispatching_polling(task, HartId(0)));

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

struct DeadlineDomainParkWithoutMailboxWaker {
    hart: usize,
    deadline_ns: u64,
    polls: Arc<AtomicUsize>,
    guard: Option<TimerGuard>,
}

impl Future for DeadlineDomainParkWithoutMailboxWaker {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.polls.fetch_add(1, Ordering::SeqCst);

        let mailbox = current_task_mailbox(this.hart).expect("current task mailbox");
        if this.guard.is_none() {
            let registrar = current_deadline_registrar(this.hart).expect("deadline registrar");
            this.guard = Some(
                registrar
                    .register_deadline(
                        DeadlineNs::new(this.deadline_ns),
                        TimerRole::PrimarySleep,
                        TimerTarget::TaskMailbox(Arc::downgrade(&mailbox)),
                    )
                    .expect("primary sleep registration"),
            );
            return Poll::Pending;
        }

        while let Some(event) = mailbox.poll() {
            if matches!(event, MailboxEvent::TimerFired { .. }) {
                this.guard.take();
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
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

#[test]
fn deadline_domain_expiry_routes_parked_task_through_scheduler_without_mailbox_waker() {
    const TIMER_HART: usize = 7;
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        DeadlineDomainParkWithoutMailboxWaker {
            hart: TIMER_HART,
            deadline_ns: 10,
            polls: Arc::clone(&polls),
            guard: None,
        },
        InitialSchedMeta::fair()
            .userspace_thread()
            .with_affinity(1 << TIMER_HART),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(TIMER_HART)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_deadline_ns(), Some(10));

    assert_eq!(reactor.advance_time_to(10), 1);
    let stats = reactor.run_until_idle_on_hart(HartId(TIMER_HART));

    assert_eq!(stats.completed, 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
}

#[test]
fn deadline_domain_expiry_from_remote_hart_requests_remote_ipi_and_requeues_owner() {
    const TIMER_HART: usize = 6;
    const CURRENT_HART: usize = 1;
    let reactor = Reactor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = reactor.submit_task_with_meta(
        DeadlineDomainParkWithoutMailboxWaker {
            hart: TIMER_HART,
            deadline_ns: 10,
            polls: Arc::clone(&polls),
            guard: None,
        },
        InitialSchedMeta::fair()
            .userspace_thread()
            .with_affinity(1 << TIMER_HART),
    );

    assert_eq!(reactor.run_until_idle_on_hart(HartId(TIMER_HART)).polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(reactor.next_deadline_ns(), Some(10));

    let mut signal = RecordingRescheduleSignal::default();
    let (_wakes, report) =
        reactor.advance_time_to_from_hart_with_reschedule(10, HartId(CURRENT_HART), &mut signal);

    assert_eq!(
        report,
        WakeDispatchReport {
            placements: 1,
            local_reschedules: 0,
            remote_ipis: 1,
        }
    );
    assert_eq!(signal.sent, vec![HartId(TIMER_HART)]);
    assert!(reactor.dispatch_markers(HartId(TIMER_HART)).need_resched());

    let stats = reactor.run_until_idle_on_hart(HartId(TIMER_HART));
    assert_eq!(stats.completed, 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Completed));
}
