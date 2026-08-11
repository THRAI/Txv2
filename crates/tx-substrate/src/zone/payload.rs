//! Type-level hooks for identity/payload object modeling.
//!
//! These traits are intentionally light in this slice. They give upper
//! subsystems a place to express whether operational evidence is the identity
//! `Cap<T>` itself or a separate payload/contribution handle.

use crate::epoch::Guard;

use super::{Cap, Dead, IdentRef, PayloadCap};

pub trait Entity: Sized + 'static {
    /// Evidence required to operate on this entity after observation.
    type OperationalEvidence;

    /// Upgrade identity-retaining evidence into the operational evidence shape
    /// that payload-using paths should carry across step boundaries.
    fn upgrade_operational(identity: &Cap<Self>) -> Result<Self::OperationalEvidence, Dead>;

    /// Upgrade with a caller-owned epoch guard. Split entities whose payload
    /// binding is RCU-published override this entry point to avoid opening a
    /// second guard; other entity families retain their existing behavior.
    fn upgrade_operational_with_guard(
        identity: &Cap<Self>,
        _guard: &Guard<'_>,
    ) -> Result<Self::OperationalEvidence, Dead> {
        Self::upgrade_operational(identity)
    }

    /// Guard-scoped helper: revalidate the observation, retain identity if
    /// needed, then upgrade into operational evidence.
    fn upgrade_operational_ref(
        identity: &IdentRef<'_, Self>,
    ) -> Result<Self::OperationalEvidence, Dead> {
        let cap = identity.to_cap()?;
        Self::upgrade_operational(&cap)
    }
}

/// Marker for entities whose identity and payload live in the same slot.
pub trait CoLocatedEntity: Sized + 'static {}

impl<T: CoLocatedEntity> Entity for T {
    type OperationalEvidence = Cap<T>;

    fn upgrade_operational(identity: &Cap<Self>) -> Result<Self::OperationalEvidence, Dead> {
        Ok(identity.clone())
    }
}

pub trait OperationalCapExt<T: Entity> {
    fn upgrade_operational(&self) -> Result<T::OperationalEvidence, Dead>;

    fn upgrade_operational_with_guard(
        &self,
        guard: &Guard<'_>,
    ) -> Result<T::OperationalEvidence, Dead>;
}

impl<T: Entity> OperationalCapExt<T> for Cap<T> {
    fn upgrade_operational(&self) -> Result<T::OperationalEvidence, Dead> {
        T::upgrade_operational(self)
    }

    fn upgrade_operational_with_guard(
        &self,
        guard: &Guard<'_>,
    ) -> Result<T::OperationalEvidence, Dead> {
        T::upgrade_operational_with_guard(self, guard)
    }
}

pub trait OperationalRefExt<T: Entity> {
    fn upgrade_operational(&self) -> Result<T::OperationalEvidence, Dead>;
}

impl<T: Entity> OperationalRefExt<T> for IdentRef<'_, T> {
    fn upgrade_operational(&self) -> Result<T::OperationalEvidence, Dead> {
        T::upgrade_operational_ref(self)
    }
}

pub struct PayloadBinding<T: 'static> {
    /// Identity-owned payload evidence in split-entity layouts.
    ///
    /// This is the "mounted identity bindings" style disjunct from the design:
    /// as long as the binding is installed, the payload remains strongly
    /// retained through the identity side.
    payload: Option<PayloadCap<T>>,
}

impl<T: 'static> core::fmt::Debug for PayloadBinding<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PayloadBinding")
            .field("installed", &self.payload.is_some())
            .finish()
    }
}

impl<T: 'static> PayloadBinding<T> {
    pub const fn empty() -> Self {
        Self { payload: None }
    }

    pub fn pending() -> Self {
        Self::empty()
    }

    pub fn installed(payload: PayloadCap<T>) -> Self {
        Self {
            payload: Some(payload),
        }
    }

    pub fn install(&mut self, payload: PayloadCap<T>) -> Option<PayloadCap<T>> {
        self.payload.replace(payload)
    }

    pub fn take(&mut self) -> Option<PayloadCap<T>> {
        self.payload.take()
    }

    pub fn payload(&self) -> Option<&PayloadCap<T>> {
        self.payload.as_ref()
    }

    pub fn upgrade(&self) -> Result<PayloadCap<T>, Dead> {
        self.payload.clone().ok_or(Dead)
    }

    pub fn is_installed(&self) -> bool {
        self.payload.is_some()
    }
}
