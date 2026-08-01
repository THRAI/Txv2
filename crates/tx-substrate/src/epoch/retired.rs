//! Crossbeam-style bags used by EBR.
//!
//! Each CPU accumulates deferred callbacks in a small local bag. A full (or
//! explicitly flushed) bag is sealed with the current epoch and appended to a
//! shared FIFO. Collection examines only the oldest sealed bags and executes a
//! bounded number of callbacks, avoiding a scan of every non-expired object.

use core::mem;
use core::ptr::{self, NonNull};

use crate::page_allocator::{self, AllocError, PageAllocator, ZeroPolicy};
use tx_hal::Ppn;

/// Crossbeam uses 64 deferred callbacks per thread-local bag. Keep the same
/// batching point: it amortizes publication without making a collection step
/// excessively large.
pub const RETIRED_BAG_CAPACITY: usize = 64;
const RETIRED_BAG_CACHE_CAPACITY: usize = 64;

#[derive(Clone, Copy)]
pub(crate) struct RetiredEntry {
    pub(crate) ptr: *mut u8,
    pub(crate) reclaim_fn: unsafe fn(*mut u8),
}

impl RetiredEntry {
    const EMPTY: Self = Self {
        ptr: ptr::null_mut(),
        reclaim_fn: noop_reclaim,
    };
}

unsafe fn noop_reclaim(_ptr: *mut u8) {}

pub(crate) struct LocalBag {
    entries: [RetiredEntry; RETIRED_BAG_CAPACITY],
    len: usize,
}

impl LocalBag {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [RetiredEntry::EMPTY; RETIRED_BAG_CAPACITY],
            len: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == RETIRED_BAG_CAPACITY
    }

    pub(crate) fn push(&mut self, entry: RetiredEntry) {
        debug_assert!(!self.is_full());
        self.entries[self.len] = entry;
        self.len += 1;
    }

    pub(crate) fn reset(&mut self) {
        for entry in &mut self.entries[..self.len] {
            *entry = RetiredEntry::EMPTY;
        }
        self.len = 0;
    }
}

/// A sealed bag occupies one allocator page and can therefore be transferred
/// between CPUs without recursively using the Rust global allocator.
#[repr(C)]
pub(crate) struct SealedBag {
    pub(crate) next: Option<NonNull<SealedBag>>,
    pub(crate) ppn: Ppn,
    pub(crate) epoch: u64,
    pub(crate) owner_cpu: usize,
    pub(crate) len: usize,
    pub(crate) cursor: usize,
    pub(crate) entries: [RetiredEntry; RETIRED_BAG_CAPACITY],
}

const _: () = assert!(mem::size_of::<SealedBag>() <= 4096);

impl SealedBag {
    pub(crate) unsafe fn fill_from_local(
        bag: NonNull<Self>,
        local: &mut LocalBag,
        epoch: u64,
        owner_cpu: usize,
    ) {
        let ppn = unsafe { bag.as_ref().ppn };
        unsafe {
            bag.as_ptr().write(Self {
                next: None,
                ppn,
                epoch,
                owner_cpu,
                len: local.len,
                cursor: 0,
                entries: local.entries,
            });
        }
        local.reset();
    }

    pub(crate) fn is_expired(&self, global_epoch: u64) -> bool {
        global_epoch >= self.epoch.saturating_add(2)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.len.saturating_sub(self.cursor)
    }

    pub(crate) fn reset_for_cache(&mut self) {
        self.epoch = 0;
        self.owner_cpu = 0;
        self.len = 0;
        self.cursor = 0;
        self.next = None;
    }
}

pub(crate) fn allocate_bag_page() -> Result<NonNull<SealedBag>, AllocError> {
    let frame = page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite)?.commit();
    let ppn = frame.ppn();
    let ptr = page_allocator::frame_kernel_addr(ppn)? as *mut SealedBag;
    let bag = NonNull::new(ptr).ok_or(AllocError::FrameKernelAddrUnavailable)?;
    unsafe {
        bag.as_ptr().write(SealedBag {
            next: None,
            ppn,
            epoch: 0,
            owner_cpu: 0,
            len: 0,
            cursor: 0,
            entries: [RetiredEntry::EMPTY; RETIRED_BAG_CAPACITY],
        });
    }
    core::mem::forget(frame);
    Ok(bag)
}

pub(crate) struct BagQueue {
    head: Option<NonNull<SealedBag>>,
    tail: Option<NonNull<SealedBag>>,
    free: Option<NonNull<SealedBag>>,
    cached_bags: usize,
    queued_entries: usize,
}

impl BagQueue {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            free: None,
            cached_bags: 0,
            queued_entries: 0,
        }
    }

    pub(crate) fn take_cached(&mut self) -> Option<NonNull<SealedBag>> {
        let mut bag = self.free?;
        self.free = unsafe { bag.as_ref().next };
        self.cached_bags -= 1;
        unsafe {
            bag.as_mut().next = None;
        }
        Some(bag)
    }

    pub(crate) fn push_back(&mut self, mut bag: NonNull<SealedBag>) {
        let remaining = unsafe { bag.as_ref().remaining() };
        unsafe {
            bag.as_mut().next = None;
        }
        match self.tail {
            Some(mut tail) => unsafe {
                tail.as_mut().next = Some(bag);
            },
            None => self.head = Some(bag),
        }
        self.tail = Some(bag);
        self.queued_entries = self.queued_entries.saturating_add(remaining);
    }

    pub(crate) fn push_front(&mut self, mut bag: NonNull<SealedBag>) {
        let remaining = unsafe { bag.as_ref().remaining() };
        unsafe {
            bag.as_mut().next = self.head;
        }
        self.head = Some(bag);
        if self.tail.is_none() {
            self.tail = Some(bag);
        }
        self.queued_entries = self.queued_entries.saturating_add(remaining);
    }

    pub(crate) fn pop_expired(&mut self, global_epoch: u64) -> Option<NonNull<SealedBag>> {
        let mut bag = self.head?;
        if !unsafe { bag.as_ref().is_expired(global_epoch) } {
            return None;
        }
        let remaining = unsafe { bag.as_ref().remaining() };
        self.head = unsafe { bag.as_ref().next };
        if self.head.is_none() {
            self.tail = None;
        }
        unsafe {
            bag.as_mut().next = None;
        }
        self.queued_entries = self.queued_entries.saturating_sub(remaining);
        Some(bag)
    }

    pub(crate) fn recycle(&mut self, mut bag: NonNull<SealedBag>) {
        if self.cached_bags >= RETIRED_BAG_CACHE_CAPACITY {
            let ppn = unsafe { bag.as_ref().ppn };
            if let Ok(allocator) = page_allocator::installed_bitmap_allocator() {
                allocator.release_owned(ppn);
                return;
            }
        }
        unsafe {
            bag.as_mut().reset_for_cache();
            bag.as_mut().next = self.free;
        }
        self.free = Some(bag);
        self.cached_bags += 1;
    }

    /// Discard queued callbacks during test-domain reset and retain their pages
    /// in the bag cache. Production never resets a live epoch domain.
    pub(crate) fn reset(&mut self) {
        while let Some(mut bag) = self.head {
            self.head = unsafe { bag.as_ref().next };
            unsafe { bag.as_mut().next = None };
            self.recycle(bag);
        }
        self.tail = None;
        self.queued_entries = 0;
    }
}
