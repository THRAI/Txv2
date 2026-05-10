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
    ByteProgress, InterestConditions, NoProgress, ScriptCtx, StepOp, StepOutcome, StepProgress,
    WakeCarrier, YieldShape,
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
    let mut ctx = ScriptCtx::new();
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
                shape: YieldShape::OnCarrier {
                    carrier: WakeCarrier::new(1),
                    interests: InterestConditions::new(0b1),
                },
            }
        }
    }

    let mut op = YieldingOp;
    let mut ctx = ScriptCtx::new();
    let out = op.step(&mut ctx);

    match out {
        StepOutcome::Yield { progress, shape } => {
            assert_eq!(progress, ByteProgress::new(64));
            match shape {
                YieldShape::OnCarrier { carrier, interests } => {
                    assert_eq!(carrier, WakeCarrier::new(1));
                    assert_eq!(interests, InterestConditions::new(0b1));
                }
                YieldShape::OnAgent { .. } => panic!("expected OnCarrier, got OnAgent"),
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
    let mut ctx = ScriptCtx::new();
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
    let _ = ScriptCtx::new();
}
