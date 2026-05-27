use tx_reactor::{
    HartId, InitialSchedMeta, Phase1QueueKind, Phase1Scheduler, PiLockToken, PriorityBoostError,
    PriorityKey, SchedulerAffinityError, SliceConfig, StopReason, TaskHandle, TaskId, TaskRunOwner,
    WakeHint,
};

fn submit_fair(scheduler: &mut Phase1Scheduler, raw: usize) -> TaskId {
    let task = TaskId(raw);
    scheduler.task_submitted(task, TaskHandle::new(task), InitialSchedMeta::fair());
    task
}

fn submit_kernel(scheduler: &mut Phase1Scheduler, raw: usize) -> TaskId {
    let task = TaskId(raw);
    scheduler.task_submitted(task, TaskHandle::new(task), InitialSchedMeta::kernel());
    task
}

fn submit_fair_affinity(scheduler: &mut Phase1Scheduler, raw: usize, affinity: u64) -> TaskId {
    let task = TaskId(raw);
    scheduler.task_submitted(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair().with_affinity(affinity),
    );
    task
}

fn pick_id_and_slice(
    scheduler: &mut Phase1Scheduler,
    hart: HartId,
) -> Option<(TaskId, SliceConfig)> {
    scheduler
        .pick_next(hart)
        .map(|(handle, slice)| (handle.id(), slice))
}

#[test]
fn submitted_fair_task_enters_new_queue_once() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 0);

    scheduler.task_runnable(task, WakeHint::Normal);
    scheduler.task_runnable(task, WakeHint::SignalDelivery);

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.kernel, 0);
    assert_eq!(depths.new, 1);
    assert_eq!(depths.preempted, 0);
    assert!(scheduler.is_queued(task));

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            },
        ))
    );
    assert!(!scheduler.is_queued(task));
    assert_eq!(
        scheduler.remaining_budget_ns(task),
        Some(Phase1Scheduler::NEW_QUEUE_SLICE_NS)
    );
    assert_eq!(scheduler.total_runtime_ns(task), Some(0));
    assert_eq!(pick_id_and_slice(&mut scheduler, HartId(0)), None);
}

#[test]
fn submitted_task_uses_first_allowed_affinity_hart() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 20, 0b0100);

    assert_eq!(scheduler.queue_depths(HartId(0)).new, 0);
    assert_eq!(scheduler.queue_depths(HartId(2)).new, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(2)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            },
        ))
    );
}

#[test]
fn submitted_task_can_start_in_preempted_queue() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(72);
    scheduler.task_submitted(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair().preempted_on_submit(),
    );

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 1);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn submitted_movable_fair_tasks_spread_across_allowed_harts() {
    let mut scheduler = Phase1Scheduler::new();
    let first = TaskId(23);
    let second = TaskId(24);
    let third = TaskId(25);
    for task in [first, second, third] {
        scheduler.task_submitted(
            task,
            TaskHandle::new(task),
            InitialSchedMeta::fair()
                .with_affinity(0b0111)
                .spread_on_submit(),
        );
    }

    assert_eq!(scheduler.queue_depths(HartId(0)).new, 1);
    assert_eq!(scheduler.queue_depths(HartId(1)).new, 1);
    assert_eq!(scheduler.queue_depths(HartId(2)).new, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(first)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(second)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(2)).map(|x| x.0),
        Some(third)
    );
}

#[test]
fn userspace_thread_meta_is_explicit_and_can_be_movable() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(26);
    scheduler.task_submitted(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair()
            .with_affinity(0b0011)
            .userspace_thread()
            .movable()
            .spread_on_submit(),
    );

    assert!(scheduler.is_userspace_thread(task));
    assert!(scheduler.can_migrate(task));
}

#[test]
fn blocked_task_wakes_on_last_allowed_hart_and_reports_remote_target() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 21, 0b0110);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(task, StopReason::Blocked, 10, HartId(1));

    let placement = scheduler
        .task_runnable_from(task, WakeHint::Normal, HartId(0))
        .expect("wake placement");

    assert_eq!(placement.target_hart, HartId(1));
    assert!(placement.wake_remote);
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
    assert_eq!(pick_id_and_slice(&mut scheduler, HartId(0)), None);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(task)
    );
}

#[test]
fn zero_affinity_normalizes_to_boot_hart() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 22, 0);

    assert_eq!(scheduler.queue_depths(HartId(0)).new, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
}

#[test]
fn yielded_fair_task_resets_budget_and_requeues_with_fresh_preempted_slice() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 1);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            },
        ))
    );

    scheduler.task_stopped(task, StopReason::Yielded, 250_000, HartId(0));

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.kernel, 0);
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 1);
    assert_eq!(scheduler.remaining_budget_ns(task), Some(0));
    assert_eq!(scheduler.total_runtime_ns(task), Some(250_000));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS,
            },
        ))
    );
}

#[test]
fn yielded_kernel_task_stays_cooperative() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_kernel(&mut scheduler, 2);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((task, SliceConfig::Cooperative))
    );

    scheduler.task_stopped(task, StopReason::Yielded, 123, HartId(0));

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.kernel, 1);
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 0);
    assert_eq!(scheduler.remaining_budget_ns(task), Some(0));
    assert_eq!(scheduler.total_runtime_ns(task), Some(123));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((task, SliceConfig::Cooperative))
    );
}

#[test]
fn blocked_task_preserves_remaining_budget_until_wake() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 3);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            },
        ))
    );

    scheduler.task_stopped(task, StopReason::Blocked, 300_000, HartId(0));

    let remaining = Phase1Scheduler::NEW_QUEUE_SLICE_NS - 300_000;
    assert_eq!(scheduler.remaining_budget_ns(task), Some(remaining));
    assert_eq!(scheduler.total_runtime_ns(task), Some(300_000));
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 0);
    assert_eq!(pick_id_and_slice(&mut scheduler, HartId(0)), None);

    scheduler.task_runnable(task, WakeHint::Normal);
    scheduler.task_runnable(task, WakeHint::PriorityBoost);

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: remaining
            }
        ))
    );
    assert_eq!(pick_id_and_slice(&mut scheduler, HartId(0)), None);
}

#[test]
fn blocked_userspace_thread_wakes_through_new_queue_even_with_remaining_budget() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(72);
    scheduler.task_submitted(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair().userspace_thread(),
    );
    let preempted = TaskId(73);
    scheduler.task_submitted(
        preempted,
        TaskHandle::new(preempted),
        InitialSchedMeta::fair().preempted_on_submit(),
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(task, StopReason::Blocked, 100_000, HartId(0));
    assert_eq!(
        scheduler.remaining_budget_ns(task),
        Some(Phase1Scheduler::NEW_QUEUE_SLICE_NS - 100_000)
    );

    scheduler.task_runnable(task, WakeHint::Normal);

    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::New,
        })
    );
    assert_eq!(scheduler.queue_depths(HartId(0)).new, 1);
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
}

#[test]
fn blocked_task_with_exhausted_budget_wakes_as_new() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 4);

    assert!(pick_id_and_slice(&mut scheduler, HartId(0)).is_some());
    scheduler.task_stopped(
        task,
        StopReason::Blocked,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );
    assert_eq!(scheduler.remaining_budget_ns(task), Some(0));

    scheduler.task_runnable(task, WakeHint::Normal);

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.new, 1);
    assert_eq!(depths.preempted, 0);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            },
        ))
    );
}

#[test]
fn userspace_trap_preserves_remaining_budget_at_front() {
    let mut scheduler = Phase1Scheduler::new();
    let first = submit_fair(&mut scheduler, 5);
    let second = submit_fair(&mut scheduler, 6);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(first)
    );
    scheduler.task_stopped(
        first,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(second)
    );
    scheduler.task_stopped(second, StopReason::UserspaceTrap, 200_000, HartId(0));

    let remaining = Phase1Scheduler::NEW_QUEUE_SLICE_NS - 200_000;
    assert_eq!(scheduler.remaining_budget_ns(second), Some(remaining));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            second,
            SliceConfig::Preemptive {
                slice_ns: remaining
            }
        ))
    );
}

#[test]
fn userspace_trap_without_remaining_budget_requeues_with_fresh_slice() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 7);

    assert!(pick_id_and_slice(&mut scheduler, HartId(0)).is_some());
    scheduler.task_stopped(
        task,
        StopReason::UserspaceTrap,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(scheduler.remaining_budget_ns(task), Some(0));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            task,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS,
            },
        ))
    );
}

#[test]
fn userspace_thread_trap_requeues_at_front_even_after_budget_exhaustion() {
    let mut scheduler = Phase1Scheduler::new();
    let userspace = TaskId(70);
    scheduler.task_submitted(
        userspace,
        TaskHandle::new(userspace),
        InitialSchedMeta::fair().userspace_thread(),
    );
    let peer = submit_fair(&mut scheduler, 71);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(userspace)
    );
    scheduler.task_stopped(
        userspace,
        StopReason::UserspaceTrap,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(userspace)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(peer)
    );
}

#[test]
fn slice_expired_resets_budget_and_moves_to_preempted_back() {
    let mut scheduler = Phase1Scheduler::new();
    let first = submit_fair(&mut scheduler, 8);
    let second = submit_fair(&mut scheduler, 9);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(first)
    );
    scheduler.task_stopped(
        first,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(scheduler.remaining_budget_ns(first), Some(0));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(second)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            first,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS,
            },
        ))
    );
}

#[test]
fn external_preemption_preserves_remaining_budget_at_back() {
    let mut scheduler = Phase1Scheduler::new();
    let first = submit_fair(&mut scheduler, 10);
    let second = submit_fair(&mut scheduler, 11);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(first)
    );
    scheduler.task_stopped(
        first,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(second)
    );
    scheduler.task_stopped(second, StopReason::PreemptedExternal, 100_000, HartId(0));

    let remaining = Phase1Scheduler::NEW_QUEUE_SLICE_NS - 100_000;
    assert_eq!(scheduler.remaining_budget_ns(second), Some(remaining));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            first,
            SliceConfig::Preemptive {
                slice_ns: Phase1Scheduler::PREEMPTED_QUEUE_SLICE_NS,
            }
        ))
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)),
        Some((
            second,
            SliceConfig::Preemptive {
                slice_ns: remaining
            }
        ))
    );
}

#[test]
fn duplicate_runnable_notifications_do_not_duplicate_preempted_entry() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 12);

    assert!(pick_id_and_slice(&mut scheduler, HartId(0)).is_some());
    scheduler.task_stopped(task, StopReason::Blocked, 1, HartId(0));

    scheduler.task_runnable(task, WakeHint::Normal);
    scheduler.task_runnable(task, WakeHint::SignalDelivery);
    scheduler.task_runnable(task, WakeHint::PriorityBoost);
    scheduler.task_runnable(task, WakeHint::None);

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    assert_eq!(pick_id_and_slice(&mut scheduler, HartId(0)), None);
}

#[test]
fn task_owner_tracks_submit_pick_block_wake_and_drop() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 30, 0b0100);

    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(2),
            queue: Phase1QueueKind::New,
        })
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(2)).map(|x| x.0),
        Some(task)
    );
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Polling { hart: HartId(2) })
    );
    assert!(!scheduler.is_queued(task));

    scheduler.task_stopped(task, StopReason::Blocked, 125_000, HartId(2));
    assert_eq!(scheduler.task_owner(task), Some(TaskRunOwner::Parked));
    assert!(!scheduler.is_queued(task));

    let placement = scheduler
        .task_runnable_from(task, WakeHint::Normal, HartId(0))
        .expect("blocked task should be placed");
    assert_eq!(placement.target_hart, HartId(2));
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(2),
            queue: Phase1QueueKind::Preempted,
        })
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(2)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(task, StopReason::Completed, 0, HartId(2));
    assert_eq!(scheduler.task_owner(task), Some(TaskRunOwner::Terminal));

    scheduler.task_dropped(task);
    assert_eq!(scheduler.task_owner(task), None);
}

#[test]
fn polling_task_can_requeue_once_when_woken_during_poll() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair(&mut scheduler, 31);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Polling { hart: HartId(0) })
    );

    let placement = scheduler
        .task_runnable_from(task, WakeHint::Normal, HartId(0))
        .expect("woken polling task should be requeued");
    assert_eq!(placement.target_hart, HartId(0));
    assert!(!placement.wake_remote);

    scheduler.task_runnable(task, WakeHint::PriorityBoost);
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 1);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn idle_hart_steals_preempted_task_when_affinity_allows() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 40, 0b0011);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(
        task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 1);
    assert_eq!(
        scheduler.try_steal(HartId(1), HartId(0)).map(|h| h.id()),
        Some(task)
    );
    assert_eq!(scheduler.stats().work_steals, 1);
    assert_eq!(scheduler.stats().rebalance_moves, 0);
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 0);
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(task)
    );
}

#[test]
fn recently_stolen_task_cannot_ping_pong_before_it_runs() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 45, 0b0011);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(
        task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(
        scheduler.try_steal(HartId(1), HartId(0)).map(|h| h.id()),
        Some(task)
    );
    assert_eq!(
        scheduler.try_steal(HartId(0), HartId(1)).map(|h| h.id()),
        None,
        "a freshly stolen task must not be immediately stolen back",
    );
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(task, StopReason::Yielded, 0, HartId(1));

    assert_eq!(
        scheduler.try_steal(HartId(0), HartId(1)).map(|h| h.id()),
        Some(task),
        "after the task has run once on the thief hart, it may migrate again",
    );
}

#[test]
fn idle_steal_prefers_busiest_preempted_victim() {
    let mut scheduler = Phase1Scheduler::new();
    let lightly_loaded = submit_fair_affinity(&mut scheduler, 46, 0b0010);
    let busy_first = submit_fair_affinity(&mut scheduler, 47, 0b0100);
    let busy_second = submit_fair_affinity(&mut scheduler, 48, 0b0100);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(lightly_loaded)
    );
    scheduler.task_stopped(
        lightly_loaded,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(1),
    );
    scheduler
        .set_affinity(lightly_loaded, 0b0011, HartId(1))
        .expect("widen light victim affinity");

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(2)).map(|x| x.0),
        Some(busy_first)
    );
    scheduler.task_stopped(
        busy_first,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(2),
    );
    scheduler
        .set_affinity(busy_first, 0b0101, HartId(2))
        .expect("widen first busy victim affinity");
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(2)).map(|x| x.0),
        Some(busy_second)
    );
    scheduler.task_stopped(
        busy_second,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(2),
    );
    scheduler
        .set_affinity(busy_second, 0b0101, HartId(2))
        .expect("widen second busy victim affinity");

    assert_eq!(
        scheduler
            .try_steal_from_any(HartId(0))
            .map(|handle| handle.id()),
        Some(busy_second),
        "idle steal should choose the hart with the deepest preempted queue",
    );
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
    assert_eq!(scheduler.queue_depths(HartId(2)).preempted, 1);
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 1);
}

#[test]
fn work_stealing_can_take_new_queue_but_respects_affinity() {
    let mut scheduler = Phase1Scheduler::new();
    let new_task = submit_fair_affinity(&mut scheduler, 41, 0b0011);
    let restricted_task = submit_fair_affinity(&mut scheduler, 42, 0b0001);

    assert_eq!(
        scheduler.try_steal(HartId(1), HartId(0)).map(|h| h.id()),
        Some(new_task)
    );
    assert_eq!(scheduler.queue_depths(HartId(0)).new, 1);
    assert_eq!(scheduler.queue_depths(HartId(1)).new, 1);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(new_task)
    );
    scheduler.task_stopped(
        new_task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(1),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(restricted_task)
    );
    scheduler.task_stopped(
        restricted_task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(scheduler.try_steal(HartId(1), HartId(0)), None);
    assert_eq!(
        scheduler.task_owner(new_task),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
    );
    assert_eq!(
        scheduler.task_owner(restricted_task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn pinned_fair_task_is_not_stolen() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(43);
    scheduler.task_submitted(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair().with_affinity(0b0011).pinned(),
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(
        task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert!(!scheduler.can_migrate(task));
    assert_eq!(scheduler.try_steal(HartId(1), HartId(0)), None);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn pinned_userspace_thread_is_not_stolen_until_declared_movable() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(44);
    scheduler.task_submitted(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair()
            .with_affinity(0b0011)
            .userspace_thread()
            .pinned(),
    );

    assert!(scheduler.is_userspace_thread(task));
    assert!(!scheduler.can_migrate(task));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(
        task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(scheduler.try_steal(HartId(1), HartId(0)), None);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn rebalance_waits_for_period_and_moves_one_preempted_task() {
    let mut scheduler = Phase1Scheduler::new();
    let first = submit_fair_affinity(&mut scheduler, 50, 0b0011);
    let second = submit_fair_affinity(&mut scheduler, 51, 0b0011);
    let third = submit_fair_affinity(&mut scheduler, 52, 0b0011);

    for task in [first, second, third] {
        assert_eq!(
            pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
            Some(task)
        );
        scheduler.task_stopped(
            task,
            StopReason::SliceExpired,
            Phase1Scheduler::NEW_QUEUE_SLICE_NS,
            HartId(0),
        );
    }

    assert_eq!(scheduler.rebalance_at(HartId(1), 1_000_000), None);
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 0);

    let stolen = scheduler
        .rebalance_at(HartId(1), Phase1Scheduler::BALANCE_PERIOD_NS)
        .expect("rebalance should steal one task")
        .id();
    assert_eq!(stolen, third);
    assert_eq!(scheduler.stats().work_steals, 1);
    assert_eq!(scheduler.stats().rebalance_moves, 1);
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 2);
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
    assert_eq!(
        scheduler.task_owner(stolen),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn rebalance_respects_minimum_imbalance() {
    let mut scheduler = Phase1Scheduler::new();
    let first = submit_fair_affinity(&mut scheduler, 53, 0b0011);
    let second = submit_fair_affinity(&mut scheduler, 54, 0b0011);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(first)
    );
    scheduler.task_stopped(
        first,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(second)
    );
    scheduler.task_stopped(
        second,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );
    assert_eq!(
        scheduler.try_steal(HartId(1), HartId(0)).map(|h| h.id()),
        Some(second)
    );

    assert_eq!(
        scheduler.rebalance_at(HartId(1), Phase1Scheduler::BALANCE_PERIOD_NS),
        None
    );
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 1);
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
}

#[test]
fn set_affinity_moves_queued_task_to_allowed_hart() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 60, 0b0011);

    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::New,
        })
    );

    let placement = scheduler
        .set_affinity(task, 0b0010, HartId(0))
        .expect("affinity update should succeed")
        .expect("queued task should move");

    assert_eq!(placement.target_hart, HartId(1));
    assert!(placement.wake_remote);
    assert_eq!(scheduler.queue_depths(HartId(0)).new, 0);
    assert_eq!(scheduler.queue_depths(HartId(1)).new, 1);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::New,
        })
    );
}

#[test]
fn set_affinity_on_polling_task_migrates_after_stop() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 61, 0b0011);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    assert_eq!(scheduler.set_affinity(task, 0b0010, HartId(0)), Ok(None));

    scheduler.task_stopped(
        task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 0);
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn set_affinity_rejects_unknown_and_terminal_tasks() {
    let mut scheduler = Phase1Scheduler::new();
    assert_eq!(
        scheduler.set_affinity(TaskId(999), 0b1, HartId(0)),
        Err(SchedulerAffinityError::UnknownTask)
    );

    let task = submit_fair(&mut scheduler, 62);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    scheduler.task_stopped(task, StopReason::Completed, 0, HartId(0));
    assert_eq!(
        scheduler.set_affinity(task, 0b1, HartId(0)),
        Err(SchedulerAffinityError::TerminalTask)
    );
}

#[test]
fn priority_donation_picks_boosted_fair_task_before_ordinary_fair_task() {
    let mut scheduler = Phase1Scheduler::new();
    let ordinary = submit_fair(&mut scheduler, 70);
    let owner = submit_fair(&mut scheduler, 71);

    let token = scheduler
        .donate_priority(owner, ordinary, 50)
        .expect("donation should install");

    assert_eq!(scheduler.effective_rt_priority(owner), Some(50));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(owner)
    );

    scheduler
        .drop_priority_donation(token)
        .expect("donation should revoke");
    assert_eq!(scheduler.effective_rt_priority(owner), Some(0));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(ordinary)
    );
}

#[test]
fn priority_donation_revoke_before_pick_restores_fifo_order() {
    let mut scheduler = Phase1Scheduler::new();
    let ordinary = submit_fair(&mut scheduler, 72);
    let owner = submit_fair(&mut scheduler, 73);

    let token = scheduler
        .donate_priority(owner, ordinary, 50)
        .expect("donation should install");
    scheduler
        .drop_priority_donation(token)
        .expect("donation should revoke");

    assert_eq!(scheduler.effective_rt_priority(owner), Some(0));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(ordinary)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(owner)
    );
}

#[test]
fn priority_donation_recomputes_to_next_highest_after_revoke() {
    let mut scheduler = Phase1Scheduler::new();
    let low_donor = submit_fair(&mut scheduler, 74);
    let high_donor = submit_fair(&mut scheduler, 75);
    let owner = submit_fair(&mut scheduler, 76);

    let low = scheduler
        .donate_priority(owner, low_donor, 20)
        .expect("low donation should install");
    let high = scheduler
        .donate_priority(owner, high_donor, 70)
        .expect("high donation should install");

    assert_eq!(scheduler.effective_rt_priority(owner), Some(70));
    scheduler
        .drop_priority_donation(high)
        .expect("high donation should revoke");
    assert_eq!(scheduler.effective_rt_priority(owner), Some(20));
    scheduler
        .drop_priority_donation(low)
        .expect("low donation should revoke");
    assert_eq!(scheduler.effective_rt_priority(owner), Some(0));
}

#[test]
fn pi_waiters_keep_only_top_waiter_per_owned_lock() {
    let mut scheduler = Phase1Scheduler::new();
    let owner = submit_fair(&mut scheduler, 79);
    let low_waiter = submit_fair(&mut scheduler, 80);
    let high_waiter = submit_fair(&mut scheduler, 81);
    let lock_a = PiLockToken::new(0xa, 0x1000);
    let lock_b = PiLockToken::new(0xb, 0x2000);

    scheduler
        .upsert_pi_waiter(owner, lock_a, low_waiter, PriorityKey::rt(20, 1))
        .expect("lock A top waiter installs");
    scheduler
        .upsert_pi_waiter(owner, lock_b, high_waiter, PriorityKey::rt(70, 2))
        .expect("lock B top waiter installs");
    scheduler
        .upsert_pi_waiter(owner, lock_a, high_waiter, PriorityKey::rt(50, 3))
        .expect("lock A replacement stays below lock B");

    assert_eq!(scheduler.effective_rt_priority(owner), Some(70));
    assert_eq!(
        scheduler.effective_priority_key(owner),
        Some(PriorityKey::rt(70, 2))
    );
}

#[test]
fn pi_waiter_removal_deboosts_to_next_lock_then_base() {
    let mut scheduler = Phase1Scheduler::new();
    let owner = submit_fair(&mut scheduler, 82);
    let low_waiter = submit_fair(&mut scheduler, 83);
    let high_waiter = submit_fair(&mut scheduler, 84);
    let lock_a = PiLockToken::new(0xa, 0x3000);
    let lock_b = PiLockToken::new(0xb, 0x4000);

    scheduler
        .upsert_pi_waiter(owner, lock_a, low_waiter, PriorityKey::rt(30, 1))
        .expect("lower waiter installs");
    scheduler
        .upsert_pi_waiter(owner, lock_b, high_waiter, PriorityKey::rt(80, 2))
        .expect("higher waiter installs");

    assert_eq!(scheduler.effective_rt_priority(owner), Some(80));
    scheduler
        .remove_pi_waiter(owner, lock_b)
        .expect("highest waiter removed");
    assert_eq!(scheduler.effective_rt_priority(owner), Some(30));
    scheduler
        .remove_pi_waiter(owner, lock_a)
        .expect("last waiter removed");
    assert_eq!(scheduler.effective_rt_priority(owner), Some(0));
}

#[test]
fn priority_donation_rejects_unknown_terminal_and_zero_priority() {
    let mut scheduler = Phase1Scheduler::new();
    let owner = submit_fair(&mut scheduler, 77);
    let donor = submit_fair(&mut scheduler, 78);

    assert_eq!(
        scheduler.donate_priority(TaskId(999), donor, 50),
        Err(PriorityBoostError::UnknownTask)
    );
    assert_eq!(
        scheduler.donate_priority(owner, donor, 0),
        Err(PriorityBoostError::InvalidPriority)
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(owner)
    );
    scheduler.task_stopped(owner, StopReason::Completed, 0, HartId(0));
    assert_eq!(
        scheduler.donate_priority(owner, donor, 50),
        Err(PriorityBoostError::TerminalTask)
    );
}
