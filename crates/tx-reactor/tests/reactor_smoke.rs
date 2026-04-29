use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};

use tx_reactor::{Reactor, RunStats, TaskId};

static READY_POLLS: AtomicUsize = AtomicUsize::new(0);
static PENDING_POLLS: AtomicUsize = AtomicUsize::new(0);

struct CountOnce;

impl Future for CountOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        READY_POLLS.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(())
    }
}

struct ParkForever;

impl Future for ParkForever {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        PENDING_POLLS.fetch_add(1, Ordering::SeqCst);
        Poll::Pending
    }
}

#[test]
fn submitted_ready_task_runs_to_completion() {
    READY_POLLS.store(0, Ordering::SeqCst);

    let mut reactor = Reactor::new();
    let task_id = reactor.submit(CountOnce);

    assert_eq!(task_id, TaskId(0));
    assert!(!reactor.is_idle());

    let stats = reactor.run_until_idle();

    assert_eq!(
        stats,
        RunStats {
            polled: 1,
            completed: 1
        }
    );
    assert_eq!(READY_POLLS.load(Ordering::SeqCst), 1);
    assert!(reactor.is_idle());
}

#[test]
fn pending_task_is_parked_after_one_poll() {
    PENDING_POLLS.store(0, Ordering::SeqCst);

    let mut reactor = Reactor::new();
    let task_id = reactor.submit(ParkForever);

    assert_eq!(task_id, TaskId(0));

    let first = reactor.run_until_idle();
    let second = reactor.run_until_idle();

    assert_eq!(
        first,
        RunStats {
            polled: 1,
            completed: 0
        }
    );
    assert_eq!(
        second,
        RunStats {
            polled: 0,
            completed: 0
        }
    );
    assert_eq!(PENDING_POLLS.load(Ordering::SeqCst), 1);
    assert!(reactor.is_idle());
}
