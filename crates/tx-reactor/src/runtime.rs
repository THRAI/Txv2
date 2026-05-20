//! Cooperative host reactor runtime.

use alloc::{sync::Arc, vec::Vec};
use core::{
    future::Future,
    task::{Context, Poll},
};

use crate::adapter::bus_wire::{
    DeclaredPort, DeclaredQueue, WireDeclaration, WireDeclarationError, WireEventSet,
};
use tx_substrate::wake::mailbox::TaskMailbox;

use crate::{
    ast::{AstBatch, AstMarker, AstQueueEffect},
    dispatch::{NoopRescheduleSignal, RescheduleSignal, WakeDispatchAction, WakeDispatchReport},
    hart_loop::HartLoopStep,
    preempt::PreemptMarkers,
    scheduler::{
        HartId, InitialSchedMeta, Phase1Scheduler, RunnablePlacement, SchedulerAffinityError,
        SchedulerStats, SliceConfig, StopReason, TaskHandle, TaskRunOwner, WakeHint,
    },
    spin_lock::SpinLock,
    task::{TaskDrainRecord, TaskId, TaskKey, TaskLifecycleError, TaskStatus, TaskTable},
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
    tasks: TaskTable,
    scheduler: Phase1Scheduler,
    observability: ReactorObservability,
    timers: TimerQueue,
    timer_wheel: tx_substrate::wake::timer::TimerWheel,
    delegate_registry: Arc<tx_substrate::step::DelegateRegistry>,
    userspace: UserspaceRunSlot,
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
    reactor: SpinLock<Option<Reactor>>,
}

impl SharedReactor {
    pub const fn empty() -> Self {
        Self {
            reactor: SpinLock::new(None),
        }
    }

    pub fn init(&self) -> bool {
        let mut reactor = self.reactor.lock();
        if reactor.is_some() {
            return false;
        }

        *reactor = Some(Reactor::new());
        true
    }

    pub fn is_initialized(&self) -> bool {
        self.reactor.lock().is_some()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Reactor) -> R) -> Option<R> {
        let mut reactor = self.reactor.lock();
        reactor.as_mut().map(f)
    }

    /// Phase 1a poll lease: lock → pick+take future → unlock → poll → lock → commit.
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

        let mut stats = RunStats::empty();
        let timer_wakes: usize;
        let wake_report: WakeDispatchReport;

        // Phase 1: short lock — advance time & drain wakes
        {
            let mut guard = self.reactor.lock();
            let reactor = guard.as_mut()?;
            timer_wakes = reactor.advance_time_to(now_ns);
            wake_report = reactor.drain_wakes_for_hart(hart, signal);
            reactor.scheduler.rebalance_at(hart, now_ns);
        }

        // Phase 2: poll loop — lock/unlock per task
        loop {
            let poll_packet = {
                let mut guard = self.reactor.lock();
                let reactor = guard.as_mut()?;
                reactor.drain_wakes_for_hart(hart, signal);
                let Some((handle, slice)) = reactor.scheduler.pick_next_or_steal(hart) else {
                    break;
                };
                let Some(key) = reactor.tasks.key_for_id(handle.id()) else {
                    reactor.scheduler.task_dropped(handle.id());
                    continue;
                };
                if reactor.tasks.status(key) != Some(TaskStatus::Runnable) {
                    match reactor.tasks.status(key) {
                        Some(TaskStatus::Completed | TaskStatus::Cancelled) => {
                            reactor.scheduler.task_dropped(handle.id());
                        }
                        _ => {
                            reactor.scheduler.task_stopped(
                                handle.id(),
                                StopReason::Blocked,
                                0,
                                hart,
                            );
                        }
                    }
                    continue;
                }
                match reactor.tasks.take_future(key) {
                    Ok((future, wake_state, mailbox)) => {
                        crate::task::set_current_mailbox(hart.0, Some(mailbox));
                        crate::task::set_current_timer_wheel(
                            hart.0,
                            Some(reactor.timer_wheel.clone()),
                        );
                        crate::task::set_current_delegate_registry(
                            hart.0,
                            Some(Arc::clone(&reactor.delegate_registry)),
                        );
                        Some((key, future, wake_state, slice))
                    }
                    Err(_) => continue,
                }
            }; // LOCK RELEASED

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

            // Phase 3: short lock — commit state
            {
                let mut guard = self.reactor.lock();
                let reactor = guard.as_mut()?;
                let _ = reactor.tasks.put_future(key, future);
                match result {
                    Poll::Ready(()) => {
                        if reactor.tasks.complete_task(key).is_ok() {
                            reactor.scheduler.task_stopped(
                                key.id(),
                                StopReason::Completed,
                                accounting.consumed_ns,
                                hart,
                            );
                            reactor.scheduler.task_dropped(key.id());
                            stats.completed += 1;
                        }
                    }
                    Poll::Pending => {
                        let userspace_preempted = reactor.take_userspace_preempt_marker(hart);
                        if userspace_preempted {
                            if let Ok(task) = reactor.tasks.task_mut(key) {
                                let _ = task.wake_state.take_wake();
                                task.status = TaskStatus::Runnable;
                                task.last_stop_reason = Some(StopReason::PreemptedExternal);
                            }
                            reactor.scheduler.task_stopped(
                                key.id(),
                                StopReason::PreemptedExternal,
                                accounting.consumed_ns,
                                hart,
                            );
                            let _ = reactor.dispatch_queued_task_from_hart(key.id(), hart, signal);
                        } else if accounting.slice_expired {
                            if let Ok(task) = reactor.tasks.task_mut(key) {
                                let _ = task.wake_state.take_wake();
                                task.status = TaskStatus::Runnable;
                                task.last_stop_reason = Some(StopReason::SliceExpired);
                            }
                            reactor.scheduler.task_stopped(
                                key.id(),
                                StopReason::SliceExpired,
                                accounting.consumed_ns,
                                hart,
                            );
                            let _ = reactor.dispatch_queued_task_from_hart(key.id(), hart, signal);
                        } else if reactor
                            .tasks
                            .task(key)
                            .map(|t| t.wake_state.take_wake())
                            .unwrap_or(false)
                        {
                            reactor.mark_runnable_from_hart(key, WakeHint::Normal, hart, signal);
                        } else if let Ok(task) = reactor.tasks.task_mut(key) {
                            task.status = TaskStatus::Parked;
                            task.last_stop_reason = Some(StopReason::Blocked);
                            reactor.scheduler.task_stopped(
                                key.id(),
                                StopReason::Blocked,
                                accounting.consumed_ns,
                                hart,
                            );
                        }
                    }
                }
            }
        }

        let (consumed_markers, next_deadline_ns) = {
            let mut guard = self.reactor.lock();
            let reactor = guard.as_mut()?;
            reactor.record_run_stats(hart, stats);
            (
                reactor.consume_dispatch_markers(hart),
                reactor.next_deadline_ns(),
            )
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

impl Reactor {
    const WAKE_INBOX_DRAIN_LIMIT: usize = 64;

    pub fn new() -> Self {
        Self {
            tasks: TaskTable::new(),
            scheduler: Phase1Scheduler::new(),
            observability: ReactorObservability::default(),
            timers: TimerQueue::new(),
            timer_wheel: tx_substrate::wake::timer::TimerWheel::new(),
            delegate_registry: Arc::new(tx_substrate::step::DelegateRegistry::new()),
            userspace: UserspaceRunSlot::new(),
        }
    }

    /// Creates a wait channel attached to this reactor's timer queue.
    pub fn channel(&self) -> wait::Channel {
        wait::Channel::with_timer_queue(self.timers.clone())
    }

    /// Creates a typed declared wait channel attached to this reactor's timer queue.
    pub fn declared_channel<E>(
        &self,
        declaration: WireDeclaration<E>,
    ) -> Result<wait::DeclaredChannel<E>, WireDeclarationError>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredChannel::with_timer_queue(declaration, self.timers.clone())
    }

    /// Attaches an existing typed declared bus port to this reactor's timer queue.
    pub fn declared_channel_from_port<E>(&self, port: DeclaredPort<E>) -> wait::DeclaredChannel<E>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredChannel::from_port_with_timer_queue(port, self.timers.clone())
    }

    /// Creates a typed declared readiness channel attached to this reactor's timer queue.
    pub fn declared_readiness_channel<E>(
        &self,
        declaration: WireDeclaration<E>,
    ) -> Result<wait::DeclaredReadinessChannel<E>, WireDeclarationError>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredReadinessChannel::with_timer_queue(declaration, self.timers.clone())
    }

    /// Attaches an existing typed declared bus queue to this reactor's timer queue.
    pub fn declared_readiness_channel_from_queue<E>(
        &self,
        queue: DeclaredQueue<E>,
    ) -> wait::DeclaredReadinessChannel<E>
    where
        E: WireEventSet + Send + Sync + 'static,
    {
        wait::DeclaredReadinessChannel::from_queue_with_timer_queue(queue, self.timers.clone())
    }

    /// Advances the reactor-owned absolute nanosecond clock and wakes expired timers.
    pub fn advance_time_to(&mut self, now_ns: u64) -> usize {
        self.timer_wheel.fire_due(now_ns);
        self.timers.advance_time_to(now_ns)
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.timers.next_deadline_ns()
    }

    /// Create a future that resolves once the reactor's clock advances past `deadline_ns`.
    pub fn sleep_until(&self, deadline_ns: u64) -> DeadlineFuture {
        self.timers.wait_until(deadline_ns)
    }

    /// Clone the reactor's timer queue so callers can schedule deadline
    /// futures without holding the reactor lock.
    pub fn timer_queue(&self) -> TimerQueue {
        self.timers.clone()
    }

    /// Drive expired timers from a monotonic clock, run ready work, then
    /// program the next absolute timer deadline.
    ///
    /// `program_deadline` receives `Some(deadline_ns)` to arm the current
    /// clock source, or `None` to cancel it. This mirrors the HAL `TimeIf`
    /// shape without making host tests depend on a platform.
    pub fn run_until_idle_with_clock<N, D>(
        &mut self,
        now_ns: N,
        program_deadline: D,
    ) -> RunIdleReport
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
    pub fn submit<F>(&mut self, future: F) -> TaskId
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.submit_task(future).id()
    }

    /// Submit a kernel-only cooperative task and return a generation-checked key.
    pub fn submit_task<F>(&mut self, future: F) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.submit_task_with_meta(future, InitialSchedMeta::kernel())
    }

    /// Submit a task with explicit scheduler metadata.
    pub fn submit_task_with_meta<F>(&mut self, future: F, initial_meta: InitialSchedMeta) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = self.tasks.submit(future);
        self.scheduler
            .task_submitted(key.id(), TaskHandle::new(key.id()), initial_meta);
        key
    }

    /// Submit a task from a running hart and dispatch a reschedule
    /// marker/IPI if initial placement chooses another hart.
    pub fn submit_task_with_meta_from_hart<F, S>(
        &mut self,
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
        if let Some(TaskRunOwner::Queued { hart, .. }) = self.scheduler.task_owner(key.id()) {
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

    pub fn cancel_task(&mut self, task: TaskKey) -> Result<(), TaskLifecycleError> {
        self.tasks.cancel_task(task)?;
        self.scheduler.task_dropped(task.id());
        Ok(())
    }

    pub fn set_task_affinity<S>(
        &mut self,
        task: TaskKey,
        affinity: u64,
        current_hart: HartId,
        signal: &mut S,
    ) -> Result<WakeDispatchReport, SchedulerAffinityError>
    where
        S: RescheduleSignal,
    {
        if self.tasks.status(task).is_none() {
            return Err(SchedulerAffinityError::UnknownTask);
        }

        let mut report = WakeDispatchReport::empty();
        if let Some(placement) = self
            .scheduler
            .set_affinity(task.id(), affinity, current_hart)?
        {
            report.record(self.apply_runnable_placement(placement, signal));
        }
        Ok(report)
    }

    pub fn task_affinity(&self, task: TaskKey) -> Result<u64, SchedulerAffinityError> {
        if self.tasks.status(task).is_none() {
            return Err(SchedulerAffinityError::UnknownTask);
        }
        self.scheduler
            .task_affinity(task.id())
            .ok_or(SchedulerAffinityError::UnknownTask)
    }

    pub fn queue_ast_marker(
        &mut self,
        task: TaskKey,
        marker: AstMarker,
    ) -> Result<AstQueueEffect, TaskLifecycleError> {
        self.tasks.queue_ast_marker(task, marker)
    }

    pub fn consume_ast_markers(&mut self, task: TaskKey) -> Result<AstBatch, TaskLifecycleError> {
        self.tasks.consume_ast_markers(task)
    }

    pub fn last_consumed_ast_batch(&self, task: TaskKey) -> Result<AstBatch, TaskLifecycleError> {
        self.tasks.last_consumed_ast_batch(task)
    }

    /// Request userspace execution for a future userspace-thread task.
    ///
    /// This is the public reactor facade for the current single-slot
    /// userspace-run shell. It is still mechanism only: there is no
    /// `ThreadPayload`, VM fault policy, signal routing, or HAL return path
    /// hidden behind this method.
    pub fn request_userspace_run(&self) -> Result<UserspaceRunWait, UserspaceRunError> {
        self.userspace.start_request()
    }

    pub fn userspace_run_status(&self) -> Option<UserspaceRunStatus> {
        self.userspace.status()
    }

    pub fn dispatch_userspace_run(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.userspace.dispatch(request)
    }

    pub fn record_userspace_timer_preemption(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.userspace.record_timer_preemption(request)
    }

    pub fn complete_userspace_run(
        &self,
        request: UserspaceRunRequest,
        trap: UserspaceTrapInfo,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.userspace.complete_interesting_trap(request, trap)
    }

    pub fn checkpoint_task_userspace_entry(
        &mut self,
        task: TaskKey,
        request: UserspaceRunRequest,
        decide: impl FnOnce(&UserspaceEntryCheckpoint) -> UserspaceEntryDecision,
    ) -> Result<UserspaceEntryOutcome, UserspaceEntryTaskError> {
        self.userspace.status_for_request(request)?;
        let ast = self.tasks.consume_ast_markers(task)?;
        Ok(self
            .userspace
            .checkpoint_userspace_entry_batch(request, ast, decide)?)
    }

    pub fn drain_completed(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_terminal(TaskStatus::Completed)
    }

    pub fn drain_cancelled(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_terminal(TaskStatus::Cancelled)
    }

    pub fn run_until_idle(&mut self) -> RunStats {
        self.run_until_idle_on_hart(HartId(0))
    }

    pub fn run_until_idle_on_hart(&mut self, hart: HartId) -> RunStats {
        let mut signal = NoopRescheduleSignal::new();
        self.run_until_idle_on_hart_with_reschedule(hart, &mut signal)
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
            // External wakers only set a task-local bit. The reactor owns the
            // transition from Parked to Runnable and does it at poll-loop
            // boundaries, where duplicate wakes naturally coalesce.
            self.drain_wakes_for_hart(hart, signal);
            let Some((handle, slice)) = self.scheduler.pick_next_or_steal(hart) else {
                break;
            };
            let Some(key) = self.tasks.key_for_id(handle.id()) else {
                self.scheduler.task_dropped(handle.id());
                continue;
            };

            if self.tasks.status(key) != Some(TaskStatus::Runnable)
                || self
                    .tasks
                    .task(key)
                    .ok()
                    .and_then(|task| task.future.as_ref())
                    .is_none()
            {
                match self.tasks.status(key) {
                    Some(TaskStatus::Completed | TaskStatus::Cancelled) => {
                        self.scheduler.task_dropped(handle.id());
                    }
                    _ => {
                        self.scheduler
                            .task_stopped(handle.id(), StopReason::Blocked, 0, hart);
                    }
                }
                continue;
            }

            let wake_state = match self.tasks.task(key) {
                Ok(task) => Arc::clone(&task.wake_state),
                Err(_) => continue,
            };
            let waker = task_waker(wake_state);
            let mut cx = Context::from_waker(&waker);

            let poll = {
                let Ok(task) = self.tasks.task_mut(key) else {
                    continue;
                };
                debug_assert_eq!(task.id, key.id());
                task.status = TaskStatus::Polling;
                task.wake_state.clear();
                task.consume_ast_markers();

                // drive-taskmb: expose the task's mailbox so the trampoline
                // can inject it into SyscallCtx (and from there into ScriptCtx
                // for drive() yield resolution).
                crate::task::set_current_mailbox(hart.0, Some(Arc::clone(&task.mailbox)));
                crate::task::set_current_timer_wheel(hart.0, Some(self.timer_wheel.clone()));
                crate::task::set_current_delegate_registry(
                    hart.0,
                    Some(Arc::clone(&self.delegate_registry)),
                );

                let Some(future) = task.future.as_mut() else {
                    crate::task::set_current_mailbox(hart.0, None);
                    crate::task::set_current_timer_wheel(hart.0, None);
                    crate::task::set_current_delegate_registry(hart.0, None);
                    continue;
                };

                stats.polled += 1;
                let timing = PollTiming::start(slice, slice_clock);
                let result = future.as_mut().poll(&mut cx);
                let accounting = timing.finish(slice_clock);
                crate::task::set_current_mailbox(hart.0, None);
                crate::task::set_current_timer_wheel(hart.0, None);
                crate::task::set_current_delegate_registry(hart.0, None);
                (result, accounting)
            };

            match poll {
                (Poll::Ready(()), accounting) => {
                    if self.tasks.complete_task(key).is_ok() {
                        self.scheduler.task_stopped(
                            key.id(),
                            StopReason::Completed,
                            accounting.consumed_ns,
                            hart,
                        );
                        self.scheduler.task_dropped(key.id());
                        stats.completed += 1;
                    }
                }
                (Poll::Pending, accounting) => {
                    let userspace_preempted = self.take_userspace_preempt_marker(hart);
                    if userspace_preempted {
                        if let Ok(task) = self.tasks.task_mut(key) {
                            let _ = task.wake_state.take_wake();
                            task.status = TaskStatus::Runnable;
                            task.last_stop_reason = Some(StopReason::PreemptedExternal);
                        }
                        self.scheduler.task_stopped(
                            key.id(),
                            StopReason::PreemptedExternal,
                            accounting.consumed_ns,
                            hart,
                        );
                        let _ = self.dispatch_queued_task_from_hart(key.id(), hart, signal);
                    } else if accounting.slice_expired {
                        if let Ok(task) = self.tasks.task_mut(key) {
                            let _ = task.wake_state.take_wake();
                            task.status = TaskStatus::Runnable;
                            task.last_stop_reason = Some(StopReason::SliceExpired);
                        }
                        self.scheduler.task_stopped(
                            key.id(),
                            StopReason::SliceExpired,
                            accounting.consumed_ns,
                            hart,
                        );
                        let _ = self.dispatch_queued_task_from_hart(key.id(), hart, signal);
                    } else if self
                        .tasks
                        .task(key)
                        .map(|task| task.wake_state.take_wake())
                        .unwrap_or(false)
                    {
                        self.mark_runnable_from_hart(key, WakeHint::Normal, hart, signal);
                    } else if let Ok(task) = self.tasks.task_mut(key) {
                        task.status = TaskStatus::Parked;
                        task.last_stop_reason = Some(StopReason::Blocked);
                        self.scheduler.task_stopped(
                            key.id(),
                            StopReason::Blocked,
                            accounting.consumed_ns,
                            hart,
                        );
                    }
                }
            }
        }

        self.record_run_stats(hart, stats);
        stats
    }

    pub fn run_rescheduled_on_hart_with_reschedule<S>(
        &mut self,
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

    fn run_until_idle_with_clock_source<C>(&mut self, clock: &mut C) -> RunIdleReport
    where
        C: ClockSource,
    {
        let timer_wakes = self.timers.advance_time_to(clock.now_ns());
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
        self.tasks.is_idle()
    }

    pub fn task_status(&self, task: TaskId) -> Option<TaskStatus> {
        self.tasks.status_by_id(task)
    }

    pub fn task_key_status(&self, task: TaskKey) -> Option<TaskStatus> {
        self.tasks.status(task)
    }

    pub fn last_stop_reason(&self, task: TaskId) -> Option<StopReason> {
        self.tasks.last_stop_reason_by_id(task)
    }

    /// Returns the task's `TaskMailbox` for yield resolution (drive-taskmb).
    pub fn task_mailbox(&self, task: TaskKey) -> Result<Arc<TaskMailbox>, TaskLifecycleError> {
        self.tasks.mailbox(task)
    }

    pub fn next_scheduled_task(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        let mut signal = NoopRescheduleSignal::new();
        self.drain_wakes_for_hart(hart, &mut signal);
        self.scheduler.peek_next(hart)
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
        for id in self.tasks.drain_wake_ids() {
            let Some(target_hart) = self.scheduler.target_hart_for_wake(id) else {
                continue;
            };
            self.scheduler.push_wake_inbox(target_hart, id);
            if target_hart != current_hart {
                signal.send_reschedule_ipi(target_hart);
                report.record(WakeDispatchAction {
                    target_hart,
                    wake_remote: true,
                });
            }
        }

        for id in self
            .scheduler
            .drain_wake_inbox(current_hart, Self::WAKE_INBOX_DRAIN_LIMIT)
        {
            let Some(key) = self.tasks.take_wake_if_parked_by_id(id) else {
                continue;
            };
            if let Some(placement) =
                self.scheduler
                    .task_runnable_from(key.id(), WakeHint::Normal, current_hart)
            {
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

    pub fn dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.scheduler.snapshot_markers(hart)
    }

    pub fn consume_dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.scheduler.consume_markers(hart)
    }

    pub(crate) fn take_userspace_preempt_marker(&self, hart: HartId) -> bool {
        self.scheduler.take_userspace_preempt(hart)
    }

    pub fn scheduler_stats(&self) -> SchedulerStats {
        self.scheduler.stats()
    }

    pub fn observability(&self) -> ReactorObservability {
        self.observability.clone()
    }

    fn record_run_stats(&mut self, hart: HartId, stats: RunStats) {
        self.observability.record_run_stats(hart, stats);
    }

    /// Mark that userspace execution on `hart` should return to the reactor
    /// before the next userspace entry.
    ///
    /// This is separate from normal reschedule markers, which are also used
    /// as wake hints for idle harts. Trap/timer code should use this marker
    /// for Phase 3 userspace preemption arbitration.
    pub fn mark_userspace_preempt(&mut self, hart: HartId) {
        self.scheduler.mark_userspace_preempt(hart);
    }

    pub(crate) fn mark_runnable_from_hart<S>(
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
        if self.tasks.mark_runnable(key).is_ok() {
            if let Some(placement) = self
                .scheduler
                .task_runnable_from(key.id(), hint, current_hart)
            {
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
        self.scheduler.mark_need_resched(placement.target_hart);
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
        let Some(TaskRunOwner::Queued { hart, .. }) = self.scheduler.task_owner(task) else {
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

    fn drain_terminal(&mut self, status: TaskStatus) -> Vec<TaskDrainRecord> {
        let drained = match status {
            TaskStatus::Completed => self.tasks.drain_completed(),
            TaskStatus::Cancelled => self.tasks.drain_cancelled(),
            TaskStatus::Runnable | TaskStatus::Polling | TaskStatus::Parked => Vec::new(),
        };
        for record in &drained {
            self.scheduler.task_dropped(record.handle.id());
        }
        drained
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}
