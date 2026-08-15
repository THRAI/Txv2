//! Per-CPU epoch state.
//!
//! Each CPU publishes at most one active reader epoch and owns one retired list.
//! Callers must pin the CPU before mutating the retired list.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use super::bag::{LocalRetireState, EPOCH_BAG_COUNT};
use super::RcuHead;

pub(crate) const LOCAL_RETIRE_RESERVATION_CAPACITY: usize = 16;
const LOCAL_RETIRE_ALL_FREE: u16 = u16::MAX;

#[derive(Clone, Copy)]
struct ReservedRetireRecord {
    head: *mut RcuHead,
    retired_at: u64,
}

impl ReservedRetireRecord {
    const EMPTY: Self = Self {
        head: core::ptr::null_mut(),
        retired_at: 0,
    };
}

pub(crate) struct LocalRetirePool {
    free_mask: AtomicU16,
    ready_mask: AtomicU16,
    records: UnsafeCell<[ReservedRetireRecord; LOCAL_RETIRE_RESERVATION_CAPACITY]>,
}

unsafe impl Sync for LocalRetirePool {}

impl LocalRetirePool {
    const fn new() -> Self {
        Self {
            free_mask: AtomicU16::new(LOCAL_RETIRE_ALL_FREE),
            ready_mask: AtomicU16::new(0),
            records: UnsafeCell::new(
                [ReservedRetireRecord::EMPTY; LOCAL_RETIRE_RESERVATION_CAPACITY],
            ),
        }
    }

    pub(crate) fn try_reserve(&self) -> Option<usize> {
        let mut free = self.free_mask.load(Ordering::Acquire);
        loop {
            let slot = free.trailing_zeros() as usize;
            if slot == LOCAL_RETIRE_RESERVATION_CAPACITY {
                return None;
            }
            let bit = 1u16 << slot;
            match self.free_mask.compare_exchange_weak(
                free,
                free & !bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(slot),
                Err(observed) => free = observed,
            }
        }
    }

    pub(crate) fn validate_reserved(&self, slot: usize) -> Result<(), bool> {
        let bit = 1u16 << slot;
        if self.free_mask.load(Ordering::Acquire) & bit != 0 {
            return Err(false);
        }
        if self.ready_mask.load(Ordering::Acquire) & bit != 0 {
            return Err(true);
        }
        Ok(())
    }

    pub(crate) fn cancel(&self, slot: usize) {
        assert!(
            self.validate_reserved(slot).is_ok(),
            "local-retire cancellation requires a live unfilled reservation"
        );
        let bit = 1u16 << slot;
        let previous = self.free_mask.fetch_or(bit, Ordering::AcqRel);
        assert_eq!(previous & bit, 0, "local-retire slot released twice");
    }

    pub(crate) unsafe fn fill_reserved(
        &self,
        slot: usize,
        head: NonNull<RcuHead>,
        retired_at: u64,
    ) {
        assert!(
            self.validate_reserved(slot).is_ok(),
            "local-retire fill requires a live unfilled reservation"
        );
        unsafe {
            (*self.records.get())[slot] = ReservedRetireRecord {
                head: head.as_ptr(),
                retired_at,
            };
        }
        let bit = 1u16 << slot;
        let previous = self.ready_mask.fetch_or(bit, Ordering::Release);
        assert_eq!(previous & bit, 0, "local-retire slot filled twice");
    }

    pub(crate) unsafe fn detach_reclaimable(
        &self,
        safe_epoch: u64,
        budget: usize,
    ) -> (*mut RcuHead, usize, usize) {
        let mut ready = self.ready_mask.load(Ordering::Acquire);
        let mut reclaim_head: *mut RcuHead = core::ptr::null_mut();
        let mut detached = 0usize;
        let mut examined = 0usize;
        while ready != 0 && detached < budget {
            let slot = ready.trailing_zeros() as usize;
            let bit = 1u16 << slot;
            ready &= !bit;
            examined += 1;
            let record = unsafe { (*self.records.get())[slot] };
            if safe_epoch < record.retired_at.saturating_add(2) {
                continue;
            }
            let mut head = NonNull::new(record.head).expect("filled local-retire record head");
            unsafe {
                head.as_mut().next = if reclaim_head.is_null() {
                    head.as_ptr()
                } else {
                    reclaim_head
                };
                (*self.records.get())[slot] = ReservedRetireRecord::EMPTY;
            }
            self.ready_mask.fetch_and(!bit, Ordering::AcqRel);
            let previous = self.free_mask.fetch_or(bit, Ordering::Release);
            assert_eq!(
                previous & bit,
                0,
                "filled local-retire slot was already free"
            );
            reclaim_head = head.as_ptr();
            detached += 1;
        }
        (reclaim_head, detached, examined)
    }

    pub(crate) fn ready_count(&self) -> usize {
        self.ready_mask.load(Ordering::Acquire).count_ones() as usize
    }

    pub(crate) fn unfilled_count(&self) -> usize {
        let free = self.free_mask.load(Ordering::Acquire);
        let ready = self.ready_mask.load(Ordering::Acquire);
        ((!free) & (!ready)).count_ones() as usize
    }

    pub(crate) fn reserve_transfer_slots(
        &self,
        count: usize,
        slots: &mut [usize; LOCAL_RETIRE_RESERVATION_CAPACITY],
    ) -> bool {
        debug_assert!(count <= slots.len());
        for index in 0..count {
            let Some(slot) = self.try_reserve() else {
                self.cancel_transfer_slots(&slots[..index]);
                return false;
            };
            slots[index] = slot;
        }
        true
    }

    pub(crate) fn cancel_transfer_slots(&self, slots: &[usize]) {
        for &slot in slots {
            self.cancel(slot);
        }
    }

    pub(crate) unsafe fn transfer_ready_to_reserved(&self, target: &Self, target_slots: &[usize]) {
        let mut ready = self.ready_mask.load(Ordering::Acquire);
        assert_eq!(
            ready.count_ones() as usize,
            target_slots.len(),
            "local-retire transfer target count changed"
        );
        for &target_slot in target_slots {
            let source_slot = ready.trailing_zeros() as usize;
            let source_bit = 1u16 << source_slot;
            ready &= !source_bit;
            let record = unsafe { (*self.records.get())[source_slot] };
            let head = NonNull::new(record.head).expect("filled local-retire record head");
            unsafe {
                target.fill_reserved(target_slot, head, record.retired_at);
                (*self.records.get())[source_slot] = ReservedRetireRecord::EMPTY;
            }
            let previous_ready = self.ready_mask.fetch_and(!source_bit, Ordering::AcqRel);
            assert_ne!(
                previous_ready & source_bit,
                0,
                "transferred slot was not ready"
            );
            let previous_free = self.free_mask.fetch_or(source_bit, Ordering::Release);
            assert_eq!(
                previous_free & source_bit,
                0,
                "transferred slot was already free"
            );
        }
    }

    pub(crate) fn reset(&self) {
        self.ready_mask.store(0, Ordering::Release);
        self.free_mask
            .store(LOCAL_RETIRE_ALL_FREE, Ordering::Release);
        unsafe {
            *self.records.get() = [ReservedRetireRecord::EMPTY; LOCAL_RETIRE_RESERVATION_CAPACITY];
        }
    }
}

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
    /// Pre-reserved publication retire records, independent of bag occupancy.
    reserved_retire: LocalRetirePool,
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
            reserved_retire: LocalRetirePool::new(),
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
        self.reserved_retire.reset();
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
        self.reserved_retire.reset();
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

    /// Replace the epoch published by an admitted reader without opening a
    /// quiescent window.
    ///
    /// Guard admission uses this only before the first protected load.  The
    /// reader is already pinned and accounted active, so changing the value
    /// must not touch `active_guards` or transiently publish zero.
    pub(crate) fn republish(&self, epoch: u64) {
        debug_assert_ne!(
            self.local_epoch.load(Ordering::Relaxed),
            0,
            "only an active reader epoch may be republished"
        );
        self.local_epoch.store(epoch, Ordering::SeqCst);
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
        self.bag_retired.load(Ordering::Acquire) + self.reserved_retire.ready_count()
    }

    pub(crate) fn bag_only_retired_count(&self) -> usize {
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

    pub(crate) fn reserved_retire(&self) -> &LocalRetirePool {
        &self.reserved_retire
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
