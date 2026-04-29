#![no_std]
//! Minimal task-aware reactor machinery.
//!
//! This crate is still below the full REACTOR_v0 contract: there are no wait
//! channels, timers, AST slots, scheduler policy hooks, or long-running idle
//! loop yet. The implemented invariant is narrower and load-bearing for those
//! later pieces: each submitted task owns the wake state used by its `Waker`,
//! so a wake marks exactly that task runnable and does not authorize semantic
//! truth. The future must re-observe its condition on the next poll.

extern crate alloc;

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

pub mod ast {
    pub struct AstSlot;
}

pub mod preempt {
    pub struct PreemptionPoint;
}

pub mod task {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct TaskId(pub usize);

    /// Current reactor-visible state of a task future.
    ///
    /// These states describe polling mechanics only. They are not thread,
    /// process, or subsystem state, and do not carry object-model authority.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum TaskStatus {
        Runnable,
        Polling,
        Parked,
        Completed,
    }
}

pub mod wait {
    pub struct WaitToken;
}

pub use task::{TaskId, TaskStatus};

type TaskFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunStats {
    pub polled: usize,
    pub completed: usize,
}

pub struct Reactor {
    tasks: Vec<Task>,
    runnable: VecDeque<usize>,
    next_task_id: usize,
}

struct Task {
    id: TaskId,
    future: Option<TaskFuture>,
    status: TaskStatus,
    wake_state: Arc<TaskWakeState>,
}

/// Shared state behind every waker cloned from a task poll.
///
/// A single atomic bit is enough for the v0 smoke executor: multiple wake calls
/// before the next drain coalesce into one runnable transition, and the future
/// decides on poll whether the underlying wait condition actually became true.
struct TaskWakeState {
    wake_requested: AtomicBool,
}

impl TaskWakeState {
    fn new() -> Self {
        Self {
            wake_requested: AtomicBool::new(false),
        }
    }

    fn wake(&self) {
        self.wake_requested.store(true, Ordering::Release);
    }

    fn take_wake(&self) -> bool {
        self.wake_requested.swap(false, Ordering::AcqRel)
    }

    fn clear(&self) {
        self.wake_requested.store(false, Ordering::Release);
    }
}

impl Reactor {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            runnable: VecDeque::new(),
            next_task_id: 0,
        }
    }

    pub fn submit<F>(&mut self, future: F) -> TaskId
    where
        F: Future<Output = ()> + 'static,
    {
        let id = TaskId(self.next_task_id);
        self.next_task_id += 1;
        let index = self.tasks.len();
        self.tasks.push(Task {
            id,
            future: Some(Box::pin(future)),
            status: TaskStatus::Runnable,
            wake_state: Arc::new(TaskWakeState::new()),
        });
        self.runnable.push_back(index);
        id
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
            let Some(index) = self.runnable.pop_front() else {
                break;
            };

            if self.tasks[index].status != TaskStatus::Runnable
                || self.tasks[index].future.is_none()
            {
                continue;
            }

            let wake_state = Arc::clone(&self.tasks[index].wake_state);
            let waker = task_waker(wake_state);
            let mut cx = Context::from_waker(&waker);

            let poll = {
                let task = &mut self.tasks[index];
                debug_assert_eq!(task.id.0, index);
                task.status = TaskStatus::Polling;
                task.wake_state.clear();

                let Some(future) = task.future.as_mut() else {
                    continue;
                };

                stats.polled += 1;
                future.as_mut().poll(&mut cx)
            };

            match poll {
                Poll::Ready(()) => {
                    let task = &mut self.tasks[index];
                    task.future = None;
                    task.status = TaskStatus::Completed;
                    task.wake_state.clear();
                    stats.completed += 1;
                }
                Poll::Pending => {
                    if self.tasks[index].wake_state.take_wake() {
                        self.enqueue_runnable(index);
                    } else {
                        self.tasks[index].status = TaskStatus::Parked;
                    }
                }
            }
        }

        stats
    }

    pub fn is_idle(&self) -> bool {
        self.tasks.iter().all(|task| {
            task.status != TaskStatus::Runnable
                && !task.wake_state.wake_requested.load(Ordering::Acquire)
        })
    }

    pub fn task_status(&self, task: TaskId) -> Option<TaskStatus> {
        self.tasks.get(task.0).map(|entry| entry.status)
    }

    fn drain_wakes(&mut self) {
        for index in 0..self.tasks.len() {
            if self.tasks[index].wake_state.take_wake()
                && self.tasks[index].status == TaskStatus::Parked
            {
                self.enqueue_runnable(index);
            }
        }
    }

    fn enqueue_runnable(&mut self, index: usize) {
        let task = &mut self.tasks[index];
        if task.future.is_some() && task.status != TaskStatus::Runnable {
            task.status = TaskStatus::Runnable;
            self.runnable.push_back(index);
        }
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

fn task_waker(state: Arc<TaskWakeState>) -> Waker {
    unsafe { Waker::from_raw(raw_task_waker(state)) }
}

fn raw_task_waker(state: Arc<TaskWakeState>) -> RawWaker {
    RawWaker::new(Arc::into_raw(state) as *const (), &TASK_WAKER_VTABLE)
}

const TASK_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    task_waker_clone,
    task_waker_wake,
    task_waker_wake_by_ref,
    task_waker_drop,
);

unsafe fn task_waker_clone(data: *const ()) -> RawWaker {
    let state = unsafe { Arc::from_raw(data as *const TaskWakeState) };
    let cloned = Arc::clone(&state);
    let _ = Arc::into_raw(state);
    raw_task_waker(cloned)
}

unsafe fn task_waker_wake(data: *const ()) {
    let state = unsafe { Arc::from_raw(data as *const TaskWakeState) };
    state.wake();
}

unsafe fn task_waker_wake_by_ref(data: *const ()) {
    let state = unsafe { Arc::from_raw(data as *const TaskWakeState) };
    state.wake();
    let _ = Arc::into_raw(state);
}

unsafe fn task_waker_drop(data: *const ()) {
    drop(unsafe { Arc::from_raw(data as *const TaskWakeState) });
}
