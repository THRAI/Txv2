#![no_std]

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, RawWaker, RawWakerVTable, Waker},
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
}

pub mod wait {
    pub struct WaitToken;
}

pub use task::TaskId;

type TaskFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunStats {
    pub polled: usize,
    pub completed: usize,
}

pub struct Reactor {
    tasks: Vec<Task>,
    next_task_id: usize,
}

struct Task {
    future: Option<TaskFuture>,
    runnable: bool,
}

impl Reactor {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_task_id: 0,
        }
    }

    pub fn submit<F>(&mut self, future: F) -> TaskId
    where
        F: Future<Output = ()> + 'static,
    {
        let id = TaskId(self.next_task_id);
        self.next_task_id += 1;
        self.tasks.push(Task {
            future: Some(Box::pin(future)),
            runnable: true,
        });
        id
    }

    pub fn run_until_idle(&mut self) -> RunStats {
        let mut stats = RunStats {
            polled: 0,
            completed: 0,
        };
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        while let Some(index) = self.next_runnable_task_index() {
            let task = &mut self.tasks[index];
            task.runnable = false;

            let Some(future) = task.future.as_mut() else {
                continue;
            };

            stats.polled += 1;
            if future.as_mut().poll(&mut cx).is_ready() {
                task.future = None;
                stats.completed += 1;
            }
        }

        stats
    }

    pub fn is_idle(&self) -> bool {
        self.next_runnable_task_index().is_none()
    }

    fn next_runnable_task_index(&self) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.runnable && task.future.is_some())
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        noop_waker_clone,
        noop_waker_wake,
        noop_waker_wake_by_ref,
        noop_waker_drop,
    );

    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

unsafe fn noop_waker_clone(_data: *const ()) -> RawWaker {
    noop_raw_waker()
}

unsafe fn noop_waker_wake(_data: *const ()) {}

unsafe fn noop_waker_wake_by_ref(_data: *const ()) {}

unsafe fn noop_waker_drop(_data: *const ()) {}

fn noop_raw_waker() -> RawWaker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        noop_waker_clone,
        noop_waker_wake,
        noop_waker_wake_by_ref,
        noop_waker_drop,
    );

    RawWaker::new(core::ptr::null(), &VTABLE)
}
