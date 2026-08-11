//! Frame-backed slab storage for zone slots.
//!
//! A slab is one direct-mapped physical frame containing a `ZoneSlab<T>` header
//! followed by a dense array of `Slot<T>`. Free slots are tracked by a bitmap in
//! the slab header; individual slots do not carry back-pointers.

use core::mem;
use core::ptr::{self, NonNull};

use tx_hal::{PhysAddr, Ppn};

use crate::epoch::RcuHead;
use crate::page_allocator::{self, ZeroPolicy};

use super::registry::SlotKey;
use super::slot::Slot;
use super::{runtime, Zone, ZoneError};

const MAX_SLAB_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlabList {
    /// Not linked into any Keg list.
    Unlinked,
    /// Some slots are free and some are claimed/live.
    Partial,
    /// All slots are claimed/live.
    Full,
    /// All slots are free.
    Empty,
}

/// Zone-owned slab page descriptor.
///
/// The descriptor and typed slots live inside a backing page obtained directly
/// from the frame allocator. It does not allocate through the kernel heap.
#[repr(C)]
pub struct ZoneSlab<T: 'static> {
    /// One-shot intrusive retirement header. Kept separate from Keg linkage.
    retire_head: RcuHead,
    /// Per-zone slab ID used in `SlotKey`.
    id: usize,
    /// Owning zone. Stored once per slab instead of once per slot.
    zone: &'static Zone<T>,
    /// Physical frame backing this slab.
    backing_ppn: Ppn,
    page_count: usize,
    /// Number of initialized slots after the header.
    slot_count: usize,
    /// Number of bits currently set in `free_bitmap`.
    free_count: usize,
    /// Number of reclaimed slots whose generation reached the non-wrapping
    /// ceiling. These slots are logically free, but are intentionally absent
    /// from `free_bitmap` so stale `Weak` handles cannot observe ABA reuse.
    exhausted_count: usize,
    /// One bit per slot: 1 means free, 0 means claimed/live/reserved.
    free_bitmap: u64,
    /// Rotating start index for the next claim. This spreads repeated
    /// allocate/free cycles across a slab instead of hammering slot 0.
    claim_cursor: u8,
    /// Which Keg list currently owns this slab.
    list: SlabList,
    /// Intrusive next pointer for Keg lists.
    next: *mut ZoneSlab<T>,
}

unsafe impl<T: Send> Send for ZoneSlab<T> {}
unsafe impl<T: Send + Sync> Sync for ZoneSlab<T> {}

impl<T: 'static> ZoneSlab<T> {
    pub(crate) fn allocate(zone: &'static Zone<T>, id: usize) -> Result<NonNull<Self>, ZoneError> {
        let page_size = runtime::page_size();
        // The slot array starts immediately after an aligned slab header.
        let header_size = align_up(mem::size_of::<Self>(), mem::align_of::<Slot<T>>())
            .ok_or(ZoneError::AllocationFailed)?;
        let slot_size = mem::size_of::<Slot<T>>().max(1);
        let slot_capacity = page_size
            .checked_sub(header_size)
            .ok_or(ZoneError::AllocationFailed)?
            / slot_size;
        let slot_count = slot_capacity.min(MAX_SLAB_SLOTS);
        if slot_count == 0 {
            return Err(ZoneError::AllocationFailed);
        }

        // Keep the owned run live across all fallible address calculations so
        // an early error returns the frame automatically. The slab header takes
        // ownership only after it and every slot are fully initialized.
        let run = page_allocator::reserve_run(1, 1, ZeroPolicy::UninitFullOverwrite)?.commit();
        let backing_ppn = run.base();

        let phys = PhysAddr(
            backing_ppn
                .0
                .checked_mul(page_size)
                .ok_or(ZoneError::AllocationFailed)?,
        );
        let page = runtime::direct_map_ptr(phys)?;
        unsafe {
            ptr::write_bytes(page, 0, page_size);
        }

        let slab_ptr = page as *mut Self;
        let slots = unsafe { page.add(header_size) as *mut Slot<T> };
        let slots = NonNull::new(slots).ok_or(ZoneError::AllocationFailed)?;
        let slab = NonNull::new(slab_ptr).ok_or(ZoneError::AllocationFailed)?;

        unsafe {
            slab_ptr.write(Self {
                retire_head: RcuHead::new(reclaim_slab_head::<T>),
                id,
                zone,
                backing_ppn,
                page_count: 1,
                slot_count,
                free_count: slot_count,
                exhausted_count: 0,
                free_bitmap: initial_free_bitmap(slot_count),
                claim_cursor: 0,
                list: SlabList::Unlinked,
                next: ptr::null_mut(),
            });

            for index in 0..slot_count {
                let slot = slots.as_ptr().add(index);
                slot.write(Slot::new_free());
            }
        }

        core::mem::forget(run);

        Ok(slab)
    }

    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn retire_head(&mut self) -> NonNull<RcuHead> {
        NonNull::from(&mut self.retire_head)
    }

    pub fn backing_ppn(&self) -> Ppn {
        self.backing_ppn
    }

    pub fn page_count(&self) -> usize {
        self.page_count
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    pub fn free_count(&self) -> usize {
        self.free_count
    }

    pub fn is_empty(&self) -> bool {
        self.free_count == self.slot_count
    }

    pub fn is_full(&self) -> bool {
        self.free_count == 0
    }

    pub fn is_depleted(&self) -> bool {
        self.exhausted_count != 0 && self.free_count + self.exhausted_count == self.slot_count
    }

    pub(crate) fn list(&self) -> SlabList {
        self.list
    }

    pub(crate) fn set_list(&mut self, list: SlabList) {
        self.list = list;
    }

    pub(crate) fn next(&self) -> *mut ZoneSlab<T> {
        self.next
    }

    pub(crate) fn set_next(&mut self, next: *mut ZoneSlab<T>) {
        self.next = next;
    }

    pub(crate) fn try_claim_slot(&mut self) -> Option<NonNull<Slot<T>>> {
        if self.free_bitmap == 0 {
            return None;
        }
        let start = usize::from(self.claim_cursor) % self.slot_count.max(1);
        let rotated = self.free_bitmap.rotate_right(start as u32);
        let offset = rotated.trailing_zeros() as usize;
        let index = (start + offset) % 64;
        debug_assert!(index < self.slot_count);
        self.free_bitmap &= !(1u64 << index);
        self.free_count -= 1;
        unsafe { Some(NonNull::new_unchecked(self.slot_base().add(index))) }
    }

    pub(crate) fn return_slot(&mut self, slot: NonNull<Slot<T>>) {
        let index = self
            .slot_index(slot)
            .expect("returned slot must belong to this slab");
        debug_assert!(index < self.slot_count);
        debug_assert_eq!(
            self.free_bitmap & (1u64 << index),
            0,
            "returned slot must not already be free"
        );
        self.free_bitmap |= 1u64 << index;
        self.free_count += 1;
        if self.free_count == self.slot_count {
            self.claim_cursor = self.claim_cursor.wrapping_add(1);
        }
    }

    pub(crate) fn mark_slot_exhausted(&mut self, slot: NonNull<Slot<T>>) {
        let index = self
            .slot_index(slot)
            .expect("exhausted slot must belong to this slab");
        debug_assert!(index < self.slot_count);
        debug_assert_eq!(
            self.free_bitmap & (1u64 << index),
            0,
            "exhausted slot must not already be on the free list"
        );
        self.exhausted_count += 1;
    }

    pub(crate) fn slot_at(&self, index: usize) -> Option<NonNull<Slot<T>>> {
        if index >= self.slot_count {
            return None;
        }
        unsafe { Some(NonNull::new_unchecked(self.slot_base().add(index))) }
    }

    pub(crate) unsafe fn from_slot(slot: NonNull<Slot<T>>) -> NonNull<Self> {
        let page_size = runtime::page_size();
        debug_assert!(page_size.is_power_of_two());
        // Slabs are single page aligned, so masking the slot address recovers
        // the slab header address.
        let base = (slot.as_ptr() as usize) & !(page_size - 1);
        unsafe { NonNull::new_unchecked(base as *mut Self) }
    }

    pub(crate) fn key_for_slot(&self, slot: NonNull<Slot<T>>) -> Option<SlotKey> {
        SlotKey::new(self.zone_id(), self.id, self.slot_index(slot)?)
    }

    pub(crate) fn slot_index(&self, slot: NonNull<Slot<T>>) -> Option<usize> {
        let base = unsafe { self.slot_base() } as usize;
        let ptr = slot.as_ptr() as usize;
        let slot_size = mem::size_of::<Slot<T>>().max(1);
        let end = base.checked_add(self.slot_count.checked_mul(slot_size)?)?;
        if ptr < base || ptr >= end {
            return None;
        }
        let offset = ptr - base;
        if !offset.is_multiple_of(slot_size) {
            return None;
        }
        Some(offset / slot_size)
    }

    pub(crate) fn zone(&self) -> &'static Zone<T> {
        self.zone
    }

    pub fn zone_id(&self) -> super::ZoneId {
        self.zone.id()
    }

    unsafe fn slot_base(&self) -> *mut Slot<T> {
        let page = self as *const Self as *mut u8;
        let header_size = align_up(mem::size_of::<Self>(), mem::align_of::<Slot<T>>())
            .expect("zone slab header layout must be alignable");
        unsafe { page.add(header_size) as *mut Slot<T> }
    }
}

pub(crate) unsafe fn reclaim_slab<T: 'static>(ptr: *mut u8) {
    let slab = ptr as *mut ZoneSlab<T>;
    unsafe {
        let backing_ppn = (*slab).backing_ppn;
        let page_count = (*slab).page_count;
        // No live slots remain when a slab is retired. Drop only the header,
        // then return the backing frames to the page allocator.
        ptr::drop_in_place(slab);
        for offset in 0..page_count {
            let _ = page_allocator::release_owned_frame(Ppn(backing_ppn.0 + offset));
        }
    }
}

unsafe fn reclaim_slab_head<T: 'static>(
    head: *mut RcuHead,
    _guard: &mut crate::epoch::LocalRetireGuard,
) {
    unsafe { reclaim_slab::<T>(head.cast::<u8>()) }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    let mask = align - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

fn initial_free_bitmap(slot_count: usize) -> u64 {
    if slot_count >= 64 {
        u64::MAX
    } else {
        (1u64 << slot_count) - 1
    }
}
