//! v3 binding-obligation catalog pin tests.
//!
//! These tests pin the closed-catalog shape for `BindingObligation`,
//! the substrate-level vocabulary describing the strength required of
//! a signifier→identity binding at a given dereference site. Wave 3
//! lands the catalog and its total-order helpers so subsequent
//! migration PRs cannot silently widen the catalog or shift the
//! ordering relation between members.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (consumed by cap-side dereference sites)
//! - txdoc:CONCEPTS-V5-BINDINGS-1 (canonical bindings/obligations section)

use tx_substrate::step::BindingObligation;

// The helpers must remain const-callable, and these catalog truths must hold.
const _: () = {
    assert!(BindingObligation::ResolutionOnly.rank() == 0);
    assert!(BindingObligation::Addressability.rank() == 1);
    assert!(BindingObligation::Operational.rank() == 2);
    assert!(BindingObligation::Operational.at_least(BindingObligation::Operational));
    assert!(BindingObligation::Operational.at_least(BindingObligation::ResolutionOnly));
    assert!(BindingObligation::Operational.requires_operability());
    assert!(!BindingObligation::ResolutionOnly.requires_operability());
};

// -- BindingObligation closed catalog ----------------------------------------

#[test]
fn binding_obligation_has_exactly_three_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a fourth variant
    // appears later without an ARCH-3 review, this stops compiling.
    let cases: [BindingObligation; 3] = [
        BindingObligation::ResolutionOnly,
        BindingObligation::Addressability,
        BindingObligation::Operational,
    ];

    for obligation in cases {
        match obligation {
            BindingObligation::ResolutionOnly => {}
            BindingObligation::Addressability => {}
            BindingObligation::Operational => {}
        }
    }
}

// -- Total-order helpers -----------------------------------------------------

#[test]
fn binding_obligation_rank_is_total_order() {
    // Spec'd ranks: ResolutionOnly = 0, Addressability = 1,
    // Operational = 2.
    assert_eq!(BindingObligation::ResolutionOnly.rank(), 0);
    assert_eq!(BindingObligation::Addressability.rank(), 1);
    assert_eq!(BindingObligation::Operational.rank(), 2);

    // All three are distinct.
    let r0 = BindingObligation::ResolutionOnly.rank();
    let r1 = BindingObligation::Addressability.rank();
    let r2 = BindingObligation::Operational.rank();
    assert_ne!(r0, r1);
    assert_ne!(r1, r2);
    assert_ne!(r0, r2);

    // Operational > Addressability > ResolutionOnly via rank().
    assert!(
        BindingObligation::Operational.rank() > BindingObligation::Addressability.rank(),
        "Operational must outrank Addressability",
    );
    assert!(
        BindingObligation::Addressability.rank() > BindingObligation::ResolutionOnly.rank(),
        "Addressability must outrank ResolutionOnly",
    );
}

#[test]
fn binding_obligation_at_least_reflexive() {
    // For each member, m.at_least(m) == true.
    let cases: [BindingObligation; 3] = [
        BindingObligation::ResolutionOnly,
        BindingObligation::Addressability,
        BindingObligation::Operational,
    ];
    for obligation in cases {
        assert!(
            obligation.at_least(obligation),
            "at_least must be reflexive for {obligation:?}",
        );
    }
}

#[test]
fn binding_obligation_at_least_transitive() {
    // Operational ≥ Addressability and Addressability ≥ ResolutionOnly,
    // so Operational ≥ ResolutionOnly (transitive through
    // Addressability).
    assert!(
        BindingObligation::Operational.at_least(BindingObligation::Addressability),
        "Operational must discharge Addressability",
    );
    assert!(
        BindingObligation::Addressability.at_least(BindingObligation::ResolutionOnly),
        "Addressability must discharge ResolutionOnly",
    );
    assert!(
        BindingObligation::Operational.at_least(BindingObligation::ResolutionOnly),
        "Operational must discharge ResolutionOnly transitively",
    );
}

#[test]
fn binding_obligation_at_least_antisymmetric() {
    // Weaker obligations cannot discharge stronger ones.
    assert!(
        !BindingObligation::ResolutionOnly.at_least(BindingObligation::Operational),
        "ResolutionOnly must not discharge Operational",
    );
    assert!(
        !BindingObligation::Addressability.at_least(BindingObligation::Operational),
        "Addressability must not discharge Operational",
    );
    assert!(
        !BindingObligation::ResolutionOnly.at_least(BindingObligation::Addressability),
        "ResolutionOnly must not discharge Addressability",
    );
}

// -- requires_operability table ----------------------------------------------

#[test]
fn binding_obligation_requires_operability_only_for_operational() {
    let cases: &[(BindingObligation, bool)] = &[
        (BindingObligation::ResolutionOnly, false),
        (BindingObligation::Addressability, false),
        (BindingObligation::Operational, true),
    ];
    for &(obligation, expected) in cases {
        assert_eq!(
            obligation.requires_operability(),
            expected,
            "requires_operability broke for {obligation:?}",
        );
    }
}
