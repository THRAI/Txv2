//! Per-CPU epoch state.
//!
//! Each CPU publishes at most one active reader epoch and owns one retired list.
//! Callers must pin the CPU before mutating the retired list.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::retired::RetiredList;

/// Per-CPU EBR state.
pub(crate) struct CpuLocalEpochState {
    /// Set once the CPU has joined the epoch domain.
    initialized: AtomicBool,
    /// Zero means quiescent; non-zero means the CPU is inside a guard.
    local_epoch: AtomicU64,
    /// Current CPU's delayed reclamation list. Mutated only while that CPU is
    /// pinned, which is why it can live behind `UnsafeCell`.
    retired: UnsafeCell<RetiredList>,
}

unsafe impl Sync for CpuLocalEpochState {}

impl CpuLocalEpochState {
    pub(crate) const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            local_epoch: AtomicU64::new(0),
            retired: UnsafeCell::new(RetiredList::new()),
        }
    }

    pub(crate) fn init(&self) {
        self.local_epoch.store(0, Ordering::Release);
        unsafe {
            *self.retired.get() = RetiredList::new();
        }
        self.initialized.store(true, Ordering::Release);
    }

    pub(crate) fn reset(&self) {
        self.initialized.store(false, Ordering::Release);
        self.local_epoch.store(0, Ordering::Release);
        unsafe {
            *self.retired.get() = RetiredList::new();
        }
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub(crate) fn enter(&self, epoch: u64) {
        debug_assert_eq!(
            self.local_epoch.load(Ordering::Relaxed),
            0,
            "epoch guards cannot be nested on the same CPU"
        );
        // SeqCst pairs with the domain's guard acquisition fence and keeps the
        // published epoch visible before protected reads proceed.
        self.local_epoch.store(epoch, Ordering::SeqCst);
    }

    pub(crate) fn leave(&self) {
        self.local_epoch.store(0, Ordering::Release);
    }

    pub(crate) fn current(&self) -> u64 {
        self.local_epoch.load(Ordering::Acquire)
    }

    pub(crate) unsafe fn retired_mut(&self) -> &mut RetiredList {
        &mut *self.retired.get()
    }
}
