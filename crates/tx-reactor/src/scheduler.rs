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
    placement_turn: AtomicU64,
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

    fn reserve_submitted_task(
        &self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
        queue: Phase1QueueKind,
    ) -> (HartId, u64) {
        let affinity = normalize_affinity(initial_meta.affinity);
        let queued_turn = self.current_turn();
        let hart = if initial_meta.kernel_only || !initial_meta.spread_on_submit {
            first_hart_in_mask(affinity)
        } else {
            let turn = self.placement_turn.fetch_add(1, Ordering::Relaxed);
            round_robin_hart_in_mask(affinity, turn)
        };

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
            recently_stolen: false,
            must_migrate_on_stop: false,
            latency_wake: false,
            queued: true,
            queued_turn,
            owner: TaskRunOwner::Queued { hart, queue },
        });
        (hart, queued_turn)
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
    /// Removed from the physical queue with an execution lease owned by this
    /// hart. Wakes remain deferred until the hart commits the poll result.
    /// Keeping this state across `Future::poll` avoids a second global
    /// metadata-lock transaction on every userspace trap.
    Dispatching {
        hart: HartId,
    },
    /// Compatibility state for callers which explicitly publish the point at
    /// which the future has been taken from the task table. Production hart
    /// loops keep the equivalent `Dispatching` lease through the poll.
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

    /// Snapshot physical run-queue membership for one-shot stall diagnostics.
    pub fn queued_tasks(&self, limit: usize) -> (usize, Vec<(TaskId, Phase1QueueKind)>) {
        let queues = self.queues.lock();
        let total = queues.kernel_queue.len()
            + queues.boosted_queue.len()
            + queues.new_queue.len()
            + queues.preempted_queue.len();
        let mut tasks = Vec::with_capacity(total.min(limit));
        for (queue, entries) in [
            (Phase1QueueKind::Kernel, &queues.kernel_queue),
            (Phase1QueueKind::Boosted, &queues.boosted_queue),
            (Phase1QueueKind::New, &queues.new_queue),
            (Phase1QueueKind::Preempted, &queues.preempted_queue),
        ] {
            let remaining = limit.saturating_sub(tasks.len());
            tasks.extend(
                entries
                    .iter()
                    .copied()
                    .take(remaining)
                    .map(|task| (task, queue)),
            );
        }
        (total, tasks)
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
                placement_turn: AtomicU64::new(0),
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
        let _ = self.task_runnable_from(task, hint, HartId(0));
    }

    pub fn set_affinity(
        &mut self,
        task: TaskId,
        new_affinity: u64,
        current_hart: HartId,
    ) -> Result<Option<RunnablePlacement>, SchedulerAffinityError> {
        loop {
            let Some((placement, movement)) =
                self.set_affinity_for_locals(task, new_affinity, current_hart)?
            else {
                return Ok(None);
            };
            self.ensure_compat_hart(movement.from_hart);
            self.ensure_compat_hart(movement.to_hart);
            let from_local =
                self.compat_local(movement.from_hart).expect("source hart") as *const _;
            let to_local = self
                .compat_local(movement.to_hart)
                .expect("destination hart") as *const _;
            // The compatibility local array is not grown after these pointers
            // are taken. The commit locks both queues in hart-id order.
            if self.commit_affinity_move_on_locals(movement, unsafe { &*from_local }, unsafe {
                &*to_local
            })? {
                return Ok(Some(placement));
            }
        }
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
        self.shared
            .with_meta_mut(task, |meta| {
                if meta.owner == TaskRunOwner::Terminal {
                    return Err(SchedulerAffinityError::TerminalTask);
                }

                meta.affinity = new_affinity;
                match meta.owner {
                    TaskRunOwner::Queued { hart, queue } if !hart_allowed(new_affinity, hart) => {
                        let target_hart = first_hart_in_mask(new_affinity);
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
                    TaskRunOwner::Dispatching { hart } | TaskRunOwner::Polling { hart }
                        if !hart_allowed(new_affinity, hart) =>
                    {
                        meta.must_migrate_on_stop = true;
                        Ok(None)
                    }
                    TaskRunOwner::Parked
                    | TaskRunOwner::Queued { .. }
                    | TaskRunOwner::Dispatching { .. }
                    | TaskRunOwner::Polling { .. } => Ok(None),
                    TaskRunOwner::Terminal => Err(SchedulerAffinityError::TerminalTask),
                }
            })
            .ok_or(SchedulerAffinityError::UnknownTask)?
    }

    /// Move a queued task while physical membership and logical ownership are
    /// one transaction.
    ///
    /// Both hart queues are locked in numeric order, then scheduler metadata is
    /// rechecked. Returning `false` means the task changed state after routing;
    /// the caller must recompute the affinity action. This is the forced-
    /// migration form of the SCHED-SMP-1/2 lock-and-recheck rule.
    pub(crate) fn commit_affinity_move_on_locals(
        &self,
        movement: LocalAffinityMove,
        from_local: &HartSchedulerLocal,
        to_local: &HartSchedulerLocal,
    ) -> Result<bool, SchedulerAffinityError> {
        debug_assert_ne!(movement.from_hart, movement.to_hart);

        if movement.from_hart.0 < movement.to_hart.0 {
            let mut from_queues = Self::lock_queues_from_local(from_local);
            let mut to_queues = Self::lock_queues_from_local(to_local);
            self.commit_affinity_move_locked(movement, &mut from_queues, &mut to_queues)
        } else {
            let mut to_queues = Self::lock_queues_from_local(to_local);
            let mut from_queues = Self::lock_queues_from_local(from_local);
            self.commit_affinity_move_locked(movement, &mut from_queues, &mut to_queues)
        }
    }

    fn commit_affinity_move_locked(
        &self,
        movement: LocalAffinityMove,
        from_queues: &mut HartRunQueues,
        to_queues: &mut HartRunQueues,
    ) -> Result<bool, SchedulerAffinityError> {
        let mut meta_table = self.shared.meta.lock();
        let Some(meta) = meta_table.get_mut(movement.task.0).and_then(Option::as_mut) else {
            return Err(SchedulerAffinityError::UnknownTask);
        };
        if meta.owner == TaskRunOwner::Terminal {
            return Err(SchedulerAffinityError::TerminalTask);
        }
        if !meta.is_queued_on(movement.from_hart, movement.queue)
            || hart_allowed(meta.affinity, movement.from_hart)
            || first_hart_in_mask(meta.affinity) != movement.to_hart
        {
            return Ok(false);
        }

        let removed = Self::remove_from_queues(from_queues, movement.task, movement.queue);
        assert!(
            removed,
            "queued task metadata has no matching source queue entry"
        );
        Self::push_to_queues(to_queues, movement.task, movement.queue, false);
        meta.owner = TaskRunOwner::Queued {
            hart: movement.to_hart,
            queue: movement.queue,
        };
        meta.last_hart = Some(movement.to_hart);
        Ok(true)
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

    /// Return the queue owner a polling task would use after this stop.
    ///
    /// This is an optimistic routing read. `commit_stopped_on_local` rechecks
    /// affinity while holding that hart's queue lock.
    pub(crate) fn stopped_target_hart(&self, task: TaskId, hart: HartId) -> Option<HartId> {
        self.shared.meta_for(task).map(|meta| {
            if meta.must_migrate_on_stop && !hart_allowed(meta.affinity, hart) {
                first_hart_in_mask(meta.affinity)
            } else {
                hart
            }
        })
    }

    /// Commit an executing lease -> Parked/Queued/Terminal as one queue
    /// transaction.
    ///
    /// Requeueing stop reasons publish both the physical queue entry and
    /// `TaskRunOwner::Queued` under the destination queue lock. An affinity
    /// update that raced the optimistic route returns its new target without
    /// applying runtime accounting, so retrying cannot double-charge a task.
    pub(crate) fn commit_stopped_on_local(
        &self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
        claimed_hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Result<Option<LocalEnqueueRequest>, HartId> {
        let queued_turn = self.shared.current_turn();
        let mut queues = Self::lock_queues_from_local(local);
        let mut meta_table = self.shared.meta.lock();
        let Some(meta) = meta_table.get_mut(task.0).and_then(Option::as_mut) else {
            return Ok(None);
        };

        // The hart which owns the execution lease is the only path allowed to
        // publish the result of that poll. A wake racing this commit is
        // retained by the task-table wake bit and committed after the poll
        // returns. `Polling` remains accepted for compatibility callers; the
        // production loop keeps `Dispatching` to avoid a lock-only rename.
        if !matches!(
            meta.owner,
            TaskRunOwner::Dispatching { hart: owner }
                | TaskRunOwner::Polling { hart: owner }
                if owner == hart
        ) {
            return Ok(None);
        }

        let target_hart = if meta.must_migrate_on_stop && !hart_allowed(meta.affinity, hart) {
            first_hart_in_mask(meta.affinity)
        } else {
            hart
        };
        if target_hart != claimed_hart {
            return Err(target_hart);
        }

        let requeue =
            Self::apply_stopped_fields(meta, task, reason, consumed_ns, hart, target_hart);
        let Some((queue, front)) = requeue else {
            return Ok(None);
        };
        Self::push_to_queues(&mut queues, task, queue, front);
        meta.queued = true;
        meta.queued_turn = queued_turn;
        meta.latency_wake = false;
        meta.owner = TaskRunOwner::Queued {
            hart: target_hart,
            queue,
        };
        Ok(Some(LocalEnqueueRequest {
            task,
            hart: target_hart,
            queue,
            front,
        }))
    }

    pub fn task_runnable_from(
        &mut self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<RunnablePlacement> {
        let mut target = self.runnable_target_hart(task, hint, current_hart)?;
        loop {
            self.ensure_compat_hart(target);
            let local = self.compat_local(target)? as *const HartSchedulerLocal;
            match self
                .commit_runnable_on_local(task, hint, current_hart, target, unsafe { &*local })
            {
                Ok(placement) => return placement,
                Err(retry_target) => target = retry_target,
            }
        }
    }

    pub fn task_runnable_from_for_locals(
        &self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<(RunnablePlacement, LocalEnqueueRequest)> {
        self.task_runnable_inner_for_locals(task, hint, current_hart)
    }

    /// Return the hart a wake would target from the current metadata snapshot.
    ///
    /// This is only a routing hint.  [`Self::commit_runnable_on_local`] repeats
    /// the calculation while holding the destination queue lock and rejects a
    /// stale hint.  Keeping the recheck in the commit path closes the
    /// wake-vs-affinity/migration TOCTOU window required by SCHED-SMP-2.
    pub(crate) fn runnable_target_hart(
        &self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<HartId> {
        self.shared.meta_for(task).map(|meta| match meta.owner {
            // Signal delivery may promote an already-runnable userspace task.
            // Route that transaction to the queue which physically owns it.
            TaskRunOwner::Queued { hart, .. }
                if meta.userspace_thread && hint == WakeHint::SignalDelivery =>
            {
                hart
            }
            _ => self.wake_target_hart_for_meta(&meta, hint, current_hart),
        })
    }

    /// Atomically publish `task` as runnable on `claimed_hart`.
    ///
    /// The physical queue insertion and `TaskRunOwner::Queued` transition are
    /// performed while the destination queue lock is held.  The metadata is
    /// re-read under that lock; if affinity or ownership changed after routing,
    /// the caller receives the new target and retries with that hart's queue.
    pub(crate) fn commit_runnable_on_local(
        &self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
        claimed_hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Result<Option<RunnablePlacement>, HartId> {
        let queued_turn = self.shared.current_turn();
        let mut queues = Self::lock_queues_from_local(local);
        let mut meta_table = self.shared.meta.lock();
        let Some(meta) = meta_table.get_mut(task.0).and_then(Option::as_mut) else {
            return Ok(None);
        };

        if let TaskRunOwner::Queued {
            hart,
            queue: from_queue,
        } = meta.owner
        {
            if hart != claimed_hart {
                return Err(hart);
            }
            if !meta.userspace_thread
                || hint != WakeHint::SignalDelivery
                || from_queue == Phase1QueueKind::Boosted
            {
                return Ok(None);
            }

            let removed = Self::remove_from_queues(&mut queues, task, from_queue);
            assert!(
                removed,
                "queued task metadata has no matching queue entry during signal promotion"
            );
            Self::push_to_queues(&mut queues, task, Phase1QueueKind::Boosted, false);
            meta.queued_turn = queued_turn;
            meta.latency_wake = false;
            meta.owner = TaskRunOwner::Queued {
                hart,
                queue: Phase1QueueKind::Boosted,
            };
            return Ok(Some(RunnablePlacement {
                target_hart: hart,
                wake_remote: hart != current_hart,
            }));
        }

        if matches!(
            meta.owner,
            TaskRunOwner::Dispatching { .. }
                | TaskRunOwner::Polling { .. }
                | TaskRunOwner::Terminal
        ) {
            return Ok(None);
        }

        let target_hart = self.wake_target_hart_for_meta(meta, hint, current_hart);
        if target_hart != claimed_hart {
            return Err(target_hart);
        }

        let (queue, front) = Self::runnable_queue_for_meta(meta, hint);
        Self::push_to_queues(&mut queues, task, queue, front);

        meta.queued = true;
        meta.queued_turn = queued_turn;
        meta.latency_wake = queue == Phase1QueueKind::Preempted
            && hint == WakeHint::Normal
            && meta.userspace_thread;
        meta.owner = TaskRunOwner::Queued {
            hart: claimed_hart,
            queue,
        };

        emit_sched_debug(b"debug.sched.runnable.queue", pack_task_queue(task, queue));
        emit_sched_debug(
            b"debug.sched.runnable.front",
            ((task.0 as i64) << 1) | i64::from(front),
        );
        emit_sched_debug(
            b"debug.sched.runnable.hint",
            ((task.0 as i64) << 8) | wake_hint_code(hint),
        );

        Ok(Some(RunnablePlacement {
            target_hart: claimed_hart,
            wake_remote: claimed_hart != current_hart,
        }))
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
        _total_queue_depth: impl FnMut(HartId) -> usize,
    ) -> (LocalEnqueueRequest, QueuedTaskReport) {
        let queue = if initial_meta.kernel_only {
            Phase1QueueKind::Kernel
        } else if initial_meta.preempted_on_submit {
            Phase1QueueKind::Preempted
        } else {
            Phase1QueueKind::New
        };
        let (hart, queued_turn) =
            self.shared
                .reserve_submitted_task(task, handle, initial_meta, queue);
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

        // Acquire both queue locks in hart order.  The attempts are
        // non-blocking, so opposite-direction steals cannot deadlock and an
        // idle hart never burns a timeslice waiting for an active queue.
        let stolen = if thief.0 < victim.0 {
            let mut thief_queues = Self::try_lock_queues_from_local(thief_local)?;
            let mut victim_queues = Self::try_lock_queues_from_local(victim_local)?;
            self.try_transfer_preempted_locked(thief, &mut thief_queues, victim, &mut victim_queues)
        } else {
            let mut victim_queues = Self::try_lock_queues_from_local(victim_local)?;
            let mut thief_queues = Self::try_lock_queues_from_local(thief_local)?;
            self.try_transfer_preempted_locked(thief, &mut thief_queues, victim, &mut victim_queues)
        }?;

        Self::mark_need_resched_local(thief_local);
        self.shared.record_work_steal();
        Some(stolen)
    }

    /// Transfer one cold preempted task as a single physical/logical queue
    /// transaction.  New and boosted work retain their placement semantics;
    /// only tasks that have already crossed a scheduler boundary may move.
    fn try_transfer_preempted_locked(
        &self,
        thief: HartId,
        thief_queues: &mut HartRunQueues,
        victim: HartId,
        victim_queues: &mut HartRunQueues,
    ) -> Option<TaskHandle> {
        let mut rejected = Vec::new();
        let mut meta_table = self.shared.meta.lock();

        for _ in 0..Self::STEAL_SCAN_LIMIT {
            let Some(task) = victim_queues.preempted_queue.pop_front() else {
                break;
            };
            let Some(meta) = meta_table.get_mut(task.0).and_then(Option::as_mut) else {
                continue;
            };
            if !meta.is_queued_on(victim, Phase1QueueKind::Preempted) {
                continue;
            }
            if !meta.can_migrate
                || meta.kernel_only
                || meta.recently_stolen
                || !hart_allowed(meta.affinity, thief)
            {
                rejected.push(task);
                continue;
            }

            thief_queues.preempted_queue.push_front(task);
            meta.owner = TaskRunOwner::Queued {
                hart: thief,
                queue: Phase1QueueKind::Preempted,
            };
            meta.last_hart = Some(thief);
            meta.recently_stolen = true;
            let handle = meta.handle;

            for rejected_task in rejected.into_iter().rev() {
                victim_queues.preempted_queue.push_front(rejected_task);
            }
            return Some(handle);
        }

        for rejected_task in rejected.into_iter().rev() {
            victim_queues.preempted_queue.push_front(rejected_task);
        }
        None
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

    #[inline]
    fn lock_queues_from_local(local: &HartSchedulerLocal) -> SpinLockGuard<'_, HartRunQueues> {
        local.queues.lock()
    }

    #[inline]
    fn try_lock_queues_from_local(
        local: &HartSchedulerLocal,
    ) -> Option<SpinLockGuard<'_, HartRunQueues>> {
        local.queues.try_lock()
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

    pub fn push_to_local_queue(
        local: &HartSchedulerLocal,
        task: TaskId,
        queue: Phase1QueueKind,
        front: bool,
    ) {
        let mut local = Self::lock_queues_from_local(local);
        Self::push_to_queues(&mut local, task, queue, front);
    }

    fn push_to_queues(
        local: &mut HartRunQueues,
        task: TaskId,
        queue: Phase1QueueKind,
        front: bool,
    ) {
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
        Self::remove_from_queues(&mut local, task, queue)
    }

    fn remove_from_queues(local: &mut HartRunQueues, task: TaskId, queue: Phase1QueueKind) -> bool {
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
        let mut queues = Self::lock_queues_from_local(local);
        loop {
            let task = {
                let queue = match queue {
                    Phase1QueueKind::Kernel => &mut queues.kernel_queue,
                    Phase1QueueKind::Boosted => &mut queues.boosted_queue,
                    Phase1QueueKind::New => &mut queues.new_queue,
                    Phase1QueueKind::Preempted => &mut queues.preempted_queue,
                };
                queue.pop_front()?
            };
            if let Some(next) = self.finish_popped_task_locked(hart, queue, task) {
                if queue == Phase1QueueKind::New {
                    local
                        .aged_preempted_picks_since_new
                        .store(0, Ordering::Release);
                } else if queue == Phase1QueueKind::Preempted
                    && self.preempted_queue_has_latency_wake(hart, &queues.preempted_queue)
                {
                    local
                        .aged_preempted_picks_since_new
                        .fetch_add(1, Ordering::AcqRel);
                }
                return Some(next);
            }
        }
    }

    fn pop_aged_preempted_from_local(
        &self,
        hart: HartId,
        local: &HartSchedulerLocal,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let current_turn = self.shared.current_turn();
        let mut queues = Self::lock_queues_from_local(local);
        loop {
            let index = {
                let meta_table = self.shared.meta.lock();
                queues.preempted_queue.iter().position(|task| {
                    meta_table
                        .get(task.0)
                        .and_then(Option::as_ref)
                        .is_some_and(|meta| {
                            meta.is_queued_on(hart, Phase1QueueKind::Preempted)
                                && current_turn.saturating_sub(meta.queued_turn)
                                    >= Self::AGING_PROMOTION_TURNS
                        })
                })
            }?;
            let task = queues.preempted_queue.remove(index)?;
            if let Some(next) =
                self.finish_popped_task_locked(hart, Phase1QueueKind::Preempted, task)
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
        let mut queues = Self::lock_queues_from_local(local);
        loop {
            let index = {
                let meta_table = self.shared.meta.lock();
                queues.preempted_queue.iter().position(|task| {
                    meta_table
                        .get(task.0)
                        .and_then(Option::as_ref)
                        .is_some_and(|meta| {
                            meta.is_queued_on(hart, Phase1QueueKind::Preempted) && meta.latency_wake
                        })
                })
            }?;
            let task = queues.preempted_queue.remove(index)?;
            if let Some(next) =
                self.finish_popped_task_locked(hart, Phase1QueueKind::Preempted, task)
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

    /// Complete `Queued -> Dispatching` while the caller still owns the
    /// corresponding hart queue lock.
    fn finish_popped_task_locked(
        &self,
        hart: HartId,
        queue: Phase1QueueKind,
        task: TaskId,
    ) -> Option<(TaskHandle, SliceConfig)> {
        let mut meta_table = self.shared.meta.lock();
        let meta = meta_table.get_mut(task.0)?.as_mut()?;
        if !meta.is_queued_on(hart, queue) {
            return None;
        }
        let slice = Self::slice_for_meta(meta, queue);
        emit_sched_debug(b"debug.sched.pick.queue", pack_task_queue(task, queue));
        self.shared.advance_turn();
        meta.queued = false;
        meta.latency_wake = false;
        meta.owner = TaskRunOwner::Dispatching { hart };
        meta.current_slice_ns = match slice {
            SliceConfig::Cooperative => 0,
            SliceConfig::Preemptive { slice_ns } => slice_ns,
        };
        if meta.remaining_budget_ns == 0 {
            meta.remaining_budget_ns = meta.current_slice_ns;
        }
        Some((meta.handle, slice))
    }

    /// Finish the queue-to-task-table ownership handoff after TaskTable has
    /// changed Runnable to Polling and removed the future from its slot.
    pub fn mark_dispatching_polling(&self, task: TaskId, hart: HartId) -> bool {
        self.shared
            .with_meta_mut(task, |meta| {
                if meta.owner != (TaskRunOwner::Dispatching { hart }) {
                    return false;
                }
                meta.owner = TaskRunOwner::Polling { hart };
                true
            })
            .unwrap_or(false)
    }

    fn slice_for_meta(meta: &TaskSchedMeta, queue: Phase1QueueKind) -> SliceConfig {
        match queue {
            Phase1QueueKind::Kernel => SliceConfig::Cooperative,
            Phase1QueueKind::New => SliceConfig::Preemptive {
                slice_ns: Self::NEW_QUEUE_SLICE_NS,
            },
            Phase1QueueKind::Boosted | Phase1QueueKind::Preempted => SliceConfig::Preemptive {
                slice_ns: if meta.remaining_budget_ns > 0 {
                    meta.remaining_budget_ns
                } else {
                    Self::PREEMPTED_QUEUE_SLICE_NS
                },
            },
        }
    }

    fn task_runnable_inner_for_locals(
        &self,
        task: TaskId,
        hint: WakeHint,
        current_hart: HartId,
    ) -> Option<(RunnablePlacement, LocalEnqueueRequest)> {
        let queued_turn = self.shared.current_turn();
        let (hart, queue, front) = self.shared.with_meta_mut(task, |meta| {
            if meta.is_queued()
                || matches!(
                    meta.owner,
                    TaskRunOwner::Dispatching { .. }
                        | TaskRunOwner::Polling { .. }
                        | TaskRunOwner::Terminal
                )
            {
                return None;
            }

            let hart = self.wake_target_hart_for_meta(meta, hint, current_hart);
            let (queue, front) = Self::runnable_queue_for_meta(meta, hint);

            meta.queued = true;
            meta.queued_turn = queued_turn;
            meta.latency_wake = queue == Phase1QueueKind::Preempted
                && hint == WakeHint::Normal
                && meta.userspace_thread;
            meta.owner = TaskRunOwner::Queued { hart, queue };
            Some((hart, queue, front))
        })??;
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

    fn runnable_queue_for_meta(meta: &TaskSchedMeta, hint: WakeHint) -> (Phase1QueueKind, bool) {
        if meta.kernel_only {
            return (Phase1QueueKind::Kernel, false);
        }
        match meta.class {
            SchedClass::Fair => {
                if hint.is_boosted() {
                    (Phase1QueueKind::Boosted, false)
                } else if meta.remaining_budget_ns > 0 {
                    (
                        Phase1QueueKind::Preempted,
                        hint == WakeHint::WakeHandoff
                            || (!meta.userspace_thread && hint != WakeHint::SelfYield),
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
    }

    fn apply_task_stopped_meta(
        &self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    ) -> Option<(HartId, Option<(Phase1QueueKind, bool)>)> {
        let mut meta_table = self.shared.meta.lock();
        let meta = meta_table.get_mut(task.0)?.as_mut()?;
        if !matches!(
            meta.owner,
            TaskRunOwner::Dispatching { hart: owner }
                | TaskRunOwner::Polling { hart: owner }
                if owner == hart
        ) {
            return None;
        }
        let target_hart = if meta.must_migrate_on_stop && !hart_allowed(meta.affinity, hart) {
            first_hart_in_mask(meta.affinity)
        } else {
            hart
        };
        let requeue =
            Self::apply_stopped_fields(meta, task, reason, consumed_ns, hart, target_hart);

        if let Some((queue, _front)) = requeue {
            let queued_turn = self.shared.current_turn();
            meta.queued = true;
            meta.queued_turn = queued_turn;
            meta.latency_wake = false;
            meta.owner = TaskRunOwner::Queued {
                hart: target_hart,
                queue,
            };
        }

        Some((target_hart, requeue))
    }

    fn apply_stopped_fields(
        meta: &mut TaskSchedMeta,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
        target_hart: HartId,
    ) -> Option<(Phase1QueueKind, bool)> {
        emit_sched_debug(b"debug.sched.stop.reason", pack_task_stop(task, reason));
        meta.queued = false;
        meta.latency_wake = false;
        meta.total_runtime_ns = meta.total_runtime_ns.saturating_add(consumed_ns);
        meta.last_hart = Some(hart);
        meta.remaining_budget_ns = meta.remaining_budget_ns.saturating_sub(consumed_ns);
        meta.recently_stolen = false;
        meta.must_migrate_on_stop = false;
        if target_hart != hart {
            meta.last_hart = Some(target_hart);
        }

        match reason {
            StopReason::SliceExpired => {
                meta.remaining_budget_ns = 0;
                meta.owner = TaskRunOwner::Parked;
                Some((Phase1QueueKind::Preempted, false))
            }
            StopReason::Yielded => {
                meta.remaining_budget_ns = 0;
                meta.owner = TaskRunOwner::Parked;
                Some(if meta.kernel_only {
                    (Phase1QueueKind::Kernel, false)
                } else {
                    (Phase1QueueKind::Preempted, false)
                })
            }
            StopReason::UserspaceTrap => {
                meta.owner = TaskRunOwner::Parked;
                Some((
                    Phase1QueueKind::Preempted,
                    meta.remaining_budget_ns > 0 && !meta.userspace_thread,
                ))
            }
            StopReason::PreemptedExternal => {
                meta.owner = TaskRunOwner::Parked;
                Some((Phase1QueueKind::Preempted, true))
            }
            StopReason::Blocked => {
                meta.owner = TaskRunOwner::Parked;
                None
            }
            StopReason::Completed => {
                meta.owner = TaskRunOwner::Terminal;
                None
            }
        }
    }
}

fn round_robin_hart_in_mask(affinity: u64, turn: u64) -> HartId {
    let affinity = normalize_affinity(affinity);
    let mut ordinal = turn % u64::from(affinity.count_ones());
    let mut remaining = affinity;
    loop {
        let hart = HartId(remaining.trailing_zeros() as usize);
        if ordinal == 0 {
            return hart;
        }
        ordinal -= 1;
        remaining &= remaining - 1;
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
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

impl Default for Phase1Scheduler {
    fn default() -> Self {
        Self::new()
    }
}
