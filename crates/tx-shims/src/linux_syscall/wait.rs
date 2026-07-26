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

/// Atomically install a wait-source subscription while rechecking the
/// semantic blocked predicate under the source's notification commit lock.
///
/// Returns `false` when the predicate became false before registration; the
/// caller must immediately re-observe the authoritative object state instead
/// of parking.
pub(super) async fn await_wait_source_if<F>(
    ctx: &SyscallCtx<'_>,
    source: WaitSourceId,
    interests: InterestMask,
    still_blocked: F,
) -> bool
where
    F: FnOnce() -> bool,
{
    let Some(mailbox) = ctx.mailbox.as_ref() else {
        return false;
    };
    let Some(wait_source) = lookup_source(source) else {
        return false;
    };

    let generation = mailbox.next_generation();
    let active = ActiveWait::new(generation, source, interests);
    let prepared = wait_source.prepare(Arc::downgrade(mailbox), generation, interests);
    let Some(registration) = prepared.install_if(still_blocked) else {
        return false;
    };

    MailboxSourceFuture { mailbox, active }.await;
    drop(registration);
    true
}

pub(super) async fn await_any_wait_source(
    ctx: &SyscallCtx<'_>,
    sources: &[(WaitSourceId, InterestMask)],
) -> bool {
    let Some(mailbox) = ctx.mailbox.as_ref() else {
        return false;
    };

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
        return false;
    }

    MailboxAnySourceFuture {
        mailbox,
        generation,
        active: &active,
    }
    .await;
    drop(registrations);
    true
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

struct SourceRegistration {
    source: Arc<WaitSource>,
    subscriber: SubscriberId,
}

impl Drop for SourceRegistration {
    fn drop(&mut self) {
        self.source.unregister(self.subscriber);
    }
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
        // Mailbox events are hints, while the object state is authoritative.
        // A full mailbox deliberately drops the concrete event and latches
        // `overflow`; consuming that latch must therefore wake the syscall so
        // its outer loop re-observes the child/fd/futex state.  Leaving the
        // latch set makes PerHartSlotted continuously reschedule this task,
        // while this future keeps returning Pending forever.
        if self.mailbox.take_overflow() {
            self.mailbox.clear_waker();
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

struct MailboxAnySourceFuture<'a> {
    mailbox: &'a TaskMailbox,
    generation: crate::adapter::reactor_entry::WaitGeneration,
    active: &'a [(WaitSourceId, InterestMask)],
}

impl Future for MailboxAnySourceFuture<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.mailbox.register_waker(cx.waker().clone());
        while let Some(event) = self.mailbox.poll() {
            if matches_any_wait(self.generation, self.active, &event)
                || matches!(event, MailboxEvent::SignalDelivered { .. })
            {
                self.mailbox.clear_waker();
                return Poll::Ready(());
            }
        }
        if self.mailbox.take_overflow() {
            self.mailbox.clear_waker();
            return Poll::Ready(());
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
