//! Delegate-side placeholders for `YieldShape::OnAgent`.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` (txdoc:STEP-V2-YIELD-SHAPE-1), the
//! `YieldShape` catalog has two members: `OnCarrier` (carrier-side wakes,
//! pinned in PR-0) and `OnAgent` (delegate-side waits, this PR). The
//! per-field types of `OnAgent` come from `docs/Txv3/05_DELEGATE_v1.md`:
//! `DelegateEndpoint`, `DelegateRequest`, `DelegateToken`, `Deadline`,
//! and `CancelPolicy`.
//!
//! These types are intentionally PR-4 placeholders: PR-4 of the v3 TDD
//! migration plan replaces them with real cap-typed zone primitives
//! (`Cap<DelegateEndpoint<K>>` over a zone-allocated endpoint, typed
//! per-EndpointKind request variants, real reactor-driven deadlines).
//! Today's shape is exactly enough to make `YieldShape::OnAgent`
//! representable so the closed catalog and the `DriveMode::classify`
//! matrix are completable.

/// Closed catalog of cancellation policies (per `docs/Txv3/05_DELEGATE_v1.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelPolicy {
    BestEffort,
    Synchronous,
    Detached,
}

/// Deadline placeholder. PR-4 replaces with the real reactor deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline(u64);

impl Deadline {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    /// Sentinel meaning "no deadline" (used by ptrace stops, etc.).
    pub const NEVER: Self = Self(u64::MAX);
}

/// Cap-typed delegate endpoint placeholder. PR-4 replaces with the real
/// `Cap<DelegateEndpoint<K>>` over a zone-allocated endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateEndpoint {
    _private: (),
}

impl DelegateEndpoint {
    /// Construct a placeholder endpoint. PR-4 removes this constructor
    /// in favor of zone allocation.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Cap-typed delegate token placeholder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateToken {
    _private: (),
}

impl DelegateToken {
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Closed catalog of delegate requests. PR-4 replaces with typed
/// per-EndpointKind variants (UfdRequest, FuseRequest, …).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegateRequest {
    /// Placeholder. Real variants land in PR-4 + per-kind extensions.
    Placeholder,
}
