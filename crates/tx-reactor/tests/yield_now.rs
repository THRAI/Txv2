use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};
use std::{
    sync::{Arc, Mutex},
    task::{Wake, Waker},
};

use tx_reactor::{yield_now, Reactor, RunStats, TaskStatus};

struct CountingWake {
    wakes: Arc<AtomicUsize>,
}

impl Wake for CountingWake {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker(wakes: Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(CountingWake { wakes }))
}

#[test]
fn yield_now_first_poll_wakes_and_second_poll_completes() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    let mut future = Pin::from(Box::new(yield_now()));

    assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);

    assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
}

#[test]
fn reactor_task_yields_once_then_completes() {
    let stage = Arc::new(AtomicUsize::new(0));
    let mut reactor = Reactor::new();
    let task = {
        let stage = Arc::clone(&stage);
        reactor.submit(async move {
            stage.store(1, Ordering::SeqCst);
            yield_now().await;
            stage.store(2, Ordering::SeqCst);
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 2,
            completed: 1
        }
    );
    assert_eq!(stage.load(Ordering::SeqCst), 2);
    assert_eq!(reactor.task_status(task), Some(TaskStatus::Completed));
}

#[test]
fn two_yielding_tasks_both_make_progress_with_existing_policy() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut reactor = Reactor::new();

    let first = {
        let events = Arc::clone(&events);
        reactor.submit(async move {
            events.lock().expect("events poisoned").push("first-before");
            yield_now().await;
            events.lock().expect("events poisoned").push("first-after");
        })
    };
    let second = {
        let events = Arc::clone(&events);
        reactor.submit(async move {
            events
                .lock()
                .expect("events poisoned")
                .push("second-before");
            yield_now().await;
            events.lock().expect("events poisoned").push("second-after");
        })
    };

    assert_eq!(
        reactor.run_until_idle(),
        RunStats {
            polled: 4,
            completed: 2
        }
    );
    assert_eq!(
        *events.lock().expect("events poisoned"),
        vec![
            "first-before",
            "second-before",
            "first-after",
            "second-after"
        ]
    );
    assert_eq!(reactor.task_status(first), Some(TaskStatus::Completed));
    assert_eq!(reactor.task_status(second), Some(TaskStatus::Completed));
}
