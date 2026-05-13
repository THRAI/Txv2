//! v3 step-outcome / yield-shape constructor-helper pin tests.
//!
//! Wave-5 of the v3 TDD migration adds ergonomic helpers to
//! `StepOutcome<T, P>` and `YieldShape` so wave-6 fan-out workers don't
//! have to hand-roll struct literals at every cascade-probe call site.
//! These tests pin the helper surface so future PRs can't quietly drop
//! a constructor or change its parameter order.
//!
//! txdoc cross-refs (canonical anchors from `docs/Txv3/03_STEP_MODEL_v2.md`):
//! - txdoc:TXV3-STEP-MODEL-V2 (entire algebra)
//! - txdoc:STEP-V2-OUTCOME-ALGEBRA-1 (four-variant outcome is closed)
//! - txdoc:STEP-V2-YIELD-SHAPE-1 (YieldShape is a closed catalog)

use tx_substrate::step::{ByteProgress, Errno, NoProgress, StepOutcome, StepProgress, YieldShape};

#[test]
fn step_outcome_done_helper_constructs_done_variant() {
    let outcome: StepOutcome<u32, NoProgress> = StepOutcome::done(7);
    assert_eq!(outcome, StepOutcome::Done(7));
}

#[test]
fn step_outcome_err_helper_constructs_err_variant() {
    let outcome: StepOutcome<(), NoProgress> = StepOutcome::err(Errno::EAGAIN);
    assert_eq!(outcome, StepOutcome::Err(Errno::EAGAIN));
}

#[test]
fn step_outcome_continue_with_helper_constructs_continue_variant() {
    let outcome: StepOutcome<(), ByteProgress> = StepOutcome::continue_with(ByteProgress::new(64));
    match outcome {
        StepOutcome::Continue { progress } => {
            assert_eq!(progress.bytes(), 64);
        }
        _ => panic!("expected Continue"),
    }
}

#[test]
fn step_outcome_yield_on_wait_source_helper_constructs_yield_onwaitsource() {
    let outcome: StepOutcome<(), NoProgress> =
        StepOutcome::yield_on_wait_source(NoProgress, 7, 0b101);
    match outcome {
        StepOutcome::Yield { progress, shape } => {
            assert!(progress.is_empty());
            match shape {
                YieldShape::OnWaitSource { source, interests } => {
                    assert_eq!(source.raw(), 7);
                    assert_eq!(interests.raw(), 0b101);
                }
                _ => panic!("expected OnWaitSource shape"),
            }
        }
        _ => panic!("expected Yield"),
    }
}

#[test]
fn yield_shape_on_wait_source_helper_zero_translates_source_and_interests() {
    let shape = YieldShape::on_wait_source(11, 0b1100);
    match shape {
        YieldShape::OnWaitSource { source, interests } => {
            assert_eq!(source.raw(), 11);
            assert_eq!(interests.raw(), 0b1100);
        }
        _ => panic!("expected OnWaitSource"),
    }
}

#[test]
fn step_outcome_helpers_are_const() {
    // Pin `const fn`-ness of every helper. If any helper loses its
    // `const` qualifier, this stops compiling.
    const _DONE: StepOutcome<u32, NoProgress> = StepOutcome::done(1);
    const _ERR: StepOutcome<(), NoProgress> = StepOutcome::err(Errno::EAGAIN);
    const _CONTINUE: StepOutcome<(), NoProgress> = StepOutcome::continue_with(NoProgress);
    const _YIELD: StepOutcome<(), NoProgress> = StepOutcome::yield_on_wait_source(NoProgress, 0, 0);
    const _SHAPE: YieldShape = YieldShape::on_wait_source(0, 0);

    // Reference the constants so they're not dead-code-eliminated to
    // sidestep a const-eval bug.
    assert_eq!(_DONE, StepOutcome::Done(1));
    assert_eq!(_ERR, StepOutcome::Err(Errno::EAGAIN));
    assert!(matches!(_CONTINUE, StepOutcome::Continue { .. }));
    assert!(matches!(_YIELD, StepOutcome::Yield { .. }));
    assert!(matches!(_SHAPE, YieldShape::OnWaitSource { .. }));
}
