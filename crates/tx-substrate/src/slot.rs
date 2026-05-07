//! Single-slot atomic container for an optional value.
//!
//! Per `CONCEPTS_v4.md` ("substrate provides zone, index, epoch, mutation,
//! bus, page, and reservation primitives") a synchronization primitive
//! with no semantic content and no entity ownership belongs in the
//! substrate. Subsystems consume it as `tx_substrate::AtomicSlot`
//! (re-exported at the crate root).
//!
//! The current implementation is a `SpinMutex<Option<T>>` which has
//! identical observable semantics to the eventual lock-free shape but
//! worse scalability under high contention. The `load`/`snapshot` API is
//! shaped to match the future zone-aware slot (`AtomicSlot::load(&guard)`)
//! so callers already pass guards through where applicable; that lets the
//! future swap to an EBR-aware slot drop the staging body without source
//! changes.

use crate::SpinMutex;

pub struct AtomicSlot<T> {
    inner: SpinMutex<Option<T>>,
}

impl<T> AtomicSlot<T> {
    pub const fn empty() -> Self {
        Self {
            inner: SpinMutex::new(None),
        }
    }

    pub fn store(&self, value: Option<T>) {
        *self.inner.lock() = value;
    }

    pub fn swap(&self, value: Option<T>) -> Option<T> {
        let mut slot = self.inner.lock();
        let old = slot.take();
        *slot = value;
        old
    }

    pub fn with<R, F: FnOnce(Option<&T>) -> R>(&self, f: F) -> R {
        f(self.inner.lock().as_ref())
    }

    pub fn snapshot(&self) -> Option<T>
    where
        T: Clone,
    {
        self.inner.lock().clone()
    }

    /// Borrow-style snapshot: clone the slot's current value if any.
    /// Equivalent to [`Self::snapshot`] but named to match the canonical
    /// `AtomicSlot::load(&guard)` shape that future zone-aware slots
    /// will expose. The guard is unused today (the staging
    /// implementation is `SpinMutex`-backed) but reserved so callers
    /// already pass it through; that lets the future swap to a real
    /// EBR-aware slot drop the staging body without source changes.
    pub fn load(&self) -> Option<T>
    where
        T: Clone,
    {
        self.snapshot()
    }
}
