//! Slab-list manager for a single zone.
//!
//! A Keg owns all slabs for one `Zone<T>`. It maintains three intrusive slab
//! lists so allocation prefers partially used slabs, then empty slabs, and only
//! allocates a new frame-backed slab when no reusable storage exists.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::epoch;

use super::directory::SlabDirectory;
use super::registry::SlotKey;
use super::slab::{reclaim_slab, SlabList, ZoneSlab};
use super::slot::Slot;
use super::sync::SpinLock;
use super::{Zone, ZoneError};

const EMPTY_SLAB_LOW_WATER: usize = 1;
static KEG_LOCK_ACQUISITIONS: AtomicUsize = AtomicUsize::new(0);
static KEG_LOCK_COUNTING: AtomicBool = AtomicBool::new(false);

pub(crate) fn begin_lock_counting() {
    KEG_LOCK_ACQUISITIONS.store(0, Ordering::Release);
    KEG_LOCK_COUNTING.store(true, Ordering::Release);
}

pub(crate) fn finish_lock_counting() -> usize {
    KEG_LOCK_COUNTING.store(false, Ordering::Release);
    KEG_LOCK_ACQUISITIONS.load(Ordering::Acquire)
}

/// Central slab manager for one `Zone<T>`.
///
/// It owns the partial/full/empty slab lists and hands individual free slots to
/// the reservation path or per-CPU buckets. One empty slab is retained for reuse;
/// surplus empty slabs are unlinked and retired through EBR before their frames
/// return to the page allocator.
pub(crate) struct Keg<T: 'static> {
    /// Protects the three slab lists and slab-local bitmap changes.
    lock: SpinLock,
    /// Slabs with at least one live/claimed slot and at least one free slot.
    partial_head: UnsafeCell<*mut ZoneSlab<T>>,
    /// Slabs with no free slots.
    full_head: UnsafeCell<*mut ZoneSlab<T>>,
    /// Slabs with all slots free.
    empty_head: UnsafeCell<*mut ZoneSlab<T>>,
    /// Number of slabs currently owned by this Keg.
    slab_count: AtomicUsize,
    /// Number of slabs on the empty list.
    empty_count: AtomicUsize,
    /// Monotonic slab ID source for `SlotKey`.
    next_slab_id: AtomicUsize,
    /// Stable fixed-depth lookup directory. Keg lists remain writer-only
    /// allocation and reclaim metadata.
    directory: SlabDirectory<T>,
}

unsafe impl<T: 'static> Sync for Keg<T> {}

impl<T: 'static> Keg<T> {
    pub(crate) const fn const_new() -> Self {
        Self {
            lock: SpinLock::new(),
            partial_head: UnsafeCell::new(core::ptr::null_mut()),
            full_head: UnsafeCell::new(core::ptr::null_mut()),
            empty_head: UnsafeCell::new(core::ptr::null_mut()),
            slab_count: AtomicUsize::new(0),
            empty_count: AtomicUsize::new(0),
            next_slab_id: AtomicUsize::new(1),
            directory: SlabDirectory::new(),
        }
    }

    fn lock(&self) -> super::sync::SpinGuard<'_> {
        if KEG_LOCK_COUNTING.load(Ordering::Relaxed) {
            KEG_LOCK_ACQUISITIONS.fetch_add(1, Ordering::Relaxed);
        }
        self.lock.lock()
    }

    pub(crate) fn slab_count(&self) -> usize {
        self.slab_count.load(Ordering::Acquire)
    }

    pub(crate) fn empty_slab_count(&self) -> usize {
        self.empty_count.load(Ordering::Acquire)
    }

    pub(crate) fn pop_free_slot(
        &self,
        zone: &'static Zone<T>,
    ) -> Result<NonNull<Slot<T>>, ZoneError> {
        let _guard = self.lock();
        unsafe {
            // Empty slabs are reusable; allocate only when both reusable lists
            // are empty.
            if (*self.partial_head.get()).is_null() && (*self.empty_head.get()).is_null() {
                let slab = self.allocate_slab_locked(zone)?;
                self.push_slab_locked(slab, SlabList::Empty);
            }

            let slab = if !(*self.partial_head.get()).is_null() {
                NonNull::new_unchecked(*self.partial_head.get())
            } else {
                NonNull::new_unchecked(*self.empty_head.get())
            };

            let old_list = slab.as_ref().list();
            let slot = slab
                .as_ptr()
                .as_mut()
                .and_then(ZoneSlab::try_claim_slot)
                .ok_or(ZoneError::InvalidState)?;

            self.relink_after_claim_locked(slab, old_list);
            Ok(slot)
        }
    }

    pub(crate) fn return_slot(&self, slot: NonNull<Slot<T>>) {
        self.return_slot_inner(slot, true);
    }

    pub(crate) fn return_slot_without_slab_retire(&self, slot: NonNull<Slot<T>>) {
        // EBR reclaim callbacks use this path. They may return a slot to the
        // Keg, but must not recursively enqueue a whole slab into EBR.
        self.return_slot_inner(slot, false);
    }

    fn return_slot_inner(&self, slot: NonNull<Slot<T>>, allow_slab_retire: bool) {
        if !allow_slab_retire {
            let _guard = self.lock();
            unsafe {
                let mut slab = slot.as_ref().slab();
                let old_list = slab.as_ref().list();
                slab.as_mut().return_slot(slot);
                self.relink_after_return_locked(slab, old_list);
            }
            return;
        }

        epoch::with_local_retire_guard(|local_guard| {
            let retire_candidate = {
                let _guard = self.lock();
                unsafe {
                    let mut slab = slot.as_ref().slab();
                    let old_list = slab.as_ref().list();
                    slab.as_mut().return_slot(slot);
                    self.relink_after_return_locked(slab, old_list);
                    if slab.as_ref().is_empty()
                        && self.empty_count.load(Ordering::Acquire) > EMPTY_SLAB_LOW_WATER
                    {
                        self.directory
                            .unpublish(slab)
                            .expect("linked empty slab must be published");
                        self.remove_slab_locked(slab, SlabList::Empty);
                        Some(slab)
                    } else {
                        None
                    }
                }
            };
            let Some(mut slab) = retire_candidate else {
                return;
            };
            let (zone, slots, head) = unsafe {
                (
                    slab.as_ref().zone(),
                    slab.as_ref().slot_count(),
                    slab.as_mut().retire_head(),
                )
            };
            zone.note_released_slots(slots);
            self.slab_count.fetch_sub(1, Ordering::AcqRel);
            let epoch = local_guard.sample_epoch_after_barrier();
            unsafe { local_guard.enqueue_head_after_barrier(head, epoch) };
        })
        .expect("slab return requires initialized local epoch retirement");
    }

    pub(crate) fn slot_from_key(&self, key: SlotKey) -> Option<NonNull<Slot<T>>> {
        self.directory.lookup(key)
    }

    pub(crate) fn refill_bucket<const N: usize>(
        &self,
        zone: &'static Zone<T>,
        bucket: &mut super::bucket::ZoneBucket<T, N>,
    ) -> Result<(), ZoneError> {
        while !bucket.is_full() {
            let slot = self.pop_free_slot(zone)?;
            if bucket.push(slot).is_err() {
                self.return_slot(slot);
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn trim_empty_slabs(&self, limit: usize) -> usize {
        let mut retired = 0usize;

        while retired < limit {
            let did_retire = epoch::with_local_retire_guard(|local_guard| {
                let retire_candidate = {
                    let _guard = self.lock();
                    if self.empty_count.load(Ordering::Acquire) <= EMPTY_SLAB_LOW_WATER {
                        None
                    } else {
                        let mut slab = NonNull::new(unsafe { *self.empty_head.get() });
                        if let Some(slab) = slab {
                            unsafe {
                                self.directory
                                    .unpublish(slab)
                                    .expect("linked empty slab must be published");
                                self.remove_slab_locked(slab, SlabList::Empty);
                            }
                        }
                        slab.take()
                    }
                };
                let Some(mut slab) = retire_candidate else {
                    return false;
                };
                let (zone, slots, head) = unsafe {
                    (
                        slab.as_ref().zone(),
                        slab.as_ref().slot_count(),
                        slab.as_mut().retire_head(),
                    )
                };
                zone.note_released_slots(slots);
                self.slab_count.fetch_sub(1, Ordering::AcqRel);
                let epoch = local_guard.sample_epoch_after_barrier();
                unsafe { local_guard.enqueue_head_after_barrier(head, epoch) };
                true
            })
            .expect("slab trim requires initialized local epoch retirement");
            if !did_retire {
                break;
            }
            retired += 1;
        }

        retired
    }

    unsafe fn allocate_slab_locked(
        &self,
        zone: &'static Zone<T>,
    ) -> Result<NonNull<ZoneSlab<T>>, ZoneError> {
        let id = self.next_slab_id.fetch_add(1, Ordering::AcqRel);
        let slab = ZoneSlab::allocate(zone, id)?;
        if let Err(err) = self.directory.publish(slab) {
            unsafe {
                reclaim_slab::<T>(slab.as_ptr().cast::<u8>());
            }
            return Err(err);
        }
        self.slab_count.fetch_add(1, Ordering::AcqRel);
        unsafe {
            zone.note_allocated_slots(slab.as_ref().slot_count());
        }
        Ok(slab)
    }

    unsafe fn relink_after_claim_locked(&self, slab: NonNull<ZoneSlab<T>>, old_list: SlabList) {
        unsafe {
            let new_list = if slab.as_ref().is_full() {
                SlabList::Full
            } else {
                SlabList::Partial
            };
            self.move_slab_locked(slab, old_list, new_list);
        }
    }

    unsafe fn relink_after_return_locked(&self, slab: NonNull<ZoneSlab<T>>, old_list: SlabList) {
        unsafe {
            let new_list = if slab.as_ref().is_empty() {
                SlabList::Empty
            } else {
                SlabList::Partial
            };
            self.move_slab_locked(slab, old_list, new_list);
        }
    }

    unsafe fn move_slab_locked(
        &self,
        slab: NonNull<ZoneSlab<T>>,
        old_list: SlabList,
        new_list: SlabList,
    ) {
        if old_list == new_list {
            return;
        }
        unsafe {
            self.remove_slab_locked(slab, old_list);
            self.push_slab_locked(slab, new_list);
        }
    }

    unsafe fn push_slab_locked(&self, mut slab: NonNull<ZoneSlab<T>>, list: SlabList) {
        let head = self.head_for(list);
        unsafe {
            slab.as_mut().set_list(list);
            slab.as_mut().set_next(*head);
            *head = slab.as_ptr();
        }
        if list == SlabList::Empty {
            self.empty_count.fetch_add(1, Ordering::AcqRel);
        }
    }

    unsafe fn remove_slab_locked(&self, slab: NonNull<ZoneSlab<T>>, list: SlabList) {
        if list == SlabList::Unlinked {
            return;
        }

        let head = self.head_for(list);
        unsafe {
            let mut current = *head;
            let mut previous: *mut ZoneSlab<T> = core::ptr::null_mut();

            while !current.is_null() {
                if current == slab.as_ptr() {
                    let next = (*current).next();
                    if previous.is_null() {
                        *head = next;
                    } else {
                        (*previous).set_next(next);
                    }
                    (*current).set_next(core::ptr::null_mut());
                    (*current).set_list(SlabList::Unlinked);
                    if list == SlabList::Empty {
                        self.empty_count.fetch_sub(1, Ordering::AcqRel);
                    }
                    return;
                }
                previous = current;
                current = (*current).next();
            }
        }
    }

    fn head_for(&self, list: SlabList) -> *mut *mut ZoneSlab<T> {
        match list {
            SlabList::Partial => self.partial_head.get(),
            SlabList::Full => self.full_head.get(),
            SlabList::Empty => self.empty_head.get(),
            SlabList::Unlinked => unreachable!("unlinked slab has no list head"),
        }
    }
}
