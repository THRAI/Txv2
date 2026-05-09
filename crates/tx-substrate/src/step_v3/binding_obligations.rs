//! Closed catalog of binding obligations.
//!
//! Per `docs/Txv3/01_CONCEPTS_v5.md` §8 (Bindings and obligations,
//! preserved from v4) and `docs/Txv3/02_INVARIANTS_v5.md` (the OBL-*
//! family). A *binding obligation* names the strength required of a
//! signifier→identity binding at a given dereference site. The catalog
//! is closed (ARCH-3-gated) and totally ordered:
//!
//! ```text
//! Operational > Addressability > ResolutionOnly
//! ```
//!
//! Wave 3 of the v3 TDD migration lands the catalog and helper surface;
//! later PRs use `BindingObligation` as a type-level marker on cap
//! operations so a site that requires `Operational` cannot be
//! discharged by a binding that only proves `ResolutionOnly`.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (consumed by cap-side dereference sites)
//! - txdoc:CONCEPTS-V5-BINDINGS-1 (canonical bindings/obligations section)

/// Closed catalog of binding obligations (per
/// `docs/Txv3/01_CONCEPTS_v5.md`). Each variant names the strength
/// required of a signifier→identity binding at a given dereference
/// site. The order is total — `Operational > Addressability >
/// ResolutionOnly` — and the helpers expose it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingObligation {
    /// Signifier→identity binding only. The identity may be
    /// unreachable (slot freed, refcount zero); only the binding's
    /// resolution is required. Used for stale-name diagnostics,
    /// audit logs.
    ResolutionOnly,
    /// Identity must be addressable: the holder still references it
    /// (e.g. a cap holds its zone slot live). Allows reading
    /// identity-side fields but not invoking operational state.
    Addressability,
    /// Identity must be operable: not freed, not poisoned, and the
    /// referenced subsystem is up. Required for any state-mutating
    /// step.
    Operational,
}

impl BindingObligation {
    /// Returns `true` if `self` is at least as strong as `other`.
    /// Operational ≥ Addressability ≥ ResolutionOnly. A site with
    /// `Operational` may be discharged by anything yielding
    /// Operational; a site with `Addressability` may be discharged
    /// by Operational *or* Addressability.
    pub const fn at_least(&self, other: BindingObligation) -> bool {
        self.rank() >= other.rank()
    }

    /// Numeric rank for the partial order. ResolutionOnly = 0,
    /// Addressability = 1, Operational = 2.
    pub const fn rank(&self) -> u8 {
        match self {
            BindingObligation::ResolutionOnly => 0,
            BindingObligation::Addressability => 1,
            BindingObligation::Operational => 2,
        }
    }

    /// Returns `true` if the obligation requires a live, operable
    /// referent (`Operational`).
    pub const fn requires_operability(&self) -> bool {
        matches!(self, BindingObligation::Operational)
    }
}
