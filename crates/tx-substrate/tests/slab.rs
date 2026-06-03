use core::alloc::Layout;
use core::sync::atomic::AtomicU64;
use tx_hal::Ppn;
use tx_substrate::page_allocator::{BitmapPageAllocator, FrameMeta, PageAllocator, ZeroPolicy};
use tx_substrate::slab::{SlabError, SlabHeap, SlabPageProvider};

const PAGE_SIZE: usize = 4096;

#[repr(align(4096))]
#[derive(Clone, Copy)]
struct TestPage {
    _bytes: [u8; PAGE_SIZE],
}

#[derive(Clone, Copy)]
struct TestProvider<'a> {
    allocator: &'a BitmapPageAllocator<'a>,
    base: usize,
}

unsafe impl SlabPageProvider for TestProvider<'_> {
    fn page_size(&self) -> usize {
        PAGE_SIZE
    }

    fn reserve_run(
        &self,
        count: usize,
        align: usize,
    ) -> Result<Ppn, tx_substrate::page_allocator::AllocError> {
        let run = self
            .allocator
            .reserve_run(count, align, ZeroPolicy::UninitFullOverwrite)?
            .commit();
        let base = run.base();
        core::mem::forget(run);
        Ok(base)
    }

    unsafe fn release_run(&self, base: Ppn, count: usize) {
        for offset in 0..count {
            self.allocator.release_owned(Ppn(base.0 + offset));
        }
    }

    unsafe fn direct_map_ptr(&self, ppn: Ppn) -> *mut u8 {
        (self.base + ppn.0 * PAGE_SIZE) as *mut u8
    }

    fn ppn_from_direct_map_ptr(&self, ptr: *mut u8) -> Option<Ppn> {
        let addr = ptr as usize;
        let offset = addr.checked_sub(self.base)?;
        if offset % PAGE_SIZE == 0 {
            Some(Ppn(offset / PAGE_SIZE))
        } else {
            None
        }
    }
}

fn test_allocator<'a>(
    metas: &'a [FrameMeta],
    bitmap: &'a [AtomicU64],
    free_pages: usize,
) -> BitmapPageAllocator<'a> {
    let allocator = BitmapPageAllocator::new_for_test(metas, bitmap, metas.len());
    for ppn in 0..free_pages {
        allocator.mark_free_for_test(Ppn(ppn));
    }
    allocator
}

#[test]
fn slab_rejects_allocation_before_init() {
    static mut PAGES: [TestPage; 1] = [TestPage {
        _bytes: [0; PAGE_SIZE],
    }; 1];
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = test_allocator(&metas, &bitmap, 1);
    let provider = TestProvider {
        allocator: &allocator,
        base: core::ptr::addr_of_mut!(PAGES) as usize,
    };
    let heap = SlabHeap::new(provider);
    let layout = Layout::from_size_align(32, 8).expect("valid layout");

    let err = heap
        .try_alloc(layout)
        .expect_err("heap must reject allocation before explicit init");

    assert_eq!(err, SlabError::NotInitialized);
    assert_eq!(allocator.free_count(), 1);
}

#[test]
fn slab_reuses_small_objects_and_retains_one_empty_page() {
    static mut PAGES: [TestPage; 2] = [TestPage {
        _bytes: [0; PAGE_SIZE],
    }; 2];
    let metas = [FrameMeta::new(), FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = test_allocator(&metas, &bitmap, 2);
    let provider = TestProvider {
        allocator: &allocator,
        base: core::ptr::addr_of_mut!(PAGES) as usize,
    };
    let heap = SlabHeap::new(provider);
    heap.init().expect("heap init");
    let layout = Layout::from_size_align(32, 8).expect("valid layout");

    let first = heap.try_alloc(layout).expect("first object");
    let second = heap.try_alloc(layout).expect("second object");
    assert_ne!(first, second);
    assert_eq!(allocator.free_count(), 1);

    unsafe {
        heap.dealloc(first.as_ptr(), layout);
        assert_eq!(allocator.free_count(), 1);
        heap.dealloc(second.as_ptr(), layout);
    }

    assert_eq!(
        allocator.free_count(),
        1,
        "one fully empty slab page should stay retained for reuse"
    );

    let third = heap.try_alloc(layout).expect("object after return");
    assert_eq!(allocator.free_count(), 1);
    unsafe {
        heap.dealloc(third.as_ptr(), layout);
    }
}

#[test]
fn slab_returns_surplus_empty_pages_beyond_retained_page() {
    static mut PAGES: [TestPage; 3] = [TestPage {
        _bytes: [0; PAGE_SIZE],
    }; 3];
    let metas = [FrameMeta::new(), FrameMeta::new(), FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = test_allocator(&metas, &bitmap, 3);
    let provider = TestProvider {
        allocator: &allocator,
        base: core::ptr::addr_of_mut!(PAGES) as usize,
    };
    let heap = SlabHeap::new(provider);
    heap.init().expect("heap init");
    let layout = Layout::from_size_align(2048, 8).expect("valid layout");

    let first = heap
        .try_alloc(layout)
        .expect("first page-sized class object");
    let second = heap
        .try_alloc(layout)
        .expect("second page-sized class object");
    assert_eq!(allocator.free_count(), 1);

    unsafe {
        heap.dealloc(first.as_ptr(), layout);
        heap.dealloc(second.as_ptr(), layout);
    }

    assert_eq!(
        allocator.free_count(),
        2,
        "only one empty slab page should be retained per size class"
    );
}

#[test]
fn slab_refills_medium_classes_in_page_batches() {
    static mut PAGES: [TestPage; 20] = [TestPage {
        _bytes: [0; PAGE_SIZE],
    }; 20];
    let metas = [
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
    ];
    let bitmap = [AtomicU64::new(0)];
    let allocator = test_allocator(&metas, &bitmap, 20);
    let provider = TestProvider {
        allocator: &allocator,
        base: core::ptr::addr_of_mut!(PAGES) as usize,
    };
    let heap = SlabHeap::new(provider);
    heap.init().expect("heap init");
    let layout = Layout::from_size_align(128, 8).expect("valid layout");

    let first = heap.try_alloc(layout).expect("first medium object");
    assert_eq!(
        allocator.free_count(),
        12,
        "128-byte class should refill eight pages at a time"
    );

    let mut ptrs = Vec::new();
    ptrs.push(first);
    for _ in 1..(31 * 8) {
        ptrs.push(heap.try_alloc(layout).expect("batched refill object"));
    }
    assert_eq!(allocator.free_count(), 12);

    ptrs.push(heap.try_alloc(layout).expect("second batched refill"));
    assert_eq!(allocator.free_count(), 4);

    for ptr in ptrs {
        unsafe {
            heap.dealloc(ptr.as_ptr(), layout);
        }
    }
    assert_eq!(
        allocator.free_count(),
        12,
        "the latest refill batch remains cached while emptied older pages return"
    );
}

#[test]
fn large_allocation_uses_page_run_and_frees_all_pages() {
    static mut PAGES: [TestPage; 4] = [TestPage {
        _bytes: [0; PAGE_SIZE],
    }; 4];
    let metas = [
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
    ];
    let bitmap = [AtomicU64::new(0)];
    let allocator = test_allocator(&metas, &bitmap, 4);
    let provider = TestProvider {
        allocator: &allocator,
        base: core::ptr::addr_of_mut!(PAGES) as usize,
    };
    let heap = SlabHeap::new(provider);
    heap.init().expect("heap init");
    let layout = Layout::from_size_align(6000, PAGE_SIZE).expect("valid layout");

    let ptr = heap.try_alloc(layout).expect("large allocation");

    assert_eq!(ptr.as_ptr() as usize % PAGE_SIZE, 0);
    assert_eq!(allocator.free_count(), 2);

    unsafe {
        heap.dealloc(ptr.as_ptr(), layout);
    }

    assert_eq!(allocator.free_count(), 4);
}
