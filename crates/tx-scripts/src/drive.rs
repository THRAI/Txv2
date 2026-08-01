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
//! | `OnTimer` | `DeadlineRegistrar::register_deadline` → `TaskMailbox` park → timer fire |
//!
//! ## Observation
//!
//! `drive` is the convergence point for the observation subsystem per
//! `docs/Txv3/08_OBSERVATION_v1.md` §2.3: it emits L2 (drive begin/end),
//! L3 (yield begin/resume), and L4 (step begin/end with `PayloadStepOutcome`)
//! records around every `op.step(ctx)` call site and yield resolution
//! point. Span ids are `SpanId(u64)` only — no witnesses, guards, or
//! borrows — so they are safe to hold across `.await` per OBS-12.
//!
//! txdoc anchor: `txdoc:STEP-V2-DRIVER-1`

use crate::adapter::delegate_runtime::{
    AbortReason, AgentTokenGuard, DelegateRegistry, TokenDropPolicy,
};
use crate::adapter::registered_wait::{
    install_registered_mailbox_wait, RegisteredMailboxSubscription, RegisteredMailboxWait,
};
use crate::adapter::step_engine::{
    AcceptOutcome, AgentCancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken,
    DriveMode, Errno, ResumeOutcome, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
    Translation, YieldShape,
};
use crate::adapter::wake::lookup_source;
use crate::adapter::wake::{
    agent_event_matches, ActiveWait, MailboxEvent, MailboxPollAction, SubscriberId, TaskMailbox,
    WaitSource,
};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};
use tx_observe::{EventNameId, HartEmitter, SpanId};
use tx_time::{DeadlineNs, DeadlineRegistrar, DeadlineRegistrarHandle, TimerRole, TimerTarget};

struct WaitSourceSubscription {
    source: Arc<WaitSource>,
    id: SubscriberId,
}

impl Drop for WaitSourceSubscription {
    fn drop(&mut self) {
        self.source.unregister(self.id);
    }
}

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
/// * `timer_registrar` — optional [`DeadlineRegistrarHandle`] for `OnTimer` resolution.
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
    timer_registrar: Option<&DeadlineRegistrarHandle>,
) -> Result<S::Output, Errno>
where
    S: StepOp<I>,
    I: SubjectIdentity,
{
    // L2: drive begin (returns `SpanId::NONE` if no emitter installed).
    // `op_name_id::<S>()` uses `core::any::type_name::<S>()` (no `'static`
    // bound) so StepOp impls that borrow from their caller (the dominant
    // pattern in tx-shims syscall arms) still drive cleanly. Parent span
    // is read from the per-hart slot — the syscall dispatcher installs
    // the L0 `SyscallEnter` span there before invoking the arm so the L2
    // record links back deterministically without every arm having to
    // thread it through `ScriptCtx`.
    let parent_span = tx_observe::current_parent_span();
    let drive_span = emit_drive_begin::<S, I>(
        mode,
        timer_registrar.is_some(),
        ctx.task_id_low(),
        parent_span,
    );
    tx_observe::dump_registered_if_requested();
    // Install the L2 drive span as the new "current parent" so nested
    // L3/L4 records attach to it; restored at the end of drive() below.
    let prev_parent = tx_observe::set_current_parent_span(drive_span);

    let mut accumulated = S::Progress::EMPTY;
    let mut iteration: u32 = 0;

    let result: Result<S::Output, Errno> = 'drive: loop {
        // L4: step begin — opens a step span attached to the drive span.
        let step_span = emit_step_begin(iteration, drive_span);
        tx_observe::dump_registered_if_requested();

        match op.step(ctx) {
            StepOutcome::Continue { progress } => {
                let made_progress = !progress.is_empty();
                emit_step_end(step_span, 0, &progress, 0, 0);
                tx_observe::dump_registered_if_requested();
                accumulated.extend(progress);
                iteration = iteration.saturating_add(1);
                // `Continue` is allowed to request an immediate retry only
                // after making observable progress.  Retrying an empty step
                // inline can monopolise a reactor worker and starve the
                // service task whose completion the operation is polling
                // (notably file writeback/fsync on a per-hart runtime).
                // Create one scheduler boundary instead of busy-spinning.
                if !made_progress {
                    tx_reactor::yield_now().await;
                }
            }
            StepOutcome::Yield { progress, shape } => {
                let shape_kind = yield_shape_kind(&shape);
                emit_step_end(step_span, 1, &progress, shape_kind, 0);
                tx_observe::dump_registered_if_requested();
                accumulated.extend(progress);
                let progress_empty = accumulated.is_empty();
                match mode.classify(&shape, progress_empty) {
                    AcceptOutcome::Translate(Translation::Eagain) => {
                        break 'drive Err(Errno::EAGAIN);
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
                            let output: S::Output = unsafe { core::mem::transmute_copy(&val) };
                            core::mem::forget(val);
                            break 'drive Ok(output);
                        }
                        break 'drive Err(Errno::EAGAIN);
                    }
                    AcceptOutcome::Translate(Translation::UnsupportedShape) => {
                        break 'drive Err(Errno::ENOSYS);
                    }
                    AcceptOutcome::Resolve => {
                        // L3: yield begin — opens a yield span attached to the drive span.
                        let yield_span =
                            emit_yield_begin(drive_span, ctx.task_id_low(), shape_kind);
                        tx_observe::dump_registered_if_requested();

                        let interrupt_state = InterruptView::<I>::from_ctx(ctx);
                        let (resume, wait_gen) = resolve_yield(
                            &shape,
                            mailbox,
                            delegate_registry,
                            timer_registrar,
                            ctx.deadline(),
                            interrupt_state,
                        )
                        .await;

                        // L3: resume instant + yield span close. The
                        // `wait_gen` returned by `resolve_yield` matches
                        // the `WaitGeneration` minted on the mailbox; the
                        // producer side's `notify_emit` carried the same
                        // value in `PayloadWaitSourceNotify`, so the
                        // daemon's flow-id hash converges and Perfetto
                        // draws the wake.notify → Resume arrow.
                        emit_resume_end(yield_span, &resume, wait_gen);
                        tx_observe::dump_registered_if_requested();

                        // D9-A: translate Aborted(Interrupted/Killed) to
                        // the appropriate errno without calling
                        // apply_resume (which defaults to rejecting
                        // non-Retry outcomes).
                        if matches!(resume, ResumeOutcome::Aborted(AbortReason::Interrupted)) {
                            break 'drive Err(Errno::EINTR);
                        }
                        if matches!(resume, ResumeOutcome::Aborted(AbortReason::Killed)) {
                            break 'drive Err(Errno::EINTR);
                        }
                        match op.apply_resume(resume) {
                            Ok(()) => {}
                            Err(errno)
                                if matches!(
                                    resume,
                                    ResumeOutcome::Aborted(AbortReason::TimedOut)
                                ) =>
                            {
                                let errno = if errno == Errno::EINVAL {
                                    Errno::ETIMEDOUT
                                } else {
                                    errno
                                };
                                break 'drive Err(errno);
                            }
                            Err(_) => break 'drive Err(Errno::EIO),
                        }
                        iteration = iteration.saturating_add(1);
                    }
                }
            }
            StepOutcome::Done(t) => {
                emit_step_end::<S::Progress>(step_span, 2, &S::Progress::EMPTY, 0, 0);
                tx_observe::dump_registered_if_requested();
                break 'drive Ok(t);
            }
            StepOutcome::Err(e) => {
                emit_step_end::<S::Progress>(step_span, 3, &S::Progress::EMPTY, 0, e.linux_i32());
                tx_observe::dump_registered_if_requested();
                break 'drive Err(e);
            }
        }
    };

    // L2: drive end — closes the drive span with final outcome.
    emit_drive_end(drive_span, &result);
    tx_observe::dump_registered_if_requested();
    // Restore the prior parent-span slot so an outer drive (or the
    // syscall dispatcher) sees the same value it installed.
    tx_observe::set_current_parent_span(prev_parent);
    result
}

// ---------------------------------------------------------------------------
// Observation emit helpers
//
// Each helper is a no-op when `tx_observe::current()` returns `None` (no
// emitter installed, no-board, or level gate off). All helpers are
// `#[inline]` so the dead-code cost when observation is off is essentially
// a single null-pointer compare per call site.
// ---------------------------------------------------------------------------

#[inline]
fn emit_drive_begin<S, I>(
    mode: DriveMode,
    has_deadline: bool,
    task_id_low: u32,
    parent: SpanId,
) -> SpanId
where
    S: StepOp<I>,
    I: SubjectIdentity,
{
    let Some(em) = tx_observe::current() else {
        return SpanId::NONE;
    };
    let mode_wire = match mode {
        DriveMode::Nonblocking => 0,
        DriveMode::Waiting => 1,
        DriveMode::Selecting => 2,
    };
    let name = op_name_id::<S>();
    // Interrupt policy is not yet plumbed through the drive signature; default
    // to Interruptible (1) per the spec's MVP scope.
    em.drive_begin(name, mode_wire, 1, has_deadline, task_id_low, parent)
}

/// Stable `EventNameId` for a [`StepOp`] type without requiring `'static`.
///
/// Uses `core::any::type_name::<S>()` (no lifetime bound) hashed to u32.
/// This trades stability across-build (vs `TypeId`) for support of borrowed
/// `StepOp` impls — the dominant pattern in tx-shims syscall arms where the
/// op holds an `&'a PageCache` etc. The daemon resolves the u32 back to a
/// human name from a build-emitted `names.json` per OBS-V1-OPNAME.
#[inline]
fn op_name_id<S: ?Sized>() -> EventNameId {
    EventNameId::from_name(core::any::type_name::<S>().as_bytes())
}

#[inline]
fn emit_step_begin(_iteration: u32, parent: SpanId) -> SpanId {
    // Step spans share a single stable name (`step`); the per-iteration
    // index is implicit in the begin-end timing relative to the parent
    // drive span. Embedding iteration in the EventNameId was tried but
    // produced 1k+ distinct hashes per drive (one per iteration), which
    // both flooded the Perfetto name interner and rendered as
    // `name_0xN` chips that were hard to read. The iteration count is
    // still recoverable from the position of the step span within the
    // drive span when needed.
    let Some(em) = tx_observe::current() else {
        return SpanId::NONE;
    };
    em.step_begin(parent)
}

#[inline]
fn emit_step_end<P: StepProgress>(
    step_span: SpanId,
    variant: u8,
    progress: &P,
    shape_kind: u8,
    errno: i32,
) {
    let Some(em) = current_if_active(step_span) else {
        return;
    };
    em.step_end(
        step_span,
        variant,
        progress.is_empty(),
        progress.trace_kind(),
        shape_kind,
        errno,
        progress.trace_value(),
    );
}

#[inline]
fn emit_yield_begin(drive_span: SpanId, task_id_low: u32, shape_kind: u8) -> SpanId {
    let Some(em) = tx_observe::current() else {
        return SpanId::NONE;
    };
    // `wait_generation` is populated once reactor parking writes it to the
    // mailbox; OBS-3b-followup will replace 0 with the real generation pulled
    // from the resume path.
    em.yield_begin(drive_span, shape_kind, task_id_low, 0)
}

#[inline]
fn emit_resume_end(yield_span: SpanId, resume: &ResumeOutcome, wait_gen: u64) {
    let Some(em) = current_if_active(yield_span) else {
        return;
    };
    let (resume_kind, abort_reason) = resume_wire_fields(resume);
    // `wait_gen` is the `WaitGeneration::raw()` minted by `resolve_yield`;
    // matches `PayloadWaitSourceNotify.wait_generation_low` on the producer
    // side so the daemon's flow-id hash matches both ends.
    em.resume(yield_span, resume_kind, abort_reason, 0, wait_gen);
    em.span_end_empty(yield_span);
}

#[inline]
fn emit_drive_end<T>(drive_span: SpanId, result: &Result<T, Errno>) {
    let Some(em) = current_if_active(drive_span) else {
        return;
    };
    let (errno, result_kind) = match result {
        Ok(_) => (0i32, 0u8),
        Err(e) => (e.linux_i32(), 1u8),
    };
    em.drive_end(drive_span, 0, errno, result_kind);
}

/// Returns `Some(emitter)` only when the span was successfully opened.
///
/// A `SpanId::NONE` indicates the span-begin emit was skipped (no emitter
/// at the time, or filter rejected it); we must not emit a matching
/// span-end against a missing span id.
#[inline]
fn current_if_active(span: SpanId) -> Option<&'static HartEmitter> {
    if span == SpanId::NONE {
        return None;
    }
    tx_observe::current()
}

#[inline]
fn yield_shape_kind(shape: &YieldShape) -> u8 {
    match shape {
        YieldShape::OnWaitSource { .. } | YieldShape::OnEdge { .. } => 1,
        YieldShape::OnAgent { .. } => 2,
        YieldShape::OnTimer { .. } => 3,
    }
}

#[inline]
fn resume_wire_fields(resume: &ResumeOutcome) -> (u8, u8) {
    match resume {
        ResumeOutcome::Retry => (0, 0),
        ResumeOutcome::WithReply(_) => (1, 0),
        ResumeOutcome::TimerExpired(_) => (2, 0),
        ResumeOutcome::Aborted(reason) => {
            let abort = match reason {
                // 0 = Signal (covers both Interruptible and Killed wake aborts).
                AbortReason::Interrupted | AbortReason::Killed => 0,
                AbortReason::Canceled => 1,
                AbortReason::TimedOut => 2,
                AbortReason::AgentDied => 3,
                AbortReason::ScopeAbandoned => 4, // wire `BorrowerExited`
            };
            (3, abort)
        }
    }
}

#[derive(Clone, Copy)]
struct InterruptView<'a, I: SubjectIdentity> {
    thread: Option<&'a crate::adapter::step_engine::Cap<I::ThreadIdentity>>,
}

impl<'a, I: SubjectIdentity> InterruptView<'a, I> {
    fn from_ctx(ctx: &'a ScriptCtx<I>) -> Self {
        Self {
            thread: ctx.subject().and_then(|subject| subject.thread()),
        }
    }

    fn classify_signal_wake(&self) -> SignalWake {
        let Some(thread) = self.thread else {
            return SignalWake::Interrupt;
        };
        if I::thread_termination_in_force(thread) {
            SignalWake::Kill
        } else if I::thread_signal_interrupts_wait(thread) {
            SignalWake::Interrupt
        } else {
            SignalWake::Retry
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignalWake {
    Retry,
    Interrupt,
    Kill,
}

/// Resolve a yield shape. Returns the [`ResumeOutcome`] to pass to
/// [`StepOp::apply_resume`] paired with the `WaitGeneration::raw()` the
/// resolver minted on this task's mailbox (`0` when no parking happened
/// — e.g. fallback global-registry path on `OnWaitSource` without a
/// mailbox, or `OnAgent`/`OnTimer` synchronous early returns).
///
/// The generation is threaded back so `drive` can populate
/// `PayloadResume.wait_generation`, matching the value emitted on the
/// producer side via `WaitSource::notify_emit` →
/// `PayloadWaitSourceNotify.wait_generation_low`. Identical material on
/// both sides lets the host daemon hash `(task_id, wait_gen, kind)` to
/// the same Perfetto flow id, drawing the wake.notify ↔ Resume arrows
/// the spec's §6 worked example describes.
///
/// When the required runtime is not provided (`None`), falls back
/// gracefully: `OnWaitSource` uses the global channel registry;
/// `OnAgent`/`OnTimer` return `Retry` immediately (the step will
/// re-poll and the caller's `DriveMode` will translate repeated
/// yields appropriately).
async fn resolve_yield<I: SubjectIdentity>(
    shape: &YieldShape,
    mailbox: Option<&Arc<TaskMailbox>>,
    delegate_registry: Option<&DelegateRegistry>,
    timer_registrar: Option<&DeadlineRegistrarHandle>,
    deadline: Option<Deadline>,
    interrupt_state: InterruptView<'_, I>,
) -> (ResumeOutcome, u64) {
    match shape {
        YieldShape::OnWaitSource { source, interests } => {
            resolve_on_wait_source(
                *source,
                *interests,
                mailbox,
                timer_registrar,
                deadline,
                interrupt_state,
            )
            .await
        }

        YieldShape::OnEdge { source, interests } => {
            resolve_on_wait_source(
                *source,
                *interests,
                mailbox,
                timer_registrar,
                deadline,
                interrupt_state,
            )
            .await
        }

        YieldShape::OnAgent {
            endpoint,
            request,
            token,
            deadline,
            cancel,
        } => {
            // `OnAgent` parks on the delegate registry; no `WaitGeneration`
            // mints here (the delegate-token-id substitutes for it on the
            // wire today). Reserved for OBS-9 follow-up.
            let outcome = resolve_on_agent(
                endpoint,
                request,
                token,
                *deadline,
                *cancel,
                mailbox,
                delegate_registry,
                timer_registrar,
                interrupt_state,
            )
            .await;
            (outcome, 0)
        }

        YieldShape::OnTimer { token, deadline } => {
            // `OnTimer` parks through the timer registrar; the
            // issued token is the matching discriminant on the wire.
            let outcome =
                resolve_on_timer(*token, *deadline, mailbox, timer_registrar, interrupt_state)
                    .await;
            (outcome, 0)
        }
    }
}

// ---------------------------------------------------------------------------
// OnWaitSource resolution
// ---------------------------------------------------------------------------

async fn resolve_on_wait_source<I: SubjectIdentity>(
    source: crate::adapter::step_engine::WaitSourceId,
    interests: crate::adapter::step_engine::InterestMask,
    mailbox: Option<&Arc<TaskMailbox>>,
    timer_registrar: Option<&DeadlineRegistrarHandle>,
    deadline: Option<Deadline>,
    interrupt_state: InterruptView<'_, I>,
) -> (ResumeOutcome, u64) {
    if let Some(mbox) = mailbox {
        let gen = mbox.next_generation();
        let active = ActiveWait::new(gen, source, interests);

        let raw_wait = install_registered_mailbox_wait(
            source.raw(),
            interests.raw(),
            Arc::downgrade(mbox),
            gen,
        );
        if matches!(raw_wait, Some(RegisteredMailboxWait::Ready)) {
            return (ResumeOutcome::Retry, gen.raw());
        }
        let _raw_subscription: Option<RegisteredMailboxSubscription> = match raw_wait {
            Some(RegisteredMailboxWait::Pending(subscription)) => Some(subscription),
            Some(RegisteredMailboxWait::Ready) | None => None,
        };

        // Register this task's mailbox with the object's WaitSource so
        // the object side can wake us when its state changes.  The
        // registration is scoped to the park; the guard unregisters on wake
        // and when this future is cancelled while parked.
        let ws = lookup_source(source);
        let _subscription = ws.as_ref().map(|source| WaitSourceSubscription {
            source: Arc::clone(source),
            id: source.register(Arc::downgrade(mbox), gen, interests),
        });
        let timeout_guard = match (timer_registrar, deadline) {
            (Some(registrar), Some(deadline)) if deadline != Deadline::NEVER => match registrar
                .register_deadline(
                    DeadlineNs::new(deadline.raw()),
                    TimerRole::DeadlineAbort,
                    TimerTarget::TaskMailbox(Arc::downgrade(mbox)),
                ) {
                Ok(guard) => Some(guard),
                Err(_) => return (ResumeOutcome::Aborted(AbortReason::TimedOut), gen.raw()),
            },
            _ => None,
        };
        let timeout_token = timeout_guard.as_ref().map(|guard| guard.token());
        let timed_out = AtomicBool::new(false);

        let wake = await_mailbox_event(
            mbox,
            |event| {
                if active.matches(event) {
                    return MailboxPollAction::Take;
                }
                if matches!(
                    (event, timeout_token),
                    (MailboxEvent::TimerFired { token: fired }, Some(expected))
                        if *fired == expected
                ) {
                    timed_out.store(true, Ordering::Release);
                    return MailboxPollAction::Take;
                }
                match event {
                    MailboxEvent::SourceFired {
                        source: fired_source,
                        generation: fired_generation,
                        ..
                    } if *fired_source == active.source
                        && *fired_generation != active.generation =>
                    {
                        MailboxPollAction::Drop
                    }
                    _ => MailboxPollAction::Keep,
                }
            },
            interrupt_state,
        )
        .await;

        // D9-A: signal interrupt during blocked wait. The generation we
        // minted is still the right discriminant for the flow id — the
        // wake came from the interruptible-signal path, which fires
        // through the same mailbox with the same generation tag.
        match wake {
            MailboxWake::Signal(SignalWake::Interrupt) => {
                return (ResumeOutcome::Aborted(AbortReason::Interrupted), gen.raw());
            }
            MailboxWake::Signal(SignalWake::Kill) => {
                return (ResumeOutcome::Aborted(AbortReason::Killed), gen.raw());
            }
            MailboxWake::Matched if timed_out.load(Ordering::Acquire) => {
                return (ResumeOutcome::Aborted(AbortReason::TimedOut), gen.raw());
            }
            MailboxWake::Signal(SignalWake::Retry) | MailboxWake::Matched => {}
        }
        (ResumeOutcome::Retry, gen.raw())
    } else {
        // No per-task mailbox means there is nowhere to park or receive a
        // generation-tagged wake. Retry lets the caller re-observe state
        // without re-entering the retired WaitToken -> Channel bridge.
        (ResumeOutcome::Retry, 0)
    }
}

// ---------------------------------------------------------------------------
// OnAgent resolution
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn resolve_on_agent(
    endpoint: &DelegateEndpoint,
    request: &DelegateRequest,
    _caller_token: &DelegateToken,
    deadline: Deadline,
    cancel: AgentCancelPolicy,
    mailbox: Option<&Arc<TaskMailbox>>,
    delegate_registry: Option<&DelegateRegistry>,
    timer_registrar: Option<&DeadlineRegistrarHandle>,
    interrupt_state: InterruptView<'_, impl SubjectIdentity>,
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

    // Install the delegate request. The registry mints a DelegateTokenId
    // and records the subscription.
    let _guard: AgentTokenGuard<'_> = registry.install_request(
        *request,
        endpoint_marker,
        cancel_policy,
        drop_policy,
        mailbox_weak,
    );
    // guard holds the token alive; drop cancels if not yet resolved.

    // Park on mailbox until the agent replies or the request is aborted.
    let token_id = _guard.id();
    let _deadline_guard = match (timer_registrar, deadline) {
        (Some(registrar), deadline) if deadline != Deadline::NEVER => match registrar
            .register_deadline(
                DeadlineNs::new(deadline.raw()),
                TimerRole::DelegateTimeout,
                TimerTarget::DelegateToken(token_id),
            ) {
            Ok(guard) => Some(guard),
            Err(_) => return ResumeOutcome::Aborted(AbortReason::TimedOut),
        },
        _ => None,
    };
    let wake = await_mailbox_event(
        mbox,
        move |event| {
            if agent_event_matches(event, token_id) {
                MailboxPollAction::Take
            } else {
                MailboxPollAction::Keep
            }
        },
        interrupt_state,
    )
    .await;

    // D9-A: signal interrupt during blocked wait.
    match wake {
        MailboxWake::Signal(SignalWake::Interrupt) => {
            return ResumeOutcome::Aborted(AbortReason::Interrupted);
        }
        MailboxWake::Signal(SignalWake::Kill) => {
            return ResumeOutcome::Aborted(AbortReason::Killed);
        }
        MailboxWake::Signal(SignalWake::Retry) | MailboxWake::Matched => {}
    }

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
    timer_registrar: Option<&DeadlineRegistrarHandle>,
    interrupt_state: InterruptView<'_, impl SubjectIdentity>,
) -> ResumeOutcome {
    let Some(registrar) = timer_registrar else {
        return ResumeOutcome::Retry;
    };
    let Some(mbox) = mailbox else {
        return ResumeOutcome::Retry;
    };

    // Register through the time facade so the reactor domain owns queue
    // selection and mailbox routing. The issued opaque token is the one in
    // `MailboxEvent::TimerFired`, so the predicate below must
    // compare against the GUARD'S token, NOT the caller-passed
    // `TimerId` (which is opaque-to-the-wheel and frequently a
    // hard-coded constant like `TimerId::new(1)` from
    // `NanosleepOp::step`). Using the constant token caused
    // every `OnTimer` yield to wait forever for a token-1 event
    // that never matched the wheel-allocated token; sigtimedwait
    // bodies appeared to spin via the unrelated `SignalDelivered`
    // wake path (which doesn't check the token) when the child
    // exited fast, but hung outright when the child was slower.
    let Ok(_guard) = registrar.register_deadline(
        DeadlineNs::new(deadline.raw()),
        TimerRole::PrimarySleep,
        TimerTarget::TaskMailbox(Arc::downgrade(mbox)),
    ) else {
        return ResumeOutcome::Aborted(AbortReason::TimedOut);
    };
    let timer_token = _guard.token();

    // Park on mailbox until the reactor's timer-tick fires the
    // wheel and posts a TimerFired event for our token.
    let wake = await_mailbox_event(
        mbox,
        |event| match event {
            MailboxEvent::TimerFired { token: fired } if *fired == timer_token => {
                MailboxPollAction::Take
            }
            _ => MailboxPollAction::Keep,
        },
        interrupt_state,
    )
    .await;

    // D9-A: signal interrupt during blocked wait.
    match wake {
        MailboxWake::Signal(SignalWake::Interrupt) => {
            return ResumeOutcome::Aborted(AbortReason::Interrupted);
        }
        MailboxWake::Signal(SignalWake::Kill) => {
            return ResumeOutcome::Aborted(AbortReason::Killed);
        }
        MailboxWake::Signal(SignalWake::Retry) | MailboxWake::Matched => {}
    }

    ResumeOutcome::TimerExpired(token)
}

// ---------------------------------------------------------------------------
// Mailbox parking primitive
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MailboxWake {
    Matched,
    Signal(SignalWake),
}

/// Park the current task on `mailbox` until `predicate` matches an
/// incoming event. Spurious wakes are consumed and the task re-parks.
///
/// A [`MailboxEvent::SignalDelivered`] is a wake hint, not truth. On
/// signal hints this future consults the thread interrupt summary via
/// `InterruptView`: deliverable signals interrupt, terminal signals
/// kill the wait, and masked/non-deliverable hints only force a retry.
async fn await_mailbox_event<F, I>(
    mailbox: &TaskMailbox,
    predicate: F,
    interrupt_state: InterruptView<'_, I>,
) -> MailboxWake
where
    F: Fn(&MailboxEvent) -> MailboxPollAction,
    I: SubjectIdentity,
{
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll};

    struct MailboxFuture<'a, F, I: SubjectIdentity> {
        mailbox: &'a TaskMailbox,
        predicate: F,
        interrupt_state: InterruptView<'a, I>,
    }

    impl<'a, F, I> Future for MailboxFuture<'a, F, I>
    where
        F: Fn(&MailboxEvent) -> MailboxPollAction,
        I: SubjectIdentity,
    {
        type Output = MailboxWake;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<MailboxWake> {
            self.mailbox.register_waker(cx.waker().clone());
            if let Some(event) = self.mailbox.poll_select(|event| {
                if matches!(event, MailboxEvent::SignalDelivered { .. }) {
                    MailboxPollAction::Take
                } else {
                    (self.predicate)(event)
                }
            }) {
                if matches!(event, MailboxEvent::SignalDelivered { .. }) {
                    self.mailbox.clear_waker();
                    return Poll::Ready(MailboxWake::Signal(
                        self.interrupt_state.classify_signal_wake(),
                    ));
                }
                self.mailbox.clear_waker();
                return Poll::Ready(MailboxWake::Matched);
            }
            // Overflow means at least one wake hint was dropped.  The step
            // predicate is the source of truth, so resolve this suspension and
            // let drive() re-run the operation instead of spinning forever on
            // a permanently latched overflow bit.
            if self.mailbox.take_overflow() {
                self.mailbox.clear_waker();
                return Poll::Ready(MailboxWake::Matched);
            }
            Poll::Pending
        }
    }

    impl<'a, F, I> Drop for MailboxFuture<'a, F, I>
    where
        I: SubjectIdentity,
    {
        fn drop(&mut self) {}
    }

    MailboxFuture {
        mailbox,
        predicate,
        interrupt_state,
    }
    .await
}
