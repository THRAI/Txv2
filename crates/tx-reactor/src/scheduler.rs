//! Scheduler policy interface and the Phase 1 round-robin policy.

use alloc::{collections::VecDeque, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::preempt::{PreemptMarker, PreemptMarkers, PreemptionPoint};
use crate::spin_lock::{SpinLock, SpinLockGuard};
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
    SelfYield,
    WakeHandoff,
    LifecycleWake,
    PriorityBoost,
    SignalDelivery,
    None,
}

impl WakeHint {
    pub const fn is_boosted(self) -> bool {
        matches!(
            self,
            Self::LifecycleWake | Self::PriorityBoost | Self::SignalDelivery
        )
    }

    pub const fn requests_userspace_preempt(self) -> bool {
        matches!(
            self,
            Self::WakeHandoff | Self::LifecycleWake | Self::PriorityBoost | Self::SignalDelivery
        )
    }
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
pub enum MigrationPolicy {
    Pinned,
    Movable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitialSchedMeta {
    pub class: SchedClass,
    pub nice: i8,
    pub rt_priority: u8,
    pub affinity: u64,
    pub kernel_only: bool,
    pub userspace_thread: bool,
    pub migration: MigrationPolicy,
    pub spread_on_submit: bool,
    pub preempted_on_submit: bool,
}

impl InitialSchedMeta {
    pub const fn fair() -> Self {
        Self {
            class: SchedClass::Fair,
            nice: 0,
            rt_priority: 0,
            affinity: u64::MAX,
            kernel_only: false,
            userspace_thread: false,
            migration: MigrationPolicy::Movable,
            spread_on_submit: false,
            preempted_on_submit: false,
        }
    }

    pub const fn kernel() -> Self {
        Self {
            class: SchedClass::Fair,
            nice: 0,
            rt_priority: 0,
            affinity: u64::MAX,
            kernel_only: true,
            userspace_thread: false,
            migration: MigrationPolicy::Pinned,
            spread_on_submit: false,
            preempted_on_submit: false,
        }
    }

    pub const fn with_affinity(mut self, affinity: u64) -> Self {
        self.affinity = affinity;
        self
    }

    pub const fn pinned(mut self) -> Self {
        self.migration = MigrationPolicy::Pinned;
        self
    }

    pub const fn movable(mut self) -> Self {
        self.migration = MigrationPolicy::Movable;
        self
    }

    pub const fn userspace_thread(mut self) -> Self {
        self.kernel_only = false;
        self.userspace_thread = true;
        self
    }

    pub const fn spread_on_submit(mut self) -> Self {
        self.spread_on_submit = true;
        self
    }

    pub const fn preempted_on_submit(mut self) -> Self {
        self.preempted_on_submit = true;
        self
    }
}

/// Shared scheduler state: per-task metadata and global counters.
///
/// All harts share one instance.  Per-hart state lives in [`HartSchedulerLocal`].
pub struct SchedulerShared {
    pub(crate) meta: SpinLock<Vec<Option<TaskSchedMeta>>>,
    pub(crate) stats: SpinLock<SchedulerStats>,
    pick_turn: AtomicU64,
}

impl SchedulerShared {
    #[inline]
    pub(crate) fn meta_for(&self, task: TaskId) -> Option<TaskSchedMeta> {
        self.meta
            .lock()
            .get(task.0)
            .and_then(Option::as_ref)
            .cloned()
    }

    #[inline]
    pub(crate) fn with_meta_mut<R>(
        &self,
        task: TaskId,
        f: impl FnOnce(&mut TaskSchedMeta) -> R,
    ) -> Option<R> {
        self.meta
            .lock()
            .get_mut(task.0)
            .and_then(Option::as_mut)
            .map(f)
    }

    pub fn insert_meta(&self, task: TaskId, handle: TaskHandle, initial_meta: InitialSchedMeta) {
        let mut meta = self.meta.lock();
        while meta.len() <= task.0 {
            meta.push(None);
        }
        meta[task.0] = Some(TaskSchedMeta {
            handle,
            class: initial_meta.class,
            remaining_budget_ns: 0,
            current_slice_ns: 0,
            total_runtime_ns: 0,
            last_hart: None,
            affinity: normalize_affinity(initial_meta.affinity),
            kernel_only: initial_meta.kernel_only,
            userspace_thread: initial_meta.userspace_thread,
            can_migrate: initial_meta.migration == MigrationPolicy::Movable,
            spread_on_submit: initial_meta.spread_on_submit,
            recently_stolen: false,
            must_migrate_on_stop: false,
            latency_wake: false,
            queued: false,
            queued_turn: 0,
            owner: TaskRunOwner::Parked,
        });
    }

    fn current_turn(&self) -> u64 {
        self.pick_turn.load(Ordering::Acquire)
    }

    fn advance_turn(&self) -> u64 {
        self.pick_turn.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn remove_meta(&self, task: TaskId) {
        if let Some(slot) = self.meta.lock().get_mut(task.0) {
            *slot = None;
        }
    }

    pub fn stats(&self) -> SchedulerStats {
        *self.stats.lock()
    }

    pub fn record_work_steal(&self) {
        let mut stats = self.stats.lock();
        stats.work_steals = stats.work_steals.saturating_add(1);
    }

    pub fn record_rebalance_move(&self) {
        let mut stats = self.stats.lock();
        stats.rebalance_moves = stats.rebalance_moves.saturating_add(1);
    }
}

pub struct Phase1Scheduler {
    pub(crate) shared: SchedulerShared,
    /// Temporary compatibility locals for legacy scheduler-only tests and
    /// callers. Runtime-owned scheduling paths use `HartReactorLocal` instead.
    compat_locals: Vec<HartSchedulerLocal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase1QueueKind {
    Kernel,
    Boosted,
    New,
    Preempted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRunOwner {
    Parked,
    Queued {
        hart: HartId,
        queue: Phase1QueueKind,
    },
    Polling {
        hart: HartId,
    },
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Phase1QueueDepths {
    pub kernel: usize,
    pub boosted: usize,
    pub new: usize,
    pub preempted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnablePlacement {
    pub target_hart: HartId,
    pub wake_remote: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalEnqueueRequest {
    pub task: TaskId,
    pub hart: HartId,
    pub queue: Phase1QueueKind,
    pub front: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueuedTaskReport {
    pub task: TaskId,
    pub hart: HartId,
    pub queue: Phase1QueueKind,
    pub queued_turn: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalAffinityMove {
    pub(crate) task: TaskId,
    pub(crate) from_hart: HartId,
    pub(crate) to_hart: HartId,
    pub(crate) queue: Phase1QueueKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerAffinityError {
    UnknownTask,
    TerminalTask,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedulerStats {
    pub work_steals: u64,
    pub rebalance_moves: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskSchedMeta {
    handle: TaskHandle,
    class: SchedClass,
    remaining_budget_ns: u64,
    current_slice_ns: u64,
    total_runtime_ns: u64,
    last_hart: Option<HartId>,
    affinity: u64,
    kernel_only: bool,
    userspace_thread: bool,
    can_migrate: bool,
    spread_on_submit: bool,
    recently_stolen: bool,
    must_migrate_on_stop: bool,
    latency_wake: bool,
    queued: bool,
    queued_turn: u64,
    owner: TaskRunOwner,
}

impl TaskSchedMeta {
    fn is_queued(&self) -> bool {
        matches!(self.owner, TaskRunOwner::Queued { .. })
    }

    fn is_queued_on(&self, hart: HartId, queue: Phase1QueueKind) -> bool {
        self.owner == TaskRunOwner::Queued { hart, queue }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HartRunQueues {
    pub(crate) kernel_queue: VecDeque<TaskId>,
    pub(crate) boosted_queue: VecDeque<TaskId>,
    pub(crate) new_queue: VecDeque<TaskId>,
    pub(crate) preempted_queue: VecDeque<TaskId>,
}

impl HartRunQueues {
    pub fn new() -> Self {
        Self {
            kernel_queue: VecDeque::new(),
            boosted_queue: VecDeque::new(),
            new_queue: VecDeque::new(),
            preempted_queue: VecDeque::new(),
        }
    }
}

/// Per-hart scheduling state — Phase 1b shard.
pub struct HartSchedulerLocal {
    pub(crate) queues: SpinLock<HartRunQueues>,
    pub(crate) wake_inbox: SpinLock<VecDeque<TaskId>>,
    pub(crate) markers: PreemptionPoint,
    pub(crate) last_balance_ns: AtomicU64,
    aged_preempted_picks_since_new: AtomicU64,
}

impl HartSchedulerLocal {
    pub fn new() -> Self {
        Self {
            queues: SpinLock::new(HartRunQueues::new()),
            wake_inbox: SpinLock::new(VecDeque::new()),
            markers: PreemptionPoint::new(),
            last_balance_ns: AtomicU64::new(0),
            aged_preempted_picks_since_new: AtomicU64::new(0),
        }
    }

    pub fn queue_depths(&self) -> Phase1QueueDepths {
        let local = self.queues.lock();
        Phase1QueueDepths {
            kernel: local.kernel_queue.len(),
            boosted: local.boosted_queue.len(),
            new: local.new_queue.len(),
            preempted: local.preempted_queue.len(),
        }
    }

    pub fn push_wake_inbox(&self, task: TaskId) {
        self.wake_inbox.lock().push_back(task);
        self.mark_need_resched();
    }

    pub fn drain_wake_inbox(&self, limit: usize) -> Vec<TaskId> {
        let mut inbox = self.wake_inbox.lock();
        let mut drained = Vec::new();
        while drained.len() < limit {
            let Some(task) = inbox.pop_front() else {
                break;
            };
            drained.push(task);
        }
        drained
    }

    pub fn mark_need_resched(&self) {
        self.markers.mark_need_resched();
    }

    pub fn mark_userspace_preempt(&self) {
        self.markers.mark_userspace_preempt();
    }

    pub fn take_userspace_preempt(&self) -> bool {
        self.markers.take(PreemptMarker::UserspacePreempt)
    }

    pub fn snapshot_markers(&self) -> PreemptMarkers {
        self.markers.snapshot()
    }

    pub fn consume_markers(&self) -> PreemptMarkers {
        self.markers.consume()
    }
}

impl Default for HartSchedulerLocal {
    fn default() -> Self {
        Self::new()
    }
}

impl Phase1Scheduler {
    pub const BASE_SLICE_NS: u64 = 10_000_000;
    pub const NEW_QUEUE_SLICE_NS: u64 = 1_000_000;
    pub const PREEMPTED_QUEUE_SLICE_NS: u64 = 10_000_000;
    pub const BALANCE_PERIOD_NS: u64 = 4_000_000;
    pub const MIN_REBALANCE_IMBALANCE: usize = 2;
    pub const AGING_PROMOTION_TURNS: u64 = 8;
    const AGED_PREEMPTED_STREAK_LIMIT_WHILE_NEW_READY: u64 = 1;
    const STEAL_SCAN_LIMIT: usize = 8;

    pub fn new() -> Self {
        Self {
            shared: SchedulerShared {
                meta: SpinLock::new(Vec::new()),
                stats: SpinLock::new(SchedulerStats::default()),
                pick_turn: AtomicU64::new(0),
            },
            compat_locals: Vec::new(),
        }
    }

    fn ensure_compat_hart(&mut self, hart: HartId) {
        while self.compat_locals.len() <= hart.0 {
            self.compat_locals.push(HartSchedulerLocal::new());
        }
    }

    fn ensure_compat_harts_through(&mut self, hart: HartId) {
        self.ensure_compat_hart(hart);
    }

    fn compat_local(&self, hart: HartId) -> Option<&HartSchedulerLocal> {
        self.compat_locals.get(hart.0)
    }

    fn compat_total_queue_depth(&self, hart: HartId) -> usize {
        self.compat_local(hart)
            .map(|local| self.total_queue_depth_from_local(local))
            .unwrap_or(0)
    }

    pub fn queue_depths(&self, hart: HartId) -> Phase1QueueDepths {
        self.compat_local(hart)
            .map(HartSchedulerLocal::queue_depths)
            .unwrap_or(Phase1QueueDepths {
                kernel: 0,
                boosted: 0,
                new: 0,
                preempted: 0,
            })
    }

    pub fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.ensure_compat_hart(hart);
        let local = self.compat_local(hart)? as *const HartSchedulerLocal;
        // The compatibility local is stored in `self.compat_locals` and is not
        // reallocated while `pick_next_from_local` runs. Queue mutation happens
        // through the local's internal spinlock.
        self.pick_next_from_local(hart, unsafe { &*local })
    }

    pub fn task_submitted(
        &mut self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
    ) {
        let _ = self.task_submitted_report(task, handle, initial_meta);
    }

    pub fn task_submitted_report(
        &mut self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
    ) -> QueuedTaskReport {
        let max_hart = max_hart_in_affinity(initial_meta.affinity);
        self.ensure_compat_harts_through(max_hart);
        let (request, report) =
            self.task_submitted_report_for_locals(task, handle, initial_meta, |hart| {
                self.compat_total_queue_depth(hart)
            });
        self.ensure_compat_hart(request.hart);
        if let Some(local) = self.compat_local(request.hart) {
            Self::push_to_local_queue(local, request.task, request.queue, request.front);
        }
        report
    }

    pub fn task_stopped(
        &mut self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    ) {
        if let Some(request) = self.task_stopped_for_locals(task, reason, consumed_ns, hart) {
            self.apply_compat_enqueue(request);
        }
    }

    pub fn task_runnable(&mut self, task: TaskId, hint: WakeHint) {
        if let Some((_placement, request)) =
            self.task_runnable_from_for_locals(task, hint, HartId(0))
        {
            self.apply_compat_enqueue(request);
        }
    }

    pub fn set_affinity(
        &mut self,
        task: TaskId,
        new_affinity: u64,
        current_hart: HartId,
    ) -> Result<Option<RunnablePlacement>, SchedulerAffinityError> {
        let Some((placement, movement)) =
            self.set_affinity_for_locals(task, new_affinity, current_hart)?
        else {
            return Ok(None);
        };
        self.apply_compat_affinity_move(movement);
        Ok(Some(placement))
    }

    pub fn try_steal(&mut self, thief: HartId, victim: HartId) -> Option<TaskHandle> {
        self.ensure_compat_hart(thief);
        self.ensure_compat_hart(victim);
        let thief_local = self.compat_local(thief)? as *const HartSchedulerLocal;
        let victim_local = self.compat_local(victim)? as *const HartSchedulerLocal;
        // The pointers refer to stable entries in `compat_locals`; this method
        // does not grow the vec after taking them.
        self.try_steal_from_locals(thief, unsafe { &*thief_local }, victim, unsafe {
            &*victim_local
        })
    }

    pub fn try_steal_from_any(&mut self, thief: HartId) -> Option<TaskHandle> {
        let victim = self.busiest_compat_steal_victim(thief)?;
        self.try_steal(thief, victim)
    }

    pub fn rebalance_at(&mut self, hart: HartId, now_ns: u64) -> Option<TaskHandle> {
        self.ensure_compat_hart(hart);
        let local = self.compat_local(hart)? as *const HartSchedulerLocal;
        let local_count = self.compat_locals.len();
        let depths: Vec<usize> = (0..local_count)
            .map(|victim| {
                let depths = self.queue_depths(HartId(victim));
                depths.boosted + depths.new + depths.preempted
            })
            .collect();
        let victim = self.rebalance_victim_from_locals(
            hart,
            unsafe { &*local },
            now_ns,
            local_count,
            |victim| depths.get(victim.0).copied().unwrap_or(0),
        )?;
        let stolen = self.try_steal(hart, victim);
        if stolen.is_some() {
            self.record_rebalance_move();
        }
        stolen
    }

    fn apply_compat_enqueue(&mut self, request: LocalEnqueueRequest) {
        self.ensure_compat_hart(request.hart);
        if let Some(local) = self.compat_local(request.hart) {
            Self::push_to_local_queue(local, request.task, request.queue, request.front);
        }
    }

    fn apply_compat_affinity_move(&mut self, movement: LocalAffinityMove) {
        self.ensure_compat_hart(movement.from_hart);
        self.ensure_compat_hart(movement.to_hart);
        if let Some(from_local) = self.compat_local(movement.from_hart) {
            Self::remove_from_local_queue(movement.task, from_local, movement.queue);
        }
        if let Some(to_local) = self.compat_local(movement.to_hart) {
            Self::push_to_local_queue(to_local, movement.task, movement.queue, false);
        }
    }

    fn busiest_compat_steal_victim(&self, thief: HartId) -> Option<HartId> {
        let mut best = None;
        let mut best_depth = 0;
        for victim_index in 0..self.compat_locals.len() {
            let victim = HartId(victim_index);
            if victim == thief {
                continue;
            }
            let depths = self.queue_depths(victim);
            let depth = depths.boosted + depths.new + depths.preempted;
            if depth > best_depth {
                best = Some(victim);
                best_depth = depth;
            }
        }
        best
    }

    pub fn rebalance_victim_from_locals(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
        now_ns: u64,
        local_count: usize,
        mut preempted_depth_for: impl FnMut(HartId) -> usize,
    ) -> Option<HartId> {
        let last_balance_ns = local.last_balance_ns.load(Ordering::Acquire);
        if now_ns.saturating_sub(last_balance_ns) < Self::BALANCE_PERIOD_NS {
            return None;
        }
        local.last_balance_ns.store(now_ns, Ordering::Release);

        let local_depths = self.queue_depths_from_local(local);
        let local_depth = local_depths.boosted + local_depths.new + local_depths.preempted;
        let mut best_victim = None;
        let mut best_depth = local_depth;
        for victim_index in 0..local_count {
            let victim = HartId(victim_index);
            if victim == hart {
                continue;
            }
            let depth = preempted_depth_for(victim);
            if depth > best_depth {
                best_depth = depth;
                best_victim = Some(victim);
            }
        }

        if best_depth < local_depth.saturating_add(Self::MIN_REBALANCE_IMBALANCE) {
            return None;
        }

        best_victim
    }

    pub fn record_rebalance_move(&self) {
        self.shared.record_rebalance_move();
    }

    pub(crate) fn set_affinity_for_locals(
        &self,
        task: TaskId,
        new_affinity: u64,
        current_hart: HartId,
    ) -> Result<Option<(RunnablePlacement, LocalAffinityMove)>, SchedulerAffinityError> {
        let new_affinity = normalize_affinity(new_affinity);
        let owner = self
            .shared
            .with_meta_mut(task, |meta| {
                if meta.owner == TaskRunOwner::Terminal {
                    return Err(SchedulerAffinityError::TerminalTask);
                }

                meta.affinity = new_affinity;
                Ok(meta.owner)
            })
            .ok_or(SchedulerAffinityError::UnknownTask)??;

        match owner {
            TaskRunOwner::Queued { hart, queue } if !hart_allowed(new_affinity, hart) => {
                let target_hart = self.apply_queued_affinity_meta_move(task, queue, new_affinity);
                Ok(Some((
                    RunnablePlacement {
                        target_hart,
                        wake_remote: target_hart != current_hart,
                    },
                    LocalAffinityMove {
                        task,
                        from_hart: hart,
                        to_hart: target_hart,
                        queue,
                    },
                )))
            }
            TaskRunOwner::Polling { hart } if !hart_allowed(new_affinity, hart) => {
                self.shared
                    .with_meta_mut(task, |meta| {
                        meta.must_migrate_on_stop = true;
                    })
                    .ok_or(SchedulerAffinityError::UnknownTask)?;
                Ok(None)
            }
            TaskRunOwner::Parked | TaskRunOwner::Queued { .. } | TaskRunOwner::Polling { .. } => {
                Ok(None)
            }
            TaskRunOwner::Terminal => Err(SchedulerAffinityError::TerminalTask),
        }
    }

    pub fn task_stopped_for_locals(
        &self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    ) -> Option<LocalEnqueueRequest> {
        let (target_hart, requeue) =
            self.apply_task_stopped_meta(task, reason, consumed_ns, hart)?;
        let (queue, front) = requeue?;
        Some(LocalEnqueueRequest {
            task,
            hart: target_hart,
            queue,
            front,
        })
    }

    pub fn task_runnable_from(
        &mut self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<RunnablePlacement> {
        let (placement, request) = self.task_runnable_inner_for_locals(task, hint, current_hart)?;
        self.apply_compat_enqueue(request);
        Some(placement)
    }

    pub fn task_runnable_from_for_locals(
        &self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<(RunnablePlacement, LocalEnqueueRequest)> {
        self.task_runnable_inner_for_locals(task, hint, current_hart)
    }

    pub fn task_submitted_for_locals(
        &self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
        total_queue_depth: impl FnMut(HartId) -> usize,
    ) -> LocalEnqueueRequest {
        self.task_submitted_report_for_locals(task, handle, initial_meta, total_queue_depth)
            .0
    }

    pub fn task_submitted_report_for_locals(
        &self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
        total_queue_depth: impl FnMut(HartId) -> usize,
    ) -> (LocalEnqueueRequest, QueuedTaskReport) {
        self.shared.insert_meta(task, handle, initial_meta);
        let hart = self
            .shared
            .meta_for(task)
            .map(|meta| self.initial_hart_for_meta_with_depths(&meta, total_queue_depth))
            .unwrap_or_else(|| first_hart_in_mask(normalize_affinity(initial_meta.affinity)));
        let queue = if initial_meta.kernel_only {
            Phase1QueueKind::Kernel
        } else if initial_meta.preempted_on_submit {
            Phase1QueueKind::Preempted
        } else {
            Phase1QueueKind::New
        };
        let queued_turn = self.shared.current_turn();
        self.shared.with_meta_mut(task, |meta| {
            meta.queued = true;
            meta.queued_turn = queued_turn;
            meta.owner = TaskRunOwner::Queued { hart, queue };
        });
        emit_sched_debug(b"debug.sched.submit.queue", pack_task_queue(task, queue));
        (
            LocalEnqueueRequest {
                task,
                hart,
                queue,
                front: false,
            },
            QueuedTaskReport {
                task,
                hart,
                queue,
                queued_turn,
            },
        )
    }

    pub fn task_dropped(&self, task: TaskId) {
        self.shared.remove_meta(task);
    }

    pub fn remaining_budget_ns(&self, task: TaskId) -> Option<u64> {
        self.shared
            .meta_for(task)
            .map(|meta| meta.remaining_budget_ns)
    }

    pub fn total_runtime_ns(&self, task: TaskId) -> Option<u64> {
        self.shared.meta_for(task).map(|meta| meta.total_runtime_ns)
    }

    pub fn task_affinity(&self, task: TaskId) -> Option<u64> {
        self.shared.meta_for(task).map(|meta| meta.affinity)
    }

    pub fn is_queued(&self, task: TaskId) -> bool {
        self.shared
            .meta_for(task)
            .map(|meta| meta.is_queued())
            .unwrap_or(false)
    }

    pub fn task_owner(&self, task: TaskId) -> Option<TaskRunOwner> {
        self.shared.meta_for(task).map(|meta| meta.owner)
    }

    pub fn target_hart_for_wake(&self, task: TaskId) -> Option<HartId> {
        self.shared
            .meta_for(task)
            .map(|meta| self.home_hart_for_meta(&meta))
    }

    pub fn can_migrate(&self, task: TaskId) -> bool {
        self.shared
            .meta_for(task)
            .map(|meta| meta.can_migrate)
            .unwrap_or(false)
    }

    pub fn is_userspace_thread(&self, task: TaskId) -> bool {
        self.shared
            .meta_for(task)
            .map(|meta| meta.userspace_thread)
            .unwrap_or(false)
    }

    pub fn stats(&self) -> SchedulerStats {
        self.shared.stats()
    }

    pub fn queue_depths_from_local(&self, local: &HartSchedulerLocal) -> Phase1QueueDepths {
        local.queue_depths()
    }

    pub fn total_queue_depth_from_local(&self, local: &HartSchedulerLocal) -> usize {
        let depths = self.queue_depths_from_local(local);
        depths.kernel + depths.boosted + depths.new + depths.preempted
    }

    pub fn push_wake_inbox_to_local(local: &HartSchedulerLocal, task: TaskId) {
        local.push_wake_inbox(task);
    }

    pub fn drain_wake_inbox_from_local(local: &HartSchedulerLocal, limit: usize) -> Vec<TaskId> {
        local.drain_wake_inbox(limit)
    }

    pub fn peek_next_from_local(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let queues = Self::lock_queues_from_local(local);
        self.peek_next_from_queues(hart, local, &queues)
    }

    fn peek_next_from_queues(
        &self,
        hart: HartId,
        scheduler_local: &HartSchedulerLocal,
        local: &HartRunQueues,
    ) -> Option<(TaskHandle, SliceConfig)> {
        if let Some(result) = self.peek_queue(
            hart,
            Phase1QueueKind::Kernel,
            &local.kernel_queue,
            SliceConfig::Cooperative,
        ) {
            return Some(result);
        }
        if let Some(result) =
            self.peek_preempted_like_queue(hart, Phase1QueueKind::Boosted, &local.boosted_queue)
        {
            return Some(result);
        }
        let fair_ready = Self::queues_have_entries(local, Phase1QueueKind::New)
            || self.preempted_queue_has_latency_wake(hart, &local.preempted_queue);
        let aged_preempted_allowed = !fair_ready
            || scheduler_local
                .aged_preempted_picks_since_new
                .load(Ordering::Acquire)
                < Self::AGED_PREEMPTED_STREAK_LIMIT_WHILE_NEW_READY;
        if aged_preempted_allowed {
            if let Some(result) = self.peek_aged_preempted_queue(hart, &local.preempted_queue) {
                return Some(result);
            }
        }
        if scheduler_local
            .aged_preempted_picks_since_new
            .load(Ordering::Acquire)
            >= Self::AGED_PREEMPTED_STREAK_LIMIT_WHILE_NEW_READY
        {
            if let Some(result) =
                self.peek_latency_wake_preempted_queue(hart, &local.preempted_queue)
            {
                return Some(result);
            }
        }
        if let Some(result) = self.peek_queue(
            hart,
            Phase1QueueKind::New,
            &local.new_queue,
            SliceConfig::Preemptive {
                slice_ns: Self::NEW_QUEUE_SLICE_NS,
            },
        ) {
            return Some(result);
        }
        self.peek_preempted_queue(hart, &local.preempted_queue)
    }

    fn peek_preempted_like_queue(
        &self,
        hart: HartId,
        queue: Phase1QueueKind,
        tasks: &VecDeque<TaskId>,
    ) -> Option<(TaskHandle, SliceConfig)> {
        tasks.iter().find_map(|task| {
            let meta = self.shared.meta_for(*task)?;
            meta.is_queued_on(hart, queue)
                .then_some((meta.handle, self.preempted_slice_for_task(*task)))
        })
    }

    fn peek_aged_preempted_queue(
        &self,
        hart: HartId,
        tasks: &VecDeque<TaskId>,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let current_turn = self.shared.current_turn();
        tasks.iter().find_map(|task| {
            let meta = self.shared.meta_for(*task)?;
            (meta.is_queued_on(hart, Phase1QueueKind::Preempted)
                && current_turn.saturating_sub(meta.queued_turn) >= Self::AGING_PROMOTION_TURNS)
                .then_some((meta.handle, self.preempted_slice_for_task(*task)))
        })
    }

    fn peek_latency_wake_preempted_queue(
        &self,
        hart: HartId,
        tasks: &VecDeque<TaskId>,
    ) -> Option<(TaskHandle, SliceConfig)> {
        tasks.iter().find_map(|task| {
            let meta = self.shared.meta_for(*task)?;
            (meta.is_queued_on(hart, Phase1QueueKind::Preempted) && meta.latency_wake)
                .then_some((meta.handle, self.preempted_slice_for_task(*task)))
        })
    }

    pub fn try_steal_from_locals(
        &self,
        thief: HartId,
        thief_local: &HartSchedulerLocal,
        victim: HartId,
        victim_local: &HartSchedulerLocal,
    ) -> Option<TaskHandle> {
        if thief == victim {
            return None;
        }

        let mut rejected = Vec::new();
        let mut stolen = None;

        for queue in [
            Phase1QueueKind::Boosted,
            Phase1QueueKind::Preempted,
            Phase1QueueKind::New,
        ] {
            for _ in 0..Self::STEAL_SCAN_LIMIT {
                let task = {
                    let mut queues = Self::lock_queues_from_local(victim_local);
                    match queue {
                        Phase1QueueKind::Kernel => None,
                        Phase1QueueKind::Boosted => queues.boosted_queue.pop_back(),
                        Phase1QueueKind::New => queues.new_queue.pop_back(),
                        Phase1QueueKind::Preempted => queues.preempted_queue.pop_back(),
                    }
                };
                let Some(task) = task else {
                    break;
                };

                let Some((reject_still_queued, stolen_handle)) =
                    self.shared.with_meta_mut(task, |meta| {
                        let eligible = meta.can_migrate
                            && !meta.kernel_only
                            && !meta.recently_stolen
                            && hart_allowed(meta.affinity, thief)
                            && meta.is_queued_on(victim, queue);
                        if !eligible {
                            return (meta.is_queued_on(victim, queue), None);
                        }

                        meta.owner = TaskRunOwner::Queued { hart: thief, queue };
                        meta.last_hart = Some(thief);
                        meta.recently_stolen = true;
                        (false, Some(meta.handle))
                    })
                else {
                    continue;
                };
                if reject_still_queued {
                    rejected.push((task, queue));
                    continue;
                }
                if let Some(handle) = stolen_handle {
                    stolen = Some((task, handle, queue));
                    break;
                }
            }
            if stolen.is_some() {
                break;
            }
        }

        if !rejected.is_empty() {
            let mut victim_local = Self::lock_queues_from_local(victim_local);
            for (task, queue) in rejected.into_iter().rev() {
                match queue {
                    Phase1QueueKind::Kernel => {}
                    Phase1QueueKind::Boosted => victim_local.boosted_queue.push_back(task),
                    Phase1QueueKind::New => victim_local.new_queue.push_back(task),
                    Phase1QueueKind::Preempted => victim_local.preempted_queue.push_back(task),
                }
            }
        }

        let (task, handle, queue) = stolen?;
        Self::push_to_local_queue(thief_local, task, queue, true);
        Self::mark_need_resched_local(thief_local);
        self.shared.record_work_steal();
        Some(handle)
    }

    fn peek_queue(
        &self,
        hart: HartId,
        queue_kind: Phase1QueueKind,
        queue: &VecDeque<TaskId>,
        slice: SliceConfig,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let meta_table = self.shared.meta.lock();
        queue.iter().find_map(|task| {
            let meta = meta_table.get(task.0)?.as_ref()?;
            meta.is_queued_on(hart, queue_kind)
                .then_some((meta.handle, slice))
        })
    }

    fn peek_preempted_queue(
        &self,
        hart: HartId,
        queue: &VecDeque<TaskId>,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let meta_table = self.shared.meta.lock();
        queue.iter().find_map(|task| {
            let meta = meta_table.get(task.0)?.as_ref()?;
            if !meta.is_queued_on(hart, Phase1QueueKind::Preempted) {
                return None;
            }
            let slice_ns = if meta.remaining_budget_ns > 0 {
                meta.remaining_budget_ns
            } else {
                Self::PREEMPTED_QUEUE_SLICE_NS
            };
            Some((meta.handle, SliceConfig::Preemptive { slice_ns }))
        })
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

    fn wake_target_hart_for_meta(
        &self,
        meta: &TaskSchedMeta,
        hint: WakeHint,
        current_hart: HartId,
    ) -> HartId {
        if hint == WakeHint::LifecycleWake
            && meta.can_migrate
            && hart_allowed(meta.affinity, current_hart)
        {
            current_hart
        } else {
            self.home_hart_for_meta(meta)
        }
    }

    fn initial_hart_for_meta_with_depths(
        &self,
        meta: &TaskSchedMeta,
        mut total_queue_depth: impl FnMut(HartId) -> usize,
    ) -> HartId {
        if meta.kernel_only || !meta.spread_on_submit {
            return first_hart_in_mask(meta.affinity);
        }

        let affinity = normalize_affinity(meta.affinity);
        let mut bits = affinity;
        let mut best = None;
        while bits != 0 {
            let hart = HartId(bits.trailing_zeros() as usize);
            let depth = total_queue_depth(hart);
            match best {
                Some((_, best_depth)) if depth >= best_depth => {}
                _ => best = Some((hart, depth)),
            }
            bits &= bits - 1;
        }
        best.map(|(hart, _)| hart)
            .unwrap_or_else(|| first_hart_in_mask(affinity))
    }

    #[inline]
    fn lock_queues_from_local(local: &HartSchedulerLocal) -> SpinLockGuard<'_, HartRunQueues> {
        local.queues.lock()
    }

    #[inline]
    pub fn mark_need_resched_local(local: &HartSchedulerLocal) {
        local.mark_need_resched();
    }

    #[inline]
    pub fn mark_userspace_preempt_local(local: &HartSchedulerLocal) {
        local.mark_userspace_preempt();
    }

    pub fn take_userspace_preempt_local(local: &HartSchedulerLocal) -> bool {
        local.take_userspace_preempt()
    }

    pub fn snapshot_markers_from_local(local: &HartSchedulerLocal) -> PreemptMarkers {
        local.snapshot_markers()
    }

    pub fn consume_markers_from_local(local: &HartSchedulerLocal) -> PreemptMarkers {
        local.consume_markers()
    }

    fn prepare_queued_affinity_move(&self, task: TaskId, new_affinity: u64) -> HartId {
        let target_hart = first_hart_in_mask(new_affinity);
        self.shared.with_meta_mut(task, |meta| {
            meta.queued = false;
            meta.owner = TaskRunOwner::Parked;
            meta.last_hart = Some(target_hart);
        });
        target_hart
    }

    fn apply_queued_affinity_meta_move(
        &self,
        task: TaskId,
        queue: Phase1QueueKind,
        new_affinity: u64,
    ) -> HartId {
        let target_hart = self.prepare_queued_affinity_move(task, new_affinity);
        self.shared.with_meta_mut(task, |meta| {
            meta.queued = true;
            meta.owner = TaskRunOwner::Queued {
                hart: target_hart,
                queue,
            };
        });
        target_hart
    }

    pub fn push_to_local_queue(
        local: &HartSchedulerLocal,
        task: TaskId,
        queue: Phase1QueueKind,
        front: bool,
    ) {
        let mut local = Self::lock_queues_from_local(local);
        let target = match queue {
            Phase1QueueKind::Kernel => &mut local.kernel_queue,
            Phase1QueueKind::Boosted => &mut local.boosted_queue,
            Phase1QueueKind::New => &mut local.new_queue,
            Phase1QueueKind::Preempted => &mut local.preempted_queue,
        };
        if front {
            target.push_front(task);
        } else {
            target.push_back(task);
        }
    }

    pub fn remove_from_local_queue(
        task: TaskId,
        local: &HartSchedulerLocal,
        queue: Phase1QueueKind,
    ) -> bool {
        let mut local = Self::lock_queues_from_local(local);
        let queue = match queue {
            Phase1QueueKind::Kernel => &mut local.kernel_queue,
            Phase1QueueKind::Boosted => &mut local.boosted_queue,
            Phase1QueueKind::New => &mut local.new_queue,
            Phase1QueueKind::Preempted => &mut local.preempted_queue,
        };
        let Some(index) = queue.iter().position(|queued| *queued == task) else {
            return false;
        };
        queue.remove(index);
        true
    }

    pub fn pick_next_from_local(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<(TaskHandle, SliceConfig)> {
        self.pop_from_local_queue(hart, local, Phase1QueueKind::Kernel)
            .or_else(|| self.pop_from_local_queue(hart, local, Phase1QueueKind::Boosted))
            .or_else(|| self.pop_fair_aged_preempted_from_local(hart, local))
            .or_else(|| self.pop_latency_wake_preempted_from_local(hart, local))
            .or_else(|| self.pop_from_local_queue(hart, local, Phase1QueueKind::New))
            .or_else(|| self.pop_from_local_queue(hart, local, Phase1QueueKind::Preempted))
    }

    fn pop_from_local_queue(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
        queue: Phase1QueueKind,
    ) -> Option<(TaskHandle, SliceConfig)> {
        loop {
            let task = Self::pop_task_from_local_queue(local, queue)?;
            let slice = self.slice_for_popped_task(task, queue);
            if let Some(next) = self.finish_popped_task(hart, queue, task, slice) {
                if queue == Phase1QueueKind::New {
                    local
                        .aged_preempted_picks_since_new
                        .store(0, Ordering::Release);
                } else if queue == Phase1QueueKind::Preempted
                    && self.local_preempted_queue_has_latency_wake(hart, local)
                {
                    local
                        .aged_preempted_picks_since_new
                        .fetch_add(1, Ordering::AcqRel);
                }
                return Some(next);
            }
        }
    }

    fn pop_task_from_local_queue(
        local: &HartSchedulerLocal,
        queue: Phase1QueueKind,
    ) -> Option<TaskId> {
        let mut local = Self::lock_queues_from_local(local);
        let queue = match queue {
            Phase1QueueKind::Kernel => &mut local.kernel_queue,
            Phase1QueueKind::Boosted => &mut local.boosted_queue,
            Phase1QueueKind::New => &mut local.new_queue,
            Phase1QueueKind::Preempted => &mut local.preempted_queue,
        };
        queue.pop_front()
    }

    fn pop_aged_preempted_from_local(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<(TaskHandle, SliceConfig)> {
        loop {
            let task = self.pop_aged_task_from_preempted_queue(hart, local)?;
            let slice = self.slice_for_popped_task(task, Phase1QueueKind::Preempted);
            if let Some(next) =
                self.finish_popped_task(hart, Phase1QueueKind::Preempted, task, slice)
            {
                return Some(next);
            }
        }
    }

    fn pop_latency_wake_preempted_from_local(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<(TaskHandle, SliceConfig)> {
        if local.aged_preempted_picks_since_new.load(Ordering::Acquire)
            < Self::AGED_PREEMPTED_STREAK_LIMIT_WHILE_NEW_READY
        {
            return None;
        }
        loop {
            let task = self.pop_latency_wake_task_from_preempted_queue(hart, local)?;
            let slice = self.slice_for_popped_task(task, Phase1QueueKind::Preempted);
            if let Some(next) =
                self.finish_popped_task(hart, Phase1QueueKind::Preempted, task, slice)
            {
                local
                    .aged_preempted_picks_since_new
                    .store(0, Ordering::Release);
                return Some(next);
            }
        }
    }

    fn pop_fair_aged_preempted_from_local(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let fair_ready = Self::local_queue_has_entries(local, Phase1QueueKind::New)
            || self.local_preempted_queue_has_latency_wake(hart, local);
        if !fair_ready {
            local
                .aged_preempted_picks_since_new
                .store(0, Ordering::Release);
        }
        if fair_ready
            && local.aged_preempted_picks_since_new.load(Ordering::Acquire)
                >= Self::AGED_PREEMPTED_STREAK_LIMIT_WHILE_NEW_READY
        {
            return None;
        }

        let next = self.pop_aged_preempted_from_local(hart, local)?;
        if fair_ready {
            local
                .aged_preempted_picks_since_new
                .fetch_add(1, Ordering::AcqRel);
        }
        Some(next)
    }

    fn local_queue_has_entries(local: &HartSchedulerLocal, queue: Phase1QueueKind) -> bool {
        let local = Self::lock_queues_from_local(local);
        Self::queues_have_entries(&local, queue)
    }

    fn queues_have_entries(local: &HartRunQueues, queue: Phase1QueueKind) -> bool {
        match queue {
            Phase1QueueKind::Kernel => !local.kernel_queue.is_empty(),
            Phase1QueueKind::Boosted => !local.boosted_queue.is_empty(),
            Phase1QueueKind::New => !local.new_queue.is_empty(),
            Phase1QueueKind::Preempted => !local.preempted_queue.is_empty(),
        }
    }

    fn local_preempted_queue_has_latency_wake(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> bool {
        let local = Self::lock_queues_from_local(local);
        self.preempted_queue_has_latency_wake(hart, &local.preempted_queue)
    }

    fn preempted_queue_has_latency_wake(&self, hart: HartId, tasks: &VecDeque<TaskId>) -> bool {
        tasks.iter().any(|task| {
            self.shared.meta_for(*task).is_some_and(|meta| {
                meta.is_queued_on(hart, Phase1QueueKind::Preempted) && meta.latency_wake
            })
        })
    }

    fn pop_aged_task_from_preempted_queue(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<TaskId> {
        let current_turn = self.shared.current_turn();
        let mut local = Self::lock_queues_from_local(local);
        let index = local.preempted_queue.iter().position(|task| {
            self.shared.meta_for(*task).is_some_and(|meta| {
                meta.is_queued_on(hart, Phase1QueueKind::Preempted)
                    && current_turn.saturating_sub(meta.queued_turn) >= Self::AGING_PROMOTION_TURNS
            })
        })?;
        local.preempted_queue.remove(index)
    }

    fn pop_latency_wake_task_from_preempted_queue(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<TaskId> {
        let mut local = Self::lock_queues_from_local(local);
        let index = local.preempted_queue.iter().position(|task| {
            self.shared.meta_for(*task).is_some_and(|meta| {
                meta.is_queued_on(hart, Phase1QueueKind::Preempted) && meta.latency_wake
            })
        })?;
        local.preempted_queue.remove(index)
    }

    fn slice_for_popped_task(&self, task: TaskId, queue: Phase1QueueKind) -> SliceConfig {
        match queue {
            Phase1QueueKind::Kernel => SliceConfig::Cooperative,
            Phase1QueueKind::Boosted => self.preempted_slice_for_task(task),
            Phase1QueueKind::New => SliceConfig::Preemptive {
                slice_ns: Self::NEW_QUEUE_SLICE_NS,
            },
            Phase1QueueKind::Preempted => self.preempted_slice_for_task(task),
        }
    }

    fn preempted_slice_for_task(&self, task: TaskId) -> SliceConfig {
        let slice_ns = self
            .shared
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

    fn finish_popped_task(
        &self,
        hart: HartId,
        queue: Phase1QueueKind,
        task: TaskId,
        slice: SliceConfig,
    ) -> Option<(TaskHandle, SliceConfig)> {
        self.shared.with_meta_mut(task, |meta| {
            if !meta.is_queued_on(hart, queue) {
                return None;
            }
            emit_sched_debug(b"debug.sched.pick.queue", pack_task_queue(task, queue));
            self.shared.advance_turn();
            meta.queued = false;
            meta.latency_wake = false;
            meta.owner = TaskRunOwner::Polling { hart };
            meta.current_slice_ns = match slice {
                SliceConfig::Cooperative => 0,
                SliceConfig::Preemptive { slice_ns } => slice_ns,
            };
            if meta.remaining_budget_ns == 0 {
                meta.remaining_budget_ns = meta.current_slice_ns;
            }
            Some((meta.handle, slice))
        })?
    }

    fn task_runnable_inner_for_locals(
        &self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<(RunnablePlacement, LocalEnqueueRequest)> {
        let meta = self.shared.meta_for(task)?;
        let hart = self.wake_target_hart_for_meta(&meta, hint, current_hart);
        let was_queued = meta.is_queued();
        let (queue, front) = if meta.kernel_only {
            (Phase1QueueKind::Kernel, false)
        } else {
            match meta.class {
                SchedClass::Fair => {
                    if hint.is_boosted() {
                        (Phase1QueueKind::Boosted, false)
                    } else if meta.remaining_budget_ns > 0 {
                        (
                            Phase1QueueKind::Preempted,
                            !meta.userspace_thread || hint == WakeHint::WakeHandoff,
                        )
                    } else {
                        (Phase1QueueKind::New, false)
                    }
                }
                SchedClass::RtFifo
                | SchedClass::RtRoundRobin
                | SchedClass::Deadline
                | SchedClass::Idle => (Phase1QueueKind::New, false),
            }
        };

        if was_queued
            || self
                .shared
                .meta_for(task)
                .is_some_and(|meta| meta.owner == TaskRunOwner::Terminal)
        {
            return None;
        }

        let queued_turn = self.shared.current_turn();
        self.shared.with_meta_mut(task, |meta| {
            meta.queued = true;
            meta.queued_turn = queued_turn;
            meta.latency_wake = queue == Phase1QueueKind::Preempted
                && hint == WakeHint::Normal
                && meta.userspace_thread;
            meta.owner = TaskRunOwner::Queued { hart, queue };
        });
        emit_sched_debug(b"debug.sched.runnable.queue", pack_task_queue(task, queue));
        emit_sched_debug(
            b"debug.sched.runnable.front",
            ((task.0 as i64) << 1) | i64::from(front),
        );
        emit_sched_debug(
            b"debug.sched.runnable.hint",
            ((task.0 as i64) << 8) | wake_hint_code(hint),
        );

        Some((
            RunnablePlacement {
                target_hart: hart,
                wake_remote: hart != current_hart,
            },
            LocalEnqueueRequest {
                task,
                hart,
                queue,
                front,
            },
        ))
    }

    fn apply_task_stopped_meta(
        &self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    ) -> Option<(HartId, Option<(Phase1QueueKind, bool)>)> {
        let mut requeue = None;
        let mut target_hart = hart;
        emit_sched_debug(b"debug.sched.stop.reason", pack_task_stop(task, reason));
        self.shared.with_meta_mut(task, |meta| {
            meta.queued = false;
            meta.latency_wake = false;
            meta.total_runtime_ns = meta.total_runtime_ns.saturating_add(consumed_ns);
            meta.last_hart = Some(hart);
            meta.remaining_budget_ns = meta.remaining_budget_ns.saturating_sub(consumed_ns);
            meta.recently_stolen = false;
            if meta.must_migrate_on_stop && !hart_allowed(meta.affinity, hart) {
                target_hart = first_hart_in_mask(meta.affinity);
                meta.last_hart = Some(target_hart);
                meta.must_migrate_on_stop = false;
            }

            match reason {
                StopReason::SliceExpired => {
                    meta.remaining_budget_ns = 0;
                    meta.owner = TaskRunOwner::Parked;
                    requeue = Some((Phase1QueueKind::Preempted, false));
                }
                StopReason::Yielded => {
                    meta.remaining_budget_ns = 0;
                    meta.owner = TaskRunOwner::Parked;
                    requeue = Some(if meta.kernel_only {
                        (Phase1QueueKind::Kernel, false)
                    } else {
                        (Phase1QueueKind::Preempted, false)
                    });
                }
                StopReason::UserspaceTrap => {
                    if meta.remaining_budget_ns > 0 {
                        meta.owner = TaskRunOwner::Parked;
                        requeue = Some((Phase1QueueKind::Preempted, !meta.userspace_thread));
                    } else {
                        meta.owner = TaskRunOwner::Parked;
                        requeue = Some((Phase1QueueKind::Preempted, false));
                    }
                }
                StopReason::PreemptedExternal => {
                    meta.owner = TaskRunOwner::Parked;
                    requeue = Some((Phase1QueueKind::Preempted, true));
                }
                StopReason::Blocked => {
                    meta.owner = TaskRunOwner::Parked;
                }
                StopReason::Completed => {
                    meta.owner = TaskRunOwner::Terminal;
                }
            }
        })?;

        if let Some((queue, _front)) = requeue {
            let queued_turn = self.shared.current_turn();
            self.shared.with_meta_mut(task, |meta| {
                meta.queued = true;
                meta.queued_turn = queued_turn;
                meta.latency_wake = false;
                meta.owner = TaskRunOwner::Queued {
                    hart: target_hart,
                    queue,
                };
            });
        }

        Some((target_hart, requeue))
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

fn max_hart_in_affinity(mask: u64) -> HartId {
    let mask = normalize_affinity(mask);
    HartId((u64::BITS - 1 - mask.leading_zeros()) as usize)
}

fn hart_allowed(mask: u64, hart: HartId) -> bool {
    hart.0 < u64::BITS as usize && (normalize_affinity(mask) & (1u64 << hart.0)) != 0
}

fn queue_code(queue: Phase1QueueKind) -> i64 {
    match queue {
        Phase1QueueKind::Kernel => 1,
        Phase1QueueKind::Boosted => 2,
        Phase1QueueKind::New => 3,
        Phase1QueueKind::Preempted => 4,
    }
}

fn stop_reason_code(reason: StopReason) -> i64 {
    match reason {
        StopReason::Blocked => 1,
        StopReason::Completed => 2,
        StopReason::Yielded => 3,
        StopReason::SliceExpired => 4,
        StopReason::UserspaceTrap => 5,
        StopReason::PreemptedExternal => 6,
    }
}

fn wake_hint_code(hint: WakeHint) -> i64 {
    match hint {
        WakeHint::Normal => 0,
        WakeHint::SelfYield => 1,
        WakeHint::WakeHandoff => 2,
        WakeHint::LifecycleWake => 3,
        WakeHint::PriorityBoost => 4,
        WakeHint::SignalDelivery => 5,
        WakeHint::None => 6,
    }
}

fn pack_task_queue(task: TaskId, queue: Phase1QueueKind) -> i64 {
    ((task.0 as i64) << 8) | queue_code(queue)
}

fn pack_task_stop(task: TaskId, reason: StopReason) -> i64 {
    ((task.0 as i64) << 8) | stop_reason_code(reason)
}

fn emit_sched_debug(name: &[u8], value: i64) {
    if !cfg!(tx_sched_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

impl Default for Phase1Scheduler {
    fn default() -> Self {
        Self::new()
    }
}
