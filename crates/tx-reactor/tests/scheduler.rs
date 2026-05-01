use tx_reactor::{
    HartId, InitialSchedMeta, Phase1Scheduler, SliceConfig, StopReason, TaskHandle, TaskId,
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
    assert_eq!(depths.new, 0);
    assert_eq!(depths.preempted, 1);
    assert_eq!(
        pick_id_and_slice(&mut scheduler, HartId(0)).map(|x| x.0),
        Some(task)
    );
    assert_eq!(pick_id_and_slice(&mut scheduler, HartId(0)), None);
}
