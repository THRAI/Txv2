//! Syscall-side wait-source parking.
//!
//! Blocking syscall arms that still hand-drive subsystem steps converge here
//! instead of constructing legacy `WaitToken` values. The semantic object owns
//! the `WaitSource`; the syscall driver only subscribes its task mailbox and
//! re-polls the subsystem after a matching wake.

use core::future::poll_fn;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::adapter::reactor_entry::{
    lookup_source, ActiveWait, MailboxEvent, SubscriberId, TaskMailbox, WaitSource,
};
use crate::adapter::step_engine::{InterestMask, WaitSourceId};
use tx_services::time::{
    DeadlineNs, DeadlineRegistrar, DeadlineRegistrarHandle, TimerGuard, TimerRole, TimerTarget,
    TimerToken,
};

use super::SyscallCtx;

pub(super) fn deadline_timer(
    ctx: &SyscallCtx<'_>,
    deadline_ns: u64,
) -> Option<DeadlineTimerFuture> {
    ctx.timer_registrar
        .as_ref()
        .map(|registrar| DeadlineTimerFuture::new(registrar.clone(), deadline_ns))
}

pub(super) struct DeadlineTimerFuture {
    registrar: DeadlineRegistrarHandle,
    deadline_ns: u64,
    mailbox: Arc<TaskMailbox>,
    guard: Option<TimerGuard>,
    token: Option<TimerToken>,
}

impl DeadlineTimerFuture {
    fn new(registrar: DeadlineRegistrarHandle, deadline_ns: u64) -> Self {
        Self {
            registrar,
            deadline_ns,
            mailbox: Arc::new(TaskMailbox::new()),
            guard: None,
            token: None,
        }
    }
}

impl Unpin for DeadlineTimerFuture {}

impl Future for DeadlineTimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        this.mailbox.register_waker(cx.waker().clone());
        if this.guard.is_none() {
            let guard = this
                .registrar
                .register_deadline(
                    DeadlineNs::new(this.deadline_ns),
                    TimerRole::DeadlineAbort,
                    TimerTarget::TaskMailbox(Arc::downgrade(&this.mailbox)),
                )
                .expect("deadline registrar should accept task-mailbox deadline");
            this.token = Some(guard.token());
            this.guard = Some(guard);
        }

        while let Some(event) = this.mailbox.poll() {
            if let MailboxEvent::TimerFired { token } = event {
                if Some(token) == this.token {
                    this.guard = None;
                    this.token = None;
                    this.mailbox.clear_waker();
                    return Poll::Ready(());
                }
            }
        }

        Poll::Pending
    }
}

pub(super) async fn await_wait_endpoint(
    ctx: &SyscallCtx<'_>,
    endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    interests: InterestMask,
) {
    let Some(mailbox) = ctx.mailbox.as_ref() else {
        return;
    };
    await_wait_source_handle_on_mailbox(
        mailbox,
        endpoint.source(),
        endpoint.source_id(),
        interests,
    )
    .await;
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

/// Atomically subscribe to a semantic wait source and then wait for either
/// that source (including a signal mailbox event) or an absolute reactor
/// deadline.
///
/// POSIX timers are process objects, while a blocking `wait4` is parked on the
/// parent's child-exit source.  Merely programming the hart timer is not
/// sufficient: when the timer interrupt fires, the parked `wait4` task still
/// has no reason to run and therefore cannot post the POSIX signal.  Combining
/// the two wake sources here gives the timer deadline its own waker without
/// polling and without weakening the child-exit lost-wakeup recheck.
pub(super) async fn await_wait_source_if_until<F>(
    ctx: &SyscallCtx<'_>,
    source: WaitSourceId,
    interests: InterestMask,
    deadline_ns: u64,
    still_blocked: F,
) -> WaitSourceDeadline
where
    F: FnOnce() -> bool,
{
    let Some(mailbox) = ctx.mailbox.as_ref() else {
        return WaitSourceDeadline::NotInstalled;
    };
    let Some(wait_source) = lookup_source(source) else {
        return WaitSourceDeadline::NotInstalled;
    };
    let Some(mut deadline) = tx_subsystems::timer_sleep::sleep_until_ns(deadline_ns) else {
        return WaitSourceDeadline::NotInstalled;
    };

    let generation = mailbox.next_generation();
    let active = ActiveWait::new(generation, source, interests);
    let prepared = wait_source.prepare(Arc::downgrade(mailbox), generation, interests);
    let Some(registration) = prepared.install_if(still_blocked) else {
        return WaitSourceDeadline::Source;
    };

    let mut source = MailboxSourceFuture { mailbox, active };
    let outcome = poll_fn(|cx| {
        if Pin::new(&mut source).poll(cx).is_ready() {
            return Poll::Ready(WaitSourceDeadline::Source);
        }
        if Pin::new(&mut deadline).poll(cx).is_ready() {
            return Poll::Ready(WaitSourceDeadline::Deadline);
        }
        Poll::Pending
    })
    .await;
    drop(registration);
    outcome
}

pub(super) async fn await_any_wait_source(
    ctx: &SyscallCtx<'_>,
    sources: &[(WaitSourceId, InterestMask, Option<Arc<WaitSource>>)],
    deadline_ns: Option<u64>,
) -> bool {
    let Some(mailbox) = ctx.mailbox.as_ref() else {
        return false;
    };

    let generation = mailbox.next_generation();
    let mut registrations = Vec::new();
    let mut active = Vec::new();
    for (source, interests, endpoint) in sources {
        let source = *source;
        let interests = *interests;
        if source.raw() == 0 || interests.raw() == 0 {
            continue;
        }
        let wait_source = if let Some(endpoint) = endpoint {
            Arc::clone(endpoint)
        } else if let Some(wait_source) = lookup_source(source) {
            wait_source
        } else {
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

    let (timer_guard, deadline_token) = match deadline_ns {
        Some(deadline_ns) => {
            let Some(registrar) = ctx.timer_registrar.as_ref() else {
                return false;
            };
            let guard = registrar
                .register_deadline(
                    DeadlineNs::new(deadline_ns),
                    TimerRole::DeadlineAbort,
                    TimerTarget::TaskMailbox(Arc::downgrade(mailbox)),
                )
                .expect("deadline registrar should accept task-mailbox deadline");
            let token = guard.token();
            (Some(guard), Some(token))
        }
        None => (None, None),
    };

    let woke = MailboxAnySourceFuture {
        mailbox,
        generation,
        active: &active,
        _timer_guard: timer_guard,
        deadline_token,
    }
    .await;
    drop(registrations);
    woke
}

async fn await_wait_source_handle_on_mailbox(
    mailbox: &Arc<TaskMailbox>,
    wait_source: Arc<WaitSource>,
    source: WaitSourceId,
    interests: InterestMask,
) {
    let generation = mailbox.next_generation();
    let active = ActiveWait::new(generation, source, interests);
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
            if self.active.matches(&event)
                || matches!(
                    event,
                    MailboxEvent::SignalDelivered { .. } | MailboxEvent::SignalTimerFired { .. }
                )
            {
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
    _timer_guard: Option<TimerGuard>,
    deadline_token: Option<TimerToken>,
}

impl Future for MailboxAnySourceFuture<'_> {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        self.mailbox.register_waker(cx.waker().clone());
        while let Some(event) = self.mailbox.poll() {
            if let MailboxEvent::TimerFired { token } = event {
                if self.deadline_token == Some(token) {
                    self.mailbox.clear_waker();
                    return Poll::Ready(false);
                }
            }
            if matches_any_wait(self.generation, self.active, &event)
                || matches!(
                    event,
                    MailboxEvent::SignalDelivered { .. } | MailboxEvent::SignalTimerFired { .. }
                )
            {
                self.mailbox.clear_waker();
                return Poll::Ready(true);
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
