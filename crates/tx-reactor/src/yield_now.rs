//! Cooperative task-yield future.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Future returned by [`yield_now`].
///
/// The first poll wakes the current task and yields `Pending`, creating a
/// reactor poll boundary. The next poll completes.
#[derive(Clone, Copy, Debug, Default)]
#[must_use = "futures do nothing unless awaited or polled"]
pub struct YieldNow {
    yielded: bool,
}

impl YieldNow {
    pub const fn new() -> Self {
        Self { yielded: false }
    }
}

/// Cooperatively yield the current reactor task once.
#[must_use = "futures do nothing unless awaited or polled"]
pub fn yield_now() -> impl Future<Output = ()> {
    YieldNow::new()
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        if this.yielded {
            Poll::Ready(())
        } else {
            this.yielded = true;
            crate::task::mark_current_task_yielded();
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
