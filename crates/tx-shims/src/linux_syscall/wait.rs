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
use alloc::vec::Vec;

use crate::adapter::reactor_entry::{
    lookup_source, ActiveWait, MailboxEvent, SubscriberId, TaskMailbox, WaitSource,
};
use crate::adapter::step_engine::{InterestMask, WaitSourceId};
use tx_subsystems::execution::WaitToken;

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

pub(super) async fn await_wait_token(ctx: &SyscallCtx<'_>, token: WaitToken) -> bool {
    let (source, interests) = wait_token_source(token);
    await_any_wait_source(ctx, &[(source, interests)]).await
}

pub(super) async fn await_any_wait_source(
    ctx: &SyscallCtx<'_>,
    sources: &[(WaitSourceId, InterestMask)],
) -> bool {
    let Some(future) = any_wait_source_future(ctx, sources) else {
        return false;
    };
    future.await;
    true
}

pub(super) fn wait_token_source(token: WaitToken) -> (WaitSourceId, InterestMask) {
    (
        WaitSourceId::new(token.source_id()),
        InterestMask::new(token.interest()),
    )
}

pub(super) fn wait_token_future<'a>(
    ctx: &'a SyscallCtx<'_>,
    token: WaitToken,
) -> Option<MailboxSourceFuture<'a>> {
    let (source, interests) = wait_token_source(token);
    wait_source_future(ctx, source, interests)
}

pub(super) fn wait_source_future<'a>(
    ctx: &'a SyscallCtx<'_>,
    source: WaitSourceId,
    interests: InterestMask,
) -> Option<MailboxSourceFuture<'a>> {
    let mailbox = ctx.mailbox.as_ref()?;
    wait_source_future_on_mailbox(mailbox, source, interests)
}

pub(super) fn any_wait_source_future<'a>(
    ctx: &'a SyscallCtx<'_>,
    sources: &[(WaitSourceId, InterestMask)],
) -> Option<MailboxAnySourceFuture<'a>> {
    let mailbox = ctx.mailbox.as_ref()?;
    let generation = mailbox.next_generation();
    let mut registrations = Vec::new();
    let mut active = Vec::new();
    for (source, interests) in sources.iter().copied() {
        if source.raw() == 0 || interests.raw() == 0 {
            continue;
        }
        let Some(wait_source) = lookup_source(source) else {
            continue;
        };
        let subscriber = wait_source.register(Arc::downgrade(mailbox), generation, interests);
        registrations.push(SourceRegistration {
            source: wait_source,
            subscriber,
        });
        active.push((source, interests));
    }

    if active.is_empty() {
        return None;
    }

    Some(MailboxAnySourceFuture {
        mailbox,
        generation,
        active,
        _registrations: registrations,
    })
}

async fn await_wait_source_on_mailbox(
    mailbox: &Arc<TaskMailbox>,
    source: WaitSourceId,
    interests: InterestMask,
) {
    if let Some(future) = wait_source_future_on_mailbox(mailbox, source, interests) {
        future.await;
    }
}

fn wait_source_future_on_mailbox(
    mailbox: &Arc<TaskMailbox>,
    source: WaitSourceId,
    interests: InterestMask,
) -> Option<MailboxSourceFuture<'_>> {
    let generation = mailbox.next_generation();
    let active = ActiveWait::new(generation, source, interests);
    let wait_source = lookup_source(source)?;
    let subscriber = wait_source.register(Arc::downgrade(mailbox), generation, interests);
    Some(MailboxSourceFuture {
        mailbox,
        active,
        _registration: SourceRegistration {
            source: wait_source,
            subscriber,
        },
    })
}

struct SourceRegistration {
    source: Arc<WaitSource>,
    subscriber: SubscriberId,
}

impl Drop for SourceRegistration {
    fn drop(&mut self) {
        self.source.unregister(self.subscriber);
    }
}

pub(super) struct MailboxSourceFuture<'a> {
    mailbox: &'a TaskMailbox,
    active: ActiveWait,
    _registration: SourceRegistration,
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

pub(super) struct MailboxAnySourceFuture<'a> {
    mailbox: &'a TaskMailbox,
    generation: crate::adapter::reactor_entry::WaitGeneration,
    active: Vec<(WaitSourceId, InterestMask)>,
    _registrations: Vec<SourceRegistration>,
}

impl Future for MailboxAnySourceFuture<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.mailbox.register_waker(cx.waker().clone());
        while let Some(event) = self.mailbox.poll() {
            if matches_any_wait(self.generation, &self.active, &event)
                || matches!(event, MailboxEvent::SignalDelivered { .. })
            {
                self.mailbox.clear_waker();
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

fn matches_any_wait(
    generation: crate::adapter::reactor_entry::WaitGeneration,
    active: &[(WaitSourceId, InterestMask)],
    event: &MailboxEvent,
) -> bool {
    let MailboxEvent::SourceFired {
        generation: fired_generation,
        source: fired_source,
        interests: fired_interests,
    } = event
    else {
        return false;
    };
    *fired_generation == generation
        && active.iter().any(|(source, interests)| {
            *source == *fired_source && interests.raw() & fired_interests.raw() != 0
        })
}
