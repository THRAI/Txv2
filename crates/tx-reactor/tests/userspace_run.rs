use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::{sync::Arc, task::Wake};

use tx_reactor::{
    ast::{AstMarker, AstQueueEffect, AstSlot},
    userspace::{
        FatalTrapInfo, SyscallRequest, UserspaceEntryAction, UserspaceEntryDecision,
        UserspaceEntryTaskError, UserspaceRunError, UserspaceRunPhase, UserspaceRunSlot,
        UserspaceTrapInfo,
    },
    Reactor, TaskLifecycleError, TaskStatus,
};

struct CountWake {
    wakes: Arc<AtomicUsize>,
}

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker(wakes: Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(CountWake { wakes }))
}

fn syscall_trap(nr: u64) -> UserspaceTrapInfo {
    UserspaceTrapInfo::Syscall(SyscallRequest::new(nr, [1, 2, 3, 4, 5, 6]))
}

fn fatal_trap(cause: u64) -> UserspaceTrapInfo {
    UserspaceTrapInfo::Fatal(FatalTrapInfo::new(cause, 0xfeed))
}

#[test]
fn userspace_run_wait_stays_pending_until_interesting_trap() {
    let slot = UserspaceRunSlot::new();
    let mut wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    assert_eq!(
        slot.status().expect("active request").phase,
        UserspaceRunPhase::Pending
    );
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);

    let dispatch = slot.dispatch(request).expect("dispatch userspace");
    assert_eq!(dispatch.phase, UserspaceRunPhase::Running);
    assert_eq!(dispatch.dispatches, 1);
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);

    let trap = syscall_trap(64);
    let resolved = slot
        .complete_interesting_trap(request, trap)
        .expect("complete userspace wait");
    assert_eq!(resolved.phase, UserspaceRunPhase::Resolved);
    assert_eq!(resolved.trap, Some(trap));
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(trap));
    assert_eq!(slot.status(), None);
}

#[test]
fn timer_preemption_resolves_and_wakes_waiter() {
    let slot = UserspaceRunSlot::new();
    let mut wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);
    assert_eq!(
        slot.dispatch(request).expect("dispatch userspace").phase,
        UserspaceRunPhase::Running
    );

    let preempted = slot
        .record_timer_preemption(request)
        .expect("record timer preemption");
    assert_eq!(preempted.phase, UserspaceRunPhase::Resolved);
    assert_eq!(preempted.dispatches, 1);
    assert_eq!(preempted.preemptions, 1);
    assert_eq!(preempted.trap, Some(UserspaceTrapInfo::TimerPreempt));
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        Pin::new(&mut wait).poll(&mut cx),
        Poll::Ready(UserspaceTrapInfo::TimerPreempt)
    );
    assert_eq!(slot.status(), None);
}

#[test]
fn late_interesting_trap_overrides_timer_preemption() {
    let slot = UserspaceRunSlot::new();
    let mut wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);
    slot.dispatch(request).expect("dispatch userspace");
    slot.record_timer_preemption(request)
        .expect("record timer preemption");

    let trap = fatal_trap(13);
    let resolved = slot
        .complete_interesting_trap(request, trap)
        .expect("interesting trap overrides soft timer preemption");
    assert_eq!(resolved.phase, UserspaceRunPhase::Resolved);
    assert_eq!(resolved.trap, Some(trap));
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(trap));
}

#[test]
fn double_completion_is_rejected() {
    let slot = UserspaceRunSlot::new();
    let wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();

    slot.complete_interesting_trap(request, syscall_trap(1))
        .expect("first completion resolves");

    assert_eq!(
        slot.complete_interesting_trap(request, syscall_trap(2)),
        Err(UserspaceRunError::AlreadyResolved(request))
    );
}

#[test]
fn stale_completion_is_rejected_against_current_request() {
    let slot = UserspaceRunSlot::new();
    let first = slot.start_request().expect("start first request");
    let first_request = first.request();
    drop(first);

    let second = slot.start_request().expect("start second request");
    let second_request = second.request();
    assert_ne!(first_request, second_request);

    assert_eq!(
        slot.complete_interesting_trap(first_request, syscall_trap(1)),
        Err(UserspaceRunError::StaleRequest {
            attempted: first_request,
            active: second_request,
        })
    );
    assert_eq!(
        slot.status().expect("second request still active").request,
        second_request
    );
}

#[test]
fn entry_checkpoint_drains_ast_before_dispatching_userspace() {
    let slot = UserspaceRunSlot::new();
    let wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let mut ast = AstSlot::new();

    assert_eq!(ast.queue(AstMarker::InterruptCheck), AstQueueEffect::Queued);
    assert_eq!(ast.queue(AstMarker::Drain), AstQueueEffect::Queued);
    assert_eq!(
        ast.queue(AstMarker::InterruptCheck),
        AstQueueEffect::Coalesced
    );

    let outcome = slot
        .checkpoint_userspace_entry(request, &mut ast, |checkpoint| {
            assert_eq!(checkpoint.request, request);
            assert_eq!(
                checkpoint.ast.as_slice(),
                &[AstMarker::InterruptCheck, AstMarker::Drain]
            );
            assert_eq!(
                slot.status().expect("not dispatched yet").phase,
                UserspaceRunPhase::Pending
            );
            UserspaceEntryDecision::EnterUserspace
        })
        .expect("checkpoint and dispatch");

    assert_eq!(
        outcome.checkpoint.ast.as_slice(),
        &[AstMarker::InterruptCheck, AstMarker::Drain]
    );
    assert!(ast.is_empty());
    assert_eq!(
        outcome.action,
        UserspaceEntryAction::Entered(slot.status().expect("active userspace request"))
    );
    let UserspaceEntryAction::Entered(status) = outcome.action else {
        panic!("entry decision should dispatch userspace");
    };
    assert_eq!(status.phase, UserspaceRunPhase::Running);
    assert_eq!(status.dispatches, 1);
    assert_eq!(status.preemptions, 0);
}

#[test]
fn entry_checkpoint_repoll_consumes_ast_without_dispatching() {
    let slot = UserspaceRunSlot::new();
    let wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let mut ast = AstSlot::new();
    assert_eq!(ast.queue(AstMarker::PreemptCheck), AstQueueEffect::Queued);

    let outcome = slot
        .checkpoint_userspace_entry(request, &mut ast, |checkpoint| {
            assert_eq!(checkpoint.ast.as_slice(), &[AstMarker::PreemptCheck]);
            UserspaceEntryDecision::RePollTask
        })
        .expect("checkpoint and keep task runnable");

    assert!(ast.is_empty());
    let UserspaceEntryAction::RePollTask(status) = outcome.action else {
        panic!("entry decision should preserve the task for re-poll");
    };
    assert_eq!(status.phase, UserspaceRunPhase::Pending);
    assert_eq!(status.dispatches, 0);
    assert_eq!(slot.status(), Some(status));
}

#[test]
fn entry_checkpoint_can_resolve_userspace_wait_with_trap() {
    let slot = UserspaceRunSlot::new();
    let mut wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let mut ast = AstSlot::new();
    assert_eq!(ast.queue(AstMarker::InterruptCheck), AstQueueEffect::Queued);

    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);

    let trap = syscall_trap(172);
    let outcome = slot
        .checkpoint_userspace_entry(request, &mut ast, |checkpoint| {
            assert_eq!(checkpoint.ast.as_slice(), &[AstMarker::InterruptCheck]);
            UserspaceEntryDecision::Resolve(trap)
        })
        .expect("checkpoint and resolve wait");

    assert!(ast.is_empty());
    let UserspaceEntryAction::Resolved(status) = outcome.action else {
        panic!("entry decision should resolve userspace wait");
    };
    assert_eq!(status.phase, UserspaceRunPhase::Resolved);
    assert_eq!(status.trap, Some(trap));
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(trap));
    assert_eq!(slot.status(), None);
}

#[test]
fn timer_preemption_does_not_consume_pending_entry_ast() {
    let slot = UserspaceRunSlot::new();
    let wait = slot.start_request().expect("start userspace wait");
    let request = wait.request();
    let mut ast = AstSlot::new();

    slot.dispatch(request).expect("dispatch userspace");
    assert_eq!(ast.queue(AstMarker::Drain), AstQueueEffect::Queued);
    slot.record_timer_preemption(request)
        .expect("record timer preemption");

    assert_eq!(ast.pending(), &[AstMarker::Drain]);
    assert_eq!(
        slot.checkpoint_userspace_entry(request, &mut ast, |_| {
            panic!("resolved timer preemption must not re-enter with the same request")
        }),
        Err(UserspaceRunError::AlreadyResolved(request))
    );
    assert_eq!(ast.pending(), &[AstMarker::Drain]);
}

#[test]
fn stale_entry_checkpoint_does_not_drain_ast() {
    let slot = UserspaceRunSlot::new();
    let first = slot.start_request().expect("start first request");
    let stale = first.request();
    drop(first);

    let second = slot.start_request().expect("start second request");
    let active = second.request();
    let mut ast = AstSlot::new();
    assert_eq!(ast.queue(AstMarker::Drain), AstQueueEffect::Queued);

    assert_eq!(
        slot.checkpoint_userspace_entry(stale, &mut ast, |_| {
            panic!("stale checkpoints must not reach policy")
        }),
        Err(UserspaceRunError::StaleRequest {
            attempted: stale,
            active,
        })
    );
    assert_eq!(ast.pending(), &[AstMarker::Drain]);
}

#[test]
fn reactor_entry_checkpoint_drains_task_ast_before_dispatching_userspace() {
    let mut reactor = Reactor::new();
    let task = reactor.submit_task(std::future::pending::<()>());
    let wait = reactor
        .request_userspace_run()
        .expect("start userspace wait");
    let request = wait.request();

    assert_eq!(
        reactor.queue_ast_marker(task, AstMarker::InterruptCheck),
        Ok(AstQueueEffect::Queued)
    );
    assert_eq!(
        reactor.queue_ast_marker(task, AstMarker::Drain),
        Ok(AstQueueEffect::Queued)
    );

    let outcome = reactor
        .checkpoint_task_userspace_entry(task, request, |checkpoint| {
            assert_eq!(
                checkpoint.ast.as_slice(),
                &[AstMarker::InterruptCheck, AstMarker::Drain]
            );
            UserspaceEntryDecision::EnterUserspace
        })
        .expect("checkpoint task userspace entry");

    let UserspaceEntryAction::Entered(status) = outcome.action else {
        panic!("task checkpoint should dispatch userspace");
    };
    assert_eq!(status.phase, UserspaceRunPhase::Running);
    assert_eq!(status.dispatches, 1);
    assert!(reactor.consume_ast_markers(task).unwrap().is_empty());
}

#[test]
fn reactor_entry_checkpoint_stale_run_does_not_drain_task_ast() {
    let mut reactor = Reactor::new();
    let task = reactor.submit_task(std::future::pending::<()>());
    let first = reactor
        .request_userspace_run()
        .expect("start first request");
    let stale = first.request();
    drop(first);
    let second = reactor
        .request_userspace_run()
        .expect("start second request");
    let active = second.request();

    assert_eq!(
        reactor.queue_ast_marker(task, AstMarker::Drain),
        Ok(AstQueueEffect::Queued)
    );

    assert_eq!(
        reactor.checkpoint_task_userspace_entry(task, stale, |_| {
            panic!("stale userspace request must not reach policy")
        }),
        Err(UserspaceEntryTaskError::Run(
            UserspaceRunError::StaleRequest {
                attempted: stale,
                active,
            }
        ))
    );
    assert_eq!(
        reactor.consume_ast_markers(task).unwrap().as_slice(),
        &[AstMarker::Drain]
    );
}

#[test]
fn reactor_entry_checkpoint_terminal_task_does_not_dispatch() {
    let mut reactor = Reactor::new();
    let task = reactor.submit_task(async {});
    let wait = reactor
        .request_userspace_run()
        .expect("start userspace wait");
    let request = wait.request();

    let stats = reactor.run_until_idle();
    assert_eq!(stats.completed, 1);

    assert_eq!(
        reactor.checkpoint_task_userspace_entry(task, request, |_| {
            panic!("terminal task must not reach userspace policy")
        }),
        Err(UserspaceEntryTaskError::Task(
            TaskLifecycleError::AlreadyTerminal(TaskStatus::Completed)
        ))
    );
    let status = reactor
        .userspace_run_status()
        .expect("userspace request remains pending");
    assert_eq!(status.phase, UserspaceRunPhase::Pending);
    assert_eq!(status.dispatches, 0);
}

#[test]
fn reactor_request_userspace_run_facade_preserves_preemption_transparency() {
    let reactor = Reactor::new();
    let mut wait = reactor
        .request_userspace_run()
        .expect("start userspace wait");
    let request = wait.request();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending);
    assert_eq!(
        reactor
            .dispatch_userspace_run(request)
            .expect("dispatch userspace")
            .phase,
        UserspaceRunPhase::Running
    );
    assert_eq!(
        reactor
            .record_userspace_timer_preemption(request)
            .expect("record timer preemption")
            .phase,
        UserspaceRunPhase::Resolved
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        Pin::new(&mut wait).poll(&mut cx),
        Poll::Ready(UserspaceTrapInfo::TimerPreempt)
    );
    assert_eq!(reactor.userspace_run_status(), None);
}

#[test]
fn reactor_request_userspace_run_facade_is_single_slot_for_now() {
    let reactor = Reactor::new();
    let first = reactor
        .request_userspace_run()
        .expect("start first userspace wait");
    let busy = match reactor.request_userspace_run() {
        Ok(_) => panic!("second userspace wait should be rejected"),
        Err(error) => error,
    };
    let UserspaceRunError::Busy(status) = busy else {
        panic!("second userspace wait should report Busy");
    };
    assert_eq!(status.request, first.request());

    drop(first);
    let second = reactor
        .request_userspace_run()
        .expect("slot is reusable after wait drop");
    assert_ne!(second.request(), status.request);
}
