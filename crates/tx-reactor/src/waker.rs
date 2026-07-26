//! Task-local waker state and raw-waker glue.

use alloc::{collections::VecDeque, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{RawWaker, RawWakerVTable, Waker},
};

use crate::{spin_lock::SpinLock, task::TaskId};

/// Shared state behind every waker cloned from a task poll.
///
/// A single atomic bit is enough for the v0 smoke executor: multiple wake calls
/// before the next drain coalesce into one runnable transition, and the future
/// decides on poll whether the underlying wait condition actually became true.
pub(crate) struct TaskWakeState {
    task: TaskId,
    wake_queue: Arc<SpinLock<VecDeque<TaskId>>>,
    queued_wakes: Arc<AtomicUsize>,
    wake_requested: AtomicBool,
}

impl TaskWakeState {
    pub(crate) fn new(
        task: TaskId,
        wake_queue: Arc<SpinLock<VecDeque<TaskId>>>,
        queued_wakes: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            task,
            wake_queue,
            queued_wakes,
            wake_requested: AtomicBool::new(false),
        }
    }

    pub(crate) fn wake(&self) {
        // The wake bit owns the queue entry. Multiple wake calls before the
        // pending wake is consumed therefore collapse into one entry.
        if self.wake_requested.swap(true, Ordering::AcqRel) {
            return;
        }

        // Publish pending work before appending the id. An idle hart may retry
        // once in this short window, but cannot sleep through the publication.
        self.queued_wakes.fetch_add(1, Ordering::Release);
        self.wake_queue.lock().push_back(self.task);
    }

    pub(crate) fn take_wake(&self) -> bool {
        self.wake_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn clear(&self) {
        self.wake_requested.store(false, Ordering::Release);
    }

    pub(crate) fn is_wake_requested(&self) -> bool {
        self.wake_requested.load(Ordering::Acquire)
    }
}

pub(crate) fn task_waker(state: Arc<TaskWakeState>) -> Waker {
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
