//! Closed catalog for `ExecutionScope`.
//!
//! Per `docs/Txv3/06_EXECUTION_SCOPE_v1.md`, `ExecutionScope` is the
//! identity-context modifier orthogonal to `YieldShape`: a script runs
//! either under the calling thread's `SubjectContext` (`Thread`) or
//! under a borrowed `Cap<ProcessIdentity>` (`OnBehalfOf`).
//!
//! **PR-11 phase 0 reshape.** The `OnBehalfOf` variant now carries a
//! real `Cap<I>` (generic over `I: SubjectIdentity`) rather than the
//! Wave 3 unit-typed `OwnedProcessHandle` placeholder. The cap is the
//! principal P's process identity; the variant gates worker
//! permission checks so that authority lookups inside a borrow body
//! resolve against P's subject, not the worker thread's own.
//!
//! Catalog extension is gated on ARCH-3 review per
//! `docs/Txv3/02_INVARIANTS_v5.md` (SCOPE-1).
//!
//! The async `with_on_behalf_of` borrow primitive (PR-11 phase 0)
//! lives in [`crate::step::on_behalf_of`]; it constructs a
//! `SubjectContext::borrowed` for the body, subscribes the principal's
//! `exit_source` for abandonment routing, and drops the borrow at
//! end-of-scope. See `06_EXECUTION_SCOPE_v1.md` §3 for the spec.
//!
//! Doc tags pinned by the integration tests:
//! - `txdoc:TXV3-STEP-MODEL-V2` (step model algebra; ExecutionScope is
//!   the identity-context modifier)
//! - `txdoc:TXV3-EXECUTION-SCOPE-V1` (full ExecutionScope spec)
//! - `txdoc:SCOPE-V1-CATALOG-1` (closed catalog)

use crate::step::subject_context::{ProcessIdentity, SubjectIdentity};
use crate::zone::Cap;

/// Closed catalog of execution scopes. Per
/// `docs/Txv3/06_EXECUTION_SCOPE_v1.md`. Extension is ARCH-3.
///
/// `ExecutionScope` and `YieldShape` compose orthogonally: a script
/// running inside `OnBehalfOf` may emit any `YieldShape`, and a yield
/// does not enter or leave a scope.
///
/// Generic over `I: SubjectIdentity` (default `I = ProcessIdentity`
/// for the step_v3 placeholder) per
/// [D1](../../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md).
/// Production code resolves the alias against
/// `tx_subsystems::process::ProcessIdentity`.
///
/// **No `Copy`** post PR-11 phase 0: the `OnBehalfOf` variant owns a
/// `Cap<I>` retain, which must move (or `Clone`) explicitly so retain
/// bookkeeping stays explicit. `Clone` is provided because `Cap<I>:
/// Clone` (cap clone bumps the retain count on the zone slot).
#[derive(Clone, Debug)]
pub enum ExecutionScope<I: SubjectIdentity = ProcessIdentity> {
    /// Native syscall: the script runs under the calling thread's
    /// `SubjectContext`.
    Thread,
    /// Borrowed scope: a kernel actor (io_uring SQPOLL kthread, AIO
    /// worker, FUSE helper) runs scripts under a borrowed process
    /// identity. The cap keeps the principal addressable for the
    /// lifetime of the borrow; clone semantics are EBR-retain bumps
    /// on the zone slot.
    OnBehalfOf(Cap<I>),
}

impl<I: SubjectIdentity> ExecutionScope<I> {
    /// Returns `true` if the scope is a native thread-rooted execution
    /// (no borrow). Useful for fast-paths that skip OnBehalfOf-only
    /// machinery.
    pub const fn is_thread(&self) -> bool {
        matches!(self, ExecutionScope::Thread)
    }

    /// Returns `true` if the scope is an `OnBehalfOf` borrow. The
    /// borrowed principal cap is reachable via [`Self::borrowed_owner`].
    pub const fn is_borrowed(&self) -> bool {
        matches!(self, ExecutionScope::OnBehalfOf(_))
    }

    /// Returns the borrowed principal cap for `OnBehalfOf`, else
    /// `None`. The cap is returned by reference so callers do not
    /// implicitly clone retain.
    pub const fn borrowed_owner(&self) -> Option<&Cap<I>> {
        match self {
            ExecutionScope::Thread => None,
            ExecutionScope::OnBehalfOf(cap) => Some(cap),
        }
    }
}

impl<I: SubjectIdentity> PartialEq for ExecutionScope<I> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ExecutionScope::Thread, ExecutionScope::Thread) => true,
            (ExecutionScope::OnBehalfOf(a), ExecutionScope::OnBehalfOf(b)) => a == b,
            _ => false,
        }
    }
}

impl<I: SubjectIdentity> Eq for ExecutionScope<I> {}
