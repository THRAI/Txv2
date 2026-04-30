//! Cooperative host reactor runtime.

use alloc::{sync::Arc, vec::Vec};
use core::{
    future::Future,
    task::{Context, Poll},
};

use crate::{
    ast::{AstBatch, AstMarker, AstQueueEffect},
    scheduler::{
        HartId, InitialSchedMeta, Phase1Scheduler, SliceConfig, StopReason, TaskHandle, WakeHint,
    },
    task::{TaskDrainRecord, TaskId, TaskKey, TaskLifecycleError, TaskStatus, TaskTable},
    timer::TimerQueue,
    wait,
    waker::task_waker,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunStats {
    pub polled: usize,
    pub completed: usize,
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
    timers: TimerQueue,
}

impl Reactor {
    pub fn new() -> Self {
        Self {
            tasks: TaskTable::new(),
            scheduler: Phase1Scheduler::new(),
            timers: TimerQueue::new(),
        }
    }

    /// Creates a wait channel attached to this reactor's timer queue.
    pub fn channel(&self) -> wait::Channel {
        wait::Channel::with_timer_queue(self.timers.clone())
    }

    /// Advances the reactor-owned absolute nanosecond clock and wakes expired timers.
    pub fn advance_time_to(&mut self, now_ns: u64) -> usize {
        self.timers.advance_time_to(now_ns)
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.timers.next_deadline_ns()
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
        F: Future<Output = ()> + 'static,
    {
        self.submit_task(future).id()
    }

    /// Submit a kernel-only cooperative task and return a generation-checked key.
    pub fn submit_task<F>(&mut self, future: F) -> TaskKey
    where
        F: Future<Output = ()> + 'static,
    {
        let key = self.tasks.submit(future);
        self.scheduler.task_submitted(
            key.id(),
            TaskHandle::new(key.id()),
            InitialSchedMeta::kernel(),
        );
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

    pub fn drain_completed(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_terminal(TaskStatus::Completed)
    }

    pub fn drain_cancelled(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_terminal(TaskStatus::Cancelled)
    }

    pub fn run_until_idle(&mut self) -> RunStats {
        let mut stats = RunStats {
            polled: 0,
            completed: 0,
        };
        loop {
            // External wakers only set a task-local bit. The reactor owns the
            // transition from Parked to Runnable and does it at poll-loop
            // boundaries, where duplicate wakes naturally coalesce.
            self.drain_wakes();
            let Some((handle, _slice)) = self.scheduler.pick_next(HartId(0)) else {
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

            let poll = {
                let Ok(task) = self.tasks.task_mut(key) else {
                    continue;
                };
                debug_assert_eq!(task.id, key.id());
                task.status = TaskStatus::Polling;
                task.wake_state.clear();
                task.consume_ast_markers();

                let Some(future) = task.future.as_mut() else {
                    continue;
                };

                stats.polled += 1;
                future.as_mut().poll(&mut cx)
            };

            match poll {
                Poll::Ready(()) => {
                    if self.tasks.complete_task(key).is_ok() {
                        self.scheduler
                            .task_stopped(key.id(), StopReason::Completed, 0, HartId(0));
                        self.scheduler.task_dropped(key.id());
                        stats.completed += 1;
                    }
                }
                Poll::Pending => {
                    let woke_during_poll = self
                        .tasks
                        .task(key)
                        .map(|task| task.wake_state.take_wake())
                        .unwrap_or(false);
                    if woke_during_poll {
                        self.mark_runnable(key, WakeHint::Normal);
                    } else if let Ok(task) = self.tasks.task_mut(key) {
                        task.status = TaskStatus::Parked;
                        task.last_stop_reason = Some(StopReason::Blocked);
                        self.scheduler
                            .task_stopped(key.id(), StopReason::Blocked, 0, HartId(0));
                    }
                }
            }
        }

        stats
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

    pub fn next_scheduled_task(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.drain_wakes();
        self.scheduler.peek_next(hart)
    }

    fn drain_wakes(&mut self) -> usize {
        let woken = self.tasks.drain_wakes();
        let notified = woken.len();
        for key in woken {
            self.scheduler.task_runnable(key.id(), WakeHint::Normal);
        }
        notified
    }

    fn mark_runnable(&mut self, key: TaskKey, hint: WakeHint) {
        if self.tasks.mark_runnable(key).is_ok() {
            self.scheduler.task_runnable(key.id(), hint);
        }
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
