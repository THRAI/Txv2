//! Tiny spin lock used by early substrate code.
//!
//! This is deliberately minimal: it is enough for BSP/smoke paths and local
//! slab-list protection before the full scheduler-aware locking layer exists.

use core::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct SpinLock {
    /// False means unlocked, true means held.
    held: AtomicBool,
}

impl SpinLock {
    pub(crate) const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    pub(crate) fn lock(&self) -> SpinGuard<'_> {
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

pub(crate) struct SpinGuard<'a> {
    lock: &'a SpinLock,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}
