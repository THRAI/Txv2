//! v3 step algebra pin tests.
//!
//! These tests pin the closed-catalog shapes from
//! `docs/Txv3/03_STEP_MODEL_v2.md` so subsequent migration PRs cannot
//! silently widen the catalog or break the monoid laws on
//! `StepProgress`. Written PR-0 (test-first); the production types are
//! a minimum-viable stub in `tx_substrate::step_v3` until later PRs
//! flesh out the resolver / OnAgent / OnEdge variants under ARCH-3.
//!
//! txdoc cross-refs (canonical anchors from `docs/Txv3/03_STEP_MODEL_v2.md`):
//! - txdoc:TXV3-STEP-MODEL-V2 (entire algebra)
//! - txdoc:STEP-V2-OUTCOME-ALGEBRA-1 (four-variant outcome is closed)
//! - txdoc:STEP-V2-PROGRESS-TYPED-1 (StepProgress is a monoid)
//! - txdoc:STEP-V2-YIELD-SHAPE-1 (YieldShape is a closed catalog)
//! - txdoc:STEP-V2-DRIVER-MODE-1 (DriveMode classify matrix)

use tx_substrate::step_v3::{
    AcceptOutcome, ByteProgress, DriveMode, InterestConditions, NoProgress, StepOutcome,
    StepProgress, Translation, WakeCarrier, YieldShape,
};

// -- StepOutcome closed catalog -----------------------------------------------

#[test]
fn step_outcome_has_exactly_four_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a fifth variant
    // appears later without an ARCH-3 review, this stops compiling.
    let cases: [StepOutcome<u32, NoProgress>; 4] = [
        StepOutcome::Continue {
            progress: NoProgress,
        },
        StepOutcome::Yield {
            progress: NoProgress,
            shape: YieldShape::OnCarrier {
                carrier: WakeCarrier::new(7),
                interests: InterestConditions::new(0b101),
            },
        },
        StepOutcome::Done(42),
        StepOutcome::Err(tx_substrate::step_v3::Errno::EAGAIN),
    ];

    for outcome in cases {
        match outcome {
            StepOutcome::Continue { progress } => assert!(progress.is_empty()),
            StepOutcome::Yield { progress, shape } => {
                assert!(progress.is_empty());
                match shape {
                    YieldShape::OnCarrier { carrier, interests } => {
                        assert_eq!(carrier.raw(), 7);
                        assert_eq!(interests.raw(), 0b101);
                    }
                    YieldShape::OnAgent { .. } => {}
                }
            }
            StepOutcome::Done(v) => assert_eq!(v, 42),
            StepOutcome::Err(_) => {}
        }
    }
}

#[test]
fn step_outcome_continue_carries_progress() {
    let outcome: StepOutcome<(), ByteProgress> = StepOutcome::Continue {
        progress: ByteProgress::new(128),
    };
    if let StepOutcome::Continue { progress } = outcome {
        assert_eq!(progress.bytes(), 128);
    } else {
        panic!("expected Continue");
    }
}

#[test]
fn step_outcome_yield_carries_progress_and_shape() {
    let outcome: StepOutcome<(), ByteProgress> = StepOutcome::Yield {
        progress: ByteProgress::new(64),
        shape: YieldShape::OnCarrier {
            carrier: WakeCarrier::new(3),
            interests: InterestConditions::new(0b1),
        },
    };
    if let StepOutcome::Yield { progress, shape } = outcome {
        assert_eq!(progress.bytes(), 64);
        match shape {
            YieldShape::OnCarrier { carrier, interests } => {
                assert_eq!(carrier.raw(), 3);
                assert_eq!(interests.raw(), 0b1);
            }
            YieldShape::OnAgent { .. } => panic!("expected OnCarrier, got OnAgent"),
        }
    } else {
        panic!("expected Yield");
    }
}

// -- YieldShape closed catalog ------------------------------------------------

#[test]
fn yield_shape_has_oncarrier_variant_with_carrier_and_interests() {
    // OnAgent is reserved for PR-4 under ARCH-3. PR-0 only pins
    // OnCarrier; adding OnAgent is itself a closed-catalog extension.
    let shape = YieldShape::OnCarrier {
        carrier: WakeCarrier::new(11),
        interests: InterestConditions::new(0b1100),
    };
    match shape {
        YieldShape::OnCarrier { carrier, interests } => {
            assert_eq!(carrier.raw(), 11);
            assert_eq!(interests.raw(), 0b1100);
        }
        YieldShape::OnAgent { .. } => panic!("expected OnCarrier, got OnAgent"),
    }
}

// -- StepProgress monoid laws -------------------------------------------------
//
// (Self, EMPTY, extend) is a monoid: associative with EMPTY as identity.
// We test laws by table over deterministic seeds rather than introducing
// proptest as a dependency: the laws are total and the seed space is
// small.

const BYTE_SEEDS: [usize; 7] = [0, 1, 7, 64, 1024, 4096, 65_535];

#[test]
fn no_progress_empty_is_empty() {
    assert!(NoProgress::EMPTY.is_empty());
    assert!(NoProgress.is_empty());
}

#[test]
fn no_progress_extend_is_noop() {
    let mut p = NoProgress;
    p.extend(NoProgress);
    assert!(p.is_empty());
}

#[test]
fn byte_progress_empty_is_zero_and_is_empty() {
    assert!(ByteProgress::EMPTY.is_empty());
    assert_eq!(ByteProgress::EMPTY.bytes(), 0);
    assert!(!ByteProgress::new(1).is_empty());
}

#[test]
fn byte_progress_left_identity() {
    // EMPTY.extend(x) == x.bytes()
    for &s in &BYTE_SEEDS {
        let mut acc = ByteProgress::EMPTY;
        acc.extend(ByteProgress::new(s));
        assert_eq!(acc.bytes(), s, "left identity broke at seed {s}");
    }
}

#[test]
fn byte_progress_right_identity() {
    // x.extend(EMPTY) == x.bytes()
    for &s in &BYTE_SEEDS {
        let mut acc = ByteProgress::new(s);
        acc.extend(ByteProgress::EMPTY);
        assert_eq!(acc.bytes(), s, "right identity broke at seed {s}");
    }
}

#[test]
fn byte_progress_extend_is_associative() {
    // (a . b) . c == a . (b . c)
    for &a in &BYTE_SEEDS {
        for &b in &BYTE_SEEDS {
            for &c in &BYTE_SEEDS {
                let mut left = ByteProgress::new(a);
                left.extend(ByteProgress::new(b));
                left.extend(ByteProgress::new(c));

                let mut right_inner = ByteProgress::new(b);
                right_inner.extend(ByteProgress::new(c));
                let mut right = ByteProgress::new(a);
                right.extend(right_inner);

                assert_eq!(
                    left.bytes(),
                    right.bytes(),
                    "associativity broke at ({a}, {b}, {c})"
                );
            }
        }
    }
}

#[test]
fn byte_progress_extend_accumulates() {
    let mut p = ByteProgress::new(7);
    p.extend(ByteProgress::new(13));
    assert_eq!(p.bytes(), 20);
}

// -- DriveMode classify matrix ------------------------------------------------
//
// Per docs/Txv3/03_STEP_MODEL_v2.md §5.1. PR-0 covers OnCarrier only;
// the OnAgent rows are added in PR-4.

fn carrier_shape() -> YieldShape {
    YieldShape::OnCarrier {
        carrier: WakeCarrier::new(0),
        interests: InterestConditions::new(0),
    }
}

#[test]
fn classify_nonblocking_oncarrier_empty_progress_translates_to_eagain() {
    let outcome = DriveMode::Nonblocking.classify(&carrier_shape(), true);
    assert_eq!(outcome, AcceptOutcome::Translate(Translation::Eagain));
}

#[test]
fn classify_nonblocking_oncarrier_with_progress_translates_to_partial_return() {
    let outcome = DriveMode::Nonblocking.classify(&carrier_shape(), false);
    assert_eq!(
        outcome,
        AcceptOutcome::Translate(Translation::PartialReturn)
    );
}

#[test]
fn classify_waiting_oncarrier_resolves() {
    let outcome = DriveMode::Waiting.classify(&carrier_shape(), true);
    assert_eq!(outcome, AcceptOutcome::Resolve);
    let outcome = DriveMode::Waiting.classify(&carrier_shape(), false);
    assert_eq!(outcome, AcceptOutcome::Resolve);
}

#[test]
fn classify_selecting_oncarrier_resolves() {
    let outcome = DriveMode::Selecting.classify(&carrier_shape(), true);
    assert_eq!(outcome, AcceptOutcome::Resolve);
}
