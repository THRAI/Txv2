//! Slab-backed kernel heap.
//!
//! The slab heap is the allocation layer that becomes legal after
//! `tx_substrate::init::<P>()` has installed the page allocator. Small
//! allocations use per-size-class pages; page-sized and larger allocations
//! reserve contiguous physical page runs directly.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tx_hal::{Ppn, TxPlatform};

use crate::page_allocator::{installed_bitmap_allocator, AllocError, PageAllocator, ZeroPolicy};

const MIN_CLASS: usize = 8;
const MAX_SLAB_CLASS: usize = 2048;
const CLASS_COUNT: usize = 9;
const CLASS_SIZES: [usize; CLASS_COUNT] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];
const CLASS_REFILL_PAGES: [usize; CLASS_COUNT] = [1, 1, 1, 4, 8, 4, 2, 1, 1];
const DEFAULT_PAGE_SIZE: usize = 4096;
const RETAIN_EMPTY_SLAB_PAGES_PER_CLASS: usize = 1;

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
}

unsafe impl<P: SlabPageProvider + Sync> Sync for SlabHeap<P> {}

impl<P: SlabPageProvider> SlabHeap<P> {
    /// Construct an uninitialized heap.
    pub const fn new(provider: P) -> Self {
        Self {
            provider,
            initialized: AtomicBool::new(false),
            classes: [const { SlabClass::new() }; CLASS_COUNT],
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
            if (*class.free_list.get()).is_null() {
                self.populate_class(index, class)?;
            }

            let object = *class.free_list.get();
            if object.is_null() {
                return Err(SlabError::PageAllocator(AllocError::Exhausted));
            }

            *class.free_list.get() = (*object).next;
            let header = page_header_for_object(object as *mut u8, self.provider.page_size());
            if (*header).retained_empty {
                debug_assert_eq!((*header).free_count, (*header).capacity);
                (*header).retained_empty = false;
                *class.retained_empty_pages.get() -= 1;
            }
            (*header).free_count -= 1;

            Ok(NonNull::new_unchecked(object as *mut u8))
        }
    }

    unsafe fn dealloc_small(&self, index: usize, ptr: *mut u8) {
        let class = &self.classes[index];
        let mut page_to_release = ptr::null_mut();
        let _guard = class.lock.lock();

        unsafe {
            let header = page_header_for_object(ptr, self.provider.page_size());
            let object = ptr as *mut FreeObject;
            (*object).next = *class.free_list.get();
            *class.free_list.get() = object;
            (*header).free_count += 1;

            if (*header).free_count == (*header).capacity {
                if *class.retained_empty_pages.get() < RETAIN_EMPTY_SLAB_PAGES_PER_CLASS {
                    (*header).retained_empty = true;
                    *class.retained_empty_pages.get() += 1;
                } else {
                    let page_base = header as *mut u8;
                    remove_page_objects_from_free_list(
                        class.free_list.get(),
                        page_base,
                        self.provider.page_size(),
                    );
                    unlink_page(class.pages.get(), header);
                    page_to_release = header;
                }
            }
        }

        if !page_to_release.is_null() {
            unsafe {
                let ppn = (*page_to_release).ppn;
                ptr::write_bytes(page_to_release as *mut u8, 0, self.provider.page_size());
                self.provider.release_run(ppn, 1);
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
        NonNull::new(ptr).ok_or(SlabError::InvalidLayout)
    }

    unsafe fn dealloc_large(&self, ptr: *mut u8, layout: Layout) {
        let page_size = self.provider.page_size();
        let Some(ppn) = self.provider.ppn_from_direct_map_ptr(ptr) else {
            return;
        };
        let Some(count) = div_ceil(layout.size(), page_size) else {
            return;
        };
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
                    ppn: page_ppn,
                    capacity,
                    free_count: capacity,
                    retained_empty: false,
                    next: *class.pages.get(),
                    prev: ptr::null_mut(),
                });

                if !(*class.pages.get()).is_null() {
                    (**class.pages.get()).prev = header;
                }
                *class.pages.get() = header;

                for offset in 0..capacity {
                    let object = page.add(object_start + offset * size) as *mut FreeObject;
                    (*object).next = *class.free_list.get();
                    *class.free_list.get() = object;
                }
            }
        }

        Ok(())
    }
}

struct SlabClass {
    lock: SpinLock,
    free_list: UnsafeCell<*mut FreeObject>,
    pages: UnsafeCell<*mut SlabPageHeader>,
    retained_empty_pages: UnsafeCell<usize>,
}

unsafe impl Sync for SlabClass {}

impl SlabClass {
    const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            free_list: UnsafeCell::new(ptr::null_mut()),
            pages: UnsafeCell::new(ptr::null_mut()),
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
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
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

#[repr(C)]
struct SlabPageHeader {
    ppn: Ppn,
    capacity: usize,
    free_count: usize,
    retained_empty: bool,
    next: *mut SlabPageHeader,
    prev: *mut SlabPageHeader,
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

unsafe fn remove_page_objects_from_free_list(
    free_list: *mut *mut FreeObject,
    page_base: *mut u8,
    page_size: usize,
) {
    unsafe {
        let mut current = *free_list;
        let mut previous = ptr::null_mut::<FreeObject>();
        while !current.is_null() {
            let next = (*current).next;
            if page_header_for_object(current as *mut u8, page_size) as *mut u8 == page_base {
                if previous.is_null() {
                    *free_list = next;
                } else {
                    (*previous).next = next;
                }
            } else {
                previous = current;
            }
            current = next;
        }
    }
}

unsafe fn unlink_page(head: *mut *mut SlabPageHeader, page: *mut SlabPageHeader) {
    unsafe {
        if !(*page).prev.is_null() {
            (*(*page).prev).next = (*page).next;
        } else {
            *head = (*page).next;
        }

        if !(*page).next.is_null() {
            (*(*page).next).prev = (*page).prev;
        }
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
        match GLOBAL_HEAP.try_alloc(layout) {
            Ok(ptr) => ptr.as_ptr(),
            Err(_) => {
                record_global_alloc_failure(layout);
                ptr::null_mut()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
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
    GLOBAL_HEAP.init()
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
