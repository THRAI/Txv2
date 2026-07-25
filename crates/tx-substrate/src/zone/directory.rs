//! Fixed-depth slab directory for lock-free `SlotKey` resolution.

use core::mem;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};

use tx_hal::PhysAddr;

use crate::page_allocator::{self, ZeroPolicy};

use super::registry::SlotKey;
use super::slab::ZoneSlab;
use super::slot::Slot;
use super::{runtime, ZoneError};

const DIRECTORY_BITS: usize = 9;
const DIRECTORY_WIDTH: usize = 1 << DIRECTORY_BITS;
const DIRECTORY_MASK: usize = DIRECTORY_WIDTH - 1;

pub(crate) struct SlabDirectory<T: 'static> {
    root: AtomicPtr<SlabDirectoryRoot<T>>,
}

struct SlabDirectoryRoot<T: 'static> {
    leaves: [AtomicPtr<SlabDirectoryLeaf<T>>; DIRECTORY_WIDTH],
}

struct SlabDirectoryLeaf<T: 'static> {
    slabs: [AtomicPtr<ZoneSlab<T>>; DIRECTORY_WIDTH],
}

impl<T: 'static> SlabDirectory<T> {
    pub(crate) const fn new() -> Self {
        Self {
            root: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub(crate) fn lookup(&self, key: SlotKey) -> Option<NonNull<Slot<T>>> {
        let (root_index, leaf_index) = directory_indices(key.slab_id())?;
        let root = NonNull::new(self.root.load(Ordering::Acquire))?;
        let leaf =
            NonNull::new(unsafe { root.as_ref().leaves[root_index].load(Ordering::Acquire) })?;
        let slab =
            NonNull::new(unsafe { leaf.as_ref().slabs[leaf_index].load(Ordering::Acquire) })?;
        if unsafe { slab.as_ref().id() } != key.slab_id() {
            return None;
        }
        unsafe { slab.as_ref().slot_at(key.slot_index()) }
    }

    /// Publish one slab. The caller serializes writers with the owning Keg lock.
    pub(crate) fn publish(&self, slab: NonNull<ZoneSlab<T>>) -> Result<(), ZoneError> {
        let slab_id = unsafe { slab.as_ref().id() };
        let (root_index, leaf_index) =
            directory_indices(slab_id).ok_or(ZoneError::AllocationFailed)?;

        let root = match NonNull::new(self.root.load(Ordering::Acquire)) {
            Some(root) => root,
            None => {
                let root = allocate_directory_page(SlabDirectoryRoot::new())?;
                self.root.store(root.as_ptr(), Ordering::Release);
                root
            }
        };

        let leaf_entry = unsafe { &root.as_ref().leaves[root_index] };
        let leaf = match NonNull::new(leaf_entry.load(Ordering::Acquire)) {
            Some(leaf) => leaf,
            None => {
                let leaf = allocate_directory_page(SlabDirectoryLeaf::new())?;
                leaf_entry.store(leaf.as_ptr(), Ordering::Release);
                leaf
            }
        };

        let slab_entry = unsafe { &leaf.as_ref().slabs[leaf_index] };
        match slab_entry.compare_exchange(
            ptr::null_mut(),
            slab.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(previous) if previous == slab.as_ptr() => Ok(()),
            Err(_) => Err(ZoneError::InvalidState),
        }
    }

    /// Stop new readers from resolving `slab` before it is unlinked and retired.
    pub(crate) fn unpublish(&self, slab: NonNull<ZoneSlab<T>>) -> Result<(), ZoneError> {
        let slab_id = unsafe { slab.as_ref().id() };
        let (root_index, leaf_index) = directory_indices(slab_id).ok_or(ZoneError::InvalidState)?;
        let root =
            NonNull::new(self.root.load(Ordering::Acquire)).ok_or(ZoneError::InvalidState)?;
        let leaf =
            NonNull::new(unsafe { root.as_ref().leaves[root_index].load(Ordering::Acquire) })
                .ok_or(ZoneError::InvalidState)?;
        let slab_entry = unsafe { &leaf.as_ref().slabs[leaf_index] };
        slab_entry
            .compare_exchange(
                slab.as_ptr(),
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| ZoneError::InvalidState)
    }
}

impl<T: 'static> SlabDirectoryRoot<T> {
    const fn new() -> Self {
        Self {
            leaves: [const { AtomicPtr::new(ptr::null_mut()) }; DIRECTORY_WIDTH],
        }
    }
}

impl<T: 'static> SlabDirectoryLeaf<T> {
    const fn new() -> Self {
        Self {
            slabs: [const { AtomicPtr::new(ptr::null_mut()) }; DIRECTORY_WIDTH],
        }
    }
}

fn directory_indices(slab_id: usize) -> Option<(usize, usize)> {
    let index = slab_id.checked_sub(1)?;
    if index >= DIRECTORY_WIDTH * DIRECTORY_WIDTH {
        return None;
    }
    Some((index >> DIRECTORY_BITS, index & DIRECTORY_MASK))
}

fn allocate_directory_page<U>(value: U) -> Result<NonNull<U>, ZoneError> {
    let page_size = runtime::page_size();
    if mem::size_of::<U>() > page_size || mem::align_of::<U>() > page_size {
        return Err(ZoneError::AllocationFailed);
    }

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
        (page as *mut U).write(value);
    }

    // Directory pages are stable metadata for the static Zone lifetime.
    mem::forget(run);
    NonNull::new(page.cast::<U>()).ok_or(ZoneError::AllocationFailed)
}
