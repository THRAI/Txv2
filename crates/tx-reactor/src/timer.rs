//! Host-drivable timer queue used by reactor wait timeouts.
//!
//! The PR-8 public timer surface (`TimerWheel`, `TimerGuard`,
//! `TimerToken`, `TimerGuardRole`) moved down to
//! [`tx_substrate::wake::timer`] per
//! [`docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md`].
//! The re-export at the bottom of this module preserves the
//! existing `tx_reactor::timer::*` and `crate::timer::*` paths
//! (including the `pub use` in `crate::lib.rs`).
//!
//! What remains here is the **internal `TimerQueue`** that drives
//! the reactor's built-in `WaitProtocol::*Timeout` paths. It owns
//! the actual `Waker`s, is advanced by the host clock callback,
//! and is unrelated to the public wheel — the two were always
//! decoupled (D6 §2.2). PR-7+ may eventually consolidate them.

use alloc::{sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::spin_lock::SpinLock;
use crate::wait::WaitOutcome;

// Re-export the relocated public surface so `tx_reactor::timer::*`
// and `crate::timer::*` paths continue to resolve.
pub use tx_substrate::wake::timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};

// =========================================================================
// Internal: TimerQueue (unchanged; backs existing reactor
// `WaitProtocol::*Timeout` paths and `tests/timer_idle.rs`).
// =========================================================================

#[derive(Clone)]
pub(crate) struct TimerQueue {
    state: Arc<SpinLock<TimerQueueState>>,
}

struct TimerQueueState {
    now_ns: u64,
    next_timer: usize,
    timers: Vec<TimerWaiter>,
}

struct TimerWaiter {
    token: InternalTimerToken,
    deadline_ns: u64,
    waker: Waker,
}

/// Internal queue-local token kept private to the `TimerQueue` impl
/// below. The public [`TimerToken`] (re-exported above from
/// `tx_substrate::wake::timer`) is a separate type so we don't
/// confuse the two roles (queue waiter id vs. wheel registration id).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InternalTimerToken(usize);

pub(crate) struct DeadlineFuture {
    timers: TimerQueue,
    deadline_ns: u64,
    token: Option<InternalTimerToken>,
}

impl TimerQueue {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(SpinLock::new(TimerQueueState {
                now_ns: 0,
                next_timer: 0,
                timers: Vec::new(),
            })),
        }
    }

    pub(crate) fn advance_time_to(&self, now_ns: u64) -> usize {
        let mut wakers = Vec::new();
        {
            let mut state = self.state.lock();
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
            .lock()
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

    fn unregister(&self, token: InternalTimerToken) {
        let mut state = self.state.lock();
        if let Some(index) = state.timers.iter().position(|waiter| waiter.token == token) {
            state.timers.swap_remove(index);
        }
    }
}

impl Future for DeadlineFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.timers.state.lock();
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
                let token = InternalTimerToken(state.next_timer);
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
