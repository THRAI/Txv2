//! Central `StepOp` driver — per `docs/Txv3/03_STEP_MODEL_v2.md` §5.
//!
//! `drive` is the subsystem-agnostic loop that interprets `StepOutcome`
//! for an arbitrary `StepOp`, routing the four outcome shapes through the
//! closed `DriveMode` classify matrix and accumulating progress across
//! `Continue` and `Yield` returns.
//!
//! ## Yield resolution
//!
//! | Yield shape | Resolution |
//! |---|---|
//! | `OnWaitSource` | `TaskMailbox` + `ActiveWait::matches` (or global Channel registry fallback) |
//! | `OnAgent` | `DelegateRegistry::install_request` → `TaskMailbox` park → `AgentReplied`/`Abort` |
//! | `OnTimer` | `TimerWheel::install` → `TaskMailbox` park → timer fire |
//!
//! Observation hooks are deliberately absent; they are added in a
//! future PR as a single hook point inside `drive`.
//!
//! txdoc anchor: `txdoc:STEP-V2-DRIVER-1`

use crate::adapter::delegate_runtime::{
    AbortReason, AgentTokenGuard, DelegateRegistry, DelegateReply, DelegateTokenId,
    TokenDropPolicy,
};
use crate::adapter::step_engine::{
    AcceptOutcome, AgentCancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken,
    DriveMode, Errno, ResumeOutcome, ScriptCtx, StepOp, StepOutcome, StepProgress,
    SubjectIdentity, Translation, YieldShape,
};
use crate::adapter::wake::{
    agent_event_matches, ActiveWait, MailboxEvent, TaskMailbox, TimerGuardRole, TimerToken,
    TimerWheel,
};
use alloc::sync::{Arc, Weak};

use tx_subsystems::execution::WaitToken;
use tx_subsystems::wait_source;

/// Central `StepOp` driver.
///
/// Per `docs/Txv3/03_STEP_MODEL_v2.md` §5: the subsystem-agnostic loop
/// that interprets [`StepOutcome`] for `op`, routing the four outcome
/// shapes through the closed [`DriveMode`] classify matrix and accumulating
/// [`StepProgress`] across `Continue` and `Yield` returns.
///
/// # Arguments
///
/// * `op` — the typed `StepOp` to drive. Owned.
/// * `ctx` — per-script execution context threaded through each `step` call.
/// * `mode` — closed dispatch mode governing how yield shapes are resolved.
/// * `mailbox` — optional [`TaskMailbox`] for reactor parking.
/// * `delegate_registry` — optional [`DelegateRegistry`] for `OnAgent` resolution.
/// * `timer_wheel` — optional [`TimerWheel`] for `OnTimer` resolution.
///
/// # Returns
///
/// `Ok(S::Output)` on `Done`, `Err(Errno)` on `Err` or any translated yield.
pub async fn drive<S, I>(
    mut op: S,
    ctx: &mut ScriptCtx<I>,
    mode: DriveMode,
    mailbox: Option<&Arc<TaskMailbox>>,
    delegate_registry: Option<&DelegateRegistry>,
    timer_wheel: Option<&TimerWheel>,
) -> Result<S::Output, Errno>
where
    S: StepOp<I>,
    I: SubjectIdentity,
{
    let mut accumulated = S::Progress::EMPTY;
    loop {
        match op.step(ctx) {
            StepOutcome::Continue { progress } => {
                accumulated.extend(progress);
            }
            StepOutcome::Yield { progress, shape } => {
                accumulated.extend(progress);
                let progress_empty = accumulated.is_empty();
                match mode.classify(&shape, progress_empty) {
                    AcceptOutcome::Translate(Translation::Eagain) => {
                        return Err(Errno::EAGAIN);
                    }
                    AcceptOutcome::Translate(Translation::PartialReturn) => {
                        // Surface accumulated progress as the output.
                        // Safety: `into_output` returns `Some(val)` only
                        // for progress types where `Progress::Output ==
                        // S::Output` (e.g. ByteProgress → usize). For
                        // all other progress types it returns `None`
                        // and we fall through to EAGAIN.
                        if let Some(val) = accumulated.into_output() {
                            // transmute_copy is safe: `into_output` only
                            // returns Some when Progress::Output and
                            // S::Output are the same concrete type
                            // (usize for ByteProgress). The compiler
                            // cannot prove this, but the impl contract
                            // guarantees it.
                            let output: S::Output =
                                unsafe { core::mem::transmute_copy(&val) };
                            core::mem::forget(val);
                            return Ok(output);
                        }
                        return Err(Errno::EAGAIN);
                    }
                    AcceptOutcome::Translate(Translation::UnsupportedShape) => {
                        return Err(Errno::ENOSYS);
                    }
                    AcceptOutcome::Resolve => {
                        let resume = resolve_yield(
                            &shape, mailbox, delegate_registry, timer_wheel,
                        )
                        .await;
                        op.apply_resume(resume).map_err(|_| Errno::EIO)?;
                    }
                }
            }
            StepOutcome::Done(t) => return Ok(t),
            StepOutcome::Err(e) => return Err(e),
        }
    }
}

/// Resolve a yield shape. Returns the [`ResumeOutcome`] to pass to
/// [`StepOp::apply_resume`].
///
/// When the required runtime is not provided (`None`), falls back
/// gracefully: `OnWaitSource` uses the global channel registry;
/// `OnAgent`/`OnTimer` return `Retry` immediately (the step will
/// re-poll and the caller's `DriveMode` will translate repeated
/// yields appropriately).
async fn resolve_yield(
    shape: &YieldShape,
    mailbox: Option<&Arc<TaskMailbox>>,
    delegate_registry: Option<&DelegateRegistry>,
    timer_wheel: Option<&TimerWheel>,
) -> ResumeOutcome {
    match shape {
        YieldShape::OnWaitSource {
            source,
            interests,
        } => resolve_on_wait_source(*source, *interests, mailbox).await,

        YieldShape::OnEdge {
            source,
            interests,
        } => resolve_on_wait_source(*source, *interests, mailbox).await,

        YieldShape::OnAgent {
            endpoint,
            request,
            token,
            deadline,
            cancel,
        } => resolve_on_agent(
            endpoint, request, token, *deadline, *cancel, mailbox, delegate_registry,
            timer_wheel,
        )
        .await,

        YieldShape::OnTimer { token, deadline } => {
            resolve_on_timer(*token, *deadline, mailbox, timer_wheel).await
        }
    }
}

// ---------------------------------------------------------------------------
// OnWaitSource resolution
// ---------------------------------------------------------------------------

async fn resolve_on_wait_source(
    source: crate::adapter::step_engine::WaitSourceId,
    interests: crate::adapter::step_engine::InterestMask,
    mailbox: Option<&Arc<TaskMailbox>>,
) -> ResumeOutcome {
    if let Some(mbox) = mailbox {
        let gen = mbox.next_generation();
        let active = ActiveWait::new(gen, source, interests);
        await_mailbox_event(mbox, |event| active.matches(event)).await;
    } else {
        let token = WaitToken::new(source.raw(), interests.raw());
        if let Some(future) = wait_source::wait_on_token(token) {
            future.await;
        }
    }
    ResumeOutcome::Retry
}

// ---------------------------------------------------------------------------
// OnAgent resolution
// ---------------------------------------------------------------------------

async fn resolve_on_agent(
    endpoint: &DelegateEndpoint,
    request: &DelegateRequest,
    _caller_token: &DelegateToken,
    deadline: Deadline,
    cancel: AgentCancelPolicy,
    mailbox: Option<&Arc<TaskMailbox>>,
    delegate_registry: Option<&DelegateRegistry>,
    timer_wheel: Option<&TimerWheel>,
) -> ResumeOutcome {
    let Some(registry) = delegate_registry else {
        return ResumeOutcome::Retry;
    };
    let Some(mbox) = mailbox else {
        return ResumeOutcome::Retry;
    };

    // Extract the endpoint marker. For zone-allocated endpoints
    // (PR-10+), this is the zone slot index.
    let endpoint_marker: u64 = endpoint.marker();

    let cancel_policy = cancel;
    let drop_policy = TokenDropPolicy::CancelOnDrop;
    let mailbox_weak: Weak<TaskMailbox> = Arc::downgrade(mbox);

    let deadline_opt = if deadline != Deadline::NEVER {
        timer_wheel.map(|tw| (deadline, tw))
    } else {
        None
    };

    // Install the delegate request. The registry mints a DelegateTokenId
    // and records the subscription.
    let _guard: AgentTokenGuard<'_> = registry.install_request(
        *request,
        endpoint_marker,
        cancel_policy,
        drop_policy,
        mailbox_weak,
        deadline_opt,
    );
    // guard holds the token alive; drop cancels if not yet resolved.

    // Park on mailbox until the agent replies or the request is aborted.
    let token_id = _guard.id();
    await_mailbox_event(mbox, move |event| agent_event_matches(event, token_id)).await;

    // Extract the reply.
    match registry.take_reply(token_id) {
        Some(reply) => ResumeOutcome::WithReply(reply),
        None => {
            // Token reached a terminal state without a reply
            // (timed out, canceled, or agent died).
            ResumeOutcome::Aborted(AbortReason::Canceled)
        }
    }
}

// ---------------------------------------------------------------------------
// OnTimer resolution
// ---------------------------------------------------------------------------

async fn resolve_on_timer(
    token: crate::adapter::step_engine::TimerId,
    deadline: Deadline,
    mailbox: Option<&Arc<TaskMailbox>>,
    timer_wheel: Option<&TimerWheel>,
) -> ResumeOutcome {
    let Some(tw) = timer_wheel else {
        return ResumeOutcome::Retry;
    };
    let Some(mbox) = mailbox else {
        return ResumeOutcome::Retry;
    };

    // Convert TimerId → TimerToken (From impl added in PR-7).
    let timer_token = TimerToken::from(token);

    // Install the timer. The guard auto-cancels on drop.
    let _guard = tw.install(deadline, TimerGuardRole::PrimarySleep);
    // guard holds the timer registration alive.

    // Park on mailbox. The reactor's timer-tick path fires the
    // timer wheel, which posts a mailbox event (PR-8 wiring).
    //
    // Until PR-8 lands the fire path, use a simple generation-based
    // wait: re-poll immediately. The step will see the timer hasn't
    // fired yet and re-yield. Nonblocking mode translates to EAGAIN.
    let gen = mbox.next_generation();
    await_mailbox_event(mbox, |_event| {
        // TODO(PR-8): match on TimerFired(token) event.
        // For now, any wake is treated as a timer fire.
        true
    })
    .await;
    // Suppress unused warning for gen.
    let _ = gen;

    ResumeOutcome::TimerExpired(token)
}

// ---------------------------------------------------------------------------
// Mailbox parking primitive
// ---------------------------------------------------------------------------

/// Park the current task on `mailbox` until `predicate` matches an
/// incoming event. Spurious wakes are consumed and the task re-parks.
async fn await_mailbox_event<F>(mailbox: &TaskMailbox, predicate: F)
where
    F: Fn(&MailboxEvent) -> bool,
{
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll};

    struct MailboxFuture<'a, F> {
        mailbox: &'a TaskMailbox,
        predicate: F,
    }

    impl<'a, F: Fn(&MailboxEvent) -> bool> Future for MailboxFuture<'a, F> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.mailbox.register_waker(cx.waker().clone());
            while let Some(event) = self.mailbox.poll() {
                if (self.predicate)(&event) {
                    self.mailbox.clear_waker();
                    return Poll::Ready(());
                }
            }
            Poll::Pending
        }
    }

    impl<'a, F> Drop for MailboxFuture<'a, F> {
        fn drop(&mut self) {}
    }

    MailboxFuture {
        mailbox,
        predicate,
    }
    .await
}
