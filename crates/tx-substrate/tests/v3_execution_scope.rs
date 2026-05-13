//! v3 execution-scope catalog pin tests.
//!
//! Pins the closed-catalog shape for `ExecutionScope`. PR-11 phase 0
//! reshaped the `OnBehalfOf` variant to carry a real `Cap<I>` (generic
//! over `I: SubjectIdentity`); the unit-typed `OwnedProcessHandle`
//! placeholder is retired. Catalog extension is gated on ARCH-3
//! review.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (step model algebra; ExecutionScope is
//!   the identity-context modifier orthogonal to YieldShape)
//! - txdoc:TXV3-EXECUTION-SCOPE-V1 (full ExecutionScope spec)
//! - txdoc:SCOPE-V1-CATALOG-1 (closed catalog)

use tx_substrate::epoch;
use tx_substrate::step::{ExecutionScope, ProcessIdentity};
use tx_substrate::zone::{self, register_zone_for, reserve_for, sign_for, Cap};

static ZONE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn reset_zone_and_epoch() -> std::sync::MutexGuard<'static, ()> {
    let guard = ZONE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    epoch::testing::init_for_test();
    zone::testing::init_for_test(
        4096,
        tx_substrate::page_allocator::testing::direct_map_base_for_test(),
    )
    .expect("test zone runtime init");
    register_zone_for::<ProcessIdentity>().expect("register placeholder process zone");
    guard
}

fn mint_process_cap() -> Cap<ProcessIdentity> {
    let reservation = reserve_for::<ProcessIdentity>().expect("reserve process placeholder");
    sign_for(reservation, ProcessIdentity::placeholder())
}

// -- ExecutionScope closed catalog -------------------------------------------

#[test]
fn execution_scope_has_exactly_two_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a third variant appears
    // later without an ARCH-3 review, this stops compiling.
    let _g = reset_zone_and_epoch();
    let cap = mint_process_cap();
    let cases: [ExecutionScope; 2] = [
        ExecutionScope::Thread,
        ExecutionScope::OnBehalfOf(cap.clone()),
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
    let thread: ExecutionScope = ExecutionScope::Thread;
    assert!(thread.is_thread(), "Thread.is_thread() must be true");
    assert!(!thread.is_borrowed(), "Thread.is_borrowed() must be false",);
    assert!(
        thread.borrowed_owner().is_none(),
        "Thread.borrowed_owner() must be None",
    );
}

#[test]
fn execution_scope_on_behalf_of_is_not_thread() {
    let _g = reset_zone_and_epoch();
    let cap = mint_process_cap();
    let borrowed = ExecutionScope::OnBehalfOf(cap.clone());
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
        Some(&cap),
        "OnBehalfOf.borrowed_owner() must return the principal cap",
    );
}

#[test]
fn execution_scope_borrowed_owner_round_trips_cap() {
    // Pin: the cap returned by borrowed_owner is the exact one passed
    // to OnBehalfOf. Cap equality is raw-key equality, which means a
    // clone compares equal to the original (clone bumps EBR retain
    // but doesn't change the slot key).
    let _g = reset_zone_and_epoch();
    let cap = mint_process_cap();
    let scope = ExecutionScope::OnBehalfOf(cap.clone());
    let recovered = scope.borrowed_owner().expect("OnBehalfOf carries owner");
    assert_eq!(recovered, &cap, "borrowed_owner must round-trip cap");
}
