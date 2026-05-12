//! v3 `StepOp` trait + `ScriptCtx` placeholder pin tests.
//!
//! These tests pin the trait shape from
//! `docs/Txv3/03_STEP_MODEL_v2.md` §2.1 so PR-0 can land a minimum
//! `StepOp` definition that later PRs (resolver, SubjectContext, guard,
//! OnAgent yield) extend without breaking the trait surface. The
//! `ScriptCtx` placeholder is intentionally empty for PR-0; later PRs
//! flesh out its fields.
//!
//! txdoc cross-refs:
//! - TXV3-STEP-MODEL-V2 §2.1 (StepOp trait)
//! - STEP-1 (four-variant outcome reused via StepOutcome)
//! - STEP-3 (StepProgress monoid bound on the associated type)

use tx_substrate::step_v3::{
    AbortReason, ByteProgress, DelegateReply, Errno, InterestMask, NoProgress, ResumeOutcome,
    ScriptCtx, StepOp, StepOutcome, StepProgress, TimerId, WaitSourceId, YieldShape,
};

// -- 1. Done variant w/ NoProgress -------------------------------------------

#[test]
fn step_op_trait_has_associated_output_and_progress_types() {
    struct OneShotOp;

    impl StepOp for OneShotOp {
        type Output = u32;
        type Progress = NoProgress;

        fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress> {
            StepOutcome::Done(7u32)
        }
    }

    let mut op = OneShotOp;
    let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
    let out = op.step(&mut ctx);
    assert_eq!(out, StepOutcome::Done(7u32));
}

// -- 2. Yield with progress + shape ------------------------------------------

#[test]
fn step_op_can_yield_with_progress_and_shape() {
    struct YieldingOp;

    impl StepOp for YieldingOp {
        type Output = ();
        type Progress = ByteProgress;

        fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress> {
            StepOutcome::Yield {
                progress: ByteProgress::new(64),
                shape: YieldShape::OnWaitSource {
                    source: WaitSourceId::new(1),
                    interests: InterestMask::new(0b1),
                },
            }
        }
    }

    let mut op = YieldingOp;
    let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
    let out = op.step(&mut ctx);

    match out {
        StepOutcome::Yield { progress, shape } => {
            assert_eq!(progress, ByteProgress::new(64));
            match shape {
                YieldShape::OnWaitSource { source, interests } => {
                    assert_eq!(source, WaitSourceId::new(1));
                    assert_eq!(interests, InterestMask::new(0b1));
                }
                YieldShape::OnAgent { .. } => panic!("expected OnWaitSource, got OnAgent"),
                YieldShape::OnTimer { .. } => panic!("expected OnWaitSource, got OnTimer"),
            }
        }
        other => panic!("expected Yield, got {:?}", other),
    }
}

// -- 3. Continue ---------------------------------------------------------------

#[test]
fn step_op_can_continue() {
    struct ContinuingOp;

    impl StepOp for ContinuingOp {
        type Output = ();
        type Progress = ByteProgress;

        fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress> {
            StepOutcome::Continue {
                progress: ByteProgress::new(8),
            }
        }
    }

    let mut op = ContinuingOp;
    let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
    let out = op.step(&mut ctx);

    match out {
        StepOutcome::Continue { progress } => {
            assert_eq!(progress, ByteProgress::new(8));
        }
        other => panic!("expected Continue, got {:?}", other),
    }
}

// -- 4. Compile-only: the associated Progress type carries the StepProgress bound

#[test]
fn step_op_progress_associated_type_must_implement_step_progress() {
    fn assert_bound<O: StepOp>() {
        fn _f<T: StepProgress>() {}
        _f::<O::Progress>();
    }

    // Instantiate against a real impl so the bound is actually checked.
    struct ProbeOp;
    impl StepOp for ProbeOp {
        type Output = ();
        type Progress = NoProgress;
        fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress> {
            StepOutcome::Done(())
        }
    }

    assert_bound::<ProbeOp>();
}

// -- 5. ScriptCtx placeholder is constructible --------------------------------

#[test]
fn script_ctx_is_constructible() {
    // Default type param `I = ProcessIdentity` doesn't auto-resolve
    // when there's no contextual constraint; name it explicitly.
    let _ = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
}

// -- 6. apply_resume default accepts Retry, rejects everything else -----------

struct DefaultResumeOp;

impl StepOp for DefaultResumeOp {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(())
    }
}

#[test]
fn apply_resume_default_accepts_retry() {
    let mut op = DefaultResumeOp;
    assert_eq!(op.apply_resume(ResumeOutcome::Retry), Ok(()));
}

#[test]
fn apply_resume_default_rejects_with_reply() {
    let mut op = DefaultResumeOp;
    let r = op.apply_resume(ResumeOutcome::WithReply(DelegateReply::placeholder()));
    assert_eq!(r, Err(Errno::EINVAL));
}

#[test]
fn apply_resume_default_rejects_timer_expired() {
    let mut op = DefaultResumeOp;
    let r = op.apply_resume(ResumeOutcome::TimerExpired(TimerId::new(99)));
    assert_eq!(r, Err(Errno::EINVAL));
}

#[test]
fn apply_resume_default_rejects_aborted_variants() {
    let mut op = DefaultResumeOp;
    for reason in [
        AbortReason::Interrupted,
        AbortReason::Killed,
        AbortReason::TimedOut,
        AbortReason::ScopeAbandoned,
    ] {
        assert_eq!(
            op.apply_resume(ResumeOutcome::Aborted(reason)),
            Err(Errno::EINVAL),
            "default impl rejects Aborted({:?})",
            reason
        );
    }
}

// -- 7. Override accepts WithReply --------------------------------------------

#[test]
fn apply_resume_override_can_accept_with_reply_and_stash_in_self() {
    struct DelegateOp {
        last_reply: Option<DelegateReply>,
    }

    impl StepOp for DelegateOp {
        type Output = ();
        type Progress = NoProgress;
        fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress> {
            StepOutcome::Done(())
        }
        fn apply_resume(&mut self, resume: ResumeOutcome) -> Result<(), Errno> {
            match resume {
                ResumeOutcome::WithReply(r) => {
                    self.last_reply = Some(r);
                    Ok(())
                }
                ResumeOutcome::Retry => Ok(()),
                _ => Err(Errno::EINVAL),
            }
        }
    }

    let mut op = DelegateOp { last_reply: None };
    assert!(op.last_reply.is_none());
    op.apply_resume(ResumeOutcome::WithReply(DelegateReply::placeholder()))
        .expect("override accepts WithReply");
    assert!(op.last_reply.is_some());
}
