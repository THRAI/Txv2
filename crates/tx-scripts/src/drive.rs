//! Central `drive` loop — L2 / L4 observation instrumentation (OBS-3a).
//!
//! This module owns the canonical driver that runs a [`StepOp`] to
//! completion, emitting trace records at:
//!
//! - **L2** (`TxTraceLevel::Drive`): one `SpanBegin` at entry and one `SpanEnd`
//!   at exit of every `drive` invocation.
//! - **L4** (`TxTraceLevel::Step`): one `SpanBegin` / `SpanEnd` pair around
//!   every `op.step(ctx)` call — the span carries a `PayloadStepOutcome`.
//!
//! # Anti-pattern OBS-A-1
//!
//! L4 emission happens **at the call site in this module**, never inside the
//! `StepOp::step` body. This is the only correct placement per
//! `docs/Txv3/08_OBSERVATION_v1.md` §17 (OBS-A-1). Every call to `op.step()`
//! is wrapped with SpanBegin before and SpanEnd after; no step body emits
//! observation records directly.
//!
//! Spec refs:
//!   txdoc:OBS-V1-HOOKS-1   — hook surface map (L2 / L4 sites)
//!   txdoc:OBS-V1-ANTI-1    — OBS-A-1 anti-pattern (no emit inside step body)
//!   txdoc:OBS-V1-LEVELS-1  — level catalog

use tx_observe::{EventNameId, SpanId};
use tx_observe_types::{
    PayloadDriveBegin, PayloadDriveEnd, PayloadStepOutcome, TxPayloadTag, TxProgressKind,
    TxTraceLevel, YieldShapeKind,
};
use tx_substrate::step_v3::{ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity, YieldShape};

/// Outcome returned from [`drive`].
///
/// Mirrors the two terminal variants of [`StepOutcome`] that `drive` can
/// return to its caller:
///
/// - `Done(T)` — the op completed with output `T`.
/// - `Err(E)` — the op returned `StepOutcome::Err(errno)`.
///
/// The `yield_resolve` callback in the `Yield` arm decides whether to
/// block and retry or abort with an error; if it returns `None` the loop
/// continues after the wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriveOutcome<T, E> {
    Done(T),
    Err(E),
}

/// Run `op` to completion under `ctx`, emitting L2 and L4 observation
/// records via the current hart's emitter (if any).
///
/// # Type parameters
///
/// - `O` — the [`StepOp`] implementation to drive.
/// - `I` — the `SubjectIdentity` threaded through `ScriptCtx<I>`.
///
/// The platform context for observation is implicit: `tx_observe::current()`
/// reads the cpu-id via the function pointer installed by `tx_observe::init`
/// at boot (D16 Option C).  No `Plat` type parameter is required.
///
/// # L4 discipline (OBS-A-1)
///
/// The call to `op.step(ctx)` is wrapped on both sides:
/// ```text
/// span_begin(L4) → op.step(ctx) → span_end(L4, PayloadStepOutcome)
/// ```
/// No emit happens inside `step` bodies; only the driver emits at L4.
///
/// # Yield handling
///
/// When `StepOutcome::Yield { shape, progress }` is returned by the op,
/// the `yield_resolve` callback is invoked with `(&shape, &mut ctx)`.
/// It returns `Some(errno)` to abort the loop with `DriveOutcome::Err`,
/// or `None` to signal that the wait has been resolved and the loop should
/// re-invoke `op.step(ctx)`.
///
/// For OBS-3b, the L3 emit (yield begin / resume) will be inserted in the
/// `yield_resolve` path here; it is intentionally absent in OBS-3a.
pub fn drive<O, I>(
    op: &mut O,
    ctx: &mut ScriptCtx<I>,
    mut yield_resolve: impl FnMut(&YieldShape, &mut ScriptCtx<I>) -> Option<tx_substrate::step_v3::Errno>,
) -> DriveOutcome<O::Output, tx_substrate::step_v3::Errno>
where
    O: StepOp<I> + 'static,
    I: SubjectIdentity,
{
    // ── L2 SpanBegin ────────────────────────────────────────────────────────
    //
    // Emit a Drive-level span begin for this `drive` invocation, labelled with
    // the type-id of the op (so the daemon can resolve the human name from
    // names.json).
    let drive_span: SpanId = if let Some(em) = tx_observe::current() {
        use tx_observe::encode::{drive_begin_tag, encode_drive_begin};

        let begin = PayloadDriveBegin {
            op_type: EventNameId::of::<O>().raw(),
            mode: 1,           // 1 = Waiting (default for OBS-3a)
            interrupt: 1,      // 1 = Interruptible
            has_deadline: 0,
            _pad: 0,
            task_id_low: 0,    // task_id threading is OBS-4
        };
        let (payload_bytes, _) = encode_drive_begin(&begin);
        em.span_begin(
            TxTraceLevel::Drive,
            EventNameId::of::<O>(),
            SpanId::NONE,
            drive_begin_tag(),
            &payload_bytes,
        )
    } else {
        SpanId::NONE
    };

    // ── Main step loop ───────────────────────────────────────────────────────
    let result = loop {
        // ── L4 SpanBegin ── (before op.step — OBS-A-1 discipline) ──────────
        let step_span: SpanId = if let Some(em) = tx_observe::current() {
            em.span_begin(
                TxTraceLevel::Step,
                EventNameId::of::<O>(),
                drive_span,
                TxPayloadTag::None,
                &[],
            )
        } else {
            SpanId::NONE
        };

        // Drive the op one step.
        //
        // ANTI-PATTERN OBS-A-1: `op.step(ctx)` is called between the
        // L4 span_begin above and span_end below.  No observation code
        // runs inside the step body.
        let outcome = op.step(ctx);

        // ── L4 SpanEnd ── (after op.step returns, before any branching) ─────
        if step_span != SpanId::NONE {
            if let Some(em) = tx_observe::current() {
                use tx_observe::encode::{encode_step_outcome, step_outcome_tag};

                let (variant, shape_kind, errno_val, progress_value, progress_kind, is_empty) =
                    encode_outcome_fields::<O, I>(&outcome);
                let step_payload = PayloadStepOutcome {
                    variant,
                    progress_empty: if is_empty { 1 } else { 0 },
                    progress_kind,
                    shape_kind,
                    errno: errno_val,
                    progress_value,
                    _pad: 0,
                };
                let (payload_bytes, _) = encode_step_outcome(&step_payload);
                em.span_end(step_span, step_outcome_tag(), &payload_bytes);
            }
        }

        // ── Route on outcome ─────────────────────────────────────────────────
        match outcome {
            StepOutcome::Done(output) => break DriveOutcome::Done(output),

            StepOutcome::Err(errno) => break DriveOutcome::Err(errno),

            StepOutcome::Continue { .. } => {
                // Make progress without waiting; loop immediately.
                // L3 yield/resume is not in scope for OBS-3a.
            }

            StepOutcome::Yield { shape, .. } => {
                // L3 yield/resume hooks will be inserted here in OBS-3b.
                // For OBS-3a the yield_resolve callback decides the wait.
                if let Some(errno) = yield_resolve(&shape, ctx) {
                    break DriveOutcome::Err(errno);
                }
            }
        }
    };

    // ── L2 SpanEnd ──────────────────────────────────────────────────────────
    if drive_span != SpanId::NONE {
        if let Some(em) = tx_observe::current() {
            use tx_observe::encode::{drive_end_tag, encode_drive_end};

            let (ret_val, errno_val, result_kind) = match &result {
                DriveOutcome::Done(_) => (0i64, 0i32, 0u8),
                DriveOutcome::Err(e) => (0i64, errno_to_i32(e), 1u8),
            };
            let end = PayloadDriveEnd {
                ret: ret_val,
                errno: errno_val,
                result_kind,
                _pad: [0u8; 3],
            };
            let (payload_bytes, _) = encode_drive_end(&end);
            em.span_end(drive_span, drive_end_tag(), &payload_bytes);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a [`tx_substrate::step_v3::Errno`] to a signed i32 for wire encoding.
///
/// Only the variants used in OBS-3a are mapped; the full table lands with
/// the errno-interop slice.  Unknown variants fall back to `EIO = 5`.
fn errno_to_i32(e: &tx_substrate::step_v3::Errno) -> i32 {
    use tx_substrate::step_v3::Errno::*;
    match e {
        EACCES   => 13,
        EAGAIN   => 11,
        EBADF    => 9,
        EBUSY    => 16,
        EDQUOT   => 122,
        EEXIST   => 17,
        EFAULT   => 14,
        EINVAL   => 22,
        EIO      => 5,
        EISDIR   => 21,
        ELOOP    => 40,
        ENAMETOOLONG => 36,
        ENODEV   => 19,
        ENOEXEC  => 8,
        ENOMEM   => 12,
        ENOENT   => 2,
        ENOSYS   => 38,
        ENOTDIR  => 20,
        ENOTEMPTY => 39,
        ENOTTY   => 25,
        EPERM    => 1,
        EPIPE    => 32,
        ERANGE   => 34,
        EROFS    => 30,
        ESPIPE   => 29,
        ESRCH    => 3,
        ESTALE   => 116,
    }
}

/// Extract trace-friendly fields from a [`StepOutcome`].
///
/// Returns `(variant, shape_kind, errno, progress_value, progress_kind, progress_is_empty)`.
/// Variants: 0=Continue, 1=Yield, 2=Done, 3=Err.
fn encode_outcome_fields<O: StepOp<I>, I: SubjectIdentity>(
    outcome: &StepOutcome<O::Output, O::Progress>,
) -> (u8, u8, i32, u32, u8, bool) {
    match outcome {
        StepOutcome::Continue { progress } => {
            let (pv, pk, empty) = progress_fields(progress);
            (0, 0, 0, pv, pk, empty)
        }
        StepOutcome::Yield { progress, shape } => {
            let (pv, pk, empty) = progress_fields(progress);
            let sk = yield_shape_kind(shape);
            (1, sk, 0, pv, pk, empty)
        }
        StepOutcome::Done(_) => (2, 0, 0, 0, TxProgressKind::NoProgress as u8, true),
        StepOutcome::Err(e)  => (3, 0, errno_to_i32(e), 0, TxProgressKind::NoProgress as u8, true),
    }
}

/// Extract (progress_value, progress_kind_byte, is_empty) from a `StepProgress`.
///
/// OBS-3a uses [`TxProgressKind::NoProgress`] for all ops that do not have a
/// byte count; the OBS-4 follow-up wires per-progress-type inspection.
fn progress_fields<P: StepProgress>(progress: &P) -> (u32, u8, bool) {
    let empty = progress.is_empty();
    // OBS-3a: all progress encoded as NoProgress (value 0); later
    // specialisation wires ByteProgress, PageProgress, etc.
    (0, TxProgressKind::NoProgress as u8, empty)
}

/// Map a [`YieldShape`] to its compact wire discriminant.
fn yield_shape_kind(shape: &YieldShape) -> u8 {
    match shape {
        YieldShape::OnWaitSource { .. } => YieldShapeKind::OnWaitSource as u8,
        YieldShape::OnAgent { .. }      => YieldShapeKind::OnAgent as u8,
        YieldShape::OnTimer { .. }      => YieldShapeKind::OnTimer as u8,
    }
}
