//! Per-CPU epoch state.
//!
//! Each CPU publishes at most one active reader epoch and owns one retired list.
//! Callers must pin the CPU before mutating the retired list.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::retired::LocalBag;

const PINNINGS_BETWEEN_COLLECT: usize = 128;

/// Per-CPU EBR state.
#[repr(align(64))]
pub(crate) struct CpuLocalEpochState {
    /// Set once the CPU has joined the epoch domain.
    initialized: AtomicBool,
    /// Zero means quiescent; non-zero means the CPU is inside a guard.
    local_epoch: AtomicU64,
    /// Current CPU's Crossbeam-style local deferred-callback bag.
    bag: UnsafeCell<LocalBag>,
    /// Number of callbacks owned by this CPU across its local and sealed bags.
    pending: AtomicUsize,
    /// Periodic collector trigger, matching Crossbeam's 128 pinnings cadence.
    pin_count: AtomicUsize,
}

unsafe impl Sync for CpuLocalEpochState {}

impl CpuLocalEpochState {
    pub(crate) const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            local_epoch: AtomicU64::new(0),
            bag: UnsafeCell::new(LocalBag::new()),
            pending: AtomicUsize::new(0),
            pin_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn init(&self) {
        self.local_epoch.store(0, Ordering::Release);
        unsafe {
            *self.bag.get() = LocalBag::new();
        }
        self.pending.store(0, Ordering::Release);
        self.pin_count.store(0, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
    }

    pub(crate) fn reset(&self) {
        self.initialized.store(false, Ordering::Release);
        self.local_epoch.store(0, Ordering::Release);
        unsafe {
            *self.bag.get() = LocalBag::new();
        }
        self.pending.store(0, Ordering::Release);
        self.pin_count.store(0, Ordering::Release);
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

    pub(crate) fn note_pin_and_should_collect(&self) -> bool {
        self.pin_count
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            % PINNINGS_BETWEEN_COLLECT
            == 0
    }

    pub(crate) fn note_retired(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn note_reclaimed(&self, count: usize) {
        self.pending.fetch_sub(count, Ordering::AcqRel);
    }

    pub(crate) fn retired_count(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn bag_ptr(&self) -> *mut LocalBag {
        self.bag.get()
    }
}
