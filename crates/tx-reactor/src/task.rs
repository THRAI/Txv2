//! Reactor task identity and task-table entries.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{future::Future, pin::Pin, task::Waker};

use tx_substrate::wake::mailbox::TaskMailbox;

use crate::{
    ast::{AstBatch, AstMarker, AstQueueEffect, AstSlot},
    scheduler::StopReason,
    waker::{task_waker, TaskWakeState},
};

pub(crate) type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TaskId(pub usize);

impl TaskId {
    pub const fn index(self) -> usize {
        self.0
    }
}

/// Generation counter for a reactor task-table slot.
///
/// The generation is temporal task-table evidence only. It does not make a
/// reactor task a semantic entity and does not carry zone retention authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskGeneration(u64);

impl TaskGeneration {
    pub const INITIAL: Self = Self(0);

    pub const fn value(self) -> u64 {
        self.0
    }

    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Generation-checked identity for a live task-table entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TaskKey {
    id: TaskId,
    generation: TaskGeneration,
}

impl TaskKey {
    pub(crate) const fn new(id: TaskId, generation: TaskGeneration) -> Self {
        Self { id, generation }
    }

    pub const fn id(self) -> TaskId {
        self.id
    }

    pub const fn generation(self) -> TaskGeneration {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskLifecycleError {
    StaleHandle,
    AlreadyTerminal(TaskStatus),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskDrainRecord {
    pub handle: TaskKey,
    pub status: TaskStatus,
    pub last_stop_reason: Option<StopReason>,
}

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
    Cancelled,
}

pub(crate) struct Task {
    pub(crate) id: TaskId,
    pub(crate) generation: TaskGeneration,
    pub(crate) future: Option<TaskFuture>,
    pub(crate) status: TaskStatus,
    pub(crate) wake_state: Arc<TaskWakeState>,
    pub(crate) ast: AstSlot,
    /// Per-task wake delivery queue for yield resolution.
    /// Owned by the reactor task; borrowed by `drive()` via `ScriptCtx`.
    pub(crate) mailbox: Arc<TaskMailbox>,
    last_ast_batch: AstBatch,
    pub(crate) last_stop_reason: Option<StopReason>,
}

impl Task {
    fn new_for_handle<F>(handle: TaskKey, future: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self {
            id: handle.id,
            generation: handle.generation,
            future: Some(Box::pin(future)),
            status: TaskStatus::Runnable,
            wake_state: Arc::new(TaskWakeState::new()),
            ast: AstSlot::new(),
            mailbox: Arc::new(TaskMailbox::new()),
            last_ast_batch: AstBatch::default(),
            last_stop_reason: None,
        }
    }

    pub(crate) fn handle(&self) -> TaskKey {
        TaskKey::new(self.id, self.generation)
    }

    pub(crate) fn consume_ast_markers(&mut self) -> AstBatch {
        let batch = self.ast.consume();
        self.last_ast_batch = batch.clone();
        batch
    }
}

pub struct TaskTable {
    slots: Vec<TaskSlot>,
    free: Vec<TaskId>,
}

struct TaskSlot {
    generation: TaskGeneration,
    task: Option<Task>,
}

impl TaskTable {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    pub fn submit<F>(&mut self, future: F) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (id, generation) = self
            .take_reusable_slot()
            .unwrap_or_else(|| self.push_fresh_slot());
        let handle = TaskKey::new(id, generation);
        self.slots[id.index()].task = Some(Task::new_for_handle(handle, future));
        handle
    }

    pub fn status(&self, handle: TaskKey) -> Option<TaskStatus> {
        self.live_task(handle).ok().map(|task| task.status)
    }

    pub(crate) fn status_by_id(&self, id: TaskId) -> Option<TaskStatus> {
        self.task_by_id(id).map(|task| task.status)
    }

    pub(crate) fn last_stop_reason_by_id(&self, id: TaskId) -> Option<StopReason> {
        self.task_by_id(id).and_then(|task| task.last_stop_reason)
    }

    pub(crate) fn key_for_id(&self, id: TaskId) -> Option<TaskKey> {
        self.task_by_id(id).map(Task::handle)
    }

    pub(crate) fn task(&self, handle: TaskKey) -> Result<&Task, TaskLifecycleError> {
        self.live_task(handle)
    }

    pub(crate) fn task_mut(&mut self, handle: TaskKey) -> Result<&mut Task, TaskLifecycleError> {
        self.live_task_mut(handle)
    }

    pub fn waker(&self, handle: TaskKey) -> Result<Waker, TaskLifecycleError> {
        let task = self.live_nonterminal_task(handle)?;
        Ok(task_waker(Arc::clone(&task.wake_state)))
    }

    /// Returns a clone of the task's `TaskMailbox` for yield resolution (drive-taskmb).
    pub fn mailbox(&self, handle: TaskKey) -> Result<Arc<TaskMailbox>, TaskLifecycleError> {
        let task = self.live_nonterminal_task(handle)?;
        Ok(Arc::clone(&task.mailbox))
    }

    pub fn queue_ast_marker(
        &mut self,
        handle: TaskKey,
        marker: AstMarker,
    ) -> Result<AstQueueEffect, TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        Ok(task.ast.queue(marker))
    }

    pub fn consume_ast_markers(&mut self, handle: TaskKey) -> Result<AstBatch, TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        Ok(task.consume_ast_markers())
    }

    pub fn last_consumed_ast_batch(&self, handle: TaskKey) -> Result<AstBatch, TaskLifecycleError> {
        let task = self.live_task(handle)?;
        Ok(task.last_ast_batch.clone())
    }

    pub fn park_task(&mut self, handle: TaskKey) -> Result<(), TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        task.status = TaskStatus::Parked;
        Ok(())
    }

    pub fn mark_runnable(&mut self, handle: TaskKey) -> Result<(), TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        task.status = TaskStatus::Runnable;
        Ok(())
    }

    pub fn complete_task(&mut self, handle: TaskKey) -> Result<(), TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        task.future = None;
        task.ast.clear();
        task.status = TaskStatus::Completed;
        task.wake_state.clear();
        task.last_stop_reason = Some(StopReason::Completed);
        Ok(())
    }

    pub fn cancel_task(&mut self, handle: TaskKey) -> Result<(), TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        task.future = None;
        task.ast.clear();
        task.status = TaskStatus::Cancelled;
        task.wake_state.clear();
        task.last_stop_reason = None;
        Ok(())
    }

    pub fn drain_completed(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_status(TaskStatus::Completed)
    }

    pub fn drain_cancelled(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_status(TaskStatus::Cancelled)
    }

    pub fn drain_wakes(&mut self) -> Vec<TaskKey> {
        let mut woken = Vec::new();
        for slot in &mut self.slots {
            let Some(task) = slot.task.as_mut() else {
                continue;
            };
            if task.wake_state.take_wake() && task.status == TaskStatus::Parked {
                task.status = TaskStatus::Runnable;
                woken.push(task.handle());
            }
        }
        woken
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.slots.iter().all(|slot| {
            let Some(task) = slot.task.as_ref() else {
                return true;
            };
            task.status != TaskStatus::Runnable && !task.wake_state.is_wake_requested()
        })
    }

    fn task_by_id(&self, id: TaskId) -> Option<&Task> {
        self.slots.get(id.index())?.task.as_ref()
    }

    fn take_reusable_slot(&mut self) -> Option<(TaskId, TaskGeneration)> {
        while let Some(id) = self.free.pop() {
            let Some(slot) = self.slots.get_mut(id.index()) else {
                continue;
            };
            if slot.task.is_some() {
                continue;
            }
            let Some(generation) = slot.generation.next() else {
                continue;
            };
            slot.generation = generation;
            return Some((id, generation));
        }
        None
    }

    fn push_fresh_slot(&mut self) -> (TaskId, TaskGeneration) {
        let id = TaskId(self.slots.len());
        let generation = TaskGeneration::INITIAL;
        self.slots.push(TaskSlot {
            generation,
            task: None,
        });
        (id, generation)
    }

    fn live_task(&self, handle: TaskKey) -> Result<&Task, TaskLifecycleError> {
        let slot = self
            .slots
            .get(handle.id.index())
            .ok_or(TaskLifecycleError::StaleHandle)?;
        if slot.generation != handle.generation {
            return Err(TaskLifecycleError::StaleHandle);
        }
        slot.task.as_ref().ok_or(TaskLifecycleError::StaleHandle)
    }

    fn live_task_mut(&mut self, handle: TaskKey) -> Result<&mut Task, TaskLifecycleError> {
        let slot = self
            .slots
            .get_mut(handle.id.index())
            .ok_or(TaskLifecycleError::StaleHandle)?;
        if slot.generation != handle.generation {
            return Err(TaskLifecycleError::StaleHandle);
        }
        slot.task.as_mut().ok_or(TaskLifecycleError::StaleHandle)
    }

    fn live_nonterminal_task(&self, handle: TaskKey) -> Result<&Task, TaskLifecycleError> {
        let task = self.live_task(handle)?;
        if is_terminal(task.status) {
            return Err(TaskLifecycleError::AlreadyTerminal(task.status));
        }
        Ok(task)
    }

    fn live_nonterminal_task_mut(
        &mut self,
        handle: TaskKey,
    ) -> Result<&mut Task, TaskLifecycleError> {
        let task = self.live_task_mut(handle)?;
        if is_terminal(task.status) {
            return Err(TaskLifecycleError::AlreadyTerminal(task.status));
        }
        Ok(task)
    }

    fn drain_status(&mut self, status: TaskStatus) -> Vec<TaskDrainRecord> {
        let mut drained = Vec::new();
        let mut freed = Vec::new();

        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.task.as_ref().map(|task| task.status) != Some(status) {
                continue;
            }

            let mut task = slot.task.take().expect("checked Some above");
            task.future = None;
            task.wake_state.clear();
            drained.push(TaskDrainRecord {
                handle: task.handle(),
                status,
                last_stop_reason: task.last_stop_reason,
            });
            freed.push(TaskId(index));
        }

        self.free.extend(freed);
        drained
    }
}

impl Default for TaskTable {
    fn default() -> Self {
        Self::new()
    }
}

const fn is_terminal(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Cancelled)
}
