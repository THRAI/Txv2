//! Cooperative host reactor runtime.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    future::Future,
    ptr,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
    task::{Context, Poll},
};

use crate::adapter::bus_wire::{
    DeclaredPort, DeclaredQueue, DelegateRegistry, TaskMailbox, TimerWheel, WireDeclaration,
    WireDeclarationError, WireEventSet,
};

use crate::{
    ast::{AstBatch, AstMarker, AstQueueEffect},
    dispatch::{NoopRescheduleSignal, RescheduleSignal, WakeDispatchAction, WakeDispatchReport},
    hart_loop::HartLoopStep,
    preempt::PreemptMarkers,
    scheduler::{
        HartId, HartSchedulerLocal, InitialSchedMeta, LocalAffinityMove, LocalEnqueueRequest,
        Phase1Scheduler, RunnablePlacement, SchedulerAffinityError, SchedulerStats, SliceConfig,
        StopReason, TaskHandle, TaskRunOwner, WakeHint,
    },
    spin_lock::SpinLock,
    task::{
        PendingPollCommit, TakeRunnableError, TaskDrainRecord, TaskId, TaskKey, TaskLifecycleError,
        TaskStatus, TaskTable,
    },
    timer::{DeadlineFuture, TimerQueue},
    userspace::{
        UserspaceEntryCheckpoint, UserspaceEntryDecision, UserspaceEntryOutcome,
        UserspaceEntryTaskError, UserspaceRunError, UserspaceRunRequest, UserspaceRunSlot,
        UserspaceRunStatus, UserspaceRunWait, UserspaceTrapInfo,
    },
    wait,
    waker::task_waker,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunStats {
    pub polled: usize,
    pub completed: usize,
}

impl RunStats {
    pub const fn empty() -> Self {
        Self {
            polled: 0,
            completed: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunIdleReport {
    stats: RunStats,
    timer_wakes: usize,
    idle: bool,
    next_deadline_ns: Option<u64>,
}

impl RunIdleReport {
    pub const fn stats(&self) -> RunStats {
        self.stats
    }

    pub const fn timer_wakes(&self) -> usize {
        self.timer_wakes
    }

    pub const fn is_idle(&self) -> bool {
        self.idle
    }

    pub const fn next_deadline_ns(&self) -> Option<u64> {
        self.next_deadline_ns
    }
}

fn earliest_deadline(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub(crate) trait ClockSource {
    fn now_ns(&mut self) -> u64;
    fn set_deadline_ns(&mut self, deadline_ns: u64);
    fn cancel_deadline(&mut self);
}

struct CallbackClock<N, D> {
    now_ns: N,
    program_deadline: D,
}

impl<N, D> ClockSource for CallbackClock<N, D>
where
    N: FnMut() -> u64,
    D: FnMut(Option<u64>),
{
    fn now_ns(&mut self) -> u64 {
        (self.now_ns)()
    }

    fn set_deadline_ns(&mut self, deadline_ns: u64) {
        (self.program_deadline)(Some(deadline_ns));
    }

    fn cancel_deadline(&mut self) {
        (self.program_deadline)(None);
    }
}

struct NoopSliceClock;

impl SliceClock for NoopSliceClock {
    fn now_ns(&mut self) -> u64 {
        0
    }

    fn set_deadline_ns(&mut self, _deadline_ns: u64) {}

    fn cancel_deadline(&mut self) {}
}

pub trait SliceClock {
    fn now_ns(&mut self) -> u64;
    fn set_deadline_ns(&mut self, deadline_ns: u64);
    fn cancel_deadline(&mut self);
}

#[derive(Clone, Copy, Debug)]
struct PollTiming {
    slice: SliceConfig,
    start_ns: u64,
}

impl PollTiming {
    fn start<C: SliceClock>(slice: SliceConfig, clock: &mut C) -> Self {
        let start_ns = clock.now_ns();
        if let SliceConfig::Preemptive { slice_ns } = slice {
            clock.set_deadline_ns(start_ns.saturating_add(slice_ns));
        }
        Self { slice, start_ns }
    }

    fn finish<C: SliceClock>(self, clock: &mut C) -> PollAccounting {
        let end_ns = clock.now_ns();
        clock.cancel_deadline();
        let consumed_ns = end_ns.saturating_sub(self.start_ns);
        let slice_expired = matches!(
            self.slice,
            SliceConfig::Preemptive { slice_ns } if consumed_ns >= slice_ns
        );
        PollAccounting {
            consumed_ns,
            slice_expired,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PollAccounting {
    consumed_ns: u64,
    slice_expired: bool,
}

pub struct Reactor {
    shared: ReactorShared,
    locals: ReactorLocals,
}

pub struct ReactorShared {
    tasks: SpinLock<TaskTable>,
    scheduler: Phase1Scheduler,
    observability: SpinLock<ReactorObservability>,
    timers: SpinLock<TimerQueue>,
    timer_wheel: TimerWheel,
    delegate_registry: Arc<DelegateRegistry>,
    userspace: SpinLock<UserspaceRunSlot>,
}

const MAX_REACTOR_HARTS: usize = 64;

pub struct ReactorLocals {
    harts: [AtomicPtr<HartReactorLocal>; MAX_REACTOR_HARTS],
    len: AtomicUsize,
    init_lock: SpinLock<()>,
}

impl ReactorLocals {
    fn new() -> Self {
        Self {
            harts: [const { AtomicPtr::new(ptr::null_mut()) }; MAX_REACTOR_HARTS],
            len: AtomicUsize::new(0),
            init_lock: SpinLock::new(()),
        }
    }

    #[track_caller]
    fn ensure_hart(&self, hart: HartId) {
        if hart.0 >= MAX_REACTOR_HARTS {
            let caller = core::panic::Location::caller();
            panic!(
                "hart {} exceeds MAX_REACTOR_HARTS at {}:{}",
                hart.0,
                caller.file(),
                caller.line()
            );
        }
        if !self.harts[hart.0].load(Ordering::Acquire).is_null() {
            return;
        }
        let _guard = self.init_lock.lock();
        while self.len.load(Ordering::Acquire) <= hart.0 {
            let index = self.len.load(Ordering::Acquire);
            let local = Box::leak(Box::new(HartReactorLocal::new(HartId(index))));
            self.harts[index].store(local as *mut HartReactorLocal, Ordering::Release);
            self.len.store(index + 1, Ordering::Release);
        }
    }

    fn get(&self, hart: HartId) -> Option<&HartReactorLocal> {
        if hart.0 >= MAX_REACTOR_HARTS {
            return None;
        }
        let ptr = self.harts[hart.0].load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }

    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    fn total_queue_depth(&self, hart: HartId) -> usize {
        let Some(local) = self.get(hart) else {
            return 0;
        };
        let depths = local.scheduler().queue_depths();
        depths.kernel + depths.new + depths.preempted
    }
}

pub struct HartReactorLocal {
    hart: HartId,
    scheduler: HartSchedulerLocal,
}

impl HartReactorLocal {
    fn new(hart: HartId) -> Self {
        Self {
            hart,
            scheduler: HartSchedulerLocal::new(),
        }
    }

    pub const fn hart(&self) -> HartId {
        self.hart
    }

    pub(crate) fn scheduler(&self) -> &HartSchedulerLocal {
        &self.scheduler
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HartRunStats {
    pub polled: u64,
    pub completed: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReactorObservability {
    per_hart: Vec<HartRunStats>,
}

impl ReactorObservability {
    pub fn hart(&self, hart: HartId) -> HartRunStats {
        self.per_hart.get(hart.0).copied().unwrap_or_default()
    }

    pub fn per_hart(&self) -> &[HartRunStats] {
        &self.per_hart
    }

    fn record_run_stats(&mut self, hart: HartId, stats: RunStats) {
        while self.per_hart.len() <= hart.0 {
            self.per_hart.push(HartRunStats::default());
        }
        let slot = &mut self.per_hart[hart.0];
        slot.polled = slot.polled.saturating_add(stats.polled as u64);
        slot.completed = slot.completed.saturating_add(stats.completed as u64);
    }
}

pub struct SharedReactor {
    reactor: SpinLock<Option<&'static Reactor>>,
}

impl SharedReactor {
    pub const fn empty() -> Self {
        Self {
            reactor: SpinLock::new(None),
        }
    }

    pub fn init(&self) -> bool {
        let mut slot = self.reactor.lock();
        if slot.is_some() {
            return false;
        }

        *slot = Some(Box::leak(Box::new(Reactor::new())));
        true
    }

    pub fn is_initialized(&self) -> bool {
        self.reactor.lock().is_some()
    }

    fn initialized(&self) -> Option<&'static Reactor> {
        *self.reactor.lock()
    }

    pub fn with<R>(&self, f: impl FnOnce(&Reactor) -> R) -> Option<R> {
        self.initialized().map(f)
    }

    /// Borrow the shared reactor state together with the caller hart's local
    /// runtime slot.
    ///
    /// The outer `SharedReactor` lock is only an initialization slot now; the
    /// returned shared/local state is protected by its own narrow locks.
    pub fn with_hart<R>(
        &self,
        hart: HartId,
        f: impl FnOnce(&ReactorShared, &HartReactorLocal) -> R,
    ) -> Option<R> {
        let reactor = self.initialized()?;
        reactor.locals.ensure_hart(hart);
        let local = reactor.locals.get(hart)?;
        debug_assert_eq!(local.hart(), hart);
        let _ = local.scheduler();
        Some(f(&reactor.shared, local))
    }

    /// Borrow the reactor as a per-hart runtime view.
    ///
    /// This routes hart-loop code through the shared/locals split; task table,
    /// scheduler metadata, timers, userspace slot, observability, and hart-local
    /// queues/inboxes each provide their own synchronization.
    pub fn with_hart_runtime<R>(
        &self,
        hart: HartId,
        f: impl FnOnce(&mut HartRuntimeView<'_>) -> R,
    ) -> Option<R> {
        let reactor = self.initialized()?;
        let mut view = reactor.hart_runtime_view(hart);
        Some(f(&mut view))
    }

    /// Poll lease: take future with narrow locks → poll → commit with narrow locks.
    pub fn run_hart_loop_concurrent<S>(
        &self,
        hart: HartId,
        now_ns: u64,
        signal: &mut S,
    ) -> Option<HartLoopStep>
    where
        S: RescheduleSignal,
    {
        self.run_hart_loop_concurrent_with_slice_clock(hart, now_ns, signal, &mut NoopSliceClock)
    }

    pub fn run_hart_loop_concurrent_with_slice_clock<S, C>(
        &self,
        hart: HartId,
        now_ns: u64,
        signal: &mut S,
        slice_clock: &mut C,
    ) -> Option<HartLoopStep>
    where
        S: RescheduleSignal,
        C: SliceClock,
    {
        use crate::waker::task_waker;
        use core::task::{Context, Poll};

        let reactor = self.initialized()?;
        let mut stats = RunStats::empty();
        let timer_wakes: usize;
        let wake_report: WakeDispatchReport;

        // Phase 1: advance time & drain wakes through shared/local locks.
        {
            let mut view = reactor.hart_runtime_view(hart);
            timer_wakes = view.advance_time_to(now_ns);
            wake_report = view.drain_wakes_for_hart(hart, signal);
            view.rebalance_local_at(hart, now_ns);
        }

        // Phase 2: poll loop — take a task future, then poll outside reactor locks.
        loop {
            let poll_packet = {
                let mut view = reactor.hart_runtime_view(hart);
                view.drain_wakes_for_hart(hart, signal);
                let Some((handle, slice)) = view.pick_next_or_steal_local(hart) else {
                    break;
                };
                let packet = match view
                    .shared
                    .tasks
                    .lock()
                    .take_runnable_future_by_id(handle.id())
                {
                    Ok((key, future, wake_state, mailbox)) => {
                        crate::task::set_current_mailbox(hart.0, Some(mailbox));
                        crate::task::set_current_timer_wheel(
                            hart.0,
                            Some(view.shared.timer_wheel.clone()),
                        );
                        crate::task::set_current_delegate_registry(
                            hart.0,
                            Some(Arc::clone(&view.shared.delegate_registry)),
                        );
                        Some((key, future, wake_state, slice))
                    }
                    Err(TakeRunnableError::Missing) => {
                        view.shared.scheduler.task_dropped(handle.id());
                        continue;
                    }
                    Err(TakeRunnableError::NotRunnable(status)) => {
                        match status {
                            TaskStatus::Completed | TaskStatus::Cancelled => {
                                view.shared.scheduler.task_dropped(handle.id());
                            }
                            _ => {
                                view.task_stopped_local(handle.id(), StopReason::Blocked, 0, hart);
                            }
                        }
                        continue;
                    }
                };
                packet
            };

            let Some((key, mut future, wake_state, slice)) = poll_packet else {
                continue;
            };

            let waker = task_waker(wake_state);
            let mut cx = Context::from_waker(&waker);
            stats.polled += 1;
            let timing = PollTiming::start(slice, slice_clock);
            let result = future.as_mut().poll(&mut cx);
            let accounting = timing.finish(slice_clock);

            crate::task::set_current_mailbox(hart.0, None);
            crate::task::set_current_timer_wheel(hart.0, None);
            crate::task::set_current_delegate_registry(hart.0, None);

            // Phase 3: commit state through task/scheduler/local locks.
            {
                let mut view = reactor.hart_runtime_view(hart);
                match result {
                    Poll::Ready(()) => {
                        if view
                            .shared
                            .tasks
                            .lock()
                            .finish_polled_complete(key, future)
                            .is_ok()
                        {
                            view.task_stopped_local(
                                key.id(),
                                StopReason::Completed,
                                accounting.consumed_ns,
                                hart,
                            );
                            view.shared.scheduler.task_dropped(key.id());
                            stats.completed += 1;
                        }
                    }
                    Poll::Pending => {
                        let userspace_preempted = view.take_userspace_preempt_marker(hart);
                        if userspace_preempted {
                            if view
                                .shared
                                .tasks
                                .lock()
                                .finish_polled_runnable(key, future, StopReason::PreemptedExternal)
                                .is_ok()
                            {
                                view.task_stopped_local(
                                    key.id(),
                                    StopReason::PreemptedExternal,
                                    accounting.consumed_ns,
                                    hart,
                                );
                                let _ = view.dispatch_queued_task_from_hart(key.id(), hart, signal);
                            }
                        } else if accounting.slice_expired {
                            if view
                                .shared
                                .tasks
                                .lock()
                                .finish_polled_runnable(key, future, StopReason::SliceExpired)
                                .is_ok()
                            {
                                view.task_stopped_local(
                                    key.id(),
                                    StopReason::SliceExpired,
                                    accounting.consumed_ns,
                                    hart,
                                );
                                let _ = view.dispatch_queued_task_from_hart(key.id(), hart, signal);
                            }
                        } else {
                            let pending_commit =
                                { view.shared.tasks.lock().finish_polled_pending(key, future) };
                            match pending_commit {
                                Ok(PendingPollCommit::Woken) => {
                                    view.mark_runnable_from_hart(
                                        key,
                                        WakeHint::Normal,
                                        hart,
                                        signal,
                                    );
                                }
                                Ok(PendingPollCommit::Parked) => {
                                    view.task_stopped_local(
                                        key.id(),
                                        StopReason::Blocked,
                                        accounting.consumed_ns,
                                        hart,
                                    );
                                }
                                Err(_) => {}
                            }
                        }
                    }
                }
            }
        }

        let (consumed_markers, next_deadline_ns) = {
            let mut view = reactor.hart_runtime_view(hart);
            view.record_run_stats(hart, stats);
            (view.consume_dispatch_markers(hart), view.next_deadline_ns())
        };

        Some(HartLoopStep::new(
            hart,
            now_ns,
            stats,
            wake_report,
            consumed_markers,
            timer_wakes,
            next_deadline_ns,
        ))
    }
}

pub struct HartRuntimeView<'a> {
    shared: &'a ReactorShared,
    locals: &'a ReactorLocals,
}

impl HartRuntimeView<'_> {
    pub fn advance_time_to(&self, now_ns: u64) -> usize {
        let wheel_wakes = self.shared.timer_wheel.fire_due(now_ns);
        let queue_wakes = self.shared.timers.lock().advance_time_to(now_ns);
        wheel_wakes + queue_wakes
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        earliest_deadline(
            self.shared.timers.lock().next_deadline_ns(),
            self.shared.timer_wheel.next_deadline_ns(),
        )
    }

    pub fn drain_wakes_for_hart<S>(
        &mut self,
        current_hart: HartId,
        signal: &mut S,
    ) -> WakeDispatchReport
    where
        S: RescheduleSignal,
    {
        let mut report = WakeDispatchReport::empty();
        for id in self.shared.tasks.lock().drain_wake_ids() {
            let Some(target_hart) = self.shared.scheduler.target_hart_for_wake(id) else {
                continue;
            };
            self.locals.ensure_hart(target_hart);
            if let Some(target_local) = self.locals.get(target_hart) {
                Phase1Scheduler::push_wake_inbox_to_local(target_local.scheduler(), id);
            }
            if target_hart != current_hart {
                signal.send_reschedule_ipi(target_hart);
                report.record(WakeDispatchAction {
                    target_hart,
                    wake_remote: true,
                });
            }
        }

        self.locals.ensure_hart(current_hart);
        let drained = self
            .locals
            .get(current_hart)
            .map(|local| {
                Phase1Scheduler::drain_wake_inbox_from_local(
                    local.scheduler(),
                    Reactor::WAKE_INBOX_DRAIN_LIMIT,
                )
            })
            .unwrap_or_default();

        for id in drained {
            let Some(key) = self.shared.tasks.lock().take_wake_if_parked_by_id(id) else {
                continue;
            };
            if let Some((placement, request)) = self.shared.scheduler.task_runnable_from_for_locals(
                key.id(),
                WakeHint::Normal,
                current_hart,
            ) {
                self.apply_local_enqueue(request);
                if placement.target_hart != current_hart {
                    report.record(self.apply_runnable_placement(placement, signal));
                } else {
                    report.record(WakeDispatchAction {
                        target_hart: placement.target_hart,
                        wake_remote: false,
                    });
                }
            }
        }
        report
    }

    pub fn consume_dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.locals
            .get(hart)
            .map(|local| Phase1Scheduler::consume_markers_from_local(local.scheduler()))
            .unwrap_or_else(PreemptMarkers::empty)
    }

    pub fn run_until_idle_on_hart_with_reschedule<S>(
        &mut self,
        hart: HartId,
        signal: &mut S,
    ) -> RunStats
    where
        S: RescheduleSignal,
    {
        self.run_until_idle_on_hart_with_reschedule_and_slice_clock(
            hart,
            signal,
            &mut NoopSliceClock,
        )
    }

    pub fn run_until_idle_on_hart_with_reschedule_and_slice_clock<S, C>(
        &mut self,
        hart: HartId,
        signal: &mut S,
        slice_clock: &mut C,
    ) -> RunStats
    where
        S: RescheduleSignal,
        C: SliceClock,
    {
        let mut stats = RunStats {
            polled: 0,
            completed: 0,
        };
        loop {
            let Some((key, mut future, wake_state, slice)) = ({
                self.drain_wakes_for_hart(hart, signal);
                let Some((handle, slice)) = self.pick_next_or_steal_local(hart) else {
                    break;
                };
                match self
                    .shared
                    .tasks
                    .lock()
                    .take_runnable_future_by_id(handle.id())
                {
                    Ok((key, future, wake_state, mailbox)) => {
                        crate::task::set_current_mailbox(hart.0, Some(mailbox));
                        crate::task::set_current_timer_wheel(
                            hart.0,
                            Some(self.shared.timer_wheel.clone()),
                        );
                        crate::task::set_current_delegate_registry(
                            hart.0,
                            Some(Arc::clone(&self.shared.delegate_registry)),
                        );
                        Some((key, future, wake_state, slice))
                    }
                    Err(TakeRunnableError::Missing) => {
                        self.shared.scheduler.task_dropped(handle.id());
                        continue;
                    }
                    Err(TakeRunnableError::NotRunnable(status)) => {
                        match status {
                            TaskStatus::Completed | TaskStatus::Cancelled => {
                                self.shared.scheduler.task_dropped(handle.id());
                            }
                            _ => {
                                self.task_stopped_local(handle.id(), StopReason::Blocked, 0, hart);
                            }
                        }
                        continue;
                    }
                }
            }) else {
                continue;
            };

            let waker = task_waker(wake_state);
            let mut cx = Context::from_waker(&waker);
            stats.polled += 1;
            let timing = PollTiming::start(slice, slice_clock);
            let result = future.as_mut().poll(&mut cx);
            let accounting = timing.finish(slice_clock);
            crate::task::set_current_mailbox(hart.0, None);
            crate::task::set_current_timer_wheel(hart.0, None);
            crate::task::set_current_delegate_registry(hart.0, None);

            match result {
                Poll::Ready(()) => {
                    if self
                        .shared
                        .tasks
                        .lock()
                        .finish_polled_complete(key, future)
                        .is_ok()
                    {
                        self.task_stopped_local(
                            key.id(),
                            StopReason::Completed,
                            accounting.consumed_ns,
                            hart,
                        );
                        self.shared.scheduler.task_dropped(key.id());
                        stats.completed += 1;
                    }
                }
                Poll::Pending => {
                    let userspace_preempted = self.take_userspace_preempt_marker(hart);
                    if userspace_preempted {
                        if self
                            .shared
                            .tasks
                            .lock()
                            .finish_polled_runnable(key, future, StopReason::PreemptedExternal)
                            .is_ok()
                        {
                            self.task_stopped_local(
                                key.id(),
                                StopReason::PreemptedExternal,
                                accounting.consumed_ns,
                                hart,
                            );
                            let _ = self.dispatch_queued_task_from_hart(key.id(), hart, signal);
                        }
                    } else if accounting.slice_expired {
                        if self
                            .shared
                            .tasks
                            .lock()
                            .finish_polled_runnable(key, future, StopReason::SliceExpired)
                            .is_ok()
                        {
                            self.task_stopped_local(
                                key.id(),
                                StopReason::SliceExpired,
                                accounting.consumed_ns,
                                hart,
                            );
                            let _ = self.dispatch_queued_task_from_hart(key.id(), hart, signal);
                        }
                    } else {
                        let pending_commit =
                            { self.shared.tasks.lock().finish_polled_pending(key, future) };
                        match pending_commit {
                            Ok(PendingPollCommit::Woken) => {
                                self.mark_runnable_from_hart(key, WakeHint::Normal, hart, signal);
                            }
                            Ok(PendingPollCommit::Parked) => {
                                self.task_stopped_local(
                                    key.id(),
                                    StopReason::Blocked,
                                    accounting.consumed_ns,
                                    hart,
                                );
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
        }

        self.record_run_stats(hart, stats);
        stats
    }

    fn record_run_stats(&mut self, hart: HartId, stats: RunStats) {
        self.shared
            .observability
            .lock()
            .record_run_stats(hart, stats);
    }

    fn apply_local_enqueue(&mut self, request: LocalEnqueueRequest) {
        self.locals.ensure_hart(request.hart);
        if let Some(local) = self.locals.get(request.hart) {
            Phase1Scheduler::push_to_local_queue(
                local.scheduler(),
                request.task,
                request.queue,
                request.front,
            );
        }
    }

    fn task_stopped_local(
        &mut self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    ) {
        if let Some(request) =
            self.shared
                .scheduler
                .task_stopped_for_locals(task, reason, consumed_ns, hart)
        {
            self.apply_local_enqueue(request);
        }
    }

    fn pick_next_or_steal_local(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.locals.ensure_hart(hart);
        if let Some(local) = self.locals.get(hart) {
            if let Some(next) = self
                .shared
                .scheduler
                .pick_next_from_local(hart, local.scheduler())
            {
                return Some(next);
            }
        }

        let victim = self.busiest_local_steal_victim(hart)?;
        let handle = self.try_steal_between_locals(hart, victim)?;
        self.locals
            .get(hart)
            .and_then(|local| {
                self.shared
                    .scheduler
                    .pick_next_from_local(hart, local.scheduler())
            })
            .or(Some((
                handle,
                SliceConfig::Preemptive {
                    slice_ns: Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS,
                },
            )))
    }

    fn busiest_local_steal_victim(&self, thief: HartId) -> Option<HartId> {
        let mut best = None;
        let mut best_depth = 0;
        for victim_index in 0..self.locals.len() {
            let victim = HartId(victim_index);
            if victim == thief {
                continue;
            }
            let depth = self
                .locals
                .get(victim)
                .map(|local| {
                    let depths = local.scheduler().queue_depths();
                    depths.new + depths.preempted
                })
                .unwrap_or(0);
            if depth > best_depth {
                best = Some(victim);
                best_depth = depth;
            }
        }
        best
    }

    fn try_steal_between_locals(&mut self, thief: HartId, victim: HartId) -> Option<TaskHandle> {
        let thief_local = self.locals.get(thief)?.scheduler();
        let victim_local = self.locals.get(victim)?.scheduler();
        self.shared
            .scheduler
            .try_steal_from_locals(thief, thief_local, victim, victim_local)
    }

    fn rebalance_local_at(&mut self, hart: HartId, now_ns: u64) -> Option<TaskHandle> {
        self.locals.ensure_hart(hart);
        let local = self.locals.get(hart)?.scheduler();
        let victim = self.shared.scheduler.rebalance_victim_from_locals(
            hart,
            local,
            now_ns,
            self.locals.len(),
            |victim| {
                self.locals
                    .get(victim)
                    .map(|local| {
                        let depths = local.scheduler().queue_depths();
                        depths.new + depths.preempted
                    })
                    .unwrap_or(0)
            },
        )?;
        let stolen = self.try_steal_between_locals(hart, victim);
        if stolen.is_some() {
            self.shared.scheduler.record_rebalance_move();
        }
        stolen
    }

    fn take_userspace_preempt_marker(&self, hart: HartId) -> bool {
        self.locals
            .get(hart)
            .is_some_and(|local| Phase1Scheduler::take_userspace_preempt_local(local.scheduler()))
    }

    fn mark_runnable_from_hart<S>(
        &mut self,
        key: TaskKey,
        hint: WakeHint,
        current_hart: HartId,
        signal: &mut S,
    ) -> WakeDispatchReport
    where
        S: RescheduleSignal,
    {
        let mut report = WakeDispatchReport::empty();
        if self.shared.tasks.lock().mark_runnable(key).is_ok() {
            if let Some((placement, request)) =
                self.shared
                    .scheduler
                    .task_runnable_from_for_locals(key.id(), hint, current_hart)
            {
                self.apply_local_enqueue(request);
                report.record(self.apply_runnable_placement(placement, signal));
            }
        }
        report
    }

    fn apply_runnable_placement<S>(
        &mut self,
        placement: RunnablePlacement,
        signal: &mut S,
    ) -> WakeDispatchAction
    where
        S: RescheduleSignal,
    {
        self.locals.ensure_hart(placement.target_hart);
        if let Some(local) = self.locals.get(placement.target_hart) {
            Phase1Scheduler::mark_need_resched_local(local.scheduler());
        }
        if placement.wake_remote {
            signal.send_reschedule_ipi(placement.target_hart);
        }

        WakeDispatchAction {
            target_hart: placement.target_hart,
            wake_remote: placement.wake_remote,
        }
    }

    fn dispatch_queued_task_from_hart<S>(
        &mut self,
        task: TaskId,
        current_hart: HartId,
        signal: &mut S,
    ) -> Option<WakeDispatchAction>
    where
        S: RescheduleSignal,
    {
        let Some(TaskRunOwner::Queued { hart, .. }) = self.shared.scheduler.task_owner(task) else {
            return None;
        };

        Some(self.apply_runnable_placement(
            RunnablePlacement {
                target_hart: hart,
                wake_remote: hart != current_hart,
            },
            signal,
        ))
    }
}

impl Reactor {
    const WAKE_INBOX_DRAIN_LIMIT: usize = 64;

    pub fn new() -> Self {
        Self {
            shared: ReactorShared {
                tasks: SpinLock::new(TaskTable::new()),
                scheduler: Phase1Scheduler::new(),
                observability: SpinLock::new(ReactorObservability::default()),
                timers: SpinLock::new(TimerQueue::new()),
                timer_wheel: TimerWheel::new(),
                delegate_registry: Arc::new(DelegateRegistry::new()),
                userspace: SpinLock::new(UserspaceRunSlot::new()),
            },
            locals: ReactorLocals::new(),
        }
    }

    fn hart_runtime_view(&self, hart: HartId) -> HartRuntimeView<'_> {
        self.locals.ensure_hart(hart);
        HartRuntimeView {
            shared: &self.shared,
            locals: &self.locals,
        }
    }

    /// Creates a wait channel attached to this reactor's timer queue.
    pub fn channel(&self) -> wait::Channel {
        wait::Channel::with_timer_queue(self.shared.timers.lock().clone())
    }

    /// Creates a typed declared wait channel attached to this reactor's timer queue.
    pub fn declared_channel<E>(
        &self,
        declaration: WireDeclaration<E>,
    ) -> Result<wait::DeclaredChannel<E>, WireDeclarationError>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredChannel::with_timer_queue(declaration, self.shared.timers.lock().clone())
    }

    /// Attaches an existing typed declared bus port to this reactor's timer queue.
    pub fn declared_channel_from_port<E>(&self, port: DeclaredPort<E>) -> wait::DeclaredChannel<E>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredChannel::from_port_with_timer_queue(port, self.shared.timers.lock().clone())
    }

    /// Creates a typed declared readiness channel attached to this reactor's timer queue.
    pub fn declared_readiness_channel<E>(
        &self,
        declaration: WireDeclaration<E>,
    ) -> Result<wait::DeclaredReadinessChannel<E>, WireDeclarationError>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredReadinessChannel::with_timer_queue(
            declaration,
            self.shared.timers.lock().clone(),
        )
    }

    /// Attaches an existing typed declared bus queue to this reactor's timer queue.
    pub fn declared_readiness_channel_from_queue<E>(
        &self,
        queue: DeclaredQueue<E>,
    ) -> wait::DeclaredReadinessChannel<E>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredReadinessChannel::from_queue_with_timer_queue(
            queue,
            self.shared.timers.lock().clone(),
        )
    }

    /// Advances the reactor-owned absolute nanosecond clock and wakes expired timers.
    pub fn advance_time_to(&self, now_ns: u64) -> usize {
        let wheel_wakes = self.shared.timer_wheel.fire_due(now_ns);
        let queue_wakes = self.shared.timers.lock().advance_time_to(now_ns);
        wheel_wakes + queue_wakes
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        earliest_deadline(
            self.shared.timers.lock().next_deadline_ns(),
            self.shared.timer_wheel.next_deadline_ns(),
        )
    }

    /// Create a future that resolves once the reactor's clock advances past `deadline_ns`.
    pub fn sleep_until(&self, deadline_ns: u64) -> DeadlineFuture {
        self.shared.timers.lock().wait_until(deadline_ns)
    }

    /// Clone the reactor's timer queue so callers can schedule deadline
    /// futures without holding the reactor lock.
    pub fn timer_queue(&self) -> TimerQueue {
        self.shared.timers.lock().clone()
    }

    /// Drive expired timers from a monotonic clock, run ready work, then
    /// program the next absolute timer deadline.
    ///
    /// `program_deadline` receives `Some(deadline_ns)` to arm the current
    /// clock source, or `None` to cancel it. This mirrors the HAL `TimeIf`
    /// shape without making host tests depend on a platform.
    pub fn run_until_idle_with_clock<N, D>(&self, now_ns: N, program_deadline: D) -> RunIdleReport
    where
        N: FnMut() -> u64,
        D: FnMut(Option<u64>),
    {
        let mut clock = CallbackClock {
            now_ns,
            program_deadline,
        };
        self.run_until_idle_with_clock_source(&mut clock)
    }

    /// Submit a kernel-only cooperative task and return its legacy slot id.
    ///
    /// New code that needs stale-handle protection should use
    /// [`Reactor::submit_task`] and keep the returned [`TaskKey`].
    pub fn submit<F>(&self, future: F) -> TaskId
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.submit_task(future).id()
    }

    /// Submit a kernel-only cooperative task and return a generation-checked key.
    pub fn submit_task<F>(&self, future: F) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.submit_task_with_meta(future, InitialSchedMeta::kernel())
    }

    /// Submit a task with explicit scheduler metadata.
    pub fn submit_task_with_meta<F>(&self, future: F, initial_meta: InitialSchedMeta) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = self.shared.tasks.lock().submit(future);
        let request =
            self.scheduler_submit_request(key.id(), TaskHandle::new(key.id()), initial_meta);
        self.apply_local_enqueue(request);
        key
    }

    /// Submit a task from a running hart and dispatch a reschedule
    /// marker/IPI if initial placement chooses another hart.
    pub fn submit_task_with_meta_from_hart<F, S>(
        &self,
        future: F,
        initial_meta: InitialSchedMeta,
        current_hart: HartId,
        signal: &mut S,
    ) -> (TaskKey, WakeDispatchReport)
    where
        F: Future<Output = ()> + Send + 'static,
        S: RescheduleSignal,
    {
        let key = self.submit_task_with_meta(future, initial_meta);
        let mut report = WakeDispatchReport::empty();
        if let Some(TaskRunOwner::Queued { hart, .. }) = self.shared.scheduler.task_owner(key.id())
        {
            report.record(self.apply_runnable_placement(
                RunnablePlacement {
                    target_hart: hart,
                    wake_remote: hart != current_hart,
                },
                signal,
            ));
        }
        (key, report)
    }

    pub fn cancel_task(&self, task: TaskKey) -> Result<(), TaskLifecycleError> {
        self.shared.tasks.lock().cancel_task(task)?;
        self.shared.scheduler.task_dropped(task.id());
        Ok(())
    }

    pub fn set_task_affinity<S>(
        &self,
        task: TaskKey,
        affinity: u64,
        current_hart: HartId,
        signal: &mut S,
    ) -> Result<WakeDispatchReport, SchedulerAffinityError>
    where
        S: RescheduleSignal,
    {
        if self.shared.tasks.lock().status(task).is_none() {
            return Err(SchedulerAffinityError::UnknownTask);
        }

        let mut report = WakeDispatchReport::empty();
        if let Some((placement, movement)) =
            self.shared
                .scheduler
                .set_affinity_for_locals(task.id(), affinity, current_hart)?
        {
            self.apply_local_affinity_move(movement);
            report.record(self.apply_runnable_placement(placement, signal));
        }
        Ok(report)
    }

    pub fn task_affinity(&self, task: TaskKey) -> Result<u64, SchedulerAffinityError> {
        if self.shared.tasks.lock().status(task).is_none() {
            return Err(SchedulerAffinityError::UnknownTask);
        }
        self.shared
            .scheduler
            .task_affinity(task.id())
            .ok_or(SchedulerAffinityError::UnknownTask)
    }

    pub fn queue_ast_marker(
        &self,
        task: TaskKey,
        marker: AstMarker,
    ) -> Result<AstQueueEffect, TaskLifecycleError> {
        self.shared.tasks.lock().queue_ast_marker(task, marker)
    }

    pub fn consume_ast_markers(&self, task: TaskKey) -> Result<AstBatch, TaskLifecycleError> {
        self.shared.tasks.lock().consume_ast_markers(task)
    }

    pub fn last_consumed_ast_batch(&self, task: TaskKey) -> Result<AstBatch, TaskLifecycleError> {
        self.shared.tasks.lock().last_consumed_ast_batch(task)
    }

    /// Request userspace execution for a future userspace-thread task.
    ///
    /// This is the public reactor facade for the current single-slot
    /// userspace-run shell. It is still mechanism only: there is no
    /// `ThreadPayload`, VM fault policy, signal routing, or HAL return path
    /// hidden behind this method.
    pub fn request_userspace_run(&self) -> Result<UserspaceRunWait, UserspaceRunError> {
        self.shared.userspace.lock().start_request()
    }

    pub fn userspace_run_status(&self) -> Option<UserspaceRunStatus> {
        self.shared.userspace.lock().status()
    }

    pub fn dispatch_userspace_run(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.shared.userspace.lock().dispatch(request)
    }

    pub fn record_userspace_timer_preemption(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.shared
            .userspace
            .lock()
            .record_timer_preemption(request)
    }

    pub fn complete_userspace_run(
        &self,
        request: UserspaceRunRequest,
        trap: UserspaceTrapInfo,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.shared
            .userspace
            .lock()
            .complete_interesting_trap(request, trap)
    }

    pub fn checkpoint_task_userspace_entry(
        &self,
        task: TaskKey,
        request: UserspaceRunRequest,
        decide: impl FnOnce(&UserspaceEntryCheckpoint) -> UserspaceEntryDecision,
    ) -> Result<UserspaceEntryOutcome, UserspaceEntryTaskError> {
        self.shared.userspace.lock().status_for_request(request)?;
        let ast = self.shared.tasks.lock().consume_ast_markers(task)?;
        Ok(self
            .shared
            .userspace
            .lock()
            .checkpoint_userspace_entry_batch(request, ast, decide)?)
    }

    pub fn drain_completed(&self) -> Vec<TaskDrainRecord> {
        self.drain_terminal(TaskStatus::Completed)
    }

    pub fn drain_cancelled(&self) -> Vec<TaskDrainRecord> {
        self.drain_terminal(TaskStatus::Cancelled)
    }

    pub fn run_until_idle(&self) -> RunStats {
        self.run_until_idle_on_hart(HartId(0))
    }

    pub fn run_until_idle_on_hart(&self, hart: HartId) -> RunStats {
        let mut signal = NoopRescheduleSignal::new();
        self.run_until_idle_on_hart_with_reschedule(hart, &mut signal)
    }

    pub fn run_until_idle_on_hart_with_reschedule<S>(
        &self,
        hart: HartId,
        signal: &mut S,
    ) -> RunStats
    where
        S: RescheduleSignal,
    {
        self.run_until_idle_on_hart_with_reschedule_and_slice_clock(
            hart,
            signal,
            &mut NoopSliceClock,
        )
    }

    pub fn run_until_idle_on_hart_with_reschedule_and_slice_clock<S, C>(
        &self,
        hart: HartId,
        signal: &mut S,
        slice_clock: &mut C,
    ) -> RunStats
    where
        S: RescheduleSignal,
        C: SliceClock,
    {
        self.hart_runtime_view(hart)
            .run_until_idle_on_hart_with_reschedule_and_slice_clock(hart, signal, slice_clock)
    }

    pub fn run_rescheduled_on_hart_with_reschedule<S>(
        &self,
        hart: HartId,
        signal: &mut S,
    ) -> RunStats
    where
        S: RescheduleSignal,
    {
        if !self.consume_dispatch_markers(hart).need_resched() {
            return RunStats::empty();
        }

        self.run_until_idle_on_hart_with_reschedule(hart, signal)
    }

    fn run_until_idle_with_clock_source<C>(&self, clock: &mut C) -> RunIdleReport
    where
        C: ClockSource,
    {
        let timer_wakes = self.advance_time_to(clock.now_ns());
        let stats = self.run_until_idle();
        let next_deadline_ns = self.next_deadline_ns();
        match next_deadline_ns {
            Some(deadline_ns) => clock.set_deadline_ns(deadline_ns),
            None => clock.cancel_deadline(),
        }

        RunIdleReport {
            stats,
            timer_wakes,
            idle: self.is_idle(),
            next_deadline_ns,
        }
    }

    pub fn is_idle(&self) -> bool {
        self.shared.tasks.lock().is_idle()
    }

    pub fn task_status(&self, task: TaskId) -> Option<TaskStatus> {
        self.shared.tasks.lock().status_by_id(task)
    }

    pub fn task_key_status(&self, task: TaskKey) -> Option<TaskStatus> {
        self.shared.tasks.lock().status(task)
    }

    pub fn last_stop_reason(&self, task: TaskId) -> Option<StopReason> {
        self.shared.tasks.lock().last_stop_reason_by_id(task)
    }

    /// Returns the task's `TaskMailbox` for yield resolution (drive-taskmb).
    pub fn task_mailbox(&self, task: TaskKey) -> Result<Arc<TaskMailbox>, TaskLifecycleError> {
        self.shared.tasks.lock().mailbox(task)
    }

    pub fn next_scheduled_task(&self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        let mut signal = NoopRescheduleSignal::new();
        self.drain_wakes_for_hart(hart, &mut signal);
        self.locals.get(hart).and_then(|local| {
            self.shared
                .scheduler
                .peek_next_from_local(hart, local.scheduler())
        })
    }

    pub fn drain_wakes_for_hart<S>(
        &self,
        current_hart: HartId,
        signal: &mut S,
    ) -> WakeDispatchReport
    where
        S: RescheduleSignal,
    {
        self.hart_runtime_view(current_hart)
            .drain_wakes_for_hart(current_hart, signal)
    }

    pub fn dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.locals
            .get(hart)
            .map(|local| Phase1Scheduler::snapshot_markers_from_local(local.scheduler()))
            .unwrap_or_else(PreemptMarkers::empty)
    }

    pub fn consume_dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.locals
            .get(hart)
            .map(|local| Phase1Scheduler::consume_markers_from_local(local.scheduler()))
            .unwrap_or_else(PreemptMarkers::empty)
    }

    pub fn scheduler_stats(&self) -> SchedulerStats {
        self.shared.scheduler.stats()
    }

    pub fn observability(&self) -> ReactorObservability {
        self.shared.observability.lock().clone()
    }

    fn scheduler_submit_request(
        &self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
    ) -> LocalEnqueueRequest {
        let affinity = initial_meta.affinity;
        let max_hart = max_hart_in_affinity(affinity);
        self.ensure_local_harts_through(max_hart);
        self.shared
            .scheduler
            .task_submitted_for_locals(task, handle, initial_meta, |hart| {
                self.locals.total_queue_depth(hart)
            })
    }

    fn ensure_local_harts_through(&self, max_hart: HartId) {
        self.locals.ensure_hart(max_hart);
    }

    fn apply_local_enqueue(&self, request: LocalEnqueueRequest) {
        self.locals.ensure_hart(request.hart);
        if let Some(local) = self.locals.get(request.hart) {
            Phase1Scheduler::push_to_local_queue(
                local.scheduler(),
                request.task,
                request.queue,
                request.front,
            );
        }
    }

    fn apply_local_affinity_move(&self, movement: LocalAffinityMove) {
        self.locals.ensure_hart(movement.from_hart);
        self.locals.ensure_hart(movement.to_hart);
        if let Some(from_local) = self.locals.get(movement.from_hart) {
            Phase1Scheduler::remove_from_local_queue(
                movement.task,
                from_local.scheduler(),
                movement.queue,
            );
        }
        if let Some(to_local) = self.locals.get(movement.to_hart) {
            Phase1Scheduler::push_to_local_queue(
                to_local.scheduler(),
                movement.task,
                movement.queue,
                false,
            );
        }
    }

    /// Mark that userspace execution on `hart` should return to the reactor
    /// before the next userspace entry.
    ///
    /// This is separate from normal reschedule markers, which are also used
    /// as wake hints for idle harts. Trap/timer code should use this marker
    /// for Phase 3 userspace preemption arbitration.
    pub fn mark_userspace_preempt(&self, hart: HartId) {
        self.locals.ensure_hart(hart);
        if let Some(local) = self.locals.get(hart) {
            Phase1Scheduler::mark_userspace_preempt_local(local.scheduler());
        }
    }

    fn apply_runnable_placement<S>(
        &self,
        placement: RunnablePlacement,
        signal: &mut S,
    ) -> WakeDispatchAction
    where
        S: RescheduleSignal,
    {
        self.locals.ensure_hart(placement.target_hart);
        if let Some(local) = self.locals.get(placement.target_hart) {
            Phase1Scheduler::mark_need_resched_local(local.scheduler());
        }
        if placement.wake_remote {
            signal.send_reschedule_ipi(placement.target_hart);
        }

        WakeDispatchAction {
            target_hart: placement.target_hart,
            wake_remote: placement.wake_remote,
        }
    }

    fn drain_terminal(&self, status: TaskStatus) -> Vec<TaskDrainRecord> {
        let drained = match status {
            TaskStatus::Completed => self.shared.tasks.lock().drain_completed(),
            TaskStatus::Cancelled => self.shared.tasks.lock().drain_cancelled(),
            TaskStatus::Runnable | TaskStatus::Polling | TaskStatus::Parked => Vec::new(),
        };
        for record in &drained {
            self.shared.scheduler.task_dropped(record.handle.id());
        }
        drained
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

fn max_hart_in_affinity(affinity: u64) -> HartId {
    let mask = if affinity == 0 { 1 } else { affinity };
    HartId((u64::BITS - 1 - mask.leading_zeros()) as usize)
}
