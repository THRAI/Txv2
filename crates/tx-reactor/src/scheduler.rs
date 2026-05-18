//! Scheduler policy interface and the Phase 1 round-robin policy.

use alloc::{collections::VecDeque, vec::Vec};

use crate::task::TaskId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HartId(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskHandle {
    id: TaskId,
}

impl TaskHandle {
    pub const fn new(id: TaskId) -> Self {
        Self { id }
    }

    pub const fn id(self) -> TaskId {
        self.id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SliceConfig {
    Cooperative,
    Preemptive { slice_ns: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Blocked,
    Completed,
    Yielded,
    SliceExpired,
    UserspaceTrap,
    PreemptedExternal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeHint {
    Normal,
    SignalDelivery,
    PriorityBoost,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedClass {
    Fair,
    RtFifo,
    RtRoundRobin,
    Deadline,
    Idle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitialSchedMeta {
    pub class: SchedClass,
    pub nice: i8,
    pub rt_priority: u8,
    pub affinity: u64,
    pub kernel_only: bool,
    /// Low 32 bits of the task's trace identity (TID for user threads,
    /// 0 for kernel-internal tasks). Threaded through
    /// [`TaskTable::submit`] → [`Task::new_for_handle`] →
    /// [`TaskMailbox::with_task_id`] so `notify_emit` and
    /// `PayloadDriveBegin` carry per-thread identity in observation
    /// records (OBS-V1 §13.2 flow-id material).
    pub task_id_low: u32,
}

impl InitialSchedMeta {
    pub const fn fair() -> Self {
        Self {
            class: SchedClass::Fair,
            nice: 0,
            rt_priority: 0,
            affinity: u64::MAX,
            kernel_only: false,
            task_id_low: 0,
        }
    }

    pub const fn kernel() -> Self {
        Self {
            class: SchedClass::Fair,
            nice: 0,
            rt_priority: 0,
            affinity: u64::MAX,
            kernel_only: true,
            task_id_low: 0,
        }
    }

    pub const fn with_affinity(mut self, affinity: u64) -> Self {
        self.affinity = affinity;
        self
    }

    /// Install the task's trace identity (TID low 32 bits).
    ///
    /// Production call sites: `tx-kernel`'s thread-future submit path
    /// reads `child_thread.tid.0` from the `Cap<ThreadIdentity>` and
    /// chains this builder so the resulting `TaskMailbox` carries TID
    /// and `WaitSource::notify_emit` emits per-thread `task_id_low`.
    pub const fn with_task_id(mut self, task_id_low: u32) -> Self {
        self.task_id_low = task_id_low;
        self
    }
}

pub trait SchedulerPolicy {
    fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)>;
    fn task_stopped(&mut self, task: TaskId, reason: StopReason, consumed_ns: u64, hart: HartId);
    fn task_runnable(&mut self, task: TaskId, hint: WakeHint);
    fn task_submitted(&mut self, task: TaskId, handle: TaskHandle, initial_meta: InitialSchedMeta);
    fn task_dropped(&mut self, task: TaskId);

    fn defer_kernel_work(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub struct Phase1Scheduler {
    meta: Vec<Option<TaskSchedMeta>>,
    per_hart: Vec<HartSchedLocal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Phase1QueueDepths {
    pub kernel: usize,
    pub new: usize,
    pub preempted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnablePlacement {
    pub target_hart: HartId,
    pub wake_remote: bool,
}

#[derive(Clone, Debug)]
struct TaskSchedMeta {
    handle: TaskHandle,
    class: SchedClass,
    remaining_budget_ns: u64,
    current_slice_ns: u64,
    total_runtime_ns: u64,
    last_hart: Option<HartId>,
    affinity: u64,
    kernel_only: bool,
    queued: bool,
}

#[derive(Clone, Debug)]
struct HartSchedLocal {
    kernel_queue: VecDeque<TaskId>,
    new_queue: VecDeque<TaskId>,
    preempted_queue: VecDeque<TaskId>,
}

impl HartSchedLocal {
    fn new() -> Self {
        Self {
            kernel_queue: VecDeque::new(),
            new_queue: VecDeque::new(),
            preempted_queue: VecDeque::new(),
        }
    }
}

impl Phase1Scheduler {
    pub const BASE_SLICE_NS: u64 = 10_000_000;
    pub const NEW_QUEUE_SLICE_NS: u64 = 1_000_000;
    pub const PREEMPTED_QUEUE_SLICE_NS: u64 = 10_000_000;

    pub fn new() -> Self {
        Self {
            meta: Vec::new(),
            per_hart: alloc::vec![HartSchedLocal::new()],
        }
    }

    pub fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        <Self as SchedulerPolicy>::pick_next(self, hart)
    }

    pub fn task_stopped(
        &mut self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    ) {
        <Self as SchedulerPolicy>::task_stopped(self, task, reason, consumed_ns, hart);
    }

    pub fn task_runnable(&mut self, task: TaskId, hint: WakeHint) {
        <Self as SchedulerPolicy>::task_runnable(self, task, hint);
    }

    pub fn task_runnable_from(
        &mut self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<RunnablePlacement> {
        self.task_runnable_inner(task, hint, current_hart)
    }

    pub fn task_submitted(
        &mut self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
    ) {
        <Self as SchedulerPolicy>::task_submitted(self, task, handle, initial_meta);
    }

    pub fn task_dropped(&mut self, task: TaskId) {
        <Self as SchedulerPolicy>::task_dropped(self, task);
    }

    pub fn remaining_budget_ns(&self, task: TaskId) -> Option<u64> {
        self.meta_for(task).map(|meta| meta.remaining_budget_ns)
    }

    pub fn total_runtime_ns(&self, task: TaskId) -> Option<u64> {
        self.meta_for(task).map(|meta| meta.total_runtime_ns)
    }

    pub fn is_queued(&self, task: TaskId) -> bool {
        self.meta_for(task).map(|meta| meta.queued).unwrap_or(false)
    }

    pub fn queue_depths(&self, hart: HartId) -> Phase1QueueDepths {
        self.per_hart
            .get(hart.0)
            .map(|local| Phase1QueueDepths {
                kernel: local.kernel_queue.len(),
                new: local.new_queue.len(),
                preempted: local.preempted_queue.len(),
            })
            .unwrap_or(Phase1QueueDepths {
                kernel: 0,
                new: 0,
                preempted: 0,
            })
    }

    pub fn peek_next(&self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        let local = self.per_hart.get(hart.0)?;
        if let Some(result) = self.peek_queue(&local.kernel_queue, SliceConfig::Cooperative) {
            return Some(result);
        }
        if let Some(result) = self.peek_queue(
            &local.new_queue,
            SliceConfig::Preemptive {
                slice_ns: Self::NEW_QUEUE_SLICE_NS,
            },
        ) {
            return Some(result);
        }
        self.peek_preempted_queue(&local.preempted_queue)
    }

    fn peek_queue(
        &self,
        queue: &VecDeque<TaskId>,
        slice: SliceConfig,
    ) -> Option<(TaskHandle, SliceConfig)> {
        queue
            .iter()
            .find_map(|task| self.meta_for(*task).map(|meta| (meta.handle, slice)))
    }

    fn peek_preempted_queue(&self, queue: &VecDeque<TaskId>) -> Option<(TaskHandle, SliceConfig)> {
        queue.iter().find_map(|task| {
            self.meta_for(*task).map(|meta| {
                let slice_ns = if meta.remaining_budget_ns > 0 {
                    meta.remaining_budget_ns
                } else {
                    Self::PREEMPTED_QUEUE_SLICE_NS
                };
                (meta.handle, SliceConfig::Preemptive { slice_ns })
            })
        })
    }

    fn meta_for(&self, task: TaskId) -> Option<&TaskSchedMeta> {
        self.meta.get(task.0).and_then(Option::as_ref)
    }

    fn meta_for_mut(&mut self, task: TaskId) -> Option<&mut TaskSchedMeta> {
        self.meta.get_mut(task.0).and_then(Option::as_mut)
    }

    fn home_hart_for_meta(&self, meta: &TaskSchedMeta) -> HartId {
        let requested = meta
            .last_hart
            .unwrap_or_else(|| first_hart_in_mask(meta.affinity));
        if hart_allowed(meta.affinity, requested) {
            requested
        } else {
            first_hart_in_mask(meta.affinity)
        }
    }

    fn ensure_hart(&mut self, hart: HartId) {
        while self.per_hart.len() <= hart.0 {
            self.per_hart.push(HartSchedLocal::new());
        }
    }

    fn enqueue_kernel(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, QueueKind::Kernel, false);
    }

    fn enqueue_new(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, QueueKind::New, false);
    }

    fn enqueue_preempted_back(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, QueueKind::Preempted, false);
    }

    fn enqueue_preempted_front(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, QueueKind::Preempted, true);
    }

    fn enqueue(&mut self, task: TaskId, hart: HartId, queue: QueueKind, front: bool) {
        if self.meta_for(task).map(|meta| meta.queued) != Some(false) {
            return;
        }

        self.ensure_hart(hart);
        if let Some(meta) = self.meta_for_mut(task) {
            meta.queued = true;
        }
        let local = &mut self.per_hart[hart.0];
        let target = match queue {
            QueueKind::Kernel => &mut local.kernel_queue,
            QueueKind::New => &mut local.new_queue,
            QueueKind::Preempted => &mut local.preempted_queue,
        };
        if front {
            target.push_front(task);
        } else {
            target.push_back(task);
        }
    }

    fn pop_from_queue(
        &mut self,
        hart: HartId,
        queue: QueueKind,
    ) -> Option<(TaskHandle, SliceConfig)> {
        loop {
            let task = {
                let local = self.per_hart.get_mut(hart.0)?;
                let queue = match queue {
                    QueueKind::Kernel => &mut local.kernel_queue,
                    QueueKind::New => &mut local.new_queue,
                    QueueKind::Preempted => &mut local.preempted_queue,
                };
                queue.pop_front()?
            };

            let slice = match queue {
                QueueKind::Kernel => SliceConfig::Cooperative,
                QueueKind::New => SliceConfig::Preemptive {
                    slice_ns: Self::NEW_QUEUE_SLICE_NS,
                },
                QueueKind::Preempted => {
                    let slice_ns = self
                        .meta_for(task)
                        .map(|meta| {
                            if meta.remaining_budget_ns > 0 {
                                meta.remaining_budget_ns
                            } else {
                                Self::PREEMPTED_QUEUE_SLICE_NS
                            }
                        })
                        .unwrap_or(Self::PREEMPTED_QUEUE_SLICE_NS);
                    SliceConfig::Preemptive { slice_ns }
                }
            };

            if let Some(meta) = self.meta_for_mut(task) {
                meta.queued = false;
                meta.current_slice_ns = match slice {
                    SliceConfig::Cooperative => 0,
                    SliceConfig::Preemptive { slice_ns } => slice_ns,
                };
                if meta.remaining_budget_ns == 0 {
                    meta.remaining_budget_ns = meta.current_slice_ns;
                }
                return Some((meta.handle, slice));
            }
        }
    }

    fn task_runnable_inner(
        &mut self,
        task: TaskId,
        _hint: WakeHint,
        current_hart: HartId,
    ) -> Option<RunnablePlacement> {
        let meta = self.meta_for(task)?;
        let hart = self.home_hart_for_meta(meta);
        let was_queued = meta.queued;
        if meta.kernel_only {
            self.enqueue_kernel(task, hart);
        } else {
            match meta.class {
                SchedClass::Fair => {
                    if meta.remaining_budget_ns > 0 {
                        self.enqueue_preempted_front(task, hart);
                    } else {
                        self.enqueue_new(task, hart);
                    }
                }
                SchedClass::RtFifo
                | SchedClass::RtRoundRobin
                | SchedClass::Deadline
                | SchedClass::Idle => self.enqueue_new(task, hart),
            }
        }

        let queued = self.meta_for(task).map(|meta| meta.queued).unwrap_or(false);
        if !was_queued && queued {
            Some(RunnablePlacement {
                target_hart: hart,
                wake_remote: hart != current_hart,
            })
        } else {
            None
        }
    }
}

enum QueueKind {
    Kernel,
    New,
    Preempted,
}

impl SchedulerPolicy for Phase1Scheduler {
    fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.ensure_hart(hart);
        self.pop_from_queue(hart, QueueKind::Kernel)
            .or_else(|| self.pop_from_queue(hart, QueueKind::New))
            .or_else(|| self.pop_from_queue(hart, QueueKind::Preempted))
    }

    fn task_stopped(&mut self, task: TaskId, reason: StopReason, consumed_ns: u64, hart: HartId) {
        let mut requeue = None;
        if let Some(meta) = self.meta_for_mut(task) {
            meta.total_runtime_ns = meta.total_runtime_ns.saturating_add(consumed_ns);
            meta.last_hart = Some(hart);
            meta.remaining_budget_ns = meta.remaining_budget_ns.saturating_sub(consumed_ns);

            match reason {
                StopReason::SliceExpired => {
                    meta.remaining_budget_ns = 0;
                    requeue = Some((QueueKind::Preempted, false));
                }
                StopReason::Yielded => {
                    meta.remaining_budget_ns = 0;
                    requeue = Some(if meta.kernel_only {
                        (QueueKind::Kernel, false)
                    } else {
                        (QueueKind::Preempted, false)
                    });
                }
                StopReason::UserspaceTrap => {
                    requeue = Some((QueueKind::Preempted, meta.remaining_budget_ns > 0));
                }
                StopReason::PreemptedExternal => {
                    requeue = Some((QueueKind::Preempted, true));
                }
                StopReason::Blocked | StopReason::Completed => {}
            }
        }

        if let Some((queue, front)) = requeue {
            match (queue, front) {
                (QueueKind::Preempted, false) => self.enqueue_preempted_back(task, hart),
                (QueueKind::Preempted, true) => self.enqueue_preempted_front(task, hart),
                (QueueKind::Kernel, false) => self.enqueue_kernel(task, hart),
                (QueueKind::New, false) => self.enqueue_new(task, hart),
                (queue, front) => self.enqueue(task, hart, queue, front),
            }
        }
    }

    fn task_runnable(&mut self, task: TaskId, hint: WakeHint) {
        let _ = self.task_runnable_inner(task, hint, HartId(0));
    }

    fn task_submitted(&mut self, task: TaskId, handle: TaskHandle, initial_meta: InitialSchedMeta) {
        while self.meta.len() <= task.0 {
            self.meta.push(None);
        }
        self.meta[task.0] = Some(TaskSchedMeta {
            handle,
            class: initial_meta.class,
            remaining_budget_ns: 0,
            current_slice_ns: 0,
            total_runtime_ns: 0,
            last_hart: None,
            affinity: normalize_affinity(initial_meta.affinity),
            kernel_only: initial_meta.kernel_only,
            queued: false,
        });

        let hart = first_hart_in_mask(normalize_affinity(initial_meta.affinity));
        if initial_meta.kernel_only {
            self.enqueue_kernel(task, hart);
        } else {
            self.enqueue_new(task, hart);
        }
    }

    fn task_dropped(&mut self, task: TaskId) {
        if let Some(slot) = self.meta.get_mut(task.0) {
            *slot = None;
        }
    }
}

fn normalize_affinity(affinity: u64) -> u64 {
    if affinity == 0 {
        1
    } else {
        affinity
    }
}

fn first_hart_in_mask(mask: u64) -> HartId {
    HartId(normalize_affinity(mask).trailing_zeros() as usize)
}

fn hart_allowed(mask: u64, hart: HartId) -> bool {
    hart.0 < u64::BITS as usize && (normalize_affinity(mask) & (1u64 << hart.0)) != 0
}

impl Default for Phase1Scheduler {
    fn default() -> Self {
        Self::new()
    }
}
