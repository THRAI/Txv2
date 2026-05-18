//! Policy marker types.
//!
//! The public API exposes `Zone<T>` rather than `Zone<T, Policy>`. These marker
//! types keep the design vocabulary available internally without leaking policy
//! selection to semantic subsystems.
//!
//! Policy is wired through the `ZoneAllocated` trait's associated `Policy` type.
//! Upper subsystems declare what kind of entity a zone stores; the substrate
//! uses the policy to gate operations at compile time (for example, observer
//! nodes cannot produce `Cap<T>`).

use core::marker::PhantomData;

// ---------------------------------------------------------------------------
// ZonePolicy trait
// ---------------------------------------------------------------------------

/// Behavioural contract for a zone's slot lifecycle policy.
///
/// Implemented by the three marker types below.  Policy is compile-time
/// dispatch: a `Zone<T>` carries no policy generic, but `ZoneAllocated`'s
/// associated `Policy` type lets the substrate query these constants at
/// monomorphisation time without a vtable.
pub trait ZonePolicy: 'static {
    /// Whether this policy supports producing `Cap<T>` via `zone::sign`.
    ///
    /// `ObserverNodePolicy` zones are EBR-retired but never exposed as
    /// `Cap<T>` — attempting `sign_for` on an observer-node type is a
    /// compile error.
    const SUPPORTS_CAP: bool;

    /// Whether a slot's `T::drop` must run through EBR delayed reclamation
    /// (after all epoch guards have quiesced) rather than immediately when
    /// the last retention holder drops.
    const EBR_DELAYED_DROP: bool;
}

// ---------------------------------------------------------------------------
// CapProducingPolicy marker
// ---------------------------------------------------------------------------

/// Sealed marker for policies that allow `zone::sign` / `sign_for` / `sign`.
///
/// `sign_for<T>` requires `T::Policy: CapProducingPolicy` so that observer
/// nodes are rejected at compile time.
///
/// Implemented for `RetainedEntityPolicy` and `PayloadPolicy` only.
pub trait CapProducingPolicy: ZonePolicy {}

// ---------------------------------------------------------------------------
// Policy marker types
// ---------------------------------------------------------------------------

/// Marker for ordinary retained identity/entity slots.
///
/// Slots under this policy follow the full five-state lifecycle:
/// `Free → Reserved → Live → Dead → Retiring → Free`.  `Cap<T>` carries
/// a refcounted retain; the last `Cap` drop triggers slot retirement
/// through EBR.
pub struct RetainedEntityPolicy<T: 'static> {
    _marker: PhantomData<T>,
}

/// Marker for payload slots in split identity/payload entities.
///
/// Behaviourally identical to `RetainedEntityPolicy` in the current slice:
/// `PayloadCap<T>` wraps `Cap<T>` and follows the same retain/EBR path.
/// The separate marker exists so that split-entity audits can distinguish
/// identity zones from payload zones at the type level.
pub struct PayloadPolicy<T: 'static> {
    _marker: PhantomData<T>,
}

/// Marker for hidden observer nodes that are EBR-retired but not exposed
/// as `Cap<T>`.
///
/// Observer nodes are internal bookkeeping structures (for example future
/// VM tracking nodes or slab-internal metadata).  They are allocated and
/// retired purely through EBR; no `Cap<T>` is ever produced from an
/// observer-node zone.
pub struct ObserverNodePolicy<T: 'static> {
    _marker: PhantomData<T>,
}

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

impl<T: 'static> ZonePolicy for RetainedEntityPolicy<T> {
    const SUPPORTS_CAP: bool = true;
    const EBR_DELAYED_DROP: bool = true;
}

impl<T: 'static> ZonePolicy for PayloadPolicy<T> {
    const SUPPORTS_CAP: bool = true;
    const EBR_DELAYED_DROP: bool = true;
}

impl<T: 'static> ZonePolicy for ObserverNodePolicy<T> {
    const SUPPORTS_CAP: bool = false;
    const EBR_DELAYED_DROP: bool = true;
}

impl<T: 'static> CapProducingPolicy for RetainedEntityPolicy<T> {}
impl<T: 'static> CapProducingPolicy for PayloadPolicy<T> {}
// ObserverNodePolicy does NOT implement CapProducingPolicy.

// ---------------------------------------------------------------------------
// IsPayloadPolicy marker
// ---------------------------------------------------------------------------

/// Sealed marker for `PayloadPolicy` to distinguish payload zones from
/// identity zones at compile time.
///
/// `PayloadCap::from_cap` requires `T::Policy: IsPayloadPolicy` so that
/// an identity Cap (e.g. `Cap<ProcessIdentity>`) cannot be mistakenly
/// wrapped as a `PayloadCap`.
///
/// Implemented only for `PayloadPolicy`.
pub trait IsPayloadPolicy: ZonePolicy {}

impl<T: 'static> IsPayloadPolicy for PayloadPolicy<T> {}
// RetainedEntityPolicy and ObserverNodePolicy do NOT implement IsPayloadPolicy.

impl<T: 'static> RetainedEntityPolicy<T> {
    pub const fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<T: 'static> Default for RetainedEntityPolicy<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> PayloadPolicy<T> {
    pub const fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<T: 'static> Default for PayloadPolicy<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> ObserverNodePolicy<T> {
    pub const fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<T: 'static> Default for ObserverNodePolicy<T> {
    fn default() -> Self {
        Self::new()
    }
}
