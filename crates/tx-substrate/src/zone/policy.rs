//! Policy marker types.
//!
//! The public API exposes `Zone<T>` rather than `Zone<T, Policy>`. These marker
//! types keep the design vocabulary available internally without leaking policy
//! selection to semantic subsystems.

use core::marker::PhantomData;

/// Marker for ordinary retained identity/entity slots.
pub struct RetainedEntityPolicy<T: 'static> {
    _marker: PhantomData<T>,
}

/// Marker for payload slots in split identity/payload entities.
pub struct PayloadPolicy<T: 'static> {
    _marker: PhantomData<T>,
}

/// Marker for hidden observer nodes that are EBR-retired but not exposed as Cap.
pub struct ObserverNodePolicy<T: 'static> {
    _marker: PhantomData<T>,
}

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
