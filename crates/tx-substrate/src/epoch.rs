//! Minimal guard domain for host-testable EBR-shaped observations.
//!
//! This module intentionally does not implement full epoch advancement or
//! reclamation. It provides a guard lifetime and a small defer gate that lets
//! tests and early substrate code distinguish "guarded observation exists"
//! from "no guards are active".

use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

static DEFAULT_DOMAIN: Domain = Domain::new();

/// A bounded guard domain.
pub struct Domain {
    active: AtomicUsize,
    deferred: AtomicUsize,
}

impl Domain {
    /// Construct an empty guard domain.
    pub const fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            deferred: AtomicUsize::new(0),
        }
    }

    /// Enter a guarded observation scope.
    pub fn guard(&self) -> Guard<'_> {
        self.active.fetch_add(1, Ordering::AcqRel);
        Guard {
            domain: self,
            _not_send: PhantomData,
        }
    }

    /// Record one retired item that must wait for a guard-free point.
    pub fn defer_retired(&self) {
        self.deferred.fetch_add(1, Ordering::AcqRel);
    }

    /// Try to collect deferred retirements.
    ///
    /// Returns the number of deferred retirements released when no guards are
    /// active. Returns `0` while any guard remains active.
    pub fn collect(&self) -> usize {
        if self.active.load(Ordering::Acquire) == 0 {
            self.deferred.swap(0, Ordering::AcqRel)
        } else {
            0
        }
    }

    /// Number of currently active guards in this domain.
    pub fn active_guards(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Number of retirements waiting for a guard-free point.
    pub fn deferred_count(&self) -> usize {
        self.deferred.load(Ordering::Acquire)
    }
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

/// A guard-scoped observation token.
pub struct Guard<'g> {
    domain: &'g Domain,
    _not_send: PhantomData<*const ()>,
}

impl Guard<'_> {
    /// Domain protected by this guard.
    pub fn domain(&self) -> &Domain {
        self.domain
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.domain.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Enter the default host-test guard domain.
pub fn guard() -> Guard<'static> {
    DEFAULT_DOMAIN.guard()
}

/// Default host-test guard domain.
pub fn default_domain() -> &'static Domain {
    &DEFAULT_DOMAIN
}
