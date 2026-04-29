//! Type-level hooks for identity/payload object modeling.
//!
//! These traits are intentionally light in this slice. They give upper
//! subsystems a place to express whether operational evidence is the identity
//! `Cap<T>` itself or a separate payload/contribution handle.

use super::Cap;

pub trait Entity: Sized + 'static {
    /// Evidence required to operate on this entity after observation.
    type OperationalEvidence;
}

/// Marker for entities whose identity and payload live in the same slot.
pub trait CoLocatedEntity: Sized + 'static {}

impl<T: CoLocatedEntity> Entity for T {
    type OperationalEvidence = Cap<T>;
}

pub struct PayloadBinding<T: 'static> {
    /// Weak payload link stored by identity objects in split-entity layouts.
    payload: Option<super::Weak<T>>,
}

impl<T: 'static> PayloadBinding<T> {
    pub const fn empty() -> Self {
        Self { payload: None }
    }

    pub fn from_weak(payload: super::Weak<T>) -> Self {
        Self {
            payload: Some(payload),
        }
    }

    pub fn weak(&self) -> Option<&super::Weak<T>> {
        self.payload.as_ref()
    }
}
