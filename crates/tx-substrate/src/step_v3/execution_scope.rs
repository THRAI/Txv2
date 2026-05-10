//! Closed catalog for `ExecutionScope`.
//!
//! Per `docs/Txv3/06_EXECUTION_SCOPE_v1.md`, `ExecutionScope` is the
//! identity-context modifier orthogonal to `YieldShape`: a script runs
//! either under the calling thread's `SubjectContext` (`Thread`) or
//! under a borrowed `Cap<ProcessIdentity>` (`OnBehalfOf`). Wave 3 of
//! the v3 TDD migration plan lands the closed catalog + helper methods
//! only; the borrow primitive (`with_on_behalf_of` async fn),
//! abandonment routing through `Killable`, and resource-scoping
//! discipline are deferred to PR-7.
//!
//! Catalog extension is gated on ARCH-3 review per
//! `docs/Txv3/02_INVARIANTS_v5.md` (SCOPE-1).
//!
//! Doc tags pinned by the integration tests:
//! - `txdoc:TXV3-STEP-MODEL-V2` (step model algebra; ExecutionScope is
//!   the identity-context modifier)
//! - `txdoc:TXV3-EXECUTION-SCOPE-V1` (full ExecutionScope spec)

/// Owned process-identity handle (placeholder for `Cap<ProcessIdentity>`).
///
/// PR-7 of the v3 TDD migration replaces this with a real cap-typed
/// reference once the cap machinery is wired. Today's shape is exactly
/// enough to make `ExecutionScope::OnBehalfOf` representable and to
/// pin the closed catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnedProcessHandle {
    _private: (),
}

impl OwnedProcessHandle {
    /// Construct the unique placeholder handle. PR-7 replaces this
    /// with cap-typed construction.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Closed catalog of execution scopes. Per
/// `docs/Txv3/06_EXECUTION_SCOPE_v1.md`. Extension is ARCH-3.
///
/// `ExecutionScope` and `YieldShape` compose orthogonally: a script
/// running inside `OnBehalfOf` may emit any `YieldShape`, and a yield
/// does not enter or leave a scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionScope {
    /// Native syscall: the script runs under the calling thread's
    /// `SubjectContext`.
    Thread,
    /// Borrowed scope: a kernel actor (io_uring SQPOLL kthread, AIO
    /// worker, FUSE helper) runs scripts under a borrowed process
    /// identity. The handle keeps the owner addressable for the
    /// lifetime of the borrow.
    OnBehalfOf(OwnedProcessHandle),
}

impl ExecutionScope {
    /// Returns `true` if the scope is a native thread-rooted execution
    /// (no borrow). Useful for fast-paths that skip OnBehalfOf-only
    /// machinery.
    pub const fn is_thread(&self) -> bool {
        matches!(self, ExecutionScope::Thread)
    }

    /// Returns `true` if the scope is an `OnBehalfOf` borrow. The
    /// borrowed identity is reachable via [`Self::borrowed_owner`].
    pub const fn is_borrowed(&self) -> bool {
        matches!(self, ExecutionScope::OnBehalfOf(_))
    }

    /// Returns the borrowed owner handle for `OnBehalfOf`, else `None`.
    pub const fn borrowed_owner(&self) -> Option<OwnedProcessHandle> {
        match self {
            ExecutionScope::Thread => None,
            ExecutionScope::OnBehalfOf(handle) => Some(*handle),
        }
    }
}
