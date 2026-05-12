//! v3 `SubjectContext` pin tests.
//!
//! These tests pin the script-scoped identity context introduced in
//! v5 of the txKernel meta-framework. `SubjectContext` carries the
//! process identity, optional thread identity, and the
//! `SubjectAuthority` (credential + restriction-stack handle) for the
//! script frame.
//!
//! **PR-9 phase 4 reshape:** field storage is now `Cap<I>` /
//! `Cap<I::ThreadIdentity>` / `Cap<I::Credential>` / `Cap<I::Restrictions>`
//! rather than the corresponding types by value. Production callers
//! (`SyscallCtx<'a>` syscall arms, PR-9 phase 5) hand the constructors
//! clones of caps they already hold; placeholder/test callers mint
//! caps through the normal zone reserve/sign path. The placeholder
//! types (`step_v3::ProcessIdentity`, `ThreadIdentity`, `Credential`,
//! `RestrictionStackHandle`) implement `ZoneAllocated` against
//! dedicated static placeholder zones so these tests can stand up a
//! real cap without depending on `tx-subsystems`.
//!
//! The pin set covers:
//! - `SubjectContext::from_thread` carries the cap-typed process /
//!   thread / authority slots (native syscall entry path, per
//!   `04_SYSCALL_SHAPE_v1.md`).
//! - `SubjectContext::borrowed` produces a context with no per-thread
//!   identity (`OnBehalfOf<P>` borrow path).
//! - `SubjectAuthority::new` carries the cred and restriction-stack
//!   caps.
//! - SUBJ-1 structural pin: the only path to a `SubjectContext` is
//!   through the explicit constructors; there is no zero-arg
//!   `current_subject_context()` accessor.
//!
//! txdoc cross-refs:
//! - `txdoc:TXV3-STEP-MODEL-V2`
//! - `txdoc:TXV3-CONCEPTS-V5`

use tx_substrate::epoch;
use tx_substrate::step_v3::{
    Credential, ProcessIdentity, RestrictionStackHandle, SubjectAuthority, SubjectContext,
    ThreadIdentity,
};
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
    register_zone_for::<ThreadIdentity>().expect("register placeholder thread zone");
    register_zone_for::<Credential>().expect("register placeholder cred zone");
    register_zone_for::<RestrictionStackHandle>().expect("register placeholder restrictions zone");
    guard
}

fn mint_process_cap() -> Cap<ProcessIdentity> {
    let reservation = reserve_for::<ProcessIdentity>().expect("reserve process placeholder");
    sign_for(reservation, ProcessIdentity::placeholder())
}

fn mint_thread_cap() -> Cap<ThreadIdentity> {
    let reservation = reserve_for::<ThreadIdentity>().expect("reserve thread placeholder");
    sign_for(reservation, ThreadIdentity::placeholder())
}

fn mint_cred_cap() -> Cap<Credential> {
    let reservation = reserve_for::<Credential>().expect("reserve cred placeholder");
    sign_for(reservation, Credential::placeholder())
}

fn mint_restrictions_cap() -> Cap<RestrictionStackHandle> {
    let reservation =
        reserve_for::<RestrictionStackHandle>().expect("reserve restrictions placeholder");
    sign_for(reservation, RestrictionStackHandle::placeholder())
}

#[test]
fn subject_context_from_thread_carries_process_thread_authority() {
    let _g = reset_zone_and_epoch();
    let process_cap = mint_process_cap();
    let thread_cap = mint_thread_cap();
    let cred_cap = mint_cred_cap();
    let restrictions_cap = mint_restrictions_cap();

    let process_clone = process_cap.clone();
    let thread_clone = thread_cap.clone();
    let cred_clone = cred_cap.clone();
    let restrictions_clone = restrictions_cap.clone();

    let authority = SubjectAuthority::new(cred_clone, restrictions_clone);
    let ctx = SubjectContext::from_thread(process_clone, thread_clone, authority);

    // Accessors return cap references after PR-9 phase 4 reshaped
    // the storage. `Cap<T>: PartialEq` compares raw slot keys.
    assert_eq!(ctx.process(), &process_cap);
    assert_eq!(ctx.thread(), Some(&thread_cap));
    assert_eq!(ctx.authority().cred(), &cred_cap);
    assert_eq!(ctx.authority().restrictions(), &restrictions_cap);
}

#[test]
fn subject_context_borrowed_has_no_thread() {
    // Per docs/Txv3/04_SYSCALL_SHAPE_v1.md and SUBJ-2: an
    // `OnBehalfOf<P>` borrow has no per-thread identity (the kthread's
    // own thread is not the borrowed subject's thread).
    let _g = reset_zone_and_epoch();
    let process_cap = mint_process_cap();
    let cred_cap = mint_cred_cap();
    let restrictions_cap = mint_restrictions_cap();

    let authority = SubjectAuthority::new(cred_cap.clone(), restrictions_cap.clone());
    let ctx = SubjectContext::borrowed(process_cap.clone(), authority);

    assert_eq!(ctx.process(), &process_cap);
    assert!(ctx.thread().is_none());
    assert_eq!(ctx.authority().cred(), &cred_cap);
}

#[test]
fn subject_authority_carries_cred_and_restrictions() {
    let _g = reset_zone_and_epoch();
    let cred_cap = mint_cred_cap();
    let restrictions_cap = mint_restrictions_cap();

    // Explicit turbofish names the placeholder identity (default
    // `I = ProcessIdentity` still works at constructor sites with a
    // contextual type, but the turbofish keeps this test independent
    // of inference).
    let authority =
        SubjectAuthority::<ProcessIdentity>::new(cred_cap.clone(), restrictions_cap.clone());
    assert_eq!(authority.cred(), &cred_cap);
    assert_eq!(authority.restrictions(), &restrictions_cap);
}

// txdoc:SUBJ-1
#[test]
fn subject_context_constructors_are_the_only_path() {
    // Compile-only structural pin: `SubjectContext` can only be
    // obtained by passing explicit cap arguments through one of the
    // two named constructors. There is no zero-arg
    // `current_subject_context()` accessor anywhere on the public
    // surface — SUBJ-1 is enforced by absence.
    let _g = reset_zone_and_epoch();
    let process_cap = mint_process_cap();
    let thread_cap = mint_thread_cap();
    let cred_cap = mint_cred_cap();
    let restrictions_cap = mint_restrictions_cap();

    let _: SubjectContext = SubjectContext::from_thread(
        process_cap.clone(),
        thread_cap.clone(),
        SubjectAuthority::new(cred_cap.clone(), restrictions_cap.clone()),
    );
    let _: SubjectContext = SubjectContext::borrowed(
        process_cap.clone(),
        SubjectAuthority::new(cred_cap.clone(), restrictions_cap.clone()),
    );
}
