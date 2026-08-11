use tx_reactor::{
    HartId, InitialSchedMeta, Phase1QueueKind, Phase1Scheduler, QueuedTaskReport,
    SchedulerAffinityError, SliceConfig, StopReason, TaskHandle, TaskId, TaskRunOwner, WakeHint,
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
    assert_eq!(depths.boosted, 0);
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
    assert_eq!(depths.boosted, 0);
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
fn submitted_task_reports_queue_and_turn_at_publish_point() {
    let mut scheduler = Phase1Scheduler::new();
    let task = TaskId(73);

    let report = scheduler.task_submitted_report(
        task,
        TaskHandle::new(task),
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );

    assert_eq!(
        report,
        QueuedTaskReport {
            task,
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
            queued_turn: 0,
        }
    );
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Preempted,
        })
    );
}

#[test]
fn signal_delivery_hint_prioritizes_already_queued_userspace_task() {
    let mut scheduler = Phase1Scheduler::new();
    let aux = TaskId(74);
    let worker = TaskId(75);

    scheduler.task_submitted(
        aux,
        TaskHandle::new(aux),
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );
    scheduler.task_submitted(
        worker,
        TaskHandle::new(worker),
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );

    scheduler.task_runnable(worker, WakeHint::SignalDelivery);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|(task, _)| task),
        Some(worker),
        "thread-directed signal delivery should let an already queued worker run before older preempted peers"
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
fn spread_on_submit_rotates_when_queues_are_consumed_between_submits() {
    let mut scheduler = Phase1Scheduler::new();

    for (index, hart) in [HartId(0), HartId(1), HartId(2), HartId(3)]
        .into_iter()
        .enumerate()
    {
        let task = TaskId(230 + index);
        scheduler.task_submitted(
            task,
            TaskHandle::new(task),
            InitialSchedMeta::fair()
                .with_affinity(0b1111)
                .pinned()
                .spread_on_submit()
                .userspace_thread(),
        );

        assert_eq!(scheduler.queue_depths(hart).new, 1);
        assert_eq!(scheduler.task_affinity(task), Some(1u64 << hart.0));
        assert_eq!(
            pick_id_and_slice(&mut scheduler, hart).map(|x| x.0),
            Some(task)
        );
    }
}

#[test]
fn pinned_spread_on_submit_uses_initial_spread_without_enabling_steal() {
    let mut scheduler = Phase1Scheduler::new();
    let first = TaskId(240);
    let second = TaskId(241);

    for task in [first, second] {
        scheduler.task_submitted(
            task,
            TaskHandle::new(task),
            InitialSchedMeta::fair()
                .with_affinity(0b0011)
                .pinned()
                .spread_on_submit()
                .userspace_thread(),
        );
        assert!(!scheduler.can_migrate(task));
    }

    assert_eq!(scheduler.task_affinity(first), Some(0b0001));
    assert_eq!(scheduler.task_affinity(second), Some(0b0010));
    assert_eq!(scheduler.queue_depths(HartId(0)).new, 1);
    assert_eq!(scheduler.queue_depths(HartId(1)).new, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(first)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(second)
    );

    scheduler.task_stopped(
        second,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(1),
    );

    assert_eq!(scheduler.try_steal(HartId(0), HartId(1)), None);
    assert_eq!(
        scheduler.task_owner(second),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
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
    assert_eq!(depths.boosted, 0);
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
    assert_eq!(depths.boosted, 0);
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
    assert_eq!(depths.boosted, 0);
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
    assert_eq!(depths.boosted, 0);
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
fn userspace_trap_preserves_remaining_budget_behind_new_tasks() {
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
    let new_peer = submit_fair(&mut scheduler, 60);
    assert_eq!(scheduler.remaining_budget_ns(second), Some(remaining));
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(new_peer)
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
fn userspace_thread_trap_with_budget_yields_to_new_peers() {
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
    let new_peer = submit_fair(&mut scheduler, 72);
    scheduler.task_stopped(userspace, StopReason::UserspaceTrap, 200_000, HartId(0));

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(peer)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(new_peer)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(userspace)
    );
}

#[test]
fn userspace_thread_trap_with_budget_stays_behind_preempted_peer() {
    let mut scheduler = Phase1Scheduler::new();
    let current = TaskId(80);
    scheduler.task_submitted(
        current,
        TaskHandle::new(current),
        InitialSchedMeta::fair().userspace_thread(),
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(current)
    );

    let peer = TaskId(81);
    scheduler.task_submitted(
        peer,
        TaskHandle::new(peer),
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );
    scheduler.task_stopped(current, StopReason::UserspaceTrap, 200_000, HartId(0));

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(peer)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(current)
    );
}

#[test]
fn userspace_thread_normal_wake_with_budget_queues_behind_preempted_peers() {
    let mut scheduler = Phase1Scheduler::new();
    let userspace = TaskId(73);
    scheduler.task_submitted(
        userspace,
        TaskHandle::new(userspace),
        InitialSchedMeta::fair().userspace_thread(),
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(userspace)
    );
    scheduler.task_stopped(userspace, StopReason::Blocked, 200_000, HartId(0));

    let child = TaskId(74);
    scheduler.task_submitted(
        child,
        TaskHandle::new(child),
        InitialSchedMeta::fair()
            .userspace_thread()
            .preempted_on_submit(),
    );
    scheduler.task_runnable(userspace, WakeHint::Normal);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(child)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(userspace)
    );
}

#[test]
fn userspace_thread_normal_wake_beats_aged_preempted_storm() {
    let mut scheduler = Phase1Scheduler::new();
    let sleeper = TaskId(82);
    scheduler.task_submitted(
        sleeper,
        TaskHandle::new(sleeper),
        InitialSchedMeta::fair().userspace_thread(),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(sleeper)
    );
    scheduler.task_stopped(sleeper, StopReason::Blocked, 200_000, HartId(0));

    let workers = [TaskId(83), TaskId(84), TaskId(85)];
    for worker in workers {
        scheduler.task_submitted(
            worker,
            TaskHandle::new(worker),
            InitialSchedMeta::fair()
                .userspace_thread()
                .preempted_on_submit(),
        );
    }
    for raw in 86..94 {
        let kernel = submit_kernel(&mut scheduler, raw);
        assert_eq!(
            pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
            Some(kernel)
        );
    }

    scheduler.task_runnable(sleeper, WakeHint::Normal);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(workers[0])
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(sleeper)
    );
}

#[test]
fn userspace_thread_normal_wake_beats_unaged_preempted_storm_after_one_peer() {
    let mut scheduler = Phase1Scheduler::new();
    let sleeper = TaskId(94);
    scheduler.task_submitted(
        sleeper,
        TaskHandle::new(sleeper),
        InitialSchedMeta::fair().userspace_thread(),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(sleeper)
    );
    scheduler.task_stopped(sleeper, StopReason::Blocked, 200_000, HartId(0));

    let workers = [TaskId(95), TaskId(96), TaskId(97)];
    for worker in workers {
        scheduler.task_submitted(
            worker,
            TaskHandle::new(worker),
            InitialSchedMeta::fair()
                .userspace_thread()
                .preempted_on_submit(),
        );
    }

    scheduler.task_runnable(sleeper, WakeHint::Normal);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(workers[0])
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(sleeper)
    );
}

#[test]
fn wake_handoff_fronts_userspace_waiter_without_boosting() {
    let mut scheduler = Phase1Scheduler::new();
    let waiter = TaskId(75);
    scheduler.task_submitted(
        waiter,
        TaskHandle::new(waiter),
        InitialSchedMeta::fair().userspace_thread(),
    );

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(waiter)
    );
    scheduler.task_stopped(waiter, StopReason::Blocked, 200_000, HartId(0));

    let current_parent = TaskId(76);
    scheduler.task_submitted(
        current_parent,
        TaskHandle::new(current_parent),
        InitialSchedMeta::fair().userspace_thread(),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(current_parent)
    );
    scheduler.task_stopped(
        current_parent,
        StopReason::UserspaceTrap,
        200_000,
        HartId(0),
    );

    scheduler.task_runnable(waiter, WakeHint::WakeHandoff);

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.boosted, 0);
    assert_eq!(depths.preempted, 2);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(waiter)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(current_parent)
    );
}

#[test]
fn lifecycle_priority_and_signal_wakes_enter_boosted_queue() {
    let mut scheduler = Phase1Scheduler::new();
    let lifecycle = TaskId(77);
    let priority = TaskId(78);
    let signal = TaskId(79);

    for task in [lifecycle, priority, signal] {
        scheduler.task_submitted(
            task,
            TaskHandle::new(task),
            InitialSchedMeta::fair().userspace_thread(),
        );
        assert_eq!(
            pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
            Some(task)
        );
        scheduler.task_stopped(task, StopReason::Blocked, 100_000, HartId(0));
    }

    scheduler.task_runnable(lifecycle, WakeHint::LifecycleWake);
    scheduler.task_runnable(priority, WakeHint::PriorityBoost);
    scheduler.task_runnable(signal, WakeHint::SignalDelivery);

    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.boosted, 3);
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 0);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(lifecycle)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(priority)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(signal)
    );
}

#[test]
fn lifecycle_wake_places_joiner_on_waker_hart_when_affine() {
    let mut scheduler = Phase1Scheduler::new();
    let joiner = TaskId(801);

    scheduler.task_submitted(
        joiner,
        TaskHandle::new(joiner),
        InitialSchedMeta::fair()
            .userspace_thread()
            .movable()
            .with_affinity(0b11),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(joiner)
    );
    scheduler.task_stopped(joiner, StopReason::Blocked, 100_000, HartId(0));

    let placement = scheduler
        .task_runnable_from(joiner, WakeHint::LifecycleWake, HartId(1))
        .expect("lifecycle wake should place parked joiner");

    assert_eq!(
        placement.target_hart,
        HartId(1),
        "lifecycle wake should resume on the exiting thread's hart"
    );
    assert!(
        !placement.wake_remote,
        "sync-affine lifecycle wake should use the same-hart handoff path"
    );
    let depths = scheduler.queue_depths(HartId(1));
    assert_eq!(depths.boosted, 1);
    assert_eq!(
        scheduler.task_owner(joiner),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Boosted,
        })
    );
}

#[test]
fn lifecycle_wake_respects_pinned_joiner_affinity() {
    let mut scheduler = Phase1Scheduler::new();
    let joiner = TaskId(802);

    scheduler.task_submitted(
        joiner,
        TaskHandle::new(joiner),
        InitialSchedMeta::fair()
            .userspace_thread()
            .pinned()
            .with_affinity(0b01),
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(joiner)
    );
    scheduler.task_stopped(joiner, StopReason::Blocked, 100_000, HartId(0));

    let placement = scheduler
        .task_runnable_from(joiner, WakeHint::LifecycleWake, HartId(1))
        .expect("lifecycle wake should place pinned joiner");

    assert_eq!(placement.target_hart, HartId(0));
    assert!(placement.wake_remote);
    assert_eq!(
        scheduler.task_owner(joiner),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
            queue: Phase1QueueKind::Boosted,
        })
    );
}

#[test]
fn aged_preempted_task_beats_new_after_eight_scheduler_turns() {
    let mut scheduler = Phase1Scheduler::new();
    let aged = submit_fair(&mut scheduler, 80);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(aged)
    );
    scheduler.task_stopped(
        aged,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    for raw in 81..89 {
        let kernel = submit_kernel(&mut scheduler, raw);
        assert_eq!(
            pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
            Some(kernel)
        );
    }

    let new_peer = submit_fair(&mut scheduler, 89);
    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.preempted, 1);
    assert_eq!(depths.new, 1);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(aged)
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(new_peer)
    );
}

#[test]
fn aged_preempted_tasks_do_not_starve_new_queue() {
    let mut scheduler = Phase1Scheduler::new();
    let aged_tasks = [TaskId(90), TaskId(91), TaskId(92)];
    for task in aged_tasks {
        scheduler.task_submitted(
            task,
            TaskHandle::new(task),
            InitialSchedMeta::fair()
                .userspace_thread()
                .preempted_on_submit(),
        );
    }

    for raw in 93..101 {
        let kernel = submit_kernel(&mut scheduler, raw);
        assert_eq!(
            pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
            Some(kernel)
        );
    }

    let new_peer = submit_fair(&mut scheduler, 101);
    let depths = scheduler.queue_depths(HartId(0));
    assert_eq!(depths.preempted, aged_tasks.len());
    assert_eq!(depths.new, 1);

    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(aged_tasks[0])
    );
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(new_peer)
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
fn external_preemption_preserves_remaining_budget_at_front() {
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
    assert_eq!(depths.boosted, 0);
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
fn timer_style_wake_after_steal_targets_last_owner_hart() {
    let mut scheduler = Phase1Scheduler::new();
    let task = submit_fair_affinity(&mut scheduler, 41, 0b0011);

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
        pick_id_and_slice(&mut scheduler, HartId(1)).map(|x| x.0),
        Some(task)
    );

    scheduler.task_stopped(task, StopReason::Blocked, 0, HartId(1));
    assert_eq!(scheduler.task_owner(task), Some(TaskRunOwner::Parked));

    let placement = scheduler
        .task_runnable_from(task, WakeHint::Normal, HartId(0))
        .expect("timer-style wake should make the parked task runnable");
    assert_eq!(
        placement.target_hart,
        HartId(1),
        "wake must route to the post-steal owner hart, not the firing hart",
    );
    assert!(
        placement.wake_remote,
        "a timer firing on hart0 should send a remote reschedule to hart1",
    );
    assert_eq!(
        scheduler.task_owner(task),
        Some(TaskRunOwner::Queued {
            hart: HartId(1),
            queue: Phase1QueueKind::Preempted,
        })
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
        Some(busy_first),
        "idle steal should choose the deepest victim hart and take its preempted front",
    );
    assert_eq!(scheduler.queue_depths(HartId(1)).preempted, 1);
    assert_eq!(scheduler.queue_depths(HartId(2)).preempted, 1);
    assert_eq!(scheduler.queue_depths(HartId(0)).preempted, 1);
}

#[test]
fn work_stealing_only_takes_preempted_front_and_respects_affinity() {
    let mut scheduler = Phase1Scheduler::new();
    let restricted_task = submit_fair_affinity(&mut scheduler, 42, 0b0001);
    let allowed_task = submit_fair_affinity(&mut scheduler, 41, 0b0011);

    assert_eq!(
        scheduler.try_steal(HartId(1), HartId(0)).map(|h| h.id()),
        None,
        "direct steal must not take tasks from the New queue",
    );
    assert_eq!(scheduler.queue_depths(HartId(0)).new, 2);
    assert_eq!(scheduler.queue_depths(HartId(1)).new, 0);

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
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(allowed_task)
    );
    scheduler.task_stopped(
        allowed_task,
        StopReason::SliceExpired,
        Phase1Scheduler::NEW_QUEUE_SLICE_NS,
        HartId(0),
    );

    assert_eq!(
        scheduler.try_steal(HartId(1), HartId(0)),
        None,
        "direct steal must not bypass an ineligible preempted front task",
    );
    assert_eq!(
        scheduler.task_owner(allowed_task),
        Some(TaskRunOwner::Queued {
            hart: HartId(0),
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
    assert_eq!(stolen, first);
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
        Some(first)
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
