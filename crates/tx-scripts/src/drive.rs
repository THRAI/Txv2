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
use crate::adapter::step_engine::{
    AcceptOutcome, AgentCancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken,
    DriveMode, Errno, ResumeOutcome, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
    Translation, YieldShape,
};
use crate::adapter::wake::lookup_source;
use crate::adapter::wake::{
    agent_event_matches, ActiveWait, MailboxEvent, TaskMailbox, TimerGuardRole, TimerWheel,
};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};
use tx_observe::encode::{
    drive_begin_tag, drive_end_tag, encode_drive_begin, encode_drive_end, encode_resume,
    encode_step_outcome, encode_yield_begin, resume_tag, step_outcome_tag, yield_begin_tag,
};
use tx_observe::{EventNameId, HartEmitter, SpanId, TxTraceLevel};
use tx_observe_types::{
    PayloadDriveBegin, PayloadDriveEnd, PayloadResume, PayloadStepOutcome, PayloadYieldBegin,
    TxPayloadTag,
};

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
    // L2: drive begin (returns `SpanId::NONE` if no emitter installed).
    // `op_name_id::<S>()` uses `core::any::type_name::<S>()` (no `'static`
    // bound) so StepOp impls that borrow from their caller (the dominant
    // pattern in tx-shims syscall arms) still drive cleanly. Parent span
    // is read from the per-hart slot — the syscall dispatcher installs
    // the L0 `SyscallEnter` span there before invoking the arm so the L2
    // record links back deterministically without every arm having to
    // thread it through `ScriptCtx`.
    let parent_span = tx_observe::current_parent_span();
    let drive_span =
        emit_drive_begin::<S, I>(mode, timer_wheel.is_some(), ctx.task_id_low(), parent_span);
    // Install the L2 drive span as the new "current parent" so nested
    // L3/L4 records attach to it; restored at the end of drive() below.
    let prev_parent = tx_observe::set_current_parent_span(drive_span);

    let mut accumulated = S::Progress::EMPTY;
    let mut iteration: u32 = 0;

    let result: Result<S::Output, Errno> = 'drive: loop {
        // L4: step begin — opens a step span attached to the drive span.
        let step_span = emit_step_begin(iteration, drive_span);

        match op.step(ctx) {
            StepOutcome::Continue { progress } => {
                emit_step_end(step_span, 0, &progress, 0, 0);
                accumulated.extend(progress);
                iteration = iteration.saturating_add(1);
            }
            StepOutcome::Yield { progress, shape } => {
                let shape_kind = yield_shape_kind(&shape);
                emit_step_end(step_span, 1, &progress, shape_kind, 0);
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

                        let interrupt_state = InterruptView::<I>::from_ctx(ctx);
                        let (resume, wait_gen) = resolve_yield(
                            &shape,
                            mailbox,
                            delegate_registry,
                            timer_wheel,
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
                break 'drive Ok(t);
            }
            StepOutcome::Err(e) => {
                emit_step_end::<S::Progress>(step_span, 3, &S::Progress::EMPTY, 0, e.linux_i32());
                break 'drive Err(e);
            }
        }
    };

    // L2: drive end — closes the drive span with final outcome.
    emit_drive_end(drive_span, &result);
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
    let payload = PayloadDriveBegin {
        op_type: name.raw(),
        mode: mode_wire,
        // Interrupt policy is not yet plumbed through the drive signature;
        // default to Interruptible (1) per the spec's MVP scope.
        interrupt: 1,
        has_deadline: has_deadline as u8,
        _pad: 0,
        task_id_low,
    };
    let (enc, len) = encode_drive_begin(&payload);
    em.span_begin(
        TxTraceLevel::Drive,
        name,
        parent,
        drive_begin_tag(),
        &enc[..len as usize],
    )
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
    EventNameId::from_raw(tx_observe::fnv1a32(core::any::type_name::<S>().as_bytes()))
}

// Stable event names for the records that have no per-instance
// discriminant. Computed at compile time via `tx_observe::fnv1a32` so the
// trace carries the hash and the host-side `names.json` carries the
// string. Reuses the same FNV-1a 32 hash that `op_name_id::<S>()` uses
// for drive-level type-name hashes — single hash space for every
// `EventNameId` source so any name collision is visible at the daemon.
const RESUME_NAME: EventNameId = EventNameId::from_raw(tx_observe::fnv1a32(b"resume"));
const STEP_NAME: EventNameId = EventNameId::from_raw(tx_observe::fnv1a32(b"step"));
const YIELD_ON_WAIT_SOURCE_NAME: EventNameId =
    EventNameId::from_raw(tx_observe::fnv1a32(b"yield.OnWaitSource"));
const YIELD_ON_AGENT_NAME: EventNameId =
    EventNameId::from_raw(tx_observe::fnv1a32(b"yield.OnAgent"));
const YIELD_ON_TIMER_NAME: EventNameId =
    EventNameId::from_raw(tx_observe::fnv1a32(b"yield.OnTimer"));

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
    em.span_begin(
        TxTraceLevel::Step,
        STEP_NAME,
        parent,
        TxPayloadTag::None,
        &[],
    )
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
    let payload = PayloadStepOutcome {
        variant,
        progress_empty: progress.is_empty() as u8,
        progress_kind: progress.trace_kind(),
        shape_kind,
        errno,
        progress_value: progress.trace_value(),
        _pad: 0,
    };
    let (enc, len) = encode_step_outcome(&payload);
    em.span_end(step_span, step_outcome_tag(), &enc[..len as usize]);
}

#[inline]
fn emit_yield_begin(drive_span: SpanId, task_id_low: u32, shape_kind: u8) -> SpanId {
    let Some(em) = tx_observe::current() else {
        return SpanId::NONE;
    };
    let payload = PayloadYieldBegin {
        shape_kind,
        _pad: [0; 3],
        task_id_low,
        // `wait_generation` is populated once reactor parking writes it
        // to the mailbox; OBS-3b-followup will replace 0 with the real
        // generation pulled from the resume path.
        wait_generation: 0,
    };
    let (enc, len) = encode_yield_begin(&payload);
    let name = match shape_kind {
        1 => YIELD_ON_WAIT_SOURCE_NAME,
        2 => YIELD_ON_AGENT_NAME,
        3 => YIELD_ON_TIMER_NAME,
        _ => YIELD_ON_WAIT_SOURCE_NAME,
    };
    em.span_begin(
        TxTraceLevel::Yield,
        name,
        drive_span,
        yield_begin_tag(),
        &enc[..len as usize],
    )
}

#[inline]
fn emit_resume_end(yield_span: SpanId, resume: &ResumeOutcome, wait_gen: u64) {
    let Some(em) = current_if_active(yield_span) else {
        return;
    };
    let (resume_kind, abort_reason) = resume_wire_fields(resume);
    let payload = PayloadResume {
        resume_kind,
        abort_reason,
        _pad: [0; 2],
        object_id_low: 0,
        // `wait_gen` is the `WaitGeneration::raw()` minted by
        // `resolve_yield`; matches `PayloadWaitSourceNotify.wait_generation_low`
        // on the producer side so the daemon's flow-id hash matches
        // both ends.
        wait_generation: wait_gen,
    };
    let (enc, len) = encode_resume(&payload);
    em.instant(
        TxTraceLevel::Yield,
        RESUME_NAME,
        yield_span,
        resume_tag(),
        &enc[..len as usize],
    );
    em.span_end(yield_span, TxPayloadTag::None, &[]);
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
    let payload = PayloadDriveEnd {
        ret: 0,
        errno,
        result_kind,
        _pad: [0; 3],
    };
    let (enc, len) = encode_drive_end(&payload);
    em.span_end(drive_span, drive_end_tag(), &enc[..len as usize]);
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
        } else if I::thread_deliverable_signal_pending(thread) {
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
    timer_wheel: Option<&TimerWheel>,
    deadline: Option<Deadline>,
    interrupt_state: InterruptView<'_, I>,
) -> (ResumeOutcome, u64) {
    match shape {
        YieldShape::OnWaitSource { source, interests } => {
            resolve_on_wait_source(
                *source,
                *interests,
                mailbox,
                timer_wheel,
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
                timer_wheel,
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
                timer_wheel,
                interrupt_state,
            )
            .await;
            (outcome, 0)
        }

        YieldShape::OnTimer { token, deadline } => {
            // `OnTimer` parks on the timer wheel via a `TimerToken`; the
            // wheel's token-id is the matching discriminant on the wire.
            let outcome =
                resolve_on_timer(*token, *deadline, mailbox, timer_wheel, interrupt_state).await;
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
    timer_wheel: Option<&TimerWheel>,
    deadline: Option<Deadline>,
    interrupt_state: InterruptView<'_, I>,
) -> (ResumeOutcome, u64) {
    if let Some(mbox) = mailbox {
        let gen = mbox.next_generation();
        let active = ActiveWait::new(gen, source, interests);

        // Register this task's mailbox with the object's WaitSource so
        // the object side can wake us when its state changes.  The
        // registration is scoped to the park; we unregister on wake.
        let ws = lookup_source(source);
        let sub_id = ws
            .as_ref()
            .map(|ws| ws.register(Arc::downgrade(mbox), gen, interests));
        let timeout_guard = match (timer_wheel, deadline) {
            (Some(tw), Some(deadline)) if deadline != Deadline::NEVER => Some(tw.install_for_task(
                deadline,
                TimerGuardRole::PrimarySleep,
                Arc::downgrade(mbox),
            )),
            _ => None,
        };
        let timeout_token = timeout_guard.as_ref().map(|guard| guard.token());
        let timed_out = AtomicBool::new(false);

        let wake = await_mailbox_event(
            mbox,
            |event| {
                if active.matches(event) {
                    return true;
                }
                if matches!(
                    (event, timeout_token),
                    (MailboxEvent::TimerFired { token: fired }, Some(expected))
                        if *fired == expected
                ) {
                    timed_out.store(true, Ordering::Release);
                    return true;
                }
                false
            },
            interrupt_state,
        )
        .await;

        // Clean up the WaitSource subscription now that we're awake.
        if let (Some(ws), Some(id)) = (&ws, sub_id) {
            ws.unregister(id);
        }

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
        // Fallback path: no per-task mailbox, so no `WaitGeneration` is
        // minted. The global registry path doesn't have flow-id material
        // beyond the `WaitToken`; return 0 (the "no-gen" sentinel the
        // daemon's flow-hash treats as a never-matches placeholder).
        let token = WaitToken::new(source.raw(), interests.raw());
        if let Some(future) = wait_source::wait_on_token(token) {
            future.await;
        }
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
    timer_wheel: Option<&TimerWheel>,
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
    let wake = await_mailbox_event(
        mbox,
        move |event| agent_event_matches(event, token_id),
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
    timer_wheel: Option<&TimerWheel>,
    interrupt_state: InterruptView<'_, impl SubjectIdentity>,
) -> ResumeOutcome {
    let Some(tw) = timer_wheel else {
        return ResumeOutcome::Retry;
    };
    let Some(mbox) = mailbox else {
        return ResumeOutcome::Retry;
    };

    // PR-8B: Install the timer with a weak mailbox reference so
    // the reactor's clock tick can post TimerFired on expiry.
    // `install_for_task` allocates a fresh wheel-internal
    // `TimerToken` (from the wheel's `next_token` counter) — that
    // is the token the reactor's `fire_due` posts in
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
    let _guard = tw.install_for_task(deadline, TimerGuardRole::PrimarySleep, Arc::downgrade(mbox));
    let timer_token = _guard.token();

    // Park on mailbox until the reactor's timer-tick fires the
    // wheel and posts a TimerFired event for our token.
    let wake = await_mailbox_event(
        mbox,
        |event| match event {
            MailboxEvent::TimerFired { token: fired } => *fired == timer_token,
            _ => false,
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
    F: Fn(&MailboxEvent) -> bool,
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
        F: Fn(&MailboxEvent) -> bool,
        I: SubjectIdentity,
    {
        type Output = MailboxWake;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<MailboxWake> {
            self.mailbox.register_waker(cx.waker().clone());
            while let Some(event) = self.mailbox.poll() {
                if matches!(event, MailboxEvent::SignalDelivered { .. }) {
                    self.mailbox.clear_waker();
                    return Poll::Ready(MailboxWake::Signal(
                        self.interrupt_state.classify_signal_wake(),
                    ));
                }
                if (self.predicate)(&event) {
                    self.mailbox.clear_waker();
                    return Poll::Ready(MailboxWake::Matched);
                }
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
