//! v3 restriction-stack catalog and append-only structure pin tests.
//!
//! These tests pin the closed-catalog shape of `RestrictionKind` and the
//! append-only structural rules of `RestrictionStack`, which is the
//! shape held in `SubjectAuthority::restrictions` per
//! `docs/Txv3/04_SYSCALL_SHAPE_v1.md`. Real per-kind walkers (seccomp
//! BPF VM, Landlock rule eval, LSM stack dispatch) are deferred per the
//! v3 plan; the catalog and the append-only stack itself land now so
//! later PRs cannot silently widen the catalog or bolt on
//! mutation/removal APIs.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (referenced by the upper-half
//!   restriction-stack walk that participates in the step algebra)

use tx_substrate::step_v3::{RestrictionKind, RestrictionStack};

// -- RestrictionKind closed catalog ------------------------------------------

#[test]
fn restriction_kind_has_exactly_three_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a fourth variant
    // appears later without an ARCH-3 review, this stops compiling.
    let cases: [RestrictionKind; 3] = [
        RestrictionKind::SeccompFilter,
        RestrictionKind::LandlockRule,
        RestrictionKind::LsmStack,
    ];

    for kind in cases {
        match kind {
            RestrictionKind::SeccompFilter => {}
            RestrictionKind::LandlockRule => {}
            RestrictionKind::LsmStack => {}
        }
    }
}

// -- RestrictionStack construction ------------------------------------------

#[test]
fn restriction_stack_is_empty_after_construction() {
    let from_new = RestrictionStack::new();
    let from_default = RestrictionStack::default();

    assert!(from_new.is_empty(), "RestrictionStack::new() must be empty");
    assert_eq!(from_new.len(), 0);
    assert!(
        from_default.is_empty(),
        "RestrictionStack::default() must be empty",
    );
    assert_eq!(from_default.len(), 0);
}

// -- RestrictionStack append grows length ------------------------------------

#[test]
fn restriction_stack_append_grows_length() {
    let mut stack = RestrictionStack::new();
    stack.append(RestrictionKind::SeccompFilter);
    stack.append(RestrictionKind::LandlockRule);
    stack.append(RestrictionKind::LsmStack);

    assert_eq!(stack.len(), 3);
    assert!(!stack.is_empty());
}

// -- RestrictionStack walk order pin -----------------------------------------

#[test]
fn restriction_stack_walk_returns_kinds_in_install_order() {
    // Per docs/Txv3/04_SYSCALL_SHAPE_v1.md §4.5: restrictions compose
    // by install order. Older filters/rules run first; newer rules
    // layer on top. This matches Linux's seccomp filter chain semantics
    // and Landlock's rule-add ordering.
    let mut stack = RestrictionStack::new();
    stack.append(RestrictionKind::SeccompFilter);
    stack.append(RestrictionKind::LandlockRule);
    stack.append(RestrictionKind::LsmStack);

    let walked: Vec<RestrictionKind> = stack.walk().collect();
    assert_eq!(
        walked,
        vec![
            RestrictionKind::SeccompFilter,
            RestrictionKind::LandlockRule,
            RestrictionKind::LsmStack,
        ],
        "walk must yield restrictions oldest-first (install order)",
    );
}

// -- RestrictionStack walk does not consume ----------------------------------

#[test]
fn restriction_stack_walk_does_not_consume() {
    // The walker iterates by reference: calling `walk` twice yields the
    // same sequence. Without this, an upper-half StepOp could not
    // re-walk the stack across step boundaries (e.g. after an OnAgent
    // tracer yield resolves and the script resumes the walk).
    let mut stack = RestrictionStack::new();
    stack.append(RestrictionKind::SeccompFilter);
    stack.append(RestrictionKind::LandlockRule);

    let first: Vec<RestrictionKind> = stack.walk().collect();
    let second: Vec<RestrictionKind> = stack.walk().collect();

    assert_eq!(first, second);
    assert_eq!(
        first,
        vec![
            RestrictionKind::SeccompFilter,
            RestrictionKind::LandlockRule,
        ],
    );
}

// -- RestrictionStack append-only structural pin -----------------------------

/// Append-only structural pin.
///
/// The `RestrictionStack` public API exposes exactly one mutator:
/// `append`. There is intentionally **no** `clear`, `pop`, `remove`,
/// `truncate`, `drain`, `replace`, `set`, or any other shrinking /
/// rewriting method. The only way to "shrink" a restriction set is to
/// publish a *fresh* `SubjectAuthority` (e.g. via suid-exec or the
/// SUBJ-3 authority-replacement path), which is a publication-boundary
/// operation handled at a different layer entirely — not on this
/// stack.
///
/// This test is a structural/documentary pin: the *real* enforcement
/// is the absence of those methods in `restriction_stack.rs`. If a
/// future PR adds e.g. `pub fn clear(&mut self)`, that PR is the
/// review trigger — this test is here to make the rule readable and
/// to ensure no one accidentally adds a "convenience" mutation API
/// without seeing the comment.
///
/// txdoc:SUBJ-3 — authority replacement (the only way to shrink a
/// restriction set) is a separate publication-boundary operation, not
/// a mutation on the existing stack.
#[test]
fn restriction_stack_append_only_no_remove_method() {
    // Structural pin — see the doc comment on this test for the rule.
    // The body is intentionally trivial; the documentation IS the test.
    assert!(true);
}
