//! v3 execution-scope catalog pin tests.
//!
//! These tests pin the closed-catalog shape for `ExecutionScope` and the
//! placeholder `OwnedProcessHandle`. Wave 3 lands the enum + helpers only;
//! the actual borrow primitive (`with_on_behalf_of` async fn), abandonment
//! routing, and resource-scoping machinery are deferred to PR-7 of the v3
//! TDD migration plan. Wave 3 just makes the closed catalog representable
//! so subsequent migration PRs cannot silently widen it or shift helper
//! semantics.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (step model algebra; ExecutionScope is the
//!   identity-context modifier orthogonal to YieldShape)
//! - txdoc:TXV3-EXECUTION-SCOPE-V1 (full ExecutionScope spec)

use tx_substrate::step_v3::{ExecutionScope, OwnedProcessHandle};

// -- ExecutionScope closed catalog -------------------------------------------

#[test]
fn execution_scope_has_exactly_two_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a third variant appears
    // later without an ARCH-3 review, this stops compiling.
    let cases: [ExecutionScope; 2] = [
        ExecutionScope::Thread,
        ExecutionScope::OnBehalfOf(OwnedProcessHandle::placeholder()),
    ];

    for scope in cases {
        match scope {
            ExecutionScope::Thread => {}
            ExecutionScope::OnBehalfOf(_) => {}
        }
    }
}

// -- ExecutionScope helper-method semantics ----------------------------------

#[test]
fn execution_scope_thread_is_not_borrowed() {
    let thread = ExecutionScope::Thread;
    assert!(thread.is_thread(), "Thread.is_thread() must be true");
    assert!(
        !thread.is_borrowed(),
        "Thread.is_borrowed() must be false",
    );
    assert_eq!(
        thread.borrowed_owner(),
        None,
        "Thread.borrowed_owner() must be None",
    );
}

#[test]
fn execution_scope_on_behalf_of_is_not_thread() {
    let handle = OwnedProcessHandle::placeholder();
    let borrowed = ExecutionScope::OnBehalfOf(handle);
    assert!(
        !borrowed.is_thread(),
        "OnBehalfOf.is_thread() must be false",
    );
    assert!(
        borrowed.is_borrowed(),
        "OnBehalfOf.is_borrowed() must be true",
    );
    assert_eq!(
        borrowed.borrowed_owner(),
        Some(handle),
        "OnBehalfOf.borrowed_owner() must return the handle",
    );
}

#[test]
fn execution_scope_borrowed_owner_round_trips_handle() {
    // Pin: the handle returned by borrowed_owner is the exact one passed
    // to OnBehalfOf. (Eq round-trip; once the placeholder is replaced by
    // a real Cap<ProcessIdentity> in PR-7, this should pin the same
    // identity equality.)
    let handle = OwnedProcessHandle::placeholder();
    let scope = ExecutionScope::OnBehalfOf(handle);
    let recovered = scope.borrowed_owner().expect("OnBehalfOf carries owner");
    assert_eq!(recovered, handle, "borrowed_owner must round-trip handle");
}

// -- OwnedProcessHandle placeholder ------------------------------------------

#[test]
fn owned_process_handle_placeholder_is_constructible() {
    // The placeholder constructor must be reachable from outside the
    // crate, and two calls must compare equal (it is the unique
    // placeholder value until PR-7 swaps in Cap<ProcessIdentity>).
    let a = OwnedProcessHandle::placeholder();
    let b = OwnedProcessHandle::placeholder();
    assert_eq!(a, b, "placeholder() must be the unique placeholder value");
}

// -- const-fn shape pin ------------------------------------------------------

// txdoc:SCOPE-1
// Compile-only: invoke is_thread / is_borrowed / borrowed_owner in a
// const context to pin their `const fn` shape. If a future change drops
// `const`, this stops compiling.
const _THREAD_SCOPE: ExecutionScope = ExecutionScope::Thread;
const _IS_THREAD: bool = _THREAD_SCOPE.is_thread();
const _IS_BORROWED: bool = _THREAD_SCOPE.is_borrowed();
const _BORROWED_OWNER: Option<OwnedProcessHandle> = _THREAD_SCOPE.borrowed_owner();

#[test]
fn execution_scope_helpers_are_const() {
    // Reference the const-evaluated values so the compiler must keep
    // them. The actual pinning happens at compile time above.
    assert!(_IS_THREAD);
    assert!(!_IS_BORROWED);
    assert!(_BORROWED_OWNER.is_none());
}
