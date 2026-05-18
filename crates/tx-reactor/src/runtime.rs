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
    dispatch::{DispatchState, NoopRescheduleSignal, RescheduleSignal, WakeDispatchReport},
    preempt::PreemptMarkers,
    scheduler::{
        HartId, InitialSchedMeta, Phase1Scheduler, SliceConfig, StopReason, TaskHandle, WakeHint,
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

pub struct Reactor {
    tasks: TaskTable,
    scheduler: Phase1Scheduler,
    dispatch: DispatchState,
    timers: TimerQueue,
    timer_wheel: tx_substrate::wake::timer::TimerWheel,
    delegate_registry: Arc<tx_substrate::step::DelegateRegistry>,
    userspace: UserspaceRunSlot,
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
}

impl Reactor {
    pub fn new() -> Self {
        Self {
            tasks: TaskTable::new(),
            scheduler: Phase1Scheduler::new(),
            dispatch: DispatchState::new(),
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
    ///
    /// `initial_meta.task_id_low` is threaded into the task's
    /// `TaskMailbox` via `TaskTable::submit_with_task_id`; the scheduler
    /// also sees the full meta for fairness/affinity bookkeeping.
    pub fn submit_task_with_meta<F>(&mut self, future: F, initial_meta: InitialSchedMeta) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = self.tasks.submit_with_task_id(future, initial_meta.task_id_low);
        self.scheduler
            .task_submitted(key.id(), TaskHandle::new(key.id()), initial_meta);
        key
    }

    pub fn cancel_task(&mut self, task: TaskKey) -> Result<(), TaskLifecycleError> {
        self.tasks.cancel_task(task)?;
        self.scheduler.task_dropped(task.id());
        Ok(())
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
        let mut stats = RunStats {
            polled: 0,
            completed: 0,
        };
        loop {
            // External wakers only set a task-local bit. The reactor owns the
            // transition from Parked to Runnable and does it at poll-loop
            // boundaries, where duplicate wakes naturally coalesce.
            self.drain_wakes_for_hart(hart, signal);
            let Some((handle, _slice)) = self.scheduler.pick_next(hart) else {
                break;
            };
            let Some(key) = self.tasks.key_for_id(handle.id()) else {
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
                continue;
            }

            let wake_state = match self.tasks.task(key) {
                Ok(task) => Arc::clone(&task.wake_state),
                Err(_) => continue,
            };
            let waker = task_waker(wake_state);
            let mut cx = Context::from_waker(&waker);

            // OBS-9 reactor scheduler track: capture the task's
            // `task_id_low` (mailbox-installed TID) before the poll so we
            // can pair the SpanBegin/SpanEnd records by the same identity
            // even if the task structure has rotated by the time we
            // close the span.
            let task_id_low = self
                .tasks
                .task(key)
                .ok()
                .map(|t| t.mailbox.task_id_low())
                .unwrap_or(0);
            let sched_span = emit_sched_begin(hart, task_id_low);

            let poll = {
                let Ok(task) = self.tasks.task_mut(key) else {
                    emit_sched_end(sched_span, hart, task_id_low, tx_observe_types::SchedReason::None);
                    continue;
                };
                debug_assert_eq!(task.id, key.id());
                task.status = TaskStatus::Polling;
                task.wake_state.clear();
                task.consume_ast_markers();

                // drive-taskmb: expose the task's mailbox so the trampoline
                // can inject it into SyscallCtx (and from there into ScriptCtx
                // for drive() yield resolution).
                crate::task::set_current_mailbox(Some(Arc::clone(&task.mailbox)));

                // drive-taskmb: expose the reactor's timer wheel for OnTimer
                // yield resolution.
                crate::task::set_current_timer_wheel(Some(self.timer_wheel.clone()));

                // drive-taskmb: expose the reactor's delegate registry
                // for OnAgent yield resolution.
                crate::task::set_current_delegate_registry(Some(Arc::clone(
                    &self.delegate_registry,
                )));

                let Some(future) = task.future.as_mut() else {
                    crate::task::set_current_mailbox(None);
                    crate::task::set_current_timer_wheel(None);
                    crate::task::set_current_delegate_registry(None);
                    emit_sched_end(sched_span, hart, task_id_low, tx_observe_types::SchedReason::None);
                    continue;
                };

                stats.polled += 1;
                let result = future.as_mut().poll(&mut cx);
                crate::task::set_current_mailbox(None);
                crate::task::set_current_timer_wheel(None);
                crate::task::set_current_delegate_registry(None);
                result
            };

            match poll {
                Poll::Ready(()) => {
                    if self.tasks.complete_task(key).is_ok() {
                        self.scheduler
                            .task_stopped(key.id(), StopReason::Completed, 0, hart);
                        self.scheduler.task_dropped(key.id());
                        stats.completed += 1;
                    }
                    emit_sched_end(sched_span, hart, task_id_low, tx_observe_types::SchedReason::Completed);
                }
                Poll::Pending => {
                    let woke_during_poll = self
                        .tasks
                        .task(key)
                        .map(|task| task.wake_state.take_wake())
                        .unwrap_or(false);
                    if woke_during_poll {
                        self.mark_runnable_from_hart(key, WakeHint::Normal, hart, signal);
                        emit_sched_end(
                            sched_span,
                            hart,
                            task_id_low,
                            tx_observe_types::SchedReason::WokeDuringPoll,
                        );
                    } else if let Ok(task) = self.tasks.task_mut(key) {
                        task.status = TaskStatus::Parked;
                        task.last_stop_reason = Some(StopReason::Blocked);
                        self.scheduler
                            .task_stopped(key.id(), StopReason::Blocked, 0, hart);
                        emit_sched_end(sched_span, hart, task_id_low, tx_observe_types::SchedReason::Parked);
                    } else {
                        emit_sched_end(sched_span, hart, task_id_low, tx_observe_types::SchedReason::None);
                    }
                }
            }
        }

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
        self.drain_wakes_for_hart(HartId(0), &mut signal);
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
        let woken = self.tasks.drain_wakes();
        let mut report = WakeDispatchReport::empty();
        for key in woken {
            if let Some(placement) =
                self.scheduler
                    .task_runnable_from(key.id(), WakeHint::Normal, current_hart)
            {
                report.record(self.dispatch.apply_runnable_placement(placement, signal));
            }
        }
        report
    }

    pub fn dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.dispatch.snapshot_markers(hart)
    }

    pub fn consume_dispatch_markers(&self, hart: HartId) -> PreemptMarkers {
        self.dispatch.consume_markers(hart)
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
        if self.tasks.mark_runnable(key).is_ok() {
            if let Some(placement) = self
                .scheduler
                .task_runnable_from(key.id(), hint, current_hart)
            {
                report.record(self.dispatch.apply_runnable_placement(placement, signal));
            }
        }
        report
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

// ---------------------------------------------------------------------------
// OBS-9 reactor scheduler track — `08_OBSERVATION_v1.md` §15.6.
//
// Each `Future::poll` call in `run_until_idle_on_hart_with_reschedule`
// is wrapped in `SpanBegin(Sched)` / `SpanEnd(Sched)` so the daemon can
// render a sched_switch-equivalent Gantt timeline of which reactor task
// held each hart over time. The payload carries `(task_id_low, hart_id,
// kind, reason)` per OBS-V1-§8.10. SpanBegin uses `kind=Dispatch,
// reason=None`; SpanEnd uses `kind=Yield` with one of `Parked` /
// `Completed` / `WokeDuringPoll` / `None`.
//
// No-op when no `HartEmitter` is installed on this hart (test
// scaffolds, boards without an observation ring).
// ---------------------------------------------------------------------------

#[inline]
fn emit_sched_begin(hart: HartId, task_id_low: u32) -> tx_observe::SpanId {
    use tx_observe::encode::{encode_sched_switch, sched_switch_tag};
    use tx_observe::{EventNameId, TxTraceLevel};
    use tx_observe_types::{PayloadSchedSwitch, SchedKind, SchedReason};
    let Some(em) = tx_observe::current() else {
        return tx_observe::SpanId::NONE;
    };
    let payload = PayloadSchedSwitch {
        task_id_low,
        hart_id: hart.0 as u8,
        kind: SchedKind::Dispatch as u8,
        reason: SchedReason::None as u8,
        _pad: [0; 9],
    };
    let (enc, len) = encode_sched_switch(&payload);
    em.span_begin(
        TxTraceLevel::Sched,
        // EventNameId carries the task_id_low so the daemon can render
        // per-task labels without a separate names.json lookup.
        EventNameId::from_raw(task_id_low),
        tx_observe::SpanId::NONE,
        sched_switch_tag(),
        &enc[..len as usize],
    )
}

#[inline]
fn emit_sched_end(
    span: tx_observe::SpanId,
    hart: HartId,
    task_id_low: u32,
    reason: tx_observe_types::SchedReason,
) {
    use tx_observe::encode::{encode_sched_switch, sched_switch_tag};
    use tx_observe_types::{PayloadSchedSwitch, SchedKind};
    if span == tx_observe::SpanId::NONE {
        return;
    }
    let Some(em) = tx_observe::current() else {
        return;
    };
    let payload = PayloadSchedSwitch {
        task_id_low,
        hart_id: hart.0 as u8,
        kind: SchedKind::Yield as u8,
        reason: reason as u8,
        _pad: [0; 9],
    };
    let (enc, len) = encode_sched_switch(&payload);
    em.span_end(span, sched_switch_tag(), &enc[..len as usize]);
}
