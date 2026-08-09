//! Tiny spin lock used by early substrate code.
//!
//! This is deliberately minimal: it is enough for BSP/smoke paths and local
//! slab-list protection before the full scheduler-aware locking layer exists.

use core::sync::atomic::{AtomicBool, Ordering};

use tx_hal::LocalExecutionGuard;

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
        let local_execution = super::runtime::exclude_local_execution();
        let mut wait = crate::SpinWait::new();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            wait.tick();
        }
        SpinGuard {
            lock: self,
            _local_execution: local_execution,
        }
    }
}

pub(crate) struct SpinGuard<'a> {
    lock: &'a SpinLock,
    _local_execution: LocalExecutionGuard,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}
