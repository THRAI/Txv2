//! Reactor task identity and task-table entries.

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{future::Future, pin::Pin, task::Waker};

use tx_substrate::wake::mailbox::{MailboxSchedulerHint, TaskMailbox};

use crate::{
    ast::{AstBatch, AstMarker, AstQueueEffect, AstSlot},
    scheduler::StopReason,
    spin_lock::SpinLock,
    waker::{task_waker, TaskWakeState},
};

// Per REACTOR_v0 §Submission: submitted futures must be `Send + 'static`.
// Substrate's `Guard<'_>` is intentionally `!Send`/`!Sync` (EBR-7); StepOps
// that hold `&Guard` are confined to synchronous `drive_oneshot` paths and
// must not be carried across `.await`. The async `drive(...).await` path
// instead uses StepOps that acquire their own guard inside `step()` per
// STEP_MODEL_v2 §1, so the boxed future remains `Send`.
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

    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TakeRunnableError {
    Missing,
    NotRunnable(TaskStatus),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingPollCommit {
    Woken {
        hint: MailboxSchedulerHint,
        mailbox_event: bool,
    },
    Parked,
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
    fn new_for_handle<F>(
        handle: TaskKey,
        future: F,
        wake_queue: Arc<SpinLock<VecDeque<TaskId>>>,
    ) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let future: TaskFuture = Box::pin(future);
        emit_task_submit_debug(b"debug.task.submit.future_box.after", handle.id);
        let wake_state = Arc::new(TaskWakeState::new(handle.id, wake_queue));
        emit_task_submit_debug(b"debug.task.submit.wake_state.after", handle.id);
        let mailbox = Arc::new(
            TaskMailbox::new()
                .with_task_id(handle.id.0 as u32)
                .with_scheduler_owner(handle.id.0, handle.generation.value()),
        );
        emit_task_submit_debug(b"debug.task.submit.mailbox.after", handle.id);
        Self {
            id: handle.id,
            generation: handle.generation,
            future: Some(future),
            status: TaskStatus::Runnable,
            wake_state,
            ast: AstSlot::new(),
            mailbox,
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
    completed: Vec<TaskId>,
    cancelled: Vec<TaskId>,
    wake_queue: Arc<SpinLock<VecDeque<TaskId>>>,
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
            completed: Vec::new(),
            cancelled: Vec::new(),
            wake_queue: Arc::new(SpinLock::new(VecDeque::new())),
        }
    }

    pub fn submit<F>(&mut self, future: F) -> TaskKey
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (id, generation) = self
            .take_reusable_slot()
            .unwrap_or_else(|| self.push_fresh_slot());
        emit_task_submit_debug(b"debug.task.submit.slot.after", id);
        emit_task_submit_value(
            b"debug.task.submit.future_size",
            core::mem::size_of::<F>() as i64,
        );
        let handle = TaskKey::new(id, generation);
        let task = Task::new_for_handle(handle, future, Arc::clone(&self.wake_queue));
        emit_task_submit_debug(b"debug.task.submit.construct.after", id);
        self.slots[id.index()].task = Some(task);
        emit_task_submit_debug(b"debug.task.submit.store.after", id);
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

    /// Resolve a queued task id, verify it is runnable, and take its future
    /// under one task-table lock acquisition.
    pub(crate) fn take_runnable_future_by_id(
        &mut self,
        id: TaskId,
    ) -> Result<(TaskKey, TaskFuture, Arc<TaskWakeState>, Arc<TaskMailbox>), TakeRunnableError>
    {
        let task = self
            .slots
            .get_mut(id.index())
            .and_then(|slot| slot.task.as_mut())
            .ok_or(TakeRunnableError::Missing)?;
        let handle = task.handle();
        if task.status != TaskStatus::Runnable {
            return Err(TakeRunnableError::NotRunnable(task.status));
        }
        let future = task.future.take().ok_or(TakeRunnableError::Missing)?;
        task.status = TaskStatus::Polling;
        task.wake_state.clear();
        task.consume_ast_markers();
        let wake_state = Arc::clone(&task.wake_state);
        let mailbox = Arc::clone(&task.mailbox);
        Ok((handle, future, wake_state, mailbox))
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
        {
            let task = self.live_nonterminal_task_mut(handle)?;
            task.future = None;
            task.ast.clear();
            task.status = TaskStatus::Completed;
            task.wake_state.clear();
            task.last_stop_reason = Some(StopReason::Completed);
        }
        self.completed.push(handle.id);
        Ok(())
    }

    pub(crate) fn finish_polled_complete(
        &mut self,
        handle: TaskKey,
        _future: TaskFuture,
    ) -> Result<(), TaskLifecycleError> {
        self.complete_task(handle)
    }

    pub(crate) fn finish_polled_runnable(
        &mut self,
        handle: TaskKey,
        future: TaskFuture,
        reason: StopReason,
    ) -> Result<(), TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        task.future = Some(future);
        let _ = task.wake_state.take_wake();
        task.status = TaskStatus::Runnable;
        task.last_stop_reason = Some(reason);
        Ok(())
    }

    pub(crate) fn finish_polled_pending(
        &mut self,
        handle: TaskKey,
        future: TaskFuture,
    ) -> Result<PendingPollCommit, TaskLifecycleError> {
        let task = self.live_nonterminal_task_mut(handle)?;
        task.future = Some(future);
        if task.wake_state.take_wake() {
            let mailbox_event = !task.mailbox.is_empty() || task.mailbox.overflow();
            Ok(PendingPollCommit::Woken {
                hint: task.mailbox.take_scheduler_hint(),
                mailbox_event,
            })
        } else {
            task.status = TaskStatus::Parked;
            task.last_stop_reason = Some(StopReason::Blocked);
            Ok(PendingPollCommit::Parked)
        }
    }

    pub fn cancel_task(&mut self, handle: TaskKey) -> Result<(), TaskLifecycleError> {
        {
            let task = self.live_nonterminal_task_mut(handle)?;
            task.future = None;
            task.ast.clear();
            task.status = TaskStatus::Cancelled;
            task.wake_state.clear();
            task.last_stop_reason = None;
        }
        self.cancelled.push(handle.id);
        Ok(())
    }

    pub fn drain_completed(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_status(TaskStatus::Completed)
    }

    pub fn drain_cancelled(&mut self) -> Vec<TaskDrainRecord> {
        self.drain_status(TaskStatus::Cancelled)
    }

    pub fn drain_wakes(&mut self) -> Vec<TaskKey> {
        let ids = self.drain_wake_ids();
        let mut woken = Vec::new();
        for id in ids {
            if let Some(key) = self.take_wake_if_parked_by_id(id) {
                woken.push(key);
            }
        }
        woken
    }

    pub(crate) fn drain_wake_ids(&mut self) -> Vec<TaskId> {
        let mut woken = Vec::new();
        loop {
            let id = {
                let mut queue = self.wake_queue.lock();
                queue.pop_front()
            };
            let Some(id) = id else {
                break;
            };
            woken.push(id);
        }
        woken
    }

    pub(crate) fn take_wake_if_parked_by_id(&mut self, id: TaskId) -> Option<TaskKey> {
        self.take_wake_if_parked_by_id_with_hint(id)
            .map(|(key, _hint)| key)
    }

    pub(crate) fn take_wake_if_parked_by_id_with_hint(
        &mut self,
        id: TaskId,
    ) -> Option<(TaskKey, MailboxSchedulerHint)> {
        let task = self.slots.get_mut(id.index())?.task.as_mut()?;
        if !task.wake_state.take_wake() {
            return None;
        }
        if task.status == TaskStatus::Parked {
            task.status = TaskStatus::Runnable;
            let hint = task.mailbox.take_scheduler_hint();
            Some((task.handle(), hint))
        } else {
            None
        }
    }

    pub(crate) fn make_owner_runnable_with_hint(
        &mut self,
        id: TaskId,
        generation: TaskGeneration,
    ) -> Option<(TaskKey, MailboxSchedulerHint)> {
        let task = self.slots.get_mut(id.index())?.task.as_mut()?;
        if task.generation != generation {
            return None;
        }
        match task.status {
            TaskStatus::Parked => task.status = TaskStatus::Runnable,
            TaskStatus::Runnable => {}
            TaskStatus::Polling | TaskStatus::Completed | TaskStatus::Cancelled => return None,
        }
        let hint = task.mailbox.take_scheduler_hint();
        Some((task.handle(), hint))
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
        let terminal_ids = match status {
            TaskStatus::Completed => core::mem::take(&mut self.completed),
            TaskStatus::Cancelled => core::mem::take(&mut self.cancelled),
            TaskStatus::Runnable | TaskStatus::Polling | TaskStatus::Parked => Vec::new(),
        };

        for id in terminal_ids {
            let Some(slot) = self.slots.get_mut(id.index()) else {
                continue;
            };
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
            freed.push(id);
        }

        self.free.extend(freed);
        drained
    }
}

fn emit_task_submit_debug(name: &[u8], task: TaskId) {
    emit_task_submit_value(name, task.0 as i64);
}

fn emit_task_submit_value(name: &[u8], value: i64) {
    if !cfg!(tx_thread_lifecycle_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
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

// ---------------------------------------------------------------------------
// Per-hart current-task mailbox slot (drive-taskmb trampoline injection)
// ---------------------------------------------------------------------------

/// Max harts for per-hart runtime context slots in the Phase 1 SMP shell.
const MAX_HARTS: usize = 8;

/// Per-hart slots for the currently-polling task's mailbox.
/// Indexed by `HartId.0`.  Each hart writes only its own slot before
/// `future.poll()` and clears it after — no cross‑hart contention.
static CURRENT_MAILBOX: [SpinLock<Option<Arc<TaskMailbox>>>; MAX_HARTS] =
    [const { SpinLock::new(None) }; MAX_HARTS];

/// Set the current task's mailbox for `hart` (called by reactor before poll).
pub(crate) fn set_current_mailbox(hart: usize, mailbox: Option<Arc<TaskMailbox>>) {
    if let Some(slot) = CURRENT_MAILBOX.get(hart) {
        *slot.lock() = mailbox;
    }
}

/// Read the current task's mailbox for `hart` (called by trampoline / `run_thread`).
pub fn current_task_mailbox(hart: usize) -> Option<Arc<TaskMailbox>> {
    CURRENT_MAILBOX
        .get(hart)
        .and_then(|slot| slot.lock().clone())
}

/// Return the task mailbox for the currently-polled task when the runtime can
/// identify it unambiguously.
///
/// Generic wait futures do not carry a hart id. During normal single-hart or
/// sequential host polling there is exactly one current mailbox slot set, and
/// this helper lets those futures bind to the task-owned mailbox. If more than
/// one hart is polling at the same time, the helper returns `None` rather than
/// guessing; callers then use their standalone fallback.
pub(crate) fn current_poll_task_mailbox() -> Option<Arc<TaskMailbox>> {
    let mut found = None;
    for slot in &CURRENT_MAILBOX {
        let mailbox = slot.lock().clone();
        if mailbox.is_some() {
            if found.is_some() {
                return None;
            }
            found = mailbox;
        }
    }
    found
}

// -----------------------------------------------------------------------
// drive-taskmb: deadline registrar trampoline (same pattern as CURRENT_MAILBOX)
// -----------------------------------------------------------------------

use tx_services::time::DeadlineRegistrarHandle;

/// Per-hart slots for the current reactor's deadline registrar.
static CURRENT_DEADLINE_REGISTRAR: [SpinLock<Option<DeadlineRegistrarHandle>>; MAX_HARTS] =
    [const { SpinLock::new(None) }; MAX_HARTS];

/// Set the current reactor's deadline registrar for `hart` (called before poll).
pub(crate) fn set_current_deadline_registrar(
    hart: usize,
    registrar: Option<DeadlineRegistrarHandle>,
) {
    if let Some(slot) = CURRENT_DEADLINE_REGISTRAR.get(hart) {
        *slot.lock() = registrar;
    }
}

/// Read the current reactor's deadline registrar facade for `hart`.
pub fn current_deadline_registrar(hart: usize) -> Option<DeadlineRegistrarHandle> {
    CURRENT_DEADLINE_REGISTRAR
        .get(hart)
        .and_then(|slot| slot.lock().clone())
}

// -----------------------------------------------------------------------
// drive-taskmb: delegate registry trampoline
// -----------------------------------------------------------------------

use tx_substrate::step::DelegateRegistry;

/// Per-hart slots for the current reactor's delegate registry.
static CURRENT_DELEGATE_REGISTRY: [SpinLock<Option<Arc<DelegateRegistry>>>; MAX_HARTS] =
    [const { SpinLock::new(None) }; MAX_HARTS];

/// Set the current reactor's delegate registry for `hart` (called by reactor before poll).
pub(crate) fn set_current_delegate_registry(hart: usize, registry: Option<Arc<DelegateRegistry>>) {
    if let Some(slot) = CURRENT_DELEGATE_REGISTRY.get(hart) {
        *slot.lock() = registry;
    }
}

/// Read the current reactor's delegate registry for `hart` (called by trampoline / `run_thread`).
pub fn current_delegate_registry(hart: usize) -> Option<Arc<DelegateRegistry>> {
    CURRENT_DELEGATE_REGISTRY
        .get(hart)
        .and_then(|slot| slot.lock().clone())
}
