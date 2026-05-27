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

use tx_substrate::step::{
    AcceptOutcome, AgentCancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken,
    DriveMode, InterestMask, TimerId, TokenDropPolicy, Translation, WaitSourceId, YieldShape,
};

fn on_agent_shape() -> YieldShape {
    YieldShape::OnAgent {
        endpoint: DelegateEndpoint::placeholder(),
        request: DelegateRequest::Placeholder,
        token: DelegateToken::placeholder(),
        deadline: Deadline::NEVER,
        cancel: AgentCancelPolicy::BestEffort,
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
        cancel: AgentCancelPolicy::Synchronous,
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
            assert_eq!(cancel, AgentCancelPolicy::Synchronous);
        }
        YieldShape::OnWaitSource { .. } => panic!("expected OnAgent, got OnWaitSource"),
        YieldShape::OnTimer { .. } => panic!("expected OnAgent, got OnTimer"),
        YieldShape::OnEdge { .. } => panic!("expected OnAgent, got OnEdge"),
    }
}

#[test]
fn yield_shape_has_exactly_three_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a fourth variant
    // appears later without an ARCH-3 review, this stops compiling.
    let cases: [YieldShape; 3] = [
        YieldShape::OnWaitSource {
            source: WaitSourceId::new(0),
            interests: InterestMask::new(0),
        },
        on_agent_shape(),
        YieldShape::OnTimer {
            token: TimerId::new(0),
            deadline: Deadline::NEVER,
        },
    ];

    for shape in cases {
        match shape {
            YieldShape::OnWaitSource { .. } => {}
            YieldShape::OnAgent { .. } => {}
            YieldShape::OnTimer { .. } => {}
            YieldShape::OnEdge { .. } => {}
        }
    }
}

// -- AgentCancelPolicy closed catalog ----------------------------------------------

#[test]
fn cancel_policy_has_exactly_three_variants_via_exhaustive_match() {
    let cases: [AgentCancelPolicy; 3] = [
        AgentCancelPolicy::BestEffort,
        AgentCancelPolicy::Synchronous,
        AgentCancelPolicy::Detached,
    ];
    for policy in cases {
        match policy {
            AgentCancelPolicy::BestEffort => {}
            AgentCancelPolicy::Synchronous => {}
            AgentCancelPolicy::Detached => {}
        }
    }
}

// -- DelegateRequest closed catalog -------------------------------------------

#[test]
fn delegate_request_catalog_is_a_closed_sum() {
    // The catalog grew in PR-10 phase 4 (per D7 §3.2 + W-T's flag) to
    // a closed sum keyed on endpoint kind, symmetric to
    // `DelegateReply`. `Placeholder` is retained for substrate-test
    // friendliness; the `Ufd` arm is the first real typed variant.
    // Adding a new variant without an ARCH-3 review fails to compile
    // here (no wildcard).
    use tx_substrate::step::{UfdAccessKind, UfdRequest};
    let placeholder = DelegateRequest::Placeholder;
    match placeholder {
        DelegateRequest::Placeholder => {}
        DelegateRequest::Ufd(_) => unreachable!("placeholder is not ufd"),
    }
    let ufd = DelegateRequest::Ufd(UfdRequest::PageFault {
        faulting_addr: 0x1000,
        access_kind: UfdAccessKind::Missing,
        faulting_tid: 0,
    });
    match ufd {
        DelegateRequest::Placeholder => unreachable!("ufd is not placeholder"),
        DelegateRequest::Ufd(UfdRequest::PageFault { faulting_addr, .. }) => {
            assert_eq!(faulting_addr, 0x1000);
        }
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

// -- TokenDropPolicy closed catalog -------------------------------------------

#[test]
fn token_drop_policy_exhaustive_match_smoke() {
    let cases: [TokenDropPolicy; 2] = [TokenDropPolicy::CancelOnDrop, TokenDropPolicy::Abandon];
    for policy in cases {
        match policy {
            TokenDropPolicy::CancelOnDrop => {}
            TokenDropPolicy::Abandon => {}
        }
    }
}

#[test]
fn token_drop_policy_and_agent_cancel_policy_compose_orthogonally() {
    // Sanity: the two enums are independent types.
    let drop = TokenDropPolicy::CancelOnDrop;
    let cancel = AgentCancelPolicy::Synchronous;
    assert_eq!(drop, TokenDropPolicy::CancelOnDrop);
    assert_eq!(cancel, AgentCancelPolicy::Synchronous);
}
