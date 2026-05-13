//! Central `StepOp` driver — per `docs/Txv3/03_STEP_MODEL_v2.md` §5.
//!
//! `drive` is the subsystem-agnostic loop that interprets `StepOutcome`
//! for an arbitrary `StepOp`, routing the four outcome shapes through the
//! closed `DriveMode` classify matrix and accumulating progress across
//! `Continue` and `Yield` returns.
//!
//! Observation hooks are deliberately absent in this PR; they are added
//! in the next PR so all future observation work has a single hook point
//! inside `drive` rather than being scattered across every shim.
//!
//! No callers are migrated here; existing `step_*` ad-hoc loops in the
//! shims continue to work unchanged until future PRs swap them over.
//!
//! txdoc anchor: `txdoc:STEP-V2-DRIVER-1`

use crate::adapter::step_engine::{
    AcceptOutcome, DriveMode, Errno, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
    Translation,
};

/// Central `StepOp` driver.
///
/// Per `docs/Txv3/03_STEP_MODEL_v2.md` §5: the subsystem-agnostic loop
/// that interprets [`StepOutcome`] for `op`, routing the four outcome
/// shapes through the closed [`DriveMode`] classify matrix and accumulating
/// [`StepProgress`] across `Continue` and `Yield` returns.
///
/// # Outcome handling
///
/// | `StepOutcome` | Driver action |
/// |---|---|
/// | `Continue { progress }` | Accumulate progress; re-invoke `step`. |
/// | `Done(t)` | Return `Ok(t)`. |
/// | `Err(e)` | Return `Err(e)`. |
/// | `Yield { progress, shape }` | Accumulate progress; classify via `mode`. |
///
/// ## Classify results for `Yield`
///
/// | `AcceptOutcome` | Driver action |
/// |---|---|
/// | `Translate(Eagain)` | Return `Err(EAGAIN)` (nonblocking, no progress). |
/// | `Translate(PartialReturn)` | Return `Err(EAGAIN)` — partial-return output synthesis (`into_output`) requires a future `StepProgress` trait extension; until then, nonblocking partial yields surface as `EAGAIN`. |
/// | `Translate(UnsupportedShape)` | Return `Err(ENOSYS)` (mode does not support this yield shape; spec names EOPNOTSUPP, nearest available variant is ENOSYS). |
/// | `Resolve` | Requires reactor-side parking (not yet wired in this PR). Returns `Err(EAGAIN)` as a conservative stub; future PRs connect the reactor wait channel here. |
///
/// # Arguments
///
/// * `op` — the typed `StepOp` to drive. Owned so the driver can call
///   `op.step()` and `op.apply_resume()` freely.
/// * `ctx` — per-script execution context threaded through each `step` call.
/// * `mode` — closed dispatch mode governing how yield shapes are resolved.
///
/// # Returns
///
/// `Ok(S::Output)` on `Done`, `Err(Errno)` on `Err` or any translated yield.
pub async fn drive<S, I>(
    mut op: S,
    ctx: &mut ScriptCtx<I>,
    mode: DriveMode,
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
                // loop — step made progress and may make more without waiting
            }
            StepOutcome::Yield { progress, shape } => {
                accumulated.extend(progress);
                let progress_empty = accumulated.is_empty();
                match mode.classify(&shape, progress_empty) {
                    AcceptOutcome::Translate(Translation::Eagain) => {
                        return Err(Errno::EAGAIN);
                    }
                    AcceptOutcome::Translate(Translation::PartialReturn) => {
                        // Partial-return output synthesis (`accumulated.into_output()`)
                        // requires a `StepProgress::into_output` method that does not
                        // yet exist on the trait. Until that method is added, surface
                        // partial nonblocking yields as EAGAIN. Future PR adds
                        // `into_output` and returns `Ok(...)` here.
                        return Err(Errno::EAGAIN);
                    }
                    AcceptOutcome::Translate(Translation::UnsupportedShape) => {
                        // Spec names EOPNOTSUPP; nearest available Errno variant is ENOSYS.
                        return Err(Errno::ENOSYS);
                    }
                    AcceptOutcome::Resolve => {
                        // Resolve requires reactor-side parking: the driver should await
                        // the yield shape's wait source, agent reply, or timer via the
                        // reactor's wait channel. That wiring is a future PR. Until then,
                        // surface as EAGAIN so callers are never silently blocked
                        // by an unimplemented wait path.
                        return Err(Errno::EAGAIN);
                    }
                }
            }
            StepOutcome::Done(t) => return Ok(t),
            StepOutcome::Err(e) => return Err(e),
        }
    }
}
