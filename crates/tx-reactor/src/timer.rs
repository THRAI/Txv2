//! Host-drivable timer queue used by reactor wait timeouts.

use alloc::{rc::Rc, vec::Vec};
use core::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::wait::WaitOutcome;

#[derive(Clone)]
pub(crate) struct TimerQueue {
    state: Rc<RefCell<TimerQueueState>>,
}

struct TimerQueueState {
    now_ns: u64,
    next_timer: usize,
    timers: Vec<TimerWaiter>,
}

struct TimerWaiter {
    token: TimerToken,
    deadline_ns: u64,
    waker: Waker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimerToken(usize);

pub(crate) struct DeadlineFuture {
    timers: TimerQueue,
    deadline_ns: u64,
    token: Option<TimerToken>,
}

impl TimerQueue {
    pub(crate) fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(TimerQueueState {
                now_ns: 0,
                next_timer: 0,
                timers: Vec::new(),
            })),
        }
    }

    pub(crate) fn advance_time_to(&self, now_ns: u64) -> usize {
        let mut wakers = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            state.now_ns = state.now_ns.max(now_ns);
            let mut index = 0;
            while index < state.timers.len() {
                if state.timers[index].deadline_ns <= state.now_ns {
                    let waiter = state.timers.swap_remove(index);
                    wakers.push(waiter.waker);
                } else {
                    index += 1;
                }
            }
        }

        let woke = wakers.len();
        for waker in wakers {
            waker.wake();
        }
        woke
    }

    pub(crate) fn next_deadline_ns(&self) -> Option<u64> {
        self.state
            .borrow()
            .timers
            .iter()
            .map(|waiter| waiter.deadline_ns)
            .min()
    }

    pub(crate) fn wait_until(&self, deadline_ns: u64) -> DeadlineFuture {
        DeadlineFuture {
            timers: self.clone(),
            deadline_ns,
            token: None,
        }
    }

    fn unregister(&self, token: TimerToken) {
        let mut state = self.state.borrow_mut();
        if let Some(index) = state.timers.iter().position(|waiter| waiter.token == token) {
            state.timers.swap_remove(index);
        }
    }
}

impl Future for DeadlineFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.timers.state.borrow_mut();
        if state.now_ns >= this.deadline_ns {
            this.token = None;
            return Poll::Ready(WaitOutcome::TimedOut);
        }

        match this.token {
            Some(token) => {
                if let Some(waiter) = state.timers.iter_mut().find(|waiter| waiter.token == token) {
                    waiter.deadline_ns = this.deadline_ns;
                    waiter.waker = cx.waker().clone();
                } else {
                    state.timers.push(TimerWaiter {
                        token,
                        deadline_ns: this.deadline_ns,
                        waker: cx.waker().clone(),
                    });
                }
            }
            None => {
                let token = TimerToken(state.next_timer);
                state.next_timer = state.next_timer.wrapping_add(1);
                state.timers.push(TimerWaiter {
                    token,
                    deadline_ns: this.deadline_ns,
                    waker: cx.waker().clone(),
                });
                this.token = Some(token);
            }
        }

        Poll::Pending
    }
}

impl Drop for DeadlineFuture {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.timers.unregister(token);
        }
    }
}
