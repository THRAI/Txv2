//! Per-CPU epoch state.
//!
//! Each CPU publishes at most one active reader epoch and owns one retired list.
//! Callers must pin the CPU before mutating the retired list.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::bag::{LocalRetireState, EPOCH_BAG_COUNT};

/// Per-CPU EBR state.
#[repr(align(64))]
pub(crate) struct CpuLocalEpochState {
    membership_and_pins: AtomicUsize,
    /// Zero means quiescent; non-zero means the CPU is inside a guard.
    local_epoch: AtomicU64,
    /// True while this CPU mutates its local retirement storage with local
    /// interrupt admission disabled.
    retire_active: AtomicBool,
    /// Debug-only reader accounting stays on the current CPU's cacheline.
    active_guards: AtomicUsize,
    /// Set remotely and consumed only by the owning CPU in normal context.
    drain_requested: AtomicBool,
    /// True only while an existing pinned reclaim callback runs with local
    /// execution reopened. It permits callback-owned nested retirement while
    /// an offline coordinator waits for the outer pin to quiesce.
    callback_depth: AtomicUsize,
    /// Current CPU's intrusive retirement bags.
    retire_state: UnsafeCell<LocalRetireState>,
    /// Remotely readable occupied epoch for each bag; zero means empty.
    bag_epochs: [AtomicU64; EPOCH_BAG_COUNT],
    /// Remotely readable bag node count. Only the owning CPU writes it.
    bag_retired: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(crate) enum CpuMembership {
    Offline = 0,
    Admitting = 1,
    Online = 2,
    Draining = 3,
}

const MEMBERSHIP_BITS: usize = 2;
const MEMBERSHIP_MASK: usize = (1 << MEMBERSHIP_BITS) - 1;
const PIN_ONE: usize = 1 << MEMBERSHIP_BITS;

unsafe impl Sync for CpuLocalEpochState {}

impl CpuLocalEpochState {
    pub(crate) const fn new() -> Self {
        Self {
            membership_and_pins: AtomicUsize::new(CpuMembership::Offline as usize),
            local_epoch: AtomicU64::new(0),
            retire_active: AtomicBool::new(false),
            active_guards: AtomicUsize::new(0),
            drain_requested: AtomicBool::new(false),
            callback_depth: AtomicUsize::new(0),
            retire_state: UnsafeCell::new(LocalRetireState::new()),
            bag_epochs: [const { AtomicU64::new(0) }; EPOCH_BAG_COUNT],
            bag_retired: AtomicUsize::new(0),
        }
    }

    pub(crate) fn prepare_admission(&self) {
        self.local_epoch.store(0, Ordering::Release);
        self.retire_active.store(false, Ordering::Release);
        self.active_guards.store(0, Ordering::Release);
        debug_assert_eq!(self.pin_count(), 0);
        self.drain_requested.store(false, Ordering::Release);
        self.callback_depth.store(0, Ordering::Release);
        self.clear_retire_summary();
        unsafe {
            (*self.retire_state.get()).reset();
        }
    }

    pub(crate) fn reset(&self) {
        self.membership_and_pins
            .store(CpuMembership::Offline as usize, Ordering::Release);
        self.local_epoch.store(0, Ordering::Release);
        self.retire_active.store(false, Ordering::Release);
        self.active_guards.store(0, Ordering::Release);
        self.drain_requested.store(false, Ordering::Release);
        self.callback_depth.store(0, Ordering::Release);
        self.clear_retire_summary();
        unsafe {
            (*self.retire_state.get()).reset();
        }
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.membership() != CpuMembership::Offline
    }

    pub(crate) fn membership(&self) -> CpuMembership {
        match self.membership_and_pins.load(Ordering::Acquire) & MEMBERSHIP_MASK {
            x if x == CpuMembership::Admitting as usize => CpuMembership::Admitting,
            x if x == CpuMembership::Online as usize => CpuMembership::Online,
            x if x == CpuMembership::Draining as usize => CpuMembership::Draining,
            _ => CpuMembership::Offline,
        }
    }

    pub(crate) fn set_membership(&self, membership: CpuMembership) {
        debug_assert_eq!(self.pin_count(), 0);
        self.membership_and_pins
            .store(membership as usize, Ordering::Release);
    }

    pub(crate) fn transition_membership(&self, from: CpuMembership, to: CpuMembership) -> bool {
        let mut current = self.membership_and_pins.load(Ordering::Acquire);
        loop {
            if current & MEMBERSHIP_MASK != from as usize {
                return false;
            }
            let next = (current & !MEMBERSHIP_MASK) | to as usize;
            match self.membership_and_pins.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn try_pin_online(&self) -> bool {
        self.try_pin_matching(|membership| membership == CpuMembership::Online)
    }

    pub(crate) fn try_pin_for_retire(&self) -> bool {
        self.try_pin_matching(|membership| {
            membership == CpuMembership::Online
                || (membership == CpuMembership::Draining && self.callback_active())
        })
    }

    fn try_pin_matching(&self, allowed: impl Fn(CpuMembership) -> bool) -> bool {
        let mut current = self.membership_and_pins.load(Ordering::Acquire);
        loop {
            let membership = membership_from_word(current);
            if !allowed(membership) {
                return false;
            }
            let Some(next) = current.checked_add(PIN_ONE) else {
                return false;
            };
            match self.membership_and_pins.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn unpin(&self) {
        let previous = self
            .membership_and_pins
            .fetch_sub(PIN_ONE, Ordering::AcqRel);
        debug_assert!(previous >> MEMBERSHIP_BITS > 0, "epoch pin count underflow");
    }

    pub(crate) fn pin_count(&self) -> usize {
        self.membership_and_pins.load(Ordering::Acquire) >> MEMBERSHIP_BITS
    }

    pub(crate) fn request_drain(&self) -> bool {
        !self.drain_requested.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn take_drain_request(&self) -> bool {
        self.drain_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn drain_requested(&self) -> bool {
        self.drain_requested.load(Ordering::Acquire)
    }

    pub(crate) fn begin_callback(&self) {
        self.callback_depth.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn end_callback(&self) {
        let previous = self.callback_depth.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "epoch callback depth underflow");
    }

    fn callback_active(&self) -> bool {
        self.callback_depth.load(Ordering::Acquire) != 0
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
        self.active_guards.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn leave(&self) {
        self.local_epoch.store(0, Ordering::Release);
        self.active_guards.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn current(&self) -> u64 {
        self.local_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn try_begin_retire(&self) -> bool {
        self.retire_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn end_retire(&self) {
        self.retire_active.store(false, Ordering::Release);
    }

    pub(crate) fn retire_active(&self) -> bool {
        self.retire_active.load(Ordering::Acquire)
    }

    pub(crate) fn bag_retired_count(&self) -> usize {
        self.bag_retired.load(Ordering::Acquire)
    }

    pub(crate) fn can_advance_to(&self, epoch: u64) -> bool {
        let occupied_epoch =
            self.bag_epochs[epoch as usize % EPOCH_BAG_COUNT].load(Ordering::Acquire);
        occupied_epoch == 0 || occupied_epoch == epoch
    }

    pub(crate) fn publish_retire_summary(&self, state: &LocalRetireState) {
        for (index, bag) in state.bags.iter().enumerate() {
            let occupied_epoch = if bag.is_empty() { 0 } else { bag.epoch };
            self.bag_epochs[index].store(occupied_epoch, Ordering::Release);
        }
        self.bag_retired
            .store(state.retired_count(), Ordering::Release);
    }

    fn clear_retire_summary(&self) {
        for epoch in &self.bag_epochs {
            epoch.store(0, Ordering::Release);
        }
        self.bag_retired.store(0, Ordering::Release);
    }

    pub(crate) fn active_guard_count(&self) -> usize {
        self.active_guards.load(Ordering::Relaxed)
    }

    pub(crate) fn retire_state_ptr(&self) -> *mut LocalRetireState {
        self.retire_state.get()
    }
}

fn membership_from_word(word: usize) -> CpuMembership {
    match word & MEMBERSHIP_MASK {
        x if x == CpuMembership::Admitting as usize => CpuMembership::Admitting,
        x if x == CpuMembership::Online as usize => CpuMembership::Online,
        x if x == CpuMembership::Draining as usize => CpuMembership::Draining,
        _ => CpuMembership::Offline,
    }
}
