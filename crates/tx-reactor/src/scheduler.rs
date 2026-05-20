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

pub struct Phase1Scheduler {
    meta: Vec<Option<TaskSchedMeta>>,
    hart_shards: Vec<HartShard>,
    stats: SchedulerStats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase1QueueKind {
    Kernel,
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
    pub new: usize,
    pub preempted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnablePlacement {
    pub target_hart: HartId,
    pub wake_remote: bool,
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
struct TaskSchedMeta {
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
    queued: bool,
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
struct HartSchedLocal {
    kernel_queue: VecDeque<TaskId>,
    new_queue: VecDeque<TaskId>,
    preempted_queue: VecDeque<TaskId>,
    wake_inbox: VecDeque<TaskId>,
}

impl HartSchedLocal {
    fn new() -> Self {
        Self {
            kernel_queue: VecDeque::new(),
            new_queue: VecDeque::new(),
            preempted_queue: VecDeque::new(),
            wake_inbox: VecDeque::new(),
        }
    }
}

/// Per-hart scheduling state — Phase 1b shard.
struct HartShard {
    queues: SpinLock<HartSchedLocal>,
    need_resched: PreemptionPoint,
    last_balance_ns: AtomicU64,
}

impl HartShard {
    pub fn new() -> Self {
        Self {
            queues: SpinLock::new(HartSchedLocal::new()),
            need_resched: PreemptionPoint::new(),
            last_balance_ns: AtomicU64::new(0),
        }
    }
}

impl Phase1Scheduler {
    pub const BASE_SLICE_NS: u64 = 10_000_000;
    pub const NEW_QUEUE_SLICE_NS: u64 = 1_000_000;
    pub const PREEMPTED_QUEUE_SLICE_NS: u64 = 10_000_000;
    pub const BALANCE_PERIOD_NS: u64 = 4_000_000;
    pub const MIN_REBALANCE_IMBALANCE: usize = 2;
    const STEAL_SCAN_LIMIT: usize = 8;

    pub fn new() -> Self {
        Self {
            meta: Vec::new(),
            hart_shards: alloc::vec![HartShard::new()],
            stats: SchedulerStats::default(),
        }
    }

    pub fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        <Self as SchedulerPolicy>::pick_next(self, hart)
    }

    pub fn pick_next_or_steal(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.pick_next(hart).or_else(|| {
            self.try_steal_from_any(hart)?;
            self.pick_next(hart)
        })
    }

    pub fn rebalance_at(&mut self, hart: HartId, now_ns: u64) -> Option<TaskHandle> {
        self.ensure_hart(hart);
        let shard = &self.hart_shards[hart.0];
        let last_balance_ns = shard.last_balance_ns.load(Ordering::Acquire);
        if now_ns.saturating_sub(last_balance_ns) < Self::BALANCE_PERIOD_NS {
            return None;
        }
        shard.last_balance_ns.store(now_ns, Ordering::Release);

        let local_depth = self.queue_depths(hart).preempted;
        let mut best_victim = None;
        let mut best_depth = local_depth;
        for victim_index in 0..self.hart_shards.len() {
            let victim = HartId(victim_index);
            if victim == hart {
                continue;
            }
            let depth = self.queue_depths(victim).preempted;
            if depth > best_depth {
                best_depth = depth;
                best_victim = Some(victim);
            }
        }

        if best_depth < local_depth.saturating_add(Self::MIN_REBALANCE_IMBALANCE) {
            return None;
        }

        let stolen = self.try_steal(hart, best_victim?);
        if stolen.is_some() {
            self.stats.rebalance_moves = self.stats.rebalance_moves.saturating_add(1);
        }
        stolen
    }

    pub fn set_affinity(
        &mut self,
        task: TaskId,
        new_affinity: u64,
        current_hart: HartId,
    ) -> Result<Option<RunnablePlacement>, SchedulerAffinityError> {
        let new_affinity = normalize_affinity(new_affinity);
        let owner = {
            let Some(meta) = self.meta_for_mut(task) else {
                return Err(SchedulerAffinityError::UnknownTask);
            };
            if meta.owner == TaskRunOwner::Terminal {
                return Err(SchedulerAffinityError::TerminalTask);
            }

            meta.affinity = new_affinity;
            meta.owner
        };

        match owner {
            TaskRunOwner::Queued { hart, queue } if !hart_allowed(new_affinity, hart) => {
                self.remove_from_queue(task, hart, queue);
                let target_hart = first_hart_in_mask(new_affinity);
                if let Some(meta) = self.meta_for_mut(task) {
                    meta.queued = false;
                    meta.owner = TaskRunOwner::Parked;
                    meta.last_hart = Some(target_hart);
                }
                self.enqueue(task, target_hart, queue, false);
                Ok(Some(RunnablePlacement {
                    target_hart,
                    wake_remote: target_hart != current_hart,
                }))
            }
            TaskRunOwner::Polling { hart } if !hart_allowed(new_affinity, hart) => {
                let Some(meta) = self.meta_for_mut(task) else {
                    return Err(SchedulerAffinityError::UnknownTask);
                };
                meta.must_migrate_on_stop = true;
                Ok(None)
            }
            TaskRunOwner::Parked | TaskRunOwner::Queued { .. } | TaskRunOwner::Polling { .. } => {
                Ok(None)
            }
            TaskRunOwner::Terminal => Err(SchedulerAffinityError::TerminalTask),
        }
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

    pub fn task_affinity(&self, task: TaskId) -> Option<u64> {
        self.meta_for(task).map(|meta| meta.affinity)
    }

    pub fn is_queued(&self, task: TaskId) -> bool {
        self.meta_for(task)
            .map(TaskSchedMeta::is_queued)
            .unwrap_or(false)
    }

    pub fn task_owner(&self, task: TaskId) -> Option<TaskRunOwner> {
        self.meta_for(task).map(|meta| meta.owner)
    }

    pub fn target_hart_for_wake(&self, task: TaskId) -> Option<HartId> {
        self.meta_for(task)
            .map(|meta| self.home_hart_for_meta(meta))
    }

    pub fn can_migrate(&self, task: TaskId) -> bool {
        self.meta_for(task)
            .map(|meta| meta.can_migrate)
            .unwrap_or(false)
    }

    pub fn is_userspace_thread(&self, task: TaskId) -> bool {
        self.meta_for(task)
            .map(|meta| meta.userspace_thread)
            .unwrap_or(false)
    }

    pub fn stats(&self) -> SchedulerStats {
        self.stats
    }

    pub fn queue_depths(&self, hart: HartId) -> Phase1QueueDepths {
        let Some(shard) = self.hart_shards.get(hart.0) else {
            return Phase1QueueDepths {
                kernel: 0,
                new: 0,
                preempted: 0,
            };
        };
        let local = shard.queues.lock();
        Phase1QueueDepths {
            kernel: local.kernel_queue.len(),
            new: local.new_queue.len(),
            preempted: local.preempted_queue.len(),
        }
    }

    pub(crate) fn push_wake_inbox(&mut self, hart: HartId, task: TaskId) {
        self.ensure_hart(hart);
        {
            let mut local = self.lock_queues(hart).expect("hart shard ensured");
            local.wake_inbox.push_back(task);
        }
        self.mark_need_resched(hart);
    }

    pub(crate) fn drain_wake_inbox(&self, hart: HartId, limit: usize) -> Vec<TaskId> {
        let Some(mut local) = self.lock_queues(hart) else {
            return Vec::new();
        };
        let mut drained = Vec::new();
        while drained.len() < limit {
            let Some(task) = local.wake_inbox.pop_front() else {
                break;
            };
            drained.push(task);
        }
        drained
    }

    pub fn peek_next(&self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        let local = self.lock_queues(hart)?;
        if let Some(result) = self.peek_queue(
            hart,
            Phase1QueueKind::Kernel,
            &local.kernel_queue,
            SliceConfig::Cooperative,
        ) {
            return Some(result);
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

    pub fn try_steal(&mut self, thief: HartId, victim: HartId) -> Option<TaskHandle> {
        if thief == victim || victim.0 >= self.hart_shards.len() {
            return None;
        }

        self.ensure_hart(thief);
        let mut rejected = Vec::new();
        let mut stolen = None;

        for _ in 0..Self::STEAL_SCAN_LIMIT {
            let task = {
                let mut victim_local = self.lock_queues(victim)?;
                victim_local.preempted_queue.pop_back()
            };
            let Some(task) = task else {
                break;
            };

            let Some(meta) = self.meta_for_mut(task) else {
                continue;
            };
            let eligible = meta.can_migrate
                && !meta.kernel_only
                && !meta.recently_stolen
                && hart_allowed(meta.affinity, thief)
                && meta.is_queued_on(victim, Phase1QueueKind::Preempted);
            if !eligible {
                if meta.is_queued_on(victim, Phase1QueueKind::Preempted) {
                    rejected.push(task);
                }
                continue;
            }

            meta.owner = TaskRunOwner::Queued {
                hart: thief,
                queue: Phase1QueueKind::Preempted,
            };
            meta.last_hart = Some(thief);
            meta.recently_stolen = true;
            stolen = Some((task, meta.handle));
            break;
        }

        if !rejected.is_empty() {
            let mut victim_local = self.lock_queues(victim)?;
            for task in rejected.into_iter().rev() {
                victim_local.preempted_queue.push_back(task);
            }
        }

        let (task, handle) = stolen?;
        let mut thief_local = self.lock_queues(thief).expect("thief shard ensured");
        thief_local.preempted_queue.push_front(task);
        drop(thief_local);
        self.mark_need_resched(thief);
        self.stats.work_steals = self.stats.work_steals.saturating_add(1);
        Some(handle)
    }

    pub fn try_steal_from_any(&mut self, thief: HartId) -> Option<TaskHandle> {
        let shard_count = self.hart_shards.len();
        if shard_count <= 1 {
            return None;
        }

        let mut tried = Vec::new();
        while tried.len() + 1 < shard_count {
            let Some(victim) = self.busiest_steal_victim(thief, &tried) else {
                return None;
            };
            if let Some(handle) = self.try_steal(thief, victim) {
                return Some(handle);
            }
            tried.push(victim);
        }
        None
    }

    fn busiest_steal_victim(&self, thief: HartId, tried: &[HartId]) -> Option<HartId> {
        let mut best = None;
        let mut best_depth = 0;
        for victim_index in 0..self.hart_shards.len() {
            let victim = HartId(victim_index);
            if victim == thief || tried.contains(&victim) {
                continue;
            }
            let depth = self.queue_depths(victim).preempted;
            if depth > best_depth {
                best = Some(victim);
                best_depth = depth;
            }
        }
        best
    }

    fn peek_queue(
        &self,
        hart: HartId,
        queue_kind: Phase1QueueKind,
        queue: &VecDeque<TaskId>,
        slice: SliceConfig,
    ) -> Option<(TaskHandle, SliceConfig)> {
        queue.iter().find_map(|task| {
            let meta = self.meta_for(*task)?;
            meta.is_queued_on(hart, queue_kind)
                .then_some((meta.handle, slice))
        })
    }

    fn peek_preempted_queue(
        &self,
        hart: HartId,
        queue: &VecDeque<TaskId>,
    ) -> Option<(TaskHandle, SliceConfig)> {
        queue.iter().find_map(|task| {
            let meta = self.meta_for(*task)?;
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

    fn initial_hart_for_meta(&self, meta: &TaskSchedMeta) -> HartId {
        if meta.kernel_only || !meta.can_migrate || !meta.spread_on_submit {
            return first_hart_in_mask(meta.affinity);
        }

        let affinity = normalize_affinity(meta.affinity);
        let mut bits = affinity;
        let mut best = None;
        while bits != 0 {
            let hart = HartId(bits.trailing_zeros() as usize);
            let depth = self.total_queue_depth(hart);
            match best {
                Some((_, best_depth)) if depth >= best_depth => {}
                _ => best = Some((hart, depth)),
            }
            bits &= bits - 1;
        }
        best.map(|(hart, _)| hart)
            .unwrap_or_else(|| first_hart_in_mask(affinity))
    }

    fn total_queue_depth(&self, hart: HartId) -> usize {
        let depths = self.queue_depths(hart);
        depths.kernel + depths.new + depths.preempted
    }

    #[inline]
    fn lock_queues(&self, hart: HartId) -> Option<SpinLockGuard<'_, HartSchedLocal>> {
        Some(self.hart_shards.get(hart.0)?.queues.lock())
    }

    #[inline]
    pub(crate) fn mark_need_resched(&mut self, hart: HartId) {
        self.ensure_hart(hart);
        self.hart_shards[hart.0].need_resched.mark_need_resched();
    }

    #[inline]
    pub(crate) fn mark_userspace_preempt(&mut self, hart: HartId) {
        self.ensure_hart(hart);
        self.hart_shards[hart.0]
            .need_resched
            .mark_userspace_preempt();
    }

    pub(crate) fn take_userspace_preempt(&self, hart: HartId) -> bool {
        self.hart_shards
            .get(hart.0)
            .is_some_and(|shard| shard.need_resched.take(PreemptMarker::UserspacePreempt))
    }

    pub(crate) fn snapshot_markers(&self, hart: HartId) -> PreemptMarkers {
        self.hart_shards
            .get(hart.0)
            .map(|shard| shard.need_resched.snapshot())
            .unwrap_or_else(PreemptMarkers::empty)
    }

    pub(crate) fn consume_markers(&self, hart: HartId) -> PreemptMarkers {
        self.hart_shards
            .get(hart.0)
            .map(|shard| shard.need_resched.consume())
            .unwrap_or_else(PreemptMarkers::empty)
    }

    fn ensure_hart(&mut self, hart: HartId) {
        while self.hart_shards.len() <= hart.0 {
            self.hart_shards.push(HartShard::new());
        }
    }

    fn enqueue_kernel(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, Phase1QueueKind::Kernel, false);
    }

    fn enqueue_new(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, Phase1QueueKind::New, false);
    }

    fn enqueue_preempted_back(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, Phase1QueueKind::Preempted, false);
    }

    fn enqueue_preempted_front(&mut self, task: TaskId, hart: HartId) {
        self.enqueue(task, hart, Phase1QueueKind::Preempted, true);
    }

    fn enqueue(&mut self, task: TaskId, hart: HartId, queue: Phase1QueueKind, front: bool) {
        if self
            .meta_for(task)
            .map(TaskSchedMeta::is_queued)
            .unwrap_or(true)
        {
            return;
        }
        if self
            .meta_for(task)
            .is_some_and(|meta| meta.owner == TaskRunOwner::Terminal)
        {
            return;
        }
        self.ensure_hart(hart);
        if let Some(meta) = self.meta_for_mut(task) {
            meta.queued = true;
            meta.owner = TaskRunOwner::Queued { hart, queue };
        }
        let mut local = self.lock_queues(hart).expect("hart shard ensured");
        let target = match queue {
            Phase1QueueKind::Kernel => &mut local.kernel_queue,
            Phase1QueueKind::New => &mut local.new_queue,
            Phase1QueueKind::Preempted => &mut local.preempted_queue,
        };
        if front {
            target.push_front(task);
        } else {
            target.push_back(task);
        }
    }

    fn remove_from_queue(&mut self, task: TaskId, hart: HartId, queue: Phase1QueueKind) -> bool {
        let Some(mut local) = self.lock_queues(hart) else {
            return false;
        };
        let queue = match queue {
            Phase1QueueKind::Kernel => &mut local.kernel_queue,
            Phase1QueueKind::New => &mut local.new_queue,
            Phase1QueueKind::Preempted => &mut local.preempted_queue,
        };
        let Some(index) = queue.iter().position(|queued| *queued == task) else {
            return false;
        };
        queue.remove(index);
        true
    }

    fn pop_from_queue(
        &mut self,
        hart: HartId,
        queue: Phase1QueueKind,
    ) -> Option<(TaskHandle, SliceConfig)> {
        loop {
            let task = {
                let mut local = self.lock_queues(hart)?;
                let queue_ref = match queue {
                    Phase1QueueKind::Kernel => &mut local.kernel_queue,
                    Phase1QueueKind::New => &mut local.new_queue,
                    Phase1QueueKind::Preempted => &mut local.preempted_queue,
                };
                queue_ref.pop_front()?
            };

            let slice = match queue {
                Phase1QueueKind::Kernel => SliceConfig::Cooperative,
                Phase1QueueKind::New => SliceConfig::Preemptive {
                    slice_ns: Self::NEW_QUEUE_SLICE_NS,
                },
                Phase1QueueKind::Preempted => {
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
                if !meta.is_queued_on(hart, queue) {
                    continue;
                }
                meta.queued = false;
                meta.owner = TaskRunOwner::Polling { hart };
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
        let was_queued = meta.is_queued();
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

impl SchedulerPolicy for Phase1Scheduler {
    fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.ensure_hart(hart);
        self.pop_from_queue(hart, Phase1QueueKind::Kernel)
            .or_else(|| self.pop_from_queue(hart, Phase1QueueKind::New))
            .or_else(|| self.pop_from_queue(hart, Phase1QueueKind::Preempted))
    }

    fn task_stopped(&mut self, task: TaskId, reason: StopReason, consumed_ns: u64, hart: HartId) {
        let mut requeue = None;
        let mut target_hart = hart;
        if let Some(meta) = self.meta_for_mut(task) {
            meta.queued = false;
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
                    meta.owner = TaskRunOwner::Parked;
                    requeue = Some((Phase1QueueKind::Preempted, meta.remaining_budget_ns > 0));
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
        }

        if let Some((queue, front)) = requeue {
            match (queue, front) {
                (Phase1QueueKind::Preempted, false) => {
                    self.enqueue_preempted_back(task, target_hart)
                }
                (Phase1QueueKind::Preempted, true) => {
                    self.enqueue_preempted_front(task, target_hart)
                }
                (Phase1QueueKind::Kernel, false) => self.enqueue_kernel(task, target_hart),
                (Phase1QueueKind::New, false) => self.enqueue_new(task, target_hart),
                (queue, front) => self.enqueue(task, target_hart, queue, front),
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
            userspace_thread: initial_meta.userspace_thread,
            can_migrate: initial_meta.migration == MigrationPolicy::Movable,
            spread_on_submit: initial_meta.spread_on_submit,
            recently_stolen: false,
            must_migrate_on_stop: false,
            queued: false,
            owner: TaskRunOwner::Parked,
        });

        let hart = self
            .meta_for(task)
            .map(|meta| self.initial_hart_for_meta(meta))
            .unwrap_or_else(|| first_hart_in_mask(normalize_affinity(initial_meta.affinity)));
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
