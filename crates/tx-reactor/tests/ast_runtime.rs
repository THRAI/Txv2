use tx_reactor::{
    ast::{AstMarker, AstQueueEffect},
    Reactor, TaskLifecycleError, TaskStatus,
};

#[test]
fn reactor_ast_api_preserves_task_local_coalesced_batches() {
    let mut reactor = Reactor::new();
    let first = reactor.submit_task(std::future::pending::<()>());
    let second = reactor.submit_task(std::future::pending::<()>());

    assert_eq!(
        reactor.queue_ast_marker(first, AstMarker::InterruptCheck),
        Ok(AstQueueEffect::Queued)
    );
    assert_eq!(
        reactor.queue_ast_marker(first, AstMarker::FaultInjection),
        Ok(AstQueueEffect::Queued)
    );
    assert_eq!(
        reactor.queue_ast_marker(first, AstMarker::InterruptCheck),
        Ok(AstQueueEffect::Coalesced)
    );
    assert_eq!(
        reactor.queue_ast_marker(second, AstMarker::Local(9)),
        Ok(AstQueueEffect::Queued)
    );

    let first_batch = reactor.consume_ast_markers(first).unwrap();
    assert_eq!(
        first_batch.as_slice(),
        &[AstMarker::InterruptCheck, AstMarker::FaultInjection]
    );

    let second_batch = reactor.consume_ast_markers(second).unwrap();
    assert_eq!(second_batch.as_slice(), &[AstMarker::Local(9)]);
}

#[test]
fn poll_boundary_consumes_pending_ast_before_future_poll() {
    let mut reactor = Reactor::new();
    let task = reactor.submit_task(std::future::pending::<()>());

    assert_eq!(
        reactor.queue_ast_marker(task, AstMarker::PreemptCheck),
        Ok(AstQueueEffect::Queued)
    );
    assert_eq!(
        reactor.queue_ast_marker(task, AstMarker::Drain),
        Ok(AstQueueEffect::Queued)
    );
    assert_eq!(
        reactor.queue_ast_marker(task, AstMarker::PreemptCheck),
        Ok(AstQueueEffect::Coalesced)
    );

    let stats = reactor.run_until_idle();

    assert_eq!(stats.polled, 1);
    assert_eq!(reactor.task_key_status(task), Some(TaskStatus::Parked));
    assert_eq!(
        reactor.last_consumed_ast_batch(task).unwrap().as_slice(),
        &[AstMarker::PreemptCheck, AstMarker::Drain]
    );
    assert!(reactor.consume_ast_markers(task).unwrap().is_empty());
}

#[test]
fn ast_apis_reject_terminal_and_stale_task_keys() {
    let mut reactor = Reactor::new();
    let completed = reactor.submit_task(async {});

    let stats = reactor.run_until_idle();
    assert_eq!(stats.completed, 1);
    assert_eq!(
        reactor.queue_ast_marker(completed, AstMarker::Drain),
        Err(TaskLifecycleError::AlreadyTerminal(TaskStatus::Completed))
    );
    assert_eq!(
        reactor.consume_ast_markers(completed),
        Err(TaskLifecycleError::AlreadyTerminal(TaskStatus::Completed))
    );

    let drained = reactor.drain_completed();
    assert_eq!(drained.len(), 1);
    assert_eq!(
        reactor.queue_ast_marker(completed, AstMarker::Drain),
        Err(TaskLifecycleError::StaleHandle)
    );
    assert_eq!(
        reactor.consume_ast_markers(completed),
        Err(TaskLifecycleError::StaleHandle)
    );
    assert_eq!(
        reactor.last_consumed_ast_batch(completed),
        Err(TaskLifecycleError::StaleHandle)
    );
}
