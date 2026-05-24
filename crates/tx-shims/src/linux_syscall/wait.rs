//! Syscall-side wait-source parking.
//!
//! Blocking syscall arms that still hand-drive subsystem steps converge here
//! instead of constructing legacy `WaitToken` values. The semantic object owns
//! the `WaitSource`; the syscall driver only subscribes its task mailbox and
//! re-polls the subsystem after a matching wake.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use alloc::sync::Arc;

use crate::adapter::reactor_entry::{lookup_source, ActiveWait, MailboxEvent, TaskMailbox};
use crate::adapter::step_engine::{InterestMask, WaitSourceId};

use super::SyscallCtx;

pub(super) async fn await_wait_source(
    ctx: &SyscallCtx<'_>,
    source: WaitSourceId,
    interests: InterestMask,
) {
    let Some(mailbox) = ctx.mailbox.as_ref() else {
        return;
    };
    await_wait_source_on_mailbox(mailbox, source, interests).await;
}

async fn await_wait_source_on_mailbox(
    mailbox: &Arc<TaskMailbox>,
    source: WaitSourceId,
    interests: InterestMask,
) {
    let generation = mailbox.next_generation();
    let active = ActiveWait::new(generation, source, interests);
    let Some(wait_source) = lookup_source(source) else {
        return;
    };
    let subscriber = wait_source.register(Arc::downgrade(mailbox), generation, interests);
    MailboxSourceFuture { mailbox, active }.await;
    wait_source.unregister(subscriber);
}

struct MailboxSourceFuture<'a> {
    mailbox: &'a TaskMailbox,
    active: ActiveWait,
}

impl Future for MailboxSourceFuture<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.mailbox.register_waker(cx.waker().clone());
        while let Some(event) = self.mailbox.poll() {
            if self.active.matches(&event) {
                self.mailbox.clear_waker();
                return Poll::Ready(());
            }
            if matches!(event, MailboxEvent::SignalDelivered { .. }) {
                self.mailbox.clear_waker();
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}
