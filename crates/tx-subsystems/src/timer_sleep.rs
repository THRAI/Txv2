//! Reactor timer-sleep seam for `nanosleep` / `clock_nanosleep`.
//!
//! `tx-kernel` installs the BSP reactor's `TimerQueue` (an Arc clone) at
//! boot time via [`install_timer_queue`] — called from outside the
//! reactor task loop so the BOOT_REACTOR lock is not held. The syscall
//! layer calls [`sleep_until_ns`] from inside a reactor task; it creates
//! a `DeadlineFuture` by cloning the stored Arc and calling
//! `TimerQueue::wait_until`, which does NOT re-acquire the reactor lock.
//! The waker registration in `DeadlineFuture::poll` and the wakeup path
//! in `TimerQueue::advance_time_to` share only the TimerQueue's own
//! internal SpinLock (not the reactor's top-level lock), so no deadlock.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "reactor",
    domain = "timer_ops",
    apis = ["timer"],
    reason = "wrap reactor TimerQueue and DeadlineFuture used by nanosleep/clock_nanosleep syscall layer"
)]
mod timer_ops {
    pub use tx_reactor::timer::TimerQueue;
    pub use tx_reactor::DeadlineFuture;
}

use crate::adapter::step_engine::SpinMutex;
use timer_ops::{DeadlineFuture, TimerQueue};

static TIMER_QUEUE: SpinMutex<Option<TimerQueue>> = SpinMutex::new(None);

/// Install the reactor's timer queue. Called once at boot from
/// `tx-kernel` before the reactor task loop starts (so BOOT_REACTOR
/// lock is not held at this call site).
pub fn install_timer_queue(tq: TimerQueue) {
    *TIMER_QUEUE.lock() = Some(tq);
}

/// Create a future that resolves once the reactor's clock advances
/// past `deadline_ns`. Returns `None` if the queue has not been
/// installed (unit-test context without a reactor — callers should
/// return success immediately).
pub fn sleep_until_ns(deadline_ns: u64) -> Option<DeadlineFuture> {
    TIMER_QUEUE
        .lock()
        .as_ref()
        .map(|tq| tq.wait_until(deadline_ns))
}
