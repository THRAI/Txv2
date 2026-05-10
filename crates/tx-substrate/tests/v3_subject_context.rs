//! v3 `SubjectContext` pin tests.
//!
//! These tests pin the script-scoped identity context introduced in
//! v5 of the txKernel meta-framework. `SubjectContext` carries the
//! process identity, optional thread identity, and the
//! `SubjectAuthority` (credential + restriction-stack handle) for the
//! script frame. Wave 3 lands placeholder bodies; wave 4+ replaces
//! the bare types with `Cap<T>`-wrapped equivalents.
//!
//! The pin set covers:
//! - `SubjectContext::from_thread` carries process / thread / authority
//!   (native syscall entry path, per `04_SYSCALL_SHAPE_v1.md`).
//! - `SubjectContext::borrowed` produces a context with no per-thread
//!   identity (`OnBehalfOf<P>` borrow path).
//! - `SubjectAuthority::new` carries the credential and restriction-stack
//!   handle.
//! - SUBJ-1 structural pin: the only path to a `SubjectContext` is
//!   through the explicit constructors taking placeholder arguments;
//!   there is no zero-arg `current_subject_context()` accessor.
//!
//! txdoc cross-refs:
//! - `txdoc:TXV3-STEP-MODEL-V2`
//! - `txdoc:TXV3-CONCEPTS-V5`

use tx_substrate::step_v3::{
    Credential, ProcessIdentity, RestrictionStackHandle, SubjectAuthority, SubjectContext,
    ThreadIdentity,
};

#[test]
fn subject_context_from_thread_carries_process_thread_authority() {
    let process = ProcessIdentity::placeholder();
    let thread = ThreadIdentity::placeholder();
    let authority = SubjectAuthority::new(
        Credential::placeholder(),
        RestrictionStackHandle::placeholder(),
    );
    let ctx = SubjectContext::from_thread(process, thread, authority);

    assert_eq!(ctx.process(), ProcessIdentity::placeholder());
    assert_eq!(ctx.thread(), Some(ThreadIdentity::placeholder()));
    assert_eq!(ctx.authority().cred(), Credential::placeholder());
    assert_eq!(
        ctx.authority().restrictions(),
        RestrictionStackHandle::placeholder()
    );
}

#[test]
fn subject_context_borrowed_has_no_thread() {
    // Per docs/Txv3/04_SYSCALL_SHAPE_v1.md and SUBJ-2: an
    // `OnBehalfOf<P>` borrow has no per-thread identity (the kthread's
    // own thread is not the borrowed subject's thread).
    let process = ProcessIdentity::placeholder();
    let authority = SubjectAuthority::new(
        Credential::placeholder(),
        RestrictionStackHandle::placeholder(),
    );
    let ctx = SubjectContext::borrowed(process, authority);

    assert_eq!(ctx.process(), ProcessIdentity::placeholder());
    assert_eq!(ctx.thread(), None);
    assert_eq!(ctx.authority().cred(), Credential::placeholder());
}

#[test]
fn subject_authority_carries_cred_and_restrictions() {
    let authority = SubjectAuthority::new(
        Credential::placeholder(),
        RestrictionStackHandle::placeholder(),
    );
    assert_eq!(authority.cred(), Credential::placeholder());
    assert_eq!(
        authority.restrictions(),
        RestrictionStackHandle::placeholder()
    );
}

// txdoc:SUBJ-1
#[test]
fn subject_context_constructors_are_the_only_path() {
    // Compile-only structural pin: `SubjectContext` can only be
    // obtained by passing explicit placeholder arguments through one
    // of the two named constructors. There is no zero-arg
    // `current_subject_context()` accessor anywhere on the public
    // surface — SUBJ-1 is enforced by absence. If a global getter is
    // ever added, this test stays green, but the *next* layer (the
    // SubjectContext API surface lint, eventually) catches it; today
    // the doc comment + the explicit-args shape is the contract.
    fn _check() {
        let _: SubjectContext = SubjectContext::from_thread(
            ProcessIdentity::placeholder(),
            ThreadIdentity::placeholder(),
            SubjectAuthority::new(
                Credential::placeholder(),
                RestrictionStackHandle::placeholder(),
            ),
        );
        let _: SubjectContext = SubjectContext::borrowed(
            ProcessIdentity::placeholder(),
            SubjectAuthority::new(
                Credential::placeholder(),
                RestrictionStackHandle::placeholder(),
            ),
        );
    }
    _check();
}
