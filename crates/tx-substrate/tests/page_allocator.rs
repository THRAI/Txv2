use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tx_hal::Ppn;
use tx_substrate::page_allocator::{
    AllocError, AllocatorBackendKind, BitmapPageAllocator, FrameMeta, PageAllocator, ZeroPolicy,
};

static ZEROED_PPN: AtomicUsize = AtomicUsize::new(usize::MAX);

unsafe fn record_zeroed_ppn(ppn: Ppn) {
    ZEROED_PPN.store(ppn.0, Ordering::SeqCst);
}

#[test]
fn reservation_drop_rolls_back_to_free_state() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    {
        let reservation = allocator
            .reserve_frame(ZeroPolicy::UninitFullOverwrite)
            .expect("reservation should claim the only frame");
        assert_eq!(reservation.ppn(), Ppn(0));
        assert_eq!(allocator.free_count(), 0);
        assert_eq!(metas[0].state_for_test(), 0);
    }

    assert_eq!(allocator.free_count(), 1);
    assert_eq!(metas[0].state_for_test(), 0);
    assert!(allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn committed_frame_owns_refcount_until_dropped() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();

    assert_eq!(frame.ppn(), Ppn(0));
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));

    drop(frame);

    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
    assert!(allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn role_handoff_keeps_frame_live_after_owner_drops() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let map_pin = frame.try_map_pin().expect("map pin should fit");

    assert_eq!(metas[0].refcount_for_test(), 1);
    assert_eq!(metas[0].map_count_for_test(), 1);

    drop(frame);

    assert_eq!(metas[0].refcount_for_test(), 0);
    assert_eq!(metas[0].map_count_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));

    drop(map_pin);

    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
    assert!(allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn cache_and_dma_pins_keep_frame_live_until_all_roles_drop() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let cache_pin = frame.try_cache_pin().expect("cache pin should fit");
    let dma_pin = frame.try_dma_pin().expect("DMA pin should fit");

    assert_eq!(metas[0].cache_ref_for_test(), 1);
    assert_eq!(metas[0].pin_count_for_test(), 1);

    drop(frame);
    drop(cache_pin);

    assert_eq!(metas[0].pin_count_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));

    drop(dma_pin);

    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
}

#[test]
fn gift_pin_acquire_on_live_frame_increments_transfer_evidence() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let gift_pin = frame
        .try_gift_pin()
        .expect("gift pin should retain a live frame");

    assert_eq!(gift_pin.ppn(), Ppn(0));
    assert_eq!(metas[0].refcount_for_test(), 2);
    assert_eq!(metas[0].pin_count_for_test(), 0);
    assert_eq!(allocator.free_count(), 0);
}

#[test]
fn gift_pin_drop_releases_transfer_evidence() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let gift_pin = frame
        .try_gift_pin()
        .expect("gift pin should retain a live frame");

    drop(frame);

    assert_eq!(metas[0].refcount_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));

    drop(gift_pin);

    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
    assert!(allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn gift_pin_rejects_dead_frame() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let err = allocator
        .acquire_gift_pin(Ppn(0))
        .expect_err("dead frame cannot be retained for transfer");

    assert_eq!(err, AllocError::InvalidRequest);
    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
    assert!(allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn contiguous_run_commit_can_split_into_owned_frames() {
    let metas = [
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
    ];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 4);
    for ppn in 0..4 {
        allocator.mark_free_for_test(Ppn(ppn));
    }

    let run = allocator
        .reserve_run(2, 2, ZeroPolicy::UninitFullOverwrite)
        .expect("aligned run")
        .commit();

    assert_eq!(run.base(), Ppn(0));
    assert_eq!(run.count(), 2);
    assert_eq!(allocator.free_count(), 2);
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert_eq!(metas[1].refcount_for_test(), 1);

    let mut frames = run.split();
    assert_eq!(frames.len(), 2);
    let first = frames.next().expect("first frame");
    let second = frames.next().expect("second frame");
    assert_eq!(frames.next().map(|frame| frame.ppn()), None);
    assert_eq!(first.ppn(), Ppn(0));
    assert_eq!(second.ppn(), Ppn(1));

    drop(first);
    drop(second);
    drop(frames);

    assert_eq!(allocator.free_count(), 4);
}

#[test]
fn contiguous_run_partial_failure_preserves_free_count() {
    let metas = [FrameMeta::new(), FrameMeta::new(), FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 3);
    allocator.mark_free_for_test(Ppn(0));
    allocator.mark_free_for_test(Ppn(2));

    let err = allocator
        .reserve_run(3, 1, ZeroPolicy::UninitFullOverwrite)
        .expect_err("hole in the candidate run prevents allocation");

    assert_eq!(err, AllocError::Exhausted);
    assert_eq!(allocator.free_count(), 2);
    assert!(allocator.is_free_for_test(Ppn(0)));
    assert!(allocator.is_free_for_test(Ppn(2)));
}

#[test]
fn reserved_frame_is_removed_from_allocator_pool() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    allocator.mark_reserved_for_test(Ppn(0));

    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));
    assert!(metas[0].is_reserved());

    let err = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect_err("reserved frame cannot be allocated");
    assert_eq!(err, AllocError::Exhausted);
}

#[test]
fn zeroed_allocation_requires_scrubber_and_rolls_back() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let err = allocator
        .reserve_frame(ZeroPolicy::Zeroed)
        .expect_err("zeroed allocation needs a scrubber hook");

    assert_eq!(err, AllocError::ZeroScrubUnavailable);
    assert_eq!(allocator.free_count(), 1);
    assert!(allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn zeroed_allocation_uses_installed_scrubber() {
    ZEROED_PPN.store(usize::MAX, Ordering::SeqCst);
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_with_zeroer(&metas, &bitmap, 1, record_zeroed_ppn);
    allocator.mark_free_for_test(Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::Zeroed)
        .expect("scrubber is installed")
        .commit();

    assert_eq!(frame.ppn(), Ppn(0));
    assert_eq!(ZEROED_PPN.load(Ordering::SeqCst), 0);
}

#[test]
fn installed_frame_copy_hook_copies_test_direct_map_bytes() {
    tx_substrate::testing::init_host_for_test_once();
    let source = tx_substrate::page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("source frame")
        .commit();
    let dest = tx_substrate::page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("dest frame")
        .commit();
    let pattern = [0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd];
    let mut observed = [0u8; 8];

    tx_substrate::page_allocator::testing::write_frame_bytes_for_test(source.ppn(), 64, &pattern);
    tx_substrate::page_allocator::testing::write_frame_bytes_for_test(dest.ppn(), 64, &[0u8; 8]);

    tx_substrate::page_allocator::copy_frame_contents(source.ppn(), dest.ppn())
        .expect("copy frame contents through installed hook");
    tx_substrate::page_allocator::testing::read_frame_bytes_for_test(dest.ppn(), 64, &mut observed);

    assert_eq!(observed, pattern);
}

#[test]
fn permanent_frame_anchor_never_returns_to_free_pool() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let permanent = allocator
        .claim_permanent_frame(Ppn(0))
        .expect("free frame can become a permanent anchor");

    assert_eq!(permanent.ppn(), Ppn(0));
    assert_eq!(allocator.free_count(), 0);
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert!(metas[0].is_reserved());
    assert!(metas[0].is_direct_mapped());
    assert!(metas[0].is_permanent());

    drop(permanent);

    assert_eq!(metas[0].refcount_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn reserved_boot_frame_can_become_permanent_anchor() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_reserved_for_test(Ppn(0));

    let permanent = allocator
        .claim_permanent_frame(Ppn(0))
        .expect("reserved boot frame can become a permanent anchor");

    assert_eq!(permanent.ppn(), Ppn(0));
    assert_eq!(allocator.free_count(), 0);
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert!(metas[0].is_reserved());
    assert!(metas[0].is_direct_mapped());
    assert!(metas[0].is_permanent());
    assert!(!allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn owned_frame_can_become_permanent_anchor() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let permanent = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit()
        .into_permanent_frame();

    assert_eq!(permanent.ppn(), Ppn(0));
    assert_eq!(allocator.free_count(), 0);
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert!(metas[0].is_reserved());
    assert!(metas[0].is_direct_mapped());
    assert!(metas[0].is_permanent());

    drop(permanent);

    assert_eq!(metas[0].refcount_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
    assert!(!allocator.is_free_for_test(Ppn(0)));
}

#[test]
fn dma_pin_rejects_reserved_permanent_frame() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let _permanent = allocator
        .claim_permanent_frame(Ppn(0))
        .expect("permanent anchor");

    assert_eq!(
        PageAllocator::acquire_dma_pin(&allocator, Ppn(0)),
        Err(AllocError::ReservedFrame)
    );
    assert_eq!(metas[0].pin_count_for_test(), 0);
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert!(!allocator.is_free_for_test(Ppn(0)));
}

#[test]
#[should_panic(expected = "pmap teardown attempted to release permanent frame")]
fn pmap_teardown_cannot_release_permanent_frame() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let _permanent = allocator
        .claim_permanent_frame(Ppn(0))
        .expect("permanent anchor");

    PageAllocator::release_page_table_frame(&allocator, Ppn(0));
}

#[test]
fn page_table_frame_releases_only_through_pmap_teardown() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(Ppn(0));

    let pt_frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit()
        .into_page_table_frame();

    assert_eq!(pt_frame.ppn(), Ppn(0));
    assert_eq!(allocator.free_count(), 0);
    assert_eq!(metas[0].refcount_for_test(), 1);
    assert!(metas[0].is_reserved());
    assert!(metas[0].is_direct_mapped());

    pt_frame.release_for_pmap_teardown();

    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
    assert!(!metas[0].is_reserved());
}

#[test]
fn diagnostics_report_bitmap_backend_counts() {
    let metas = [FrameMeta::new(), FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 2);
    allocator.mark_free_for_test(Ppn(0));

    let diagnostics = allocator.backend_diagnostics();

    assert_eq!(diagnostics.backend, AllocatorBackendKind::Bitmap);
    assert_eq!(diagnostics.base_ppn, Ppn(0));
    assert_eq!(diagnostics.total_count, 2);
    assert_eq!(diagnostics.free_count, 1);
    assert_eq!(diagnostics.max_contiguous_free_run, 1);
}

#[test]
fn diagnostics_report_fragmented_max_contiguous_free_run() {
    let metas = [
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
    ];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 6);
    for ppn in [0, 1, 3, 4, 5] {
        allocator.mark_free_for_test(Ppn(ppn));
    }

    let diagnostics = allocator.backend_diagnostics();

    assert_eq!(diagnostics.free_count, 5);
    assert_eq!(diagnostics.max_contiguous_free_run, 3);
}

#[test]
fn dense_base_allocator_returns_raw_ppns() {
    let metas = [
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
        FrameMeta::new(),
    ];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_with_base_for_test(&metas, &bitmap, Ppn(0x80000), 4);
    allocator.mark_free_for_test(Ppn(0x80002));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation from dense backend")
        .commit();

    let diagnostics = allocator.backend_diagnostics();
    assert_eq!(frame.ppn(), Ppn(0x80002));
    assert_eq!(diagnostics.base_ppn, Ppn(0x80000));
    assert_eq!(diagnostics.total_count, 4);
}

#[test]
fn dense_base_run_alignment_uses_raw_ppn() {
    let metas = [
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
    let allocator = BitmapPageAllocator::new_with_base_for_test(&metas, &bitmap, Ppn(10), 8);
    for ppn in 10..18 {
        allocator.mark_free_for_test(Ppn(ppn));
    }

    let run = allocator
        .reserve_run(2, 4, ZeroPolicy::UninitFullOverwrite)
        .expect("raw-aligned run")
        .commit();

    assert_eq!(run.base(), Ppn(12));
    assert_eq!(run.count(), 2);
}

#[test]
fn exhausted_allocator_reports_error() {
    let metas = [FrameMeta::new()];
    let bitmap = [AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);

    let err = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect_err("no frame was marked free");

    assert_eq!(err, AllocError::Exhausted);
}
