use core::future::pending;

use tx_reactor::{
    task::{TaskLifecycleError, TaskTable},
    Reactor, RunStats, TaskId, TaskStatus,
};

#[test]
fn task_slots_reuse_with_new_generation() {
    let mut tasks = TaskTable::new();

    let first = tasks.submit(pending::<()>());
    assert_eq!(first.id(), TaskId(0));
    assert_eq!(tasks.complete_task(first), Ok(()));
    assert_eq!(tasks.drain_completed().len(), 1);

    let second = tasks.submit(pending::<()>());

    assert_eq!(second.id(), first.id());
    assert_ne!(second.generation(), first.generation());
    assert_eq!(tasks.status(first), None);
    assert_eq!(tasks.status(second), Some(TaskStatus::Runnable));
}

#[test]
fn cancel_task_marks_and_drain_cancelled_consumes_entry() {
    let mut tasks = TaskTable::new();
    let task = tasks.submit(pending::<()>());

    assert_eq!(tasks.cancel_task(task), Ok(()));
    assert_eq!(tasks.status(task), Some(TaskStatus::Cancelled));
    assert_eq!(
        tasks.cancel_task(task),
        Err(TaskLifecycleError::AlreadyTerminal(TaskStatus::Cancelled))
    );
    assert!(tasks.drain_completed().is_empty());

    let drained = tasks.drain_cancelled();

    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].handle, task);
    assert_eq!(drained[0].status, TaskStatus::Cancelled);
    assert_eq!(tasks.status(task), None);
    assert!(tasks.drain_cancelled().is_empty());
}

#[test]
fn stale_generation_cannot_cancel_reused_slot() {
    let mut tasks = TaskTable::new();

    let stale = tasks.submit(pending::<()>());
    assert_eq!(tasks.cancel_task(stale), Ok(()));
    assert_eq!(tasks.drain_cancelled().len(), 1);

    let current = tasks.submit(pending::<()>());
    assert_eq!(current.id(), stale.id());
    assert_ne!(current.generation(), stale.generation());

    assert_eq!(
        tasks.cancel_task(stale),
        Err(TaskLifecycleError::StaleHandle)
    );
    assert_eq!(tasks.status(stale), None);
    assert_eq!(tasks.status(current), Some(TaskStatus::Runnable));
}

#[test]
fn stale_waker_does_not_wake_reused_slot() {
    let mut tasks = TaskTable::new();

    let first = tasks.submit(pending::<()>());
    let stale_waker = tasks.waker(first).expect("first task waker");
    assert_eq!(tasks.complete_task(first), Ok(()));
    assert_eq!(tasks.drain_completed().len(), 1);

    let second = tasks.submit(pending::<()>());
    assert_eq!(second.id(), first.id());
    assert_ne!(second.generation(), first.generation());
    assert_eq!(tasks.park_task(second), Ok(()));

    stale_waker.wake_by_ref();

    assert!(tasks.drain_wakes().is_empty());
    assert_eq!(tasks.status(second), Some(TaskStatus::Parked));
}

#[test]
fn repeated_wakes_coalesce_before_drain() {
    let mut tasks = TaskTable::new();
    let task = tasks.submit(pending::<()>());
    let waker = tasks.waker(task).expect("task waker");

    assert_eq!(tasks.park_task(task), Ok(()));
    waker.wake_by_ref();
    waker.wake_by_ref();
    waker.wake_by_ref();

    assert_eq!(tasks.drain_wakes(), vec![task]);
    assert!(tasks.drain_wakes().is_empty());
    assert_eq!(tasks.status(task), Some(TaskStatus::Runnable));
}

#[test]
fn reactor_cancel_task_uses_generation_checked_key_and_drain_reuses_slot() {
    let mut reactor = Reactor::new();
    let first = reactor.submit_task(pending::<()>());

    assert_eq!(reactor.cancel_task(first), Ok(()));
    assert_eq!(reactor.task_key_status(first), Some(TaskStatus::Cancelled));
    assert_eq!(
        reactor.cancel_task(first),
        Err(TaskLifecycleError::AlreadyTerminal(TaskStatus::Cancelled))
    );

    let drained = reactor.drain_cancelled();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].handle, first);
    assert_eq!(drained[0].status, TaskStatus::Cancelled);
    assert_eq!(reactor.task_key_status(first), None);

    let second = reactor.submit_task(pending::<()>());
    assert_eq!(second.id(), first.id());
    assert_ne!(second.generation(), first.generation());
    assert_eq!(
        reactor.cancel_task(first),
        Err(TaskLifecycleError::StaleHandle)
    );
    assert_eq!(reactor.task_key_status(second), Some(TaskStatus::Runnable));
}

#[test]
fn reactor_drain_completed_releases_slot_for_new_generation() {
    let mut reactor = Reactor::new();
    let first = reactor.submit_task(async {});

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(reactor.task_key_status(first), Some(TaskStatus::Completed));

    let drained = reactor.drain_completed();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].handle, first);
    assert_eq!(drained[0].status, TaskStatus::Completed);
    assert_eq!(reactor.task_key_status(first), None);

    let second = reactor.submit_task(pending::<()>());
    assert_eq!(second.id(), first.id());
    assert_ne!(second.generation(), first.generation());
    assert_eq!(reactor.task_key_status(second), Some(TaskStatus::Runnable));
}
