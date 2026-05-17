//! StepOp wrappers for signalfd operations.
//!
//! These wrap the signalfd subsystem's free functions as `StepOp`
//! impls so the reactor can drive them through its central
//! `drive()` loop.  Mirrors the pattern established by
//! `KillProcessOp` / `KillPgrpOp` in `signal/mod.rs`.

use crate::process::structure::ProcessIdentity;
use crate::signal::adapter::step_engine::{
    NoProgress, OneShotStepOp, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use crate::signalfd::adapter::step_engine::{Cap, ZoneError};
use crate::signalfd::{signalfd_create, SignalFd};

/// StepOp wrapper for [`signalfd_create`].
pub struct SignalfdCreateOp {
    pub owner_proc: Cap<ProcessIdentity>,
    pub mask: u64,
}

impl<I: SubjectIdentity> StepOp<I> for SignalfdCreateOp {
    type Output = Result<Cap<SignalFd>, ZoneError>;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        // observe — owner_proc Cap + mask validated by signalfd_create
        // upgrade — N/A: Cap<SignalFd> allocated via signalfd_create
        // reserve — signalfd_create handles zone reservation internally
        // commit — Cap returned as Done(result)
        // publish — N/A: no signal attachments (subscriber list managed by signal module)
        let result = signalfd_create(&self.owner_proc, self.mask);
        StepOutcome::Done(result)
    }
}

impl<I: SubjectIdentity> OneShotStepOp<I> for SignalfdCreateOp {}
