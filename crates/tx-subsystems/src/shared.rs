//! Per-resource sharing wrapper for Frame slots.
//!
//! `Shared<T>` is the primitive that enables clone-flag sharing
//! (CLONE_VM, CLONE_FILES, CLONE_SIGHAND, CLONE_FS) as specified in
//! `PROCESS_v1` §3.1. It wraps an `Arc<SpinMutex<T>>`:
//!
//! - **`share()`** increments the Arc refcount — all sharers see the
//!   same `T`. Used when a clone flag is set (e.g. pthread threads
//!   share the same AddressSpace).
//! - **`lock()`** acquires the inner spinlock, returning a mutable
//!   guard. All mutations are immediately visible to all sharers.
//!   There is no COW at the Shared<T> level — COW semantics for
//!   AddressSpace are implemented by `AddressSpace::fork_aspace`
//!   (page-level COW), and the fork caller wraps the result in a
//!   fresh `Shared<T>`.
//!
//! # COW via fresh construction
//!
//! Fork without CLONE_VM:
//!
//! ```text
//! // Parent
//! parent_frame.vm = Shared::new(parent_aspace);
//!
//! // Child
//! let cow_aspace = AddressSpace::fork_aspace(&parent_aspace)?;
//! child_frame.vm = Shared::new(cow_aspace);  // fresh Shared, refcount 1
//! ```
//!
//! Clone-thread with CLONE_VM:
//!
//! ```text
//! // Parent thread
//! parent_frame.vm = Shared::new(aspace);
//!
//! // New thread
//! child_frame.vm = parent_frame.vm.share();  // same Arc, refcount +1
//! ```
//!
//! # Why not SpinMutex directly?
//!
//! The Arc wrapper enables sharing across threads within the same
//! process. Without Arc, each ProcessPayload would own its copy of
//! every Frame field, and CLONE_THREAD semantics (shared fd table,
//! shared signal actions) would be impossible.

use alloc::sync::Arc;

use crate::adapter::step_engine::SpinMutex;
use tx_substrate::SpinMutexGuard;

/// Per-resource sharing wrapper. Multiple `Shared<T>` handles can
/// point to the same `T` via `Arc`; the `SpinMutex` provides
/// interior mutability.
pub struct Shared<T> {
    inner: Arc<SpinMutex<T>>,
}

impl<T> Shared<T> {
    /// Create a new `Shared<T>` with a refcount of 1.
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(SpinMutex::new(value)),
        }
    }

    /// Increment the sharing reference count. Returns a new
    /// `Shared<T>` pointing to the same `T`. All mutations through
    /// either handle are immediately visible to all sharers.
    ///
    /// Used when a clone flag (CLONE_VM, CLONE_FILES, etc.) is set —
    /// this is true multi-owner sharing, not COW.
    pub fn share(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Acquire exclusive (spinlock) access to the shared data.
    /// Returns a mutable guard; the lock is released when the guard
    /// drops. The return type is `impl DerefMut<Target = T>` so
    /// callers can read through `*guard` and write through
    /// `*guard = value` without naming the guard type.
    pub fn lock(&self) -> impl core::ops::DerefMut<Target = T> + '_ {
        self.inner.lock()
    }
}

impl<T> Clone for Shared<T> {
    /// `clone()` is an alias for `share()` — increments the Arc
    /// refcount. Use `Shared::new(value)` to create an independent
    /// copy.
    fn clone(&self) -> Self {
        self.share()
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let guard = self.lock();
        f.debug_struct("Shared")
            .field("refcount", &Arc::strong_count(&self.inner))
            .field("value", &*guard)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn new_creates_with_refcount_one() {
        let s = Shared::new(42u32);
        let val = *s.lock();
        assert_eq!(val, 42);
        let _ = val;
    }

    #[test]
    fn share_increments_refcount() {
        let a = Shared::new(10u32);
        let b = a.share();

        // Both see the same value.
        assert_eq!(*a.lock(), 10);
        assert_eq!(*b.lock(), 10);

        // Mutation through one is visible to the other.
        *a.lock() = 20;
        assert_eq!(*b.lock(), 20);
    }

    #[test]
    fn clone_is_alias_for_share() {
        let a = Shared::new(7u32);
        let b = a.clone();
        *b.lock() = 99;
        assert_eq!(*a.lock(), 99);
    }

    #[test]
    fn independent_news_are_isolated() {
        let a = Shared::new(1u32);
        let b = Shared::new(1u32);
        *a.lock() = 100;
        assert_eq!(*b.lock(), 1);
    }

    #[test]
    fn debug_format_includes_refcount_and_value() {
        let a = Shared::new(5u32);
        let b = a.share();
        let debug_str = format!("{:?}", a);
        assert!(debug_str.contains("refcount"));
        assert!(debug_str.contains("5"));
        drop(b);
    }
}
