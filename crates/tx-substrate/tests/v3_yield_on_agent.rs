//! v3 `YieldShape::OnAgent` pin tests.
//!
//! These tests pin the second member of the closed `YieldShape` catalog
//! and the rows of the `DriveMode::classify` matrix that involve
//! `OnAgent`. PR-4 of the v3 TDD migration plan replaces the placeholder
//! `DelegateEndpoint`, `DelegateToken`, and `DelegateRequest` types with
//! real cap-typed zone primitives; today's placeholders are exactly
//! enough to make `YieldShape::OnAgent` representable so the closed
//! catalog and the classify matrix are completable.
//!
//! txdoc cross-refs (canonical anchors from `docs/Txv3/03_STEP_MODEL_v2.md`):
//! - txdoc:STEP-V2-YIELD-SHAPE-1 (YieldShape catalog grows to two members)
//! - txdoc:STEP-V2-DRIVER-MODE-1 (DriveMode classify matrix, OnAgent rows)
//!
//! See also `docs/Txv3/05_DELEGATE_v1.md` for the delegate-side context
//! that PR-4 will replace these placeholders with.

use tx_substrate::step_v3::{
    AcceptOutcome, CancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken,
    DriveMode, InterestConditions, Translation, WakeCarrier, YieldShape,
};

fn on_agent_shape() -> YieldShape {
    YieldShape::OnAgent {
        endpoint: DelegateEndpoint::placeholder(),
        request: DelegateRequest::Placeholder,
        token: DelegateToken::placeholder(),
        deadline: Deadline::NEVER,
        cancel: CancelPolicy::BestEffort,
    }
}

// -- YieldShape::OnAgent shape ------------------------------------------------

#[test]
fn yield_shape_on_agent_constructs_with_placeholder_types() {
    let shape = YieldShape::OnAgent {
        endpoint: DelegateEndpoint::placeholder(),
        request: DelegateRequest::Placeholder,
        token: DelegateToken::placeholder(),
        deadline: Deadline::from_raw(42),
        cancel: CancelPolicy::Synchronous,
    };
    match shape {
        YieldShape::OnAgent {
            endpoint,
            request,
            token,
            deadline,
            cancel,
        } => {
            assert_eq!(endpoint, DelegateEndpoint::placeholder());
            assert_eq!(request, DelegateRequest::Placeholder);
            assert_eq!(token, DelegateToken::placeholder());
            assert_eq!(deadline.raw(), 42);
            assert_eq!(cancel, CancelPolicy::Synchronous);
        }
        YieldShape::OnCarrier { .. } => panic!("expected OnAgent, got OnCarrier"),
    }
}

#[test]
fn yield_shape_has_exactly_two_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a third variant
    // appears later without an ARCH-3 review, this stops compiling.
    let cases: [YieldShape; 2] = [
        YieldShape::OnCarrier {
            carrier: WakeCarrier::new(0),
            interests: InterestConditions::new(0),
        },
        on_agent_shape(),
    ];

    for shape in cases {
        match shape {
            YieldShape::OnCarrier { .. } => {}
            YieldShape::OnAgent { .. } => {}
        }
    }
}

// -- CancelPolicy closed catalog ----------------------------------------------

#[test]
fn cancel_policy_has_exactly_three_variants_via_exhaustive_match() {
    let cases: [CancelPolicy; 3] = [
        CancelPolicy::BestEffort,
        CancelPolicy::Synchronous,
        CancelPolicy::Detached,
    ];
    for policy in cases {
        match policy {
            CancelPolicy::BestEffort => {}
            CancelPolicy::Synchronous => {}
            CancelPolicy::Detached => {}
        }
    }
}

// -- DelegateRequest closed catalog (intentionally minimal until PR-4) --------

#[test]
fn delegate_request_placeholder_is_the_only_variant_today() {
    // The catalog is intentionally minimal until PR-4 fills it with
    // typed per-EndpointKind variants (UfdRequest, FuseRequest, …).
    let req = DelegateRequest::Placeholder;
    match req {
        DelegateRequest::Placeholder => {}
    }
}

// -- Deadline sentinel --------------------------------------------------------

#[test]
fn deadline_never_is_the_max_u64() {
    assert_eq!(Deadline::NEVER.raw(), u64::MAX);
}

// -- DriveMode::classify matrix, OnAgent rows --------------------------------
//
// Per docs/Txv3/03_STEP_MODEL_v2.md §5.1.

#[test]
fn classify_waiting_on_agent_resolves() {
    let outcome = DriveMode::Waiting.classify(&on_agent_shape(), true);
    assert_eq!(outcome, AcceptOutcome::Resolve);
    let outcome = DriveMode::Waiting.classify(&on_agent_shape(), false);
    assert_eq!(outcome, AcceptOutcome::Resolve);
}

#[test]
fn classify_selecting_on_agent_translates_to_unsupported_shape() {
    // Load-bearing: Selecting cannot resolve OnAgent (per doc 03 §5.1).
    let outcome = DriveMode::Selecting.classify(&on_agent_shape(), true);
    assert_eq!(
        outcome,
        AcceptOutcome::Translate(Translation::UnsupportedShape)
    );
}

#[test]
fn classify_nonblocking_on_agent_empty_progress_translates_to_eagain() {
    let outcome = DriveMode::Nonblocking.classify(&on_agent_shape(), true);
    assert_eq!(outcome, AcceptOutcome::Translate(Translation::Eagain));
}

#[test]
fn classify_nonblocking_on_agent_with_progress_translates_to_partial_return() {
    let outcome = DriveMode::Nonblocking.classify(&on_agent_shape(), false);
    assert_eq!(
        outcome,
        AcceptOutcome::Translate(Translation::PartialReturn)
    );
}
