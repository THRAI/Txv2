//! Slab-backed kernel heap.
//!
//! The slab heap is the allocation layer that becomes legal after
//! `tx_substrate::init::<P>()` has installed the page allocator. Small
//! allocations use per-size-class pages; page-sized and larger allocations
//! reserve contiguous physical page runs directly.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use tx_hal::{
    PhysAddr, PmapError, PmapInvalidation, PmapPermissions, PmapReservation, PmapReserveKind,
    PmapUnmapResult, Ppn, TxPlatform, VirtAddr,
};

use crate::page_allocator::{
    self, installed_bitmap_allocator, AllocError, PageAllocator, ZeroPolicy,
};

const MIN_CLASS: usize = 8;
const MAX_SLAB_CLASS: usize = 2048;
const CLASS_COUNT: usize = 9;
const CLASS_SIZES: [usize; CLASS_COUNT] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];
const CLASS_REFILL_PAGES: [usize; CLASS_COUNT] = [1, 1, 1, 4, 8, 4, 2, 1, 1];
const DEFAULT_PAGE_SIZE: usize = 4096;
const RETAIN_EMPTY_SLAB_PAGES_PER_CLASS: usize = 1;

// Large Rust allocations need contiguous virtual addresses, not contiguous
// physical frames. Keep moderate allocations on the direct-map heap, while
// routing genuinely large buffers through a separately mapped kernel window.
// The window starts immediately above RV64's 128-GiB direct-map range; LA64
// accepts the same high-half range through its global PGDH.  Four GiB leaves
// enough virtual headroom for a memory-sized compiler workload while remaining
// bounded.  A permanent leaf in every 1-GiB Sv39 root slot makes those slots
// present before process roots copy the kernel half of the bootstrap root.
const VMALLOC_THRESHOLD: usize = 64 * 1024;
const VMALLOC_DIRECT_TRY_MAX: usize = 1024 * 1024;
const VMALLOC_BASE: usize = 0xffff_ffe0_0000_0000;
const VMALLOC_ROOT_SLOT_SIZE: usize = 1024 * 1024 * 1024;
const VMALLOC_SIZE: usize = 4 * VMALLOC_ROOT_SLOT_SIZE;
const VMALLOC_PAGE_COUNT: usize = VMALLOC_SIZE / DEFAULT_PAGE_SIZE;
const VMALLOC_BITMAP_WORDS: usize = VMALLOC_PAGE_COUNT / u64::BITS as usize;
const VMALLOC_ROOT_SLOT_PAGES: usize = VMALLOC_ROOT_SLOT_SIZE / DEFAULT_PAGE_SIZE;

type ReserveKernelMappingFn =
    fn(VirtAddr, PhysAddr, PmapReserveKind) -> Result<Option<PmapReservation>, PmapError>;
type CommitKernelMappingFn = fn(PmapReservation, PmapPermissions);
type UnmapKernelMappingFn =
    fn(VirtAddr, PmapReserveKind) -> Result<Option<PmapUnmapResult>, PmapError>;
type ShootdownKernelMappingsFn = fn(&[PmapInvalidation]);
type ServicePendingTlbShootdownFn = fn();

/// Slab allocation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlabError {
    /// The heap has not been initialized after page allocator bring-up.
    NotInitialized,
    /// `init()` was called twice.
    AlreadyInitialized,
    /// The requested `Layout` cannot be represented by this v1 heap.
    InvalidLayout,
    /// Page allocation failed.
    PageAllocator(AllocError),
}

impl From<AllocError> for SlabError {
    fn from(value: AllocError) -> Self {
        Self::PageAllocator(value)
    }
}

/// Physical-page source used by `SlabHeap`.
///
/// # Safety
///
/// Implementors must return page-aligned direct-map pointers for reserved
/// frames and must only release runs previously reserved through the same
/// provider.
pub unsafe trait SlabPageProvider: Copy {
    /// Hardware page size used by this provider.
    fn page_size(&self) -> usize;

    /// Reserve and commit a contiguous run of pages.
    fn reserve_run(&self, count: usize, align: usize) -> Result<Ppn, AllocError>;

    /// Release a previously reserved run.
    ///
    /// # Safety
    ///
    /// `base..base + count` must name a currently live run reserved through
    /// this provider and not yet released.
    unsafe fn release_run(&self, base: Ppn, count: usize);

    /// Convert a PPN to a writable direct-map pointer.
    ///
    /// # Safety
    ///
    /// `ppn` must name a live frame reserved through this provider, and the
    /// returned pointer must be used only within that frame's lifetime.
    unsafe fn direct_map_ptr(&self, ppn: Ppn) -> *mut u8;

    /// Convert a direct-map page pointer back to a PPN.
    fn ppn_from_direct_map_ptr(&self, ptr: *mut u8) -> Option<Ppn>;
}

/// Reusable slab heap over a page provider.
pub struct SlabHeap<P: SlabPageProvider> {
    provider: P,
    initialized: AtomicBool,
    classes: [SlabClass; CLASS_COUNT],
    small_live_pages: AtomicUsize,
    small_peak_pages: AtomicUsize,
    large_live_allocations: AtomicUsize,
    large_live_pages: AtomicUsize,
    large_peak_pages: AtomicUsize,
    large_alloc_calls: AtomicUsize,
    large_free_calls: AtomicUsize,
    large_live_allocations_by_bin: [AtomicUsize; 4],
    large_live_pages_by_bin: [AtomicUsize; 4],
}

unsafe impl<P: SlabPageProvider + Sync> Sync for SlabHeap<P> {}

impl<P: SlabPageProvider> SlabHeap<P> {
    /// Construct an uninitialized heap.
    pub const fn new(provider: P) -> Self {
        Self {
            provider,
            initialized: AtomicBool::new(false),
            classes: [const { SlabClass::new() }; CLASS_COUNT],
            small_live_pages: AtomicUsize::new(0),
            small_peak_pages: AtomicUsize::new(0),
            large_live_allocations: AtomicUsize::new(0),
            large_live_pages: AtomicUsize::new(0),
            large_peak_pages: AtomicUsize::new(0),
            large_alloc_calls: AtomicUsize::new(0),
            large_free_calls: AtomicUsize::new(0),
            large_live_allocations_by_bin: [const { AtomicUsize::new(0) }; 4],
            large_live_pages_by_bin: [const { AtomicUsize::new(0) }; 4],
        }
    }

    /// Make allocation legal for this heap.
    pub fn init(&self) -> Result<(), SlabError> {
        let page_size = self.provider.page_size();
        if page_size == 0 || !page_size.is_power_of_two() {
            return Err(SlabError::InvalidLayout);
        }

        self.initialized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SlabError::AlreadyInitialized)
    }

    /// Allocate a block for `layout`.
    pub fn try_alloc(&self, layout: Layout) -> Result<NonNull<u8>, SlabError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(SlabError::NotInitialized);
        }

        if layout.size() == 0 {
            return Err(SlabError::InvalidLayout);
        }

        match class_index(layout)? {
            Some(index) => self.alloc_small(index),
            None => self.alloc_large(layout),
        }
    }

    /// Deallocate a block previously returned by `try_alloc`.
    ///
    /// # Safety
    ///
    /// `ptr` and `layout` must match a live allocation from this heap.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() || layout.size() == 0 || !self.initialized.load(Ordering::Acquire) {
            return;
        }

        match class_index(layout) {
            Ok(Some(index)) => unsafe { self.dealloc_small(index, ptr) },
            Ok(None) => unsafe { self.dealloc_large(ptr, layout) },
            Err(_) => {}
        }
    }

    fn alloc_small(&self, index: usize) -> Result<NonNull<u8>, SlabError> {
        let class = &self.classes[index];
        let _guard = class.lock.lock();

        unsafe {
            if (*class.partial_pages.get()).is_null() && (*class.empty_pages.get()).is_null() {
                self.populate_class(index, class)?;
            }

            let page = if !(*class.partial_pages.get()).is_null() {
                *class.partial_pages.get()
            } else {
                *class.empty_pages.get()
            };
            if page.is_null() {
                return Err(SlabError::PageAllocator(AllocError::Exhausted));
            }

            let object = (*page).free_list;
            if object.is_null() {
                return Err(SlabError::PageAllocator(AllocError::Exhausted));
            }

            debug_assert_eq!((*page).magic, SLAB_PAGE_MAGIC);
            debug_assert_eq!((*page).class_index, index);
            debug_assert!((*page).free_count > 0);

            (*page).free_list = (*object).next;
            if (*page).retained_empty {
                debug_assert_eq!((*page).state, SlabPageState::Empty);
                debug_assert_eq!((*page).free_count, (*page).capacity);
                (*page).retained_empty = false;
                *class.retained_empty_pages.get() -= 1;
            }
            (*page).free_count -= 1;

            let new_state = if (*page).free_count == 0 {
                SlabPageState::Full
            } else {
                SlabPageState::Partial
            };
            if (*page).state != new_state {
                move_page(class, page, new_state);
            }

            Ok(NonNull::new_unchecked(object as *mut u8))
        }
    }

    unsafe fn dealloc_small(&self, index: usize, ptr: *mut u8) {
        let class = &self.classes[index];
        let mut page_to_release = ptr::null_mut();
        let _guard = class.lock.lock();

        unsafe {
            let header = page_header_for_object(ptr, self.provider.page_size());
            debug_assert_eq!((*header).magic, SLAB_PAGE_MAGIC);
            debug_assert_eq!((*header).class_index, index);
            debug_assert_ne!((*header).state, SlabPageState::Detached);
            debug_assert_ne!((*header).state, SlabPageState::Empty);
            debug_assert!((*header).free_count < (*header).capacity);

            let object = ptr as *mut FreeObject;
            (*object).next = (*header).free_list;
            (*header).free_list = object;
            (*header).free_count += 1;

            if (*header).free_count == (*header).capacity {
                if *class.retained_empty_pages.get() < RETAIN_EMPTY_SLAB_PAGES_PER_CLASS {
                    (*header).retained_empty = true;
                    *class.retained_empty_pages.get() += 1;
                    move_page(class, header, SlabPageState::Empty);
                } else {
                    unlink_page(class, header);
                    page_to_release = header;
                }
            } else if (*header).state == SlabPageState::Full {
                move_page(class, header, SlabPageState::Partial);
            }
        }

        if !page_to_release.is_null() {
            unsafe {
                let ppn = (*page_to_release).ppn;
                ptr::write_bytes(page_to_release as *mut u8, 0, self.provider.page_size());
                self.provider.release_run(ppn, 1);
                self.small_live_pages.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    fn alloc_large(&self, layout: Layout) -> Result<NonNull<u8>, SlabError> {
        let page_size = self.provider.page_size();
        if layout.align() > page_size && !layout.align().is_multiple_of(page_size) {
            return Err(SlabError::InvalidLayout);
        }

        let count = div_ceil(layout.size(), page_size).ok_or(SlabError::InvalidLayout)?;
        let align = if layout.align() <= page_size {
            1
        } else {
            layout.align() / page_size
        };
        let ppn = self.provider.reserve_run(count, align)?;
        let ptr = unsafe { self.provider.direct_map_ptr(ppn) };
        let Some(ptr) = NonNull::new(ptr) else {
            unsafe {
                self.provider.release_run(ppn, count);
            }
            return Err(SlabError::InvalidLayout);
        };

        self.large_alloc_calls.fetch_add(1, Ordering::Relaxed);
        self.large_live_allocations.fetch_add(1, Ordering::Relaxed);
        let live_pages = self
            .large_live_pages
            .fetch_add(count, Ordering::Relaxed)
            .saturating_add(count);
        atomic_max_usize(&self.large_peak_pages, live_pages);
        let bin = large_page_bin(count);
        self.large_live_allocations_by_bin[bin].fetch_add(1, Ordering::Relaxed);
        self.large_live_pages_by_bin[bin].fetch_add(count, Ordering::Relaxed);

        Ok(ptr)
    }

    unsafe fn dealloc_large(&self, ptr: *mut u8, layout: Layout) {
        let page_size = self.provider.page_size();
        let Some(ppn) = self.provider.ppn_from_direct_map_ptr(ptr) else {
            return;
        };
        let Some(count) = div_ceil(layout.size(), page_size) else {
            return;
        };
        self.large_free_calls.fetch_add(1, Ordering::Relaxed);
        self.large_live_allocations.fetch_sub(1, Ordering::Relaxed);
        self.large_live_pages.fetch_sub(count, Ordering::Relaxed);
        let bin = large_page_bin(count);
        self.large_live_allocations_by_bin[bin].fetch_sub(1, Ordering::Relaxed);
        self.large_live_pages_by_bin[bin].fetch_sub(count, Ordering::Relaxed);
        unsafe {
            self.provider.release_run(ppn, count);
        }
    }

    unsafe fn populate_class(&self, index: usize, class: &SlabClass) -> Result<(), SlabError> {
        let size = CLASS_SIZES[index];
        let page_size = self.provider.page_size();
        let object_start = align_up(core::mem::size_of::<SlabPageHeader>(), size)
            .ok_or(SlabError::InvalidLayout)?;
        let capacity = (page_size - object_start) / size;
        if capacity == 0 {
            return Err(SlabError::InvalidLayout);
        }

        let mut page_count = CLASS_REFILL_PAGES[index].max(1);
        let ppn = match self.provider.reserve_run(page_count, 1) {
            Ok(ppn) => ppn,
            Err(err) if page_count > 1 => {
                page_count = 1;
                match self.provider.reserve_run(1, 1) {
                    Ok(ppn) => ppn,
                    Err(_) => return Err(err.into()),
                }
            }
            Err(err) => return Err(err.into()),
        };

        unsafe {
            for page_offset in 0..page_count {
                let page = self.provider.direct_map_ptr(Ppn(ppn.0 + page_offset));
                if page.is_null() {
                    self.provider.release_run(ppn, page_count);
                    return Err(SlabError::InvalidLayout);
                }
            }

            for page_offset in 0..page_count {
                let page_ppn = Ppn(ppn.0 + page_offset);
                let page = self.provider.direct_map_ptr(page_ppn);

                let header = page as *mut SlabPageHeader;
                header.write(SlabPageHeader {
                    magic: SLAB_PAGE_MAGIC,
                    ppn: page_ppn,
                    class_index: index,
                    capacity,
                    free_count: capacity,
                    free_list: ptr::null_mut(),
                    state: SlabPageState::Detached,
                    retained_empty: false,
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                });

                for offset in 0..capacity {
                    let object = page.add(object_start + offset * size) as *mut FreeObject;
                    (*object).next = (*header).free_list;
                    (*header).free_list = object;
                }

                link_page(class, header, SlabPageState::Empty);
            }
        }

        let live_pages = self
            .small_live_pages
            .fetch_add(page_count, Ordering::Relaxed)
            .saturating_add(page_count);
        atomic_max_usize(&self.small_peak_pages, live_pages);

        Ok(())
    }

    pub fn diagnostics(&self) -> SlabHeapDiagnostics {
        let mut large_live_allocations_by_bin = [0usize; 4];
        let mut large_live_pages_by_bin = [0usize; 4];
        for index in 0..4 {
            large_live_allocations_by_bin[index] =
                self.large_live_allocations_by_bin[index].load(Ordering::Acquire);
            large_live_pages_by_bin[index] =
                self.large_live_pages_by_bin[index].load(Ordering::Acquire);
        }

        let mut small_class_pages = [0usize; CLASS_COUNT];
        let mut small_class_used_objects = [0usize; CLASS_COUNT];
        let mut small_class_capacity_objects = [0usize; CLASS_COUNT];
        let mut small_class_empty_pages = [0usize; CLASS_COUNT];
        let mut small_class_partial_pages = [0usize; CLASS_COUNT];
        let mut small_class_full_pages = [0usize; CLASS_COUNT];
        let scan_limit = self
            .small_live_pages
            .load(Ordering::Acquire)
            .saturating_add(1);

        for (index, class) in self.classes.iter().enumerate() {
            let _guard = class.lock.lock();
            unsafe {
                for (head, state) in [
                    (*class.empty_pages.get(), SlabPageState::Empty),
                    (*class.partial_pages.get(), SlabPageState::Partial),
                    (*class.full_pages.get(), SlabPageState::Full),
                ] {
                    let mut page = head;
                    let mut scanned = 0usize;
                    while !page.is_null() && scanned < scan_limit {
                        small_class_pages[index] += 1;
                        small_class_capacity_objects[index] =
                            small_class_capacity_objects[index].saturating_add((*page).capacity);
                        small_class_used_objects[index] = small_class_used_objects[index]
                            .saturating_add((*page).capacity.saturating_sub((*page).free_count));
                        match state {
                            SlabPageState::Empty => small_class_empty_pages[index] += 1,
                            SlabPageState::Partial => small_class_partial_pages[index] += 1,
                            SlabPageState::Full => small_class_full_pages[index] += 1,
                            SlabPageState::Detached => {}
                        }
                        page = (*page).next;
                        scanned += 1;
                    }
                }
            }
        }

        SlabHeapDiagnostics {
            small_live_pages: self.small_live_pages.load(Ordering::Acquire),
            small_peak_pages: self.small_peak_pages.load(Ordering::Acquire),
            large_live_allocations: self.large_live_allocations.load(Ordering::Acquire),
            large_live_pages: self.large_live_pages.load(Ordering::Acquire),
            large_peak_pages: self.large_peak_pages.load(Ordering::Acquire),
            large_alloc_calls: self.large_alloc_calls.load(Ordering::Acquire),
            large_free_calls: self.large_free_calls.load(Ordering::Acquire),
            large_live_allocations_by_bin,
            large_live_pages_by_bin,
            small_class_sizes: CLASS_SIZES,
            small_class_pages,
            small_class_used_objects,
            small_class_capacity_objects,
            small_class_empty_pages,
            small_class_partial_pages,
            small_class_full_pages,
        }
    }
}

/// Snapshot of physical pages retained by the direct-map kernel heap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SlabHeapDiagnostics {
    pub small_live_pages: usize,
    pub small_peak_pages: usize,
    pub large_live_allocations: usize,
    pub large_live_pages: usize,
    pub large_peak_pages: usize,
    pub large_alloc_calls: usize,
    pub large_free_calls: usize,
    /// Live direct-map allocations in page-count bins: 1, 2..15, 16..255,
    /// and 256 or more pages.
    pub large_live_allocations_by_bin: [usize; 4],
    /// Live physical pages in the same bins as
    /// `large_live_allocations_by_bin`.
    pub large_live_pages_by_bin: [usize; 4],
    pub small_class_sizes: [usize; CLASS_COUNT],
    pub small_class_pages: [usize; CLASS_COUNT],
    pub small_class_used_objects: [usize; CLASS_COUNT],
    pub small_class_capacity_objects: [usize; CLASS_COUNT],
    pub small_class_empty_pages: [usize; CLASS_COUNT],
    pub small_class_partial_pages: [usize; CLASS_COUNT],
    pub small_class_full_pages: [usize; CLASS_COUNT],
}

const fn large_page_bin(page_count: usize) -> usize {
    match page_count {
        0 | 1 => 0,
        2..=15 => 1,
        16..=255 => 2,
        _ => 3,
    }
}

struct SlabClass {
    lock: SpinLock,
    empty_pages: UnsafeCell<*mut SlabPageHeader>,
    partial_pages: UnsafeCell<*mut SlabPageHeader>,
    full_pages: UnsafeCell<*mut SlabPageHeader>,
    retained_empty_pages: UnsafeCell<usize>,
}

unsafe impl Sync for SlabClass {}

impl SlabClass {
    const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            empty_pages: UnsafeCell::new(ptr::null_mut()),
            partial_pages: UnsafeCell::new(ptr::null_mut()),
            full_pages: UnsafeCell::new(ptr::null_mut()),
            retained_empty_pages: UnsafeCell::new(0),
        }
    }
}

struct SpinLock {
    held: AtomicBool,
}

impl SpinLock {
    const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> SpinGuard<'_> {
        let mut wait = crate::sync::SpinWait::new();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            wait.tick();
        }
        SpinGuard { lock: self }
    }

    fn lock_with_progress<F>(&self, mut progress: F) -> SpinGuard<'_>
    where
        F: FnMut(),
    {
        let mut wait = crate::sync::SpinWait::new();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            wait.tick_with(&mut progress);
        }
        SpinGuard { lock: self }
    }
}

struct SpinGuard<'a> {
    lock: &'a SpinLock,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

static VMALLOC_VA_LOCK: SpinLock = SpinLock::new();
static VMALLOC_MAP_LOCK: SpinLock = SpinLock::new();
static VMALLOC_READY: AtomicBool = AtomicBool::new(false);
static VMALLOC_INITIALIZED: AtomicBool = AtomicBool::new(false);
static VMALLOC_HINT: AtomicUsize = AtomicUsize::new(1);
static VMALLOC_BITMAP: [AtomicU64; VMALLOC_BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; VMALLOC_BITMAP_WORDS];
static VMALLOC_RESERVE_MAPPING: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_COMMIT_NEW_MAPPING: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_UNMAP_MAPPING: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_SHOOTDOWN_MAPPINGS: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_SERVICE_PENDING_TLB_SHOOTDOWN: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_ALLOC_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_ALLOC_SUCCESSES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LIVE_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LIVE_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_PEAK_LIVE_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_UNMAP_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_PARTIAL_UNMAPS: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_BAD_DEALLOCS: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_QUARANTINED_RANGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_QUARANTINED_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_FAIL_STAGE: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_FAIL_CAUSE_STAGE: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_FAIL_REQUEST_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_FAIL_MAPPED_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_FAIL_START_PAGE: AtomicUsize = AtomicUsize::new(usize::MAX);
static VMALLOC_LAST_FAIL_PMAP_ERROR: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_FAIL_ALLOC_ERROR: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_UNMAP_REQUEST_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_UNMAP_REMOVED_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_UNMAP_MISSING_PAGES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_UNMAP_FRAME_LOOKUP_FAILURES: AtomicUsize = AtomicUsize::new(0);
static VMALLOC_LAST_UNMAP_FIRST_FAILED_PAGE: AtomicUsize = AtomicUsize::new(usize::MAX);
static VMALLOC_LAST_UNMAP_PMAP_ERROR: AtomicUsize = AtomicUsize::new(0);

const VMALLOC_FAIL_NONE: usize = 0;
const VMALLOC_FAIL_NOT_INITIALIZED: usize = 1;
const VMALLOC_FAIL_INVALID_LAYOUT: usize = 2;
const VMALLOC_FAIL_VA_RANGE: usize = 3;
const VMALLOC_FAIL_ALLOCATOR_UNAVAILABLE: usize = 4;
const VMALLOC_FAIL_FRAME_ALLOC: usize = 5;
const VMALLOC_FAIL_PHYS_OVERFLOW: usize = 6;
const VMALLOC_FAIL_MAP_MISSING_RESERVATION: usize = 7;
const VMALLOC_FAIL_MAP_ERROR: usize = 8;
const VMALLOC_FAIL_ROLLBACK_INCOMPLETE: usize = 9;
const VMALLOC_FAIL_BAD_DEALLOC: usize = 10;

const VMALLOC_PMAP_NONE: usize = 0;
const VMALLOC_PMAP_MISSING: usize = 1;
const VMALLOC_PMAP_INVALID_REQUEST: usize = 2;
const VMALLOC_PMAP_ALREADY_MAPPED: usize = 3;
const VMALLOC_PMAP_EXHAUSTED: usize = 4;
const VMALLOC_PMAP_UNSUPPORTED: usize = 5;
const VMALLOC_PMAP_FRAME_LOOKUP: usize = 6;

const VMALLOC_ALLOC_ERROR_NONE: usize = 0;
const VMALLOC_ALLOC_ERROR_EXHAUSTED: usize = 1;
const VMALLOC_ALLOC_ERROR_INVALID_REQUEST: usize = 2;
const VMALLOC_ALLOC_ERROR_NOT_INITIALIZED: usize = 3;
const VMALLOC_ALLOC_ERROR_ALREADY_INSTALLED: usize = 4;
const VMALLOC_ALLOC_ERROR_ZERO_SCRUB_UNAVAILABLE: usize = 5;
const VMALLOC_ALLOC_ERROR_FRAME_COPY_UNAVAILABLE: usize = 6;
const VMALLOC_ALLOC_ERROR_FRAME_KERNEL_ADDR_UNAVAILABLE: usize = 7;
const VMALLOC_ALLOC_ERROR_RESERVED_FRAME: usize = 8;
const VMALLOC_ALLOC_ERROR_COUNTER_OVERFLOW: usize = 9;
const VMALLOC_ALLOC_ERROR_COUNTER_UNDERFLOW: usize = 10;
const VMALLOC_ALLOC_ERROR_DOUBLE_FREE: usize = 11;

#[derive(Clone, Copy, Debug, Default)]
struct VmallocUnmapReport {
    requested: usize,
    removed: usize,
    missing: usize,
    frame_lookup_failures: usize,
    first_failed_page: usize,
    pmap_error: usize,
}

impl VmallocUnmapReport {
    fn new(requested: usize) -> Self {
        Self {
            requested,
            first_failed_page: usize::MAX,
            ..Self::default()
        }
    }

    fn complete(self) -> bool {
        self.removed == self.requested
            && self.missing == 0
            && self.frame_lookup_failures == 0
            && self.pmap_error == VMALLOC_PMAP_NONE
    }

    fn record_failure(&mut self, page: usize, error: usize) {
        if self.first_failed_page == usize::MAX {
            self.first_failed_page = page;
            self.pmap_error = error;
        }
    }
}

fn init_vmalloc<P: TxPlatform>() {
    VMALLOC_RESERVE_MAPPING.store(P::reserve_kernel_mapping as usize, Ordering::Release);
    VMALLOC_COMMIT_NEW_MAPPING.store(P::commit_new_kernel_mapping as usize, Ordering::Release);
    VMALLOC_UNMAP_MAPPING.store(P::unmap_kernel_mapping as usize, Ordering::Release);
    VMALLOC_SHOOTDOWN_MAPPINGS.store(P::shootdown_kernel_mappings as usize, Ordering::Release);
    VMALLOC_SERVICE_PENDING_TLB_SHOOTDOWN
        .store(P::service_pending_tlb_shootdown as usize, Ordering::Release);

    // Keep one leaf permanently mapped in every Sv39 root slot covered by the
    // window. RV64 process roots copy the kernel-half root slots when they are
    // created; pre-populating every slot keeps its lower page-table branch
    // shared by every future process root. LA64 uses the same harmless guards
    // below its global PGDH.
    let _va_guard = VMALLOC_VA_LOCK.lock();
    let _map_guard = VMALLOC_MAP_LOCK.lock_with_progress(vmalloc_service_pending_tlb_shootdown);
    let Ok(allocator) = installed_bitmap_allocator() else {
        return;
    };
    for guard_page in (0..VMALLOC_PAGE_COUNT).step_by(VMALLOC_ROOT_SLOT_PAGES) {
        set_vmalloc_page_used(guard_page);
        let Ok(frame) = allocator.reserve_frame(ZeroPolicy::UninitFullOverwrite) else {
            clear_vmalloc_page(guard_page);
            return;
        };
        let owned = frame.commit();
        let ppn = owned.ppn();
        let Some(phys) = ppn.0.checked_mul(DEFAULT_PAGE_SIZE) else {
            clear_vmalloc_page(guard_page);
            return;
        };
        let virt = VirtAddr(VMALLOC_BASE + guard_page * DEFAULT_PAGE_SIZE);
        let Ok(Some(reservation)) =
            P::reserve_kernel_mapping(virt, PhysAddr(phys), PmapReserveKind::Page4K)
        else {
            clear_vmalloc_page(guard_page);
            return;
        };
        P::commit_new_kernel_mapping(reservation, PmapPermissions::KERNEL_RW);
        P::shootdown_kernel_mapping(PmapInvalidation::new(virt, DEFAULT_PAGE_SIZE));
        core::mem::forget(owned);
    }

    VMALLOC_READY.store(true, Ordering::Release);
    if P::KERNEL_PAGE_TABLE_ACTIVE_AT_SUBSTRATE_INIT {
        VMALLOC_INITIALIZED.store(true, Ordering::Release);
    }
}

/// Enable the page-table-backed large-object heap after the platform has made
/// its global kernel page table active. RV64 enables it during substrate init;
/// LA64 calls this after the first PGDH activation.
pub fn enable_vmalloc_after_kernel_pmap_activation() {
    if VMALLOC_READY.load(Ordering::Acquire) {
        VMALLOC_INITIALIZED.store(true, Ordering::Release);
    }
}

fn should_use_vmalloc(layout: Layout) -> bool {
    vmalloc_layout_eligible(layout) && VMALLOC_INITIALIZED.load(Ordering::Acquire)
}

fn vmalloc_layout_eligible(layout: Layout) -> bool {
    layout.size() >= VMALLOC_THRESHOLD && layout.align() <= VMALLOC_SIZE
}

fn ptr_is_vmalloc(ptr: *mut u8) -> bool {
    let addr = ptr as usize;
    addr >= VMALLOC_BASE && addr < VMALLOC_BASE + VMALLOC_SIZE
}

fn try_vmalloc(layout: Layout) -> Option<NonNull<u8>> {
    VMALLOC_ALLOC_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    VMALLOC_LAST_FAIL_ALLOC_ERROR.store(VMALLOC_ALLOC_ERROR_NONE, Ordering::Relaxed);
    let page_count = div_ceil(layout.size(), DEFAULT_PAGE_SIZE)?;
    let align_pages = if layout.align() <= DEFAULT_PAGE_SIZE {
        1
    } else {
        layout.align() / DEFAULT_PAGE_SIZE
    };
    if page_count == 0 || page_count >= VMALLOC_PAGE_COUNT || align_pages == 0 {
        record_vmalloc_failure(
            VMALLOC_FAIL_INVALID_LAYOUT,
            page_count,
            0,
            usize::MAX,
            VMALLOC_PMAP_NONE,
        );
        return None;
    }

    let start_page = {
        let _guard = VMALLOC_VA_LOCK.lock();
        match reserve_vmalloc_pages(page_count, align_pages) {
            Some(start_page) => start_page,
            None => {
                record_vmalloc_failure(
                    VMALLOC_FAIL_VA_RANGE,
                    page_count,
                    0,
                    usize::MAX,
                    VMALLOC_PMAP_NONE,
                );
                return None;
            }
        }
    };
    let allocator = match installed_bitmap_allocator() {
        Ok(allocator) => allocator,
        Err(_) => {
            record_vmalloc_failure(
                VMALLOC_FAIL_ALLOCATOR_UNAVAILABLE,
                page_count,
                0,
                start_page,
                VMALLOC_PMAP_NONE,
            );
            return None;
        }
    };
    let mut mapped = 0usize;
    let mut failure_stage = VMALLOC_FAIL_NONE;
    let mut failure_pmap_error = VMALLOC_PMAP_NONE;

    {
        let _guard = VMALLOC_MAP_LOCK.lock_with_progress(vmalloc_service_pending_tlb_shootdown);
        while mapped < page_count {
            let frame = match allocator.reserve_frame(ZeroPolicy::UninitFullOverwrite) {
                Ok(frame) => frame,
                Err(error) => {
                    failure_stage = VMALLOC_FAIL_FRAME_ALLOC;
                    VMALLOC_LAST_FAIL_ALLOC_ERROR
                        .store(vmalloc_alloc_error_code(error), Ordering::Release);
                    break;
                }
            };
            let owned = frame.commit();
            let ppn = owned.ppn();
            let Some(phys) = ppn.0.checked_mul(DEFAULT_PAGE_SIZE) else {
                failure_stage = VMALLOC_FAIL_PHYS_OVERFLOW;
                break;
            };
            let virt = VirtAddr(VMALLOC_BASE + (start_page + mapped) * DEFAULT_PAGE_SIZE);
            let reservation =
                match vmalloc_reserve_mapping(virt, PhysAddr(phys), PmapReserveKind::Page4K) {
                    Ok(Some(reservation)) => reservation,
                    Ok(None) => {
                        failure_stage = VMALLOC_FAIL_MAP_MISSING_RESERVATION;
                        failure_pmap_error = VMALLOC_PMAP_MISSING;
                        break;
                    }
                    Err(error) => {
                        failure_stage = VMALLOC_FAIL_MAP_ERROR;
                        failure_pmap_error = vmalloc_pmap_error_code(error);
                        break;
                    }
                };
            vmalloc_commit_new_mapping(reservation, PmapPermissions::KERNEL_RW);
            core::mem::forget(owned);
            mapped += 1;
        }

        if mapped != page_count {
            let report = unmap_vmalloc_pages(start_page, mapped, allocator);
            record_vmalloc_unmap_report(report);
            if !report.complete() {
                VMALLOC_QUARANTINED_RANGES.fetch_add(1, Ordering::Relaxed);
                VMALLOC_QUARANTINED_PAGES.fetch_add(page_count, Ordering::Relaxed);
                record_vmalloc_failure(
                    VMALLOC_FAIL_ROLLBACK_INCOMPLETE,
                    page_count,
                    mapped,
                    start_page,
                    if report.pmap_error == VMALLOC_PMAP_NONE {
                        failure_pmap_error
                    } else {
                        report.pmap_error
                    },
                );
                VMALLOC_LAST_FAIL_CAUSE_STAGE.store(failure_stage, Ordering::Release);
                return None;
            }
        }
    }

    if mapped != page_count {
        let _guard = VMALLOC_VA_LOCK.lock();
        clear_vmalloc_range(start_page, page_count);
        record_vmalloc_failure(
            failure_stage,
            page_count,
            mapped,
            start_page,
            failure_pmap_error,
        );
        return None;
    }

    // `commit_new_kernel_mapping` deliberately omits per-leaf synchronization.
    // Publish the complete new range once before returning its pointer.
    shootdown_vmalloc_range(
        VMALLOC_BASE + start_page * DEFAULT_PAGE_SIZE,
        VMALLOC_BASE + (start_page + page_count) * DEFAULT_PAGE_SIZE,
    );

    VMALLOC_ALLOC_SUCCESSES.fetch_add(1, Ordering::Relaxed);
    VMALLOC_LIVE_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    let live_pages = VMALLOC_LIVE_PAGES
        .fetch_add(page_count, Ordering::Relaxed)
        .saturating_add(page_count);
    atomic_max_usize(&VMALLOC_PEAK_LIVE_PAGES, live_pages);

    NonNull::new((VMALLOC_BASE + start_page * DEFAULT_PAGE_SIZE) as *mut u8)
}

unsafe fn dealloc_vmalloc(ptr: *mut u8, layout: Layout) {
    let Some(offset) = (ptr as usize).checked_sub(VMALLOC_BASE) else {
        record_bad_vmalloc_dealloc(usize::MAX, 0);
        return;
    };
    if offset % DEFAULT_PAGE_SIZE != 0 {
        record_bad_vmalloc_dealloc(offset / DEFAULT_PAGE_SIZE, 0);
        return;
    }
    let Some(page_count) = div_ceil(layout.size(), DEFAULT_PAGE_SIZE) else {
        record_bad_vmalloc_dealloc(offset / DEFAULT_PAGE_SIZE, 0);
        return;
    };
    let start_page = offset / DEFAULT_PAGE_SIZE;
    if is_vmalloc_guard_page(start_page)
        || page_count == 0
        || start_page.saturating_add(page_count) > VMALLOC_PAGE_COUNT
    {
        record_bad_vmalloc_dealloc(start_page, page_count);
        return;
    }

    let Ok(allocator) = installed_bitmap_allocator() else {
        record_bad_vmalloc_dealloc(start_page, page_count);
        return;
    };
    let report = {
        let _guard = VMALLOC_MAP_LOCK.lock_with_progress(vmalloc_service_pending_tlb_shootdown);
        unmap_vmalloc_pages(start_page, page_count, allocator)
    };
    record_vmalloc_unmap_report(report);
    if report.complete() {
        let _guard = VMALLOC_VA_LOCK.lock();
        clear_vmalloc_range(start_page, page_count);
        VMALLOC_LIVE_ALLOCATIONS.fetch_sub(1, Ordering::Relaxed);
        VMALLOC_LIVE_PAGES.fetch_sub(page_count, Ordering::Relaxed);
    } else {
        VMALLOC_QUARANTINED_RANGES.fetch_add(1, Ordering::Relaxed);
        VMALLOC_QUARANTINED_PAGES.fetch_add(page_count, Ordering::Relaxed);
    }
}

fn unmap_vmalloc_pages(
    start_page: usize,
    page_count: usize,
    allocator: &crate::page_allocator::BitmapPageAllocator<'static>,
) -> VmallocUnmapReport {
    VMALLOC_UNMAP_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    let mut report = VmallocUnmapReport::new(page_count);
    let mut deferred_head = None;
    let mut invalidation_start = usize::MAX;
    let mut invalidation_end = 0usize;

    for page_offset in 0..page_count {
        let page = start_page + page_offset;
        let virt = VirtAddr(VMALLOC_BASE + page * DEFAULT_PAGE_SIZE);
        let result = match vmalloc_unmap_mapping(virt, PmapReserveKind::Page4K) {
            Ok(Some(result)) => result,
            Ok(None) => {
                report.missing += 1;
                report.record_failure(page, VMALLOC_PMAP_MISSING);
                continue;
            }
            Err(error) => {
                report.missing += 1;
                report.record_failure(page, vmalloc_pmap_error_code(error));
                continue;
            }
        };
        report.removed += 1;

        let invalidation = result.invalidation();
        invalidation_start = invalidation_start.min(invalidation.virt().0);
        invalidation_end =
            invalidation_end.max(invalidation.virt().0.saturating_add(invalidation.size()));

        let ppn = result.base_ppn();
        let Ok(page_ptr) = page_allocator::frame_kernel_addr(ppn) else {
            // Direct-map lookup is a substrate invariant. If a platform breaks
            // it, finish the pending shootdown before returning this one frame
            // and continue without leaking or reusing a stale alias.
            shootdown_vmalloc_range(invalidation_start, invalidation_end);
            release_deferred_vmalloc_frames(deferred_head, allocator);
            allocator.release_owned(ppn);
            deferred_head = None;
            invalidation_start = usize::MAX;
            invalidation_end = 0;
            report.frame_lookup_failures += 1;
            report.record_failure(page, VMALLOC_PMAP_FRAME_LOOKUP);
            continue;
        };
        unsafe {
            // After deallocation starts the object contents are dead. Reuse
            // the first word of each still-owned frame as an intrusive free
            // chain so an arbitrarily large vfree needs no heap allocation.
            (page_ptr as *mut usize).write(deferred_head.map_or(usize::MAX, |head: Ppn| head.0));
        }
        deferred_head = Some(ppn);
    }

    if deferred_head.is_some() {
        shootdown_vmalloc_range(invalidation_start, invalidation_end);
        release_deferred_vmalloc_frames(deferred_head, allocator);
    }
    report
}

fn shootdown_vmalloc_range(start: usize, end: usize) {
    if start < end {
        let invalidation = PmapInvalidation::new(VirtAddr(start), end - start);
        vmalloc_shootdown_mappings(core::slice::from_ref(&invalidation));
    }
}

fn release_deferred_vmalloc_frames(
    mut head: Option<Ppn>,
    allocator: &crate::page_allocator::BitmapPageAllocator<'static>,
) {
    while let Some(ppn) = head {
        let next = page_allocator::frame_kernel_addr(ppn)
            .ok()
            .map(|page_ptr| unsafe { (page_ptr as *const usize).read() })
            .filter(|next| *next != usize::MAX)
            .map(Ppn);
        allocator.release_owned(ppn);
        head = next;
    }
}

fn vmalloc_pmap_error_code(error: PmapError) -> usize {
    match error {
        PmapError::InvalidRequest => VMALLOC_PMAP_INVALID_REQUEST,
        PmapError::AlreadyMapped => VMALLOC_PMAP_ALREADY_MAPPED,
        PmapError::Exhausted => VMALLOC_PMAP_EXHAUSTED,
        PmapError::Unsupported => VMALLOC_PMAP_UNSUPPORTED,
    }
}

fn vmalloc_alloc_error_code(error: AllocError) -> usize {
    match error {
        AllocError::Exhausted => VMALLOC_ALLOC_ERROR_EXHAUSTED,
        AllocError::InvalidRequest => VMALLOC_ALLOC_ERROR_INVALID_REQUEST,
        AllocError::NotInitialized => VMALLOC_ALLOC_ERROR_NOT_INITIALIZED,
        AllocError::AlreadyInstalled => VMALLOC_ALLOC_ERROR_ALREADY_INSTALLED,
        AllocError::ZeroScrubUnavailable => VMALLOC_ALLOC_ERROR_ZERO_SCRUB_UNAVAILABLE,
        AllocError::FrameCopyUnavailable => VMALLOC_ALLOC_ERROR_FRAME_COPY_UNAVAILABLE,
        AllocError::FrameKernelAddrUnavailable => VMALLOC_ALLOC_ERROR_FRAME_KERNEL_ADDR_UNAVAILABLE,
        AllocError::ReservedFrame => VMALLOC_ALLOC_ERROR_RESERVED_FRAME,
        AllocError::CounterOverflow => VMALLOC_ALLOC_ERROR_COUNTER_OVERFLOW,
        AllocError::CounterUnderflow => VMALLOC_ALLOC_ERROR_COUNTER_UNDERFLOW,
        AllocError::DoubleFree => VMALLOC_ALLOC_ERROR_DOUBLE_FREE,
    }
}

fn record_vmalloc_failure(
    stage: usize,
    request_pages: usize,
    mapped_pages: usize,
    start_page: usize,
    pmap_error: usize,
) {
    VMALLOC_LAST_FAIL_CAUSE_STAGE.store(stage, Ordering::Relaxed);
    VMALLOC_LAST_FAIL_REQUEST_PAGES.store(request_pages, Ordering::Relaxed);
    VMALLOC_LAST_FAIL_MAPPED_PAGES.store(mapped_pages, Ordering::Relaxed);
    VMALLOC_LAST_FAIL_START_PAGE.store(start_page, Ordering::Relaxed);
    VMALLOC_LAST_FAIL_PMAP_ERROR.store(pmap_error, Ordering::Relaxed);
    VMALLOC_LAST_FAIL_STAGE.store(stage, Ordering::Release);
}

fn record_vmalloc_unmap_report(report: VmallocUnmapReport) {
    if report.complete() {
        return;
    }
    VMALLOC_PARTIAL_UNMAPS.fetch_add(1, Ordering::Relaxed);
    VMALLOC_LAST_UNMAP_REQUEST_PAGES.store(report.requested, Ordering::Relaxed);
    VMALLOC_LAST_UNMAP_REMOVED_PAGES.store(report.removed, Ordering::Relaxed);
    VMALLOC_LAST_UNMAP_MISSING_PAGES.store(report.missing, Ordering::Relaxed);
    VMALLOC_LAST_UNMAP_FRAME_LOOKUP_FAILURES.store(report.frame_lookup_failures, Ordering::Relaxed);
    VMALLOC_LAST_UNMAP_FIRST_FAILED_PAGE.store(report.first_failed_page, Ordering::Relaxed);
    VMALLOC_LAST_UNMAP_PMAP_ERROR.store(report.pmap_error, Ordering::Release);
}

fn record_bad_vmalloc_dealloc(start_page: usize, page_count: usize) {
    VMALLOC_BAD_DEALLOCS.fetch_add(1, Ordering::Relaxed);
    record_vmalloc_failure(
        VMALLOC_FAIL_BAD_DEALLOC,
        page_count,
        0,
        start_page,
        VMALLOC_PMAP_NONE,
    );
}

fn atomic_max_usize(target: &AtomicUsize, value: usize) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn reserve_vmalloc_pages(page_count: usize, align_pages: usize) -> Option<usize> {
    let hint = VMALLOC_HINT
        .load(Ordering::Relaxed)
        .clamp(1, VMALLOC_PAGE_COUNT - 1);
    for (start, end) in [(hint, VMALLOC_PAGE_COUNT), (1, hint)] {
        let mut candidate = align_page_index(start, align_pages)?;
        while candidate
            .checked_add(page_count)
            .is_some_and(|range_end| range_end <= end)
        {
            let mut blocker = None;
            for page in candidate..candidate + page_count {
                if vmalloc_page_used(page) {
                    blocker = Some(page);
                    break;
                }
            }
            if let Some(blocker) = blocker {
                candidate = align_page_index(blocker + 1, align_pages)?;
                continue;
            }
            for page in candidate..candidate + page_count {
                set_vmalloc_page_used(page);
            }
            VMALLOC_HINT.store(candidate + page_count, Ordering::Relaxed);
            return Some(candidate);
        }
    }
    None
}

fn is_vmalloc_guard_page(page: usize) -> bool {
    page % VMALLOC_ROOT_SLOT_PAGES == 0
}

fn align_page_index(value: usize, align: usize) -> Option<usize> {
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

fn vmalloc_page_used(page: usize) -> bool {
    let word = VMALLOC_BITMAP[page / u64::BITS as usize].load(Ordering::Relaxed);
    word & (1u64 << (page % u64::BITS as usize)) != 0
}

fn set_vmalloc_page_used(page: usize) {
    VMALLOC_BITMAP[page / u64::BITS as usize]
        .fetch_or(1u64 << (page % u64::BITS as usize), Ordering::Relaxed);
}

fn clear_vmalloc_page(page: usize) {
    VMALLOC_BITMAP[page / u64::BITS as usize]
        .fetch_and(!(1u64 << (page % u64::BITS as usize)), Ordering::Relaxed);
}

fn clear_vmalloc_range(start_page: usize, page_count: usize) {
    for page in start_page..start_page + page_count {
        clear_vmalloc_page(page);
    }
}

fn vmalloc_reserve_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    let raw = VMALLOC_RESERVE_MAPPING.load(Ordering::Acquire);
    if raw == 0 {
        return Err(PmapError::Unsupported);
    }
    let callback: ReserveKernelMappingFn = unsafe { core::mem::transmute(raw) };
    callback(virt, phys, kind)
}

fn vmalloc_commit_new_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
    let raw = VMALLOC_COMMIT_NEW_MAPPING.load(Ordering::Acquire);
    debug_assert_ne!(raw, 0);
    let callback: CommitKernelMappingFn = unsafe { core::mem::transmute(raw) };
    callback(reservation, permissions);
}

fn vmalloc_unmap_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    let raw = VMALLOC_UNMAP_MAPPING.load(Ordering::Acquire);
    if raw == 0 {
        return Err(PmapError::Unsupported);
    }
    let callback: UnmapKernelMappingFn = unsafe { core::mem::transmute(raw) };
    callback(virt, kind)
}

fn vmalloc_shootdown_mappings(invalidations: &[PmapInvalidation]) {
    let raw = VMALLOC_SHOOTDOWN_MAPPINGS.load(Ordering::Acquire);
    debug_assert_ne!(raw, 0);
    let callback: ShootdownKernelMappingsFn = unsafe { core::mem::transmute(raw) };
    callback(invalidations);
}

fn vmalloc_service_pending_tlb_shootdown() {
    let raw = VMALLOC_SERVICE_PENDING_TLB_SHOOTDOWN.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let callback: ServicePendingTlbShootdownFn = unsafe { core::mem::transmute(raw) };
    callback();
}

#[repr(C)]
struct SlabPageHeader {
    magic: usize,
    ppn: Ppn,
    class_index: usize,
    capacity: usize,
    free_count: usize,
    free_list: *mut FreeObject,
    state: SlabPageState,
    retained_empty: bool,
    next: *mut SlabPageHeader,
    prev: *mut SlabPageHeader,
}

const SLAB_PAGE_MAGIC: usize = 0x5458_534c_4142_5047;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum SlabPageState {
    Detached,
    Empty,
    Partial,
    Full,
}

struct FreeObject {
    next: *mut FreeObject,
}

fn class_index(layout: Layout) -> Result<Option<usize>, SlabError> {
    if layout.align() > DEFAULT_PAGE_SIZE || !layout.align().is_power_of_two() {
        return Err(SlabError::InvalidLayout);
    }

    let needed = layout
        .size()
        .max(layout.align())
        .max(core::mem::size_of::<FreeObject>())
        .max(MIN_CLASS);
    if needed > MAX_SLAB_CLASS {
        return Ok(None);
    }

    let class_size = needed.next_power_of_two();
    Ok(CLASS_SIZES.iter().position(|size| *size == class_size))
}

fn page_header_for_object(ptr: *mut u8, page_size: usize) -> *mut SlabPageHeader {
    (ptr as usize & !(page_size - 1)) as *mut SlabPageHeader
}

unsafe fn page_list_head(class: &SlabClass, state: SlabPageState) -> *mut *mut SlabPageHeader {
    match state {
        SlabPageState::Empty => class.empty_pages.get(),
        SlabPageState::Partial => class.partial_pages.get(),
        SlabPageState::Full => class.full_pages.get(),
        SlabPageState::Detached => unreachable!("detached slab pages have no list"),
    }
}

unsafe fn link_page(class: &SlabClass, page: *mut SlabPageHeader, state: SlabPageState) {
    unsafe {
        debug_assert_eq!((*page).state, SlabPageState::Detached);
        let head = page_list_head(class, state);
        (*page).state = state;
        (*page).prev = ptr::null_mut();
        (*page).next = *head;
        if !(*head).is_null() {
            (**head).prev = page;
        }
        *head = page;
    }
}

unsafe fn unlink_page(class: &SlabClass, page: *mut SlabPageHeader) {
    unsafe {
        let state = (*page).state;
        debug_assert_ne!(state, SlabPageState::Detached);
        let head = page_list_head(class, state);
        if !(*page).prev.is_null() {
            (*(*page).prev).next = (*page).next;
        } else {
            *head = (*page).next;
        }

        if !(*page).next.is_null() {
            (*(*page).next).prev = (*page).prev;
        }

        (*page).next = ptr::null_mut();
        (*page).prev = ptr::null_mut();
        (*page).state = SlabPageState::Detached;
    }
}

unsafe fn move_page(class: &SlabClass, page: *mut SlabPageHeader, state: SlabPageState) {
    unsafe {
        if (*page).state == state {
            return;
        }
        unlink_page(class, page);
        link_page(class, page, state);
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

fn div_ceil(value: usize, divisor: usize) -> Option<usize> {
    if divisor == 0 {
        return None;
    }
    value.checked_add(divisor - 1).map(|v| v / divisor)
}

#[derive(Clone, Copy)]
pub struct GlobalPageProvider;

static GLOBAL_DIRECT_MAP_BASE: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_HEAP: SlabHeap<GlobalPageProvider> = SlabHeap::new(GlobalPageProvider);
static LAST_ALLOC_FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static LAST_ALLOC_FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static LAST_ALLOC_FAIL_PAGES: AtomicUsize = AtomicUsize::new(0);
static LAST_ALLOC_FAIL_FREE: AtomicUsize = AtomicUsize::new(0);
static LAST_ALLOC_FAIL_TOTAL: AtomicUsize = AtomicUsize::new(0);
static LAST_ALLOC_FAIL_MAX_RUN: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlobalAllocationFailureDiagnostics {
    pub size: usize,
    pub align: usize,
    pub pages: usize,
    pub free_count: usize,
    pub total_count: usize,
    pub max_contiguous_free_run: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmallocDiagnostics {
    pub ready: bool,
    pub initialized: bool,
    pub window_pages: usize,
    pub bitmap_used_pages: usize,
    pub bitmap_free_pages: usize,
    pub bitmap_max_free_run: usize,
    pub hint: usize,
    pub alloc_attempts: usize,
    pub alloc_successes: usize,
    pub live_allocations: usize,
    pub live_pages: usize,
    pub peak_live_pages: usize,
    pub unmap_attempts: usize,
    pub partial_unmaps: usize,
    pub bad_deallocs: usize,
    pub quarantined_ranges: usize,
    pub quarantined_pages: usize,
    pub last_failure_stage: usize,
    pub last_failure_cause_stage: usize,
    pub last_failure_request_pages: usize,
    pub last_failure_mapped_pages: usize,
    pub last_failure_start_page: usize,
    pub last_failure_pmap_error: usize,
    pub last_failure_alloc_error: usize,
    pub last_unmap_request_pages: usize,
    pub last_unmap_removed_pages: usize,
    pub last_unmap_missing_pages: usize,
    pub last_unmap_frame_lookup_failures: usize,
    pub last_unmap_first_failed_page: usize,
    pub last_unmap_pmap_error: usize,
}

pub fn vmalloc_failure_stage_name(stage: usize) -> &'static str {
    match stage {
        VMALLOC_FAIL_NONE => "none",
        VMALLOC_FAIL_NOT_INITIALIZED => "not-initialized",
        VMALLOC_FAIL_INVALID_LAYOUT => "invalid-layout",
        VMALLOC_FAIL_VA_RANGE => "va-range",
        VMALLOC_FAIL_ALLOCATOR_UNAVAILABLE => "allocator-unavailable",
        VMALLOC_FAIL_FRAME_ALLOC => "frame-alloc",
        VMALLOC_FAIL_PHYS_OVERFLOW => "phys-overflow",
        VMALLOC_FAIL_MAP_MISSING_RESERVATION => "map-missing-reservation",
        VMALLOC_FAIL_MAP_ERROR => "map-error",
        VMALLOC_FAIL_ROLLBACK_INCOMPLETE => "rollback-incomplete",
        VMALLOC_FAIL_BAD_DEALLOC => "bad-dealloc",
        _ => "unknown",
    }
}

pub fn vmalloc_pmap_error_name(error: usize) -> &'static str {
    match error {
        VMALLOC_PMAP_NONE => "none",
        VMALLOC_PMAP_MISSING => "missing",
        VMALLOC_PMAP_INVALID_REQUEST => "invalid-request",
        VMALLOC_PMAP_ALREADY_MAPPED => "already-mapped",
        VMALLOC_PMAP_EXHAUSTED => "exhausted",
        VMALLOC_PMAP_UNSUPPORTED => "unsupported",
        VMALLOC_PMAP_FRAME_LOOKUP => "frame-lookup",
        _ => "unknown",
    }
}

pub fn vmalloc_alloc_error_name(error: usize) -> &'static str {
    match error {
        VMALLOC_ALLOC_ERROR_NONE => "none",
        VMALLOC_ALLOC_ERROR_EXHAUSTED => "exhausted",
        VMALLOC_ALLOC_ERROR_INVALID_REQUEST => "invalid-request",
        VMALLOC_ALLOC_ERROR_NOT_INITIALIZED => "not-initialized",
        VMALLOC_ALLOC_ERROR_ALREADY_INSTALLED => "already-installed",
        VMALLOC_ALLOC_ERROR_ZERO_SCRUB_UNAVAILABLE => "zero-scrub-unavailable",
        VMALLOC_ALLOC_ERROR_FRAME_COPY_UNAVAILABLE => "frame-copy-unavailable",
        VMALLOC_ALLOC_ERROR_FRAME_KERNEL_ADDR_UNAVAILABLE => "frame-kaddr-unavailable",
        VMALLOC_ALLOC_ERROR_RESERVED_FRAME => "reserved-frame",
        VMALLOC_ALLOC_ERROR_COUNTER_OVERFLOW => "counter-overflow",
        VMALLOC_ALLOC_ERROR_COUNTER_UNDERFLOW => "counter-underflow",
        VMALLOC_ALLOC_ERROR_DOUBLE_FREE => "double-free",
        _ => "unknown",
    }
}

unsafe impl SlabPageProvider for GlobalPageProvider {
    fn page_size(&self) -> usize {
        let page_size = GLOBAL_PAGE_SIZE.load(Ordering::Acquire);
        if page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page_size
        }
    }

    fn reserve_run(&self, count: usize, align: usize) -> Result<Ppn, AllocError> {
        let allocator = installed_bitmap_allocator()?;
        let run = allocator
            .reserve_run(count, align, ZeroPolicy::UninitFullOverwrite)?
            .commit();
        let base = run.base();
        core::mem::forget(run);
        Ok(base)
    }

    unsafe fn release_run(&self, base: Ppn, count: usize) {
        if let Ok(allocator) = installed_bitmap_allocator() {
            for offset in 0..count {
                allocator.release_owned(Ppn(base.0 + offset));
            }
        }
    }

    unsafe fn direct_map_ptr(&self, ppn: Ppn) -> *mut u8 {
        let page_size = self.page_size();
        let phys = ppn
            .0
            .checked_mul(page_size)
            .expect("slab direct-map physical address overflow");
        GLOBAL_DIRECT_MAP_BASE
            .load(Ordering::Acquire)
            .checked_add(phys)
            .expect("slab direct-map virtual address overflow") as *mut u8
    }

    fn ppn_from_direct_map_ptr(&self, ptr: *mut u8) -> Option<Ppn> {
        let base = GLOBAL_DIRECT_MAP_BASE.load(Ordering::Acquire);
        let page_size = self.page_size();
        let offset = (ptr as usize).checked_sub(base)?;
        if offset % page_size == 0 {
            Some(Ppn(offset / page_size))
        } else {
            None
        }
    }
}

/// Global allocator shim used by no-std kernel binaries.
pub struct KernelGlobalAllocator;

unsafe impl GlobalAlloc for KernelGlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Match the kvmalloc policy: keep medium allocations on the cheap
        // direct-map path when contiguous memory is readily available, and
        // use vmalloc as the fragmentation-safe fallback. Very large requests
        // skip the bitmap allocator's expensive contiguous-run search.
        if layout.size() > VMALLOC_DIRECT_TRY_MAX && should_use_vmalloc(layout) {
            if let Some(ptr) = try_vmalloc(layout) {
                return ptr.as_ptr();
            }
        }
        match GLOBAL_HEAP.try_alloc(layout) {
            Ok(ptr) => ptr.as_ptr(),
            Err(_) => {
                if vmalloc_layout_eligible(layout) {
                    if VMALLOC_INITIALIZED.load(Ordering::Acquire) {
                        if let Some(ptr) = try_vmalloc(layout) {
                            return ptr.as_ptr();
                        }
                    } else {
                        record_vmalloc_failure(
                            VMALLOC_FAIL_NOT_INITIALIZED,
                            div_ceil(layout.size(), DEFAULT_PAGE_SIZE).unwrap_or(usize::MAX),
                            0,
                            usize::MAX,
                            VMALLOC_PMAP_NONE,
                        );
                    }
                }
                record_global_alloc_failure(layout);
                ptr::null_mut()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr_is_vmalloc(ptr) {
            unsafe {
                dealloc_vmalloc(ptr, layout);
            }
            return;
        }
        unsafe {
            GLOBAL_HEAP.dealloc(ptr, layout);
        }
    }
}

fn record_global_alloc_failure(layout: Layout) {
    let page_size = GLOBAL_PAGE_SIZE
        .load(Ordering::Acquire)
        .max(DEFAULT_PAGE_SIZE);
    let pages = div_ceil(layout.size(), page_size).unwrap_or(usize::MAX);
    let (free_count, total_count, max_run) = installed_bitmap_allocator()
        .map(|allocator| {
            let diagnostics = allocator.backend_diagnostics();
            (
                diagnostics.free_count,
                diagnostics.total_count,
                diagnostics.max_contiguous_free_run,
            )
        })
        .unwrap_or((0, 0, 0));

    LAST_ALLOC_FAIL_ALIGN.store(layout.align(), Ordering::Release);
    LAST_ALLOC_FAIL_PAGES.store(pages, Ordering::Release);
    LAST_ALLOC_FAIL_FREE.store(free_count, Ordering::Release);
    LAST_ALLOC_FAIL_TOTAL.store(total_count, Ordering::Release);
    LAST_ALLOC_FAIL_MAX_RUN.store(max_run, Ordering::Release);
    LAST_ALLOC_FAIL_SIZE.store(layout.size(), Ordering::Release);
}

pub fn last_allocation_failure() -> Option<GlobalAllocationFailureDiagnostics> {
    let size = LAST_ALLOC_FAIL_SIZE.load(Ordering::Acquire);
    if size == 0 {
        return None;
    }

    Some(GlobalAllocationFailureDiagnostics {
        size,
        align: LAST_ALLOC_FAIL_ALIGN.load(Ordering::Acquire),
        pages: LAST_ALLOC_FAIL_PAGES.load(Ordering::Acquire),
        free_count: LAST_ALLOC_FAIL_FREE.load(Ordering::Acquire),
        total_count: LAST_ALLOC_FAIL_TOTAL.load(Ordering::Acquire),
        max_contiguous_free_run: LAST_ALLOC_FAIL_MAX_RUN.load(Ordering::Acquire),
    })
}

pub fn vmalloc_diagnostics() -> VmallocDiagnostics {
    let (bitmap_used_pages, bitmap_max_free_run) = vmalloc_bitmap_usage();
    VmallocDiagnostics {
        ready: VMALLOC_READY.load(Ordering::Acquire),
        initialized: VMALLOC_INITIALIZED.load(Ordering::Acquire),
        window_pages: VMALLOC_PAGE_COUNT,
        bitmap_used_pages,
        bitmap_free_pages: VMALLOC_PAGE_COUNT.saturating_sub(bitmap_used_pages),
        bitmap_max_free_run,
        hint: VMALLOC_HINT.load(Ordering::Acquire),
        alloc_attempts: VMALLOC_ALLOC_ATTEMPTS.load(Ordering::Acquire),
        alloc_successes: VMALLOC_ALLOC_SUCCESSES.load(Ordering::Acquire),
        live_allocations: VMALLOC_LIVE_ALLOCATIONS.load(Ordering::Acquire),
        live_pages: VMALLOC_LIVE_PAGES.load(Ordering::Acquire),
        peak_live_pages: VMALLOC_PEAK_LIVE_PAGES.load(Ordering::Acquire),
        unmap_attempts: VMALLOC_UNMAP_ATTEMPTS.load(Ordering::Acquire),
        partial_unmaps: VMALLOC_PARTIAL_UNMAPS.load(Ordering::Acquire),
        bad_deallocs: VMALLOC_BAD_DEALLOCS.load(Ordering::Acquire),
        quarantined_ranges: VMALLOC_QUARANTINED_RANGES.load(Ordering::Acquire),
        quarantined_pages: VMALLOC_QUARANTINED_PAGES.load(Ordering::Acquire),
        last_failure_stage: VMALLOC_LAST_FAIL_STAGE.load(Ordering::Acquire),
        last_failure_cause_stage: VMALLOC_LAST_FAIL_CAUSE_STAGE.load(Ordering::Acquire),
        last_failure_request_pages: VMALLOC_LAST_FAIL_REQUEST_PAGES.load(Ordering::Acquire),
        last_failure_mapped_pages: VMALLOC_LAST_FAIL_MAPPED_PAGES.load(Ordering::Acquire),
        last_failure_start_page: VMALLOC_LAST_FAIL_START_PAGE.load(Ordering::Acquire),
        last_failure_pmap_error: VMALLOC_LAST_FAIL_PMAP_ERROR.load(Ordering::Acquire),
        last_failure_alloc_error: VMALLOC_LAST_FAIL_ALLOC_ERROR.load(Ordering::Acquire),
        last_unmap_request_pages: VMALLOC_LAST_UNMAP_REQUEST_PAGES.load(Ordering::Acquire),
        last_unmap_removed_pages: VMALLOC_LAST_UNMAP_REMOVED_PAGES.load(Ordering::Acquire),
        last_unmap_missing_pages: VMALLOC_LAST_UNMAP_MISSING_PAGES.load(Ordering::Acquire),
        last_unmap_frame_lookup_failures: VMALLOC_LAST_UNMAP_FRAME_LOOKUP_FAILURES
            .load(Ordering::Acquire),
        last_unmap_first_failed_page: VMALLOC_LAST_UNMAP_FIRST_FAILED_PAGE.load(Ordering::Acquire),
        last_unmap_pmap_error: VMALLOC_LAST_UNMAP_PMAP_ERROR.load(Ordering::Acquire),
    }
}

fn vmalloc_bitmap_usage() -> (usize, usize) {
    let mut used_pages = 0usize;
    let mut current_free_run = 0usize;
    let mut max_free_run = 0usize;
    for word in &VMALLOC_BITMAP {
        let bits = word.load(Ordering::Acquire);
        used_pages = used_pages.saturating_add(bits.count_ones() as usize);
        for bit in 0..u64::BITS as usize {
            if bits & (1u64 << bit) == 0 {
                current_free_run += 1;
                max_free_run = max_free_run.max(current_free_run);
            } else {
                current_free_run = 0;
            }
        }
    }
    (used_pages, max_free_run)
}

#[cfg(target_os = "none")]
#[global_allocator]
static GLOBAL_ALLOCATOR: KernelGlobalAllocator = KernelGlobalAllocator;

/// Initialize the global heap after the frame allocator is installed.
pub fn init<P: TxPlatform>() -> Result<(), SlabError> {
    if P::PAGE_SIZE != DEFAULT_PAGE_SIZE {
        return Err(SlabError::InvalidLayout);
    }

    GLOBAL_DIRECT_MAP_BASE.store(P::DIRECT_MAP_BASE.0, Ordering::Release);
    GLOBAL_PAGE_SIZE.store(P::PAGE_SIZE, Ordering::Release);
    GLOBAL_HEAP.init()?;
    init_vmalloc::<P>();
    Ok(())
}

/// Run a no-alloc smoke test against the global heap.
pub fn allocation_smoke() -> Result<(), SlabError> {
    let layout = Layout::from_size_align(64, 8).map_err(|_| SlabError::InvalidLayout)?;
    let ptr = GLOBAL_HEAP.try_alloc(layout)?;
    unsafe {
        GLOBAL_HEAP.dealloc(ptr.as_ptr(), layout);
    }
    Ok(())
}

/// Access the process-wide slab heap, mostly for diagnostics/tests.
pub fn global_heap() -> &'static SlabHeap<GlobalPageProvider> {
    &GLOBAL_HEAP
}

pub fn heap_diagnostics() -> SlabHeapDiagnostics {
    GLOBAL_HEAP.diagnostics()
}
