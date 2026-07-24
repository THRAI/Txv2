use alloc::sync::Arc;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use smoltcp::time::Instant;
use tx_reactor::wait::WaitOutcome;
use tx_services::time::{
    DeadlineNs, DeadlineRegistrar, TimerGuard, TimerRole, TimerTarget, TimerToken,
};
use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};

use super::net_delegate_kick_tick_with_post;

/// Convert a smoltcp-relative deadline into the reactor's absolute
/// nanosecond clock domain.
pub fn smoltcp_instant_to_reactor_deadline_ns(
    base: Instant,
    deadline: Instant,
    base_ns: u64,
) -> Option<u64> {
    let delta_micros = deadline
        .total_micros()
        .saturating_sub(base.total_micros())
        .max(0);
    let delta_ns = u64::try_from(delta_micros).ok()?.checked_mul(1_000)?;
    base_ns.checked_add(delta_ns)
}

/// Arm a reactor timeout that publishes a delegate TICK when it expires.
pub async fn net_delegate_wait_tick_deadline<R>(
    timer_registrar: &R,
    deadline_ns: u64,
) -> WaitOutcome
where
    R: DeadlineRegistrar + ?Sized,
{
    let outcome = NetDelegateDeadlineFuture::new(timer_registrar, deadline_ns).await;
    if matches!(outcome, WaitOutcome::TimedOut) {
        net_delegate_kick_tick_with_post(|mailbox, event| mailbox.post(event));
    }
    outcome
}

struct NetDelegateDeadlineFuture {
    mailbox: Arc<TaskMailbox>,
    guard: Option<TimerGuard>,
    token: Option<TimerToken>,
}

impl NetDelegateDeadlineFuture {
    fn new<R>(registrar: &R, deadline_ns: u64) -> Self
    where
        R: DeadlineRegistrar + ?Sized,
    {
        let mailbox = Arc::new(TaskMailbox::new());
        let guard = registrar
            .register_deadline(
                DeadlineNs::new(deadline_ns),
                TimerRole::DeadlineAbort,
                TimerTarget::TaskMailbox(Arc::downgrade(&mailbox)),
            )
            .expect("net delegate deadline registration failed");
        let token = guard.token();
        Self {
            mailbox,
            guard: Some(guard),
            token: Some(token),
        }
    }
}

impl Unpin for NetDelegateDeadlineFuture {}

impl Future for NetDelegateDeadlineFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.mailbox.register_waker(cx.waker().clone());

        while let Some(event) = this.mailbox.poll() {
            if let MailboxEvent::TimerFired { token } = event {
                if Some(token) == this.token {
                    this.guard = None;
                    this.token = None;
                    this.mailbox.clear_waker();
                    return Poll::Ready(WaitOutcome::TimedOut);
                }
            }
        }

        Poll::Pending
    }
}
