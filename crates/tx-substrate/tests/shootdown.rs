use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use tx_hal::{
    Asid, PhysAddr, PmapIf, PmapInvalidation, PmapReserveKind, PmapUnmapResult, VirtAddr,
};
use tx_substrate::page_allocator::{BitmapPageAllocator, FrameMeta, PageAllocator, ZeroPolicy};
use tx_substrate::shootdown::{AddressSpaceShootdownBatch, KernelShootdownBatch, ShootdownError};

static SHOOTDOWN_COUNT: AtomicUsize = AtomicUsize::new(0);
static SHOOTDOWN_META: AtomicPtr<FrameMeta> = AtomicPtr::new(core::ptr::null_mut());
static MAP_COUNT_AT_SHOOTDOWN: AtomicUsize = AtomicUsize::new(usize::MAX);
static LAST_INVALIDATION_VIRT: AtomicUsize = AtomicUsize::new(0);
static LAST_ASID: AtomicUsize = AtomicUsize::new(usize::MAX);
static SHOOTDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TestPmap;

impl PmapIf for TestPmap {
    fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
        SHOOTDOWN_COUNT.fetch_add(1, Ordering::AcqRel);
        LAST_INVALIDATION_VIRT.store(invalidation.virt().0, Ordering::Release);

        let meta = SHOOTDOWN_META.load(Ordering::Acquire);
        if !meta.is_null() {
            let map_count = unsafe { (*meta).map_count_for_test() as usize };
            MAP_COUNT_AT_SHOOTDOWN.store(map_count, Ordering::Release);
        }
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        SHOOTDOWN_COUNT.fetch_add(1, Ordering::AcqRel);
        LAST_ASID.store(asid.0 as usize, Ordering::Release);
        LAST_INVALIDATION_VIRT.store(invalidation.virt().0, Ordering::Release);

        let meta = SHOOTDOWN_META.load(Ordering::Acquire);
        if !meta.is_null() {
            let map_count = unsafe { (*meta).map_count_for_test() as usize };
            MAP_COUNT_AT_SHOOTDOWN.store(map_count, Ordering::Release);
        }
    }
}

#[test]
fn kernel_shootdown_releases_map_pin_only_after_invalidation() {
    let _guard = SHOOTDOWN_TEST_LOCK.lock().expect("shootdown test lock");
    SHOOTDOWN_COUNT.store(0, Ordering::Release);
    MAP_COUNT_AT_SHOOTDOWN.store(usize::MAX, Ordering::Release);
    LAST_INVALIDATION_VIRT.store(0, Ordering::Release);
    LAST_ASID.store(usize::MAX, Ordering::Release);
    SHOOTDOWN_META.store(core::ptr::null_mut(), Ordering::Release);

    let metas = [FrameMeta::new()];
    let bitmap = [core::sync::atomic::AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(tx_hal::Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let map_pin = frame.try_map_pin().expect("map pin");
    drop(frame);

    assert_eq!(metas[0].refcount_for_test(), 0);
    assert_eq!(metas[0].map_count_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);

    SHOOTDOWN_META.store(
        &metas[0] as *const FrameMeta as *mut FrameMeta,
        Ordering::Release,
    );

    let mut batch = KernelShootdownBatch::<_, 4>::new();
    batch
        .push_page_unmap_result(
            PmapUnmapResult::new(
                VirtAddr(0xffff_ffc0_4000_0000),
                PhysAddr(0),
                PmapReserveKind::Page4K,
            ),
            map_pin,
        )
        .expect("queued unmap accounting");

    assert_eq!(metas[0].map_count_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);

    batch.issue_and_release::<TestPmap>();

    assert_eq!(SHOOTDOWN_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(
        LAST_INVALIDATION_VIRT.load(Ordering::Acquire),
        0xffff_ffc0_4000_0000
    );
    assert_eq!(MAP_COUNT_AT_SHOOTDOWN.load(Ordering::Acquire), 1);
    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
}

#[test]
fn dropped_kernel_shootdown_batch_does_not_release_map_pin() {
    let _guard = SHOOTDOWN_TEST_LOCK.lock().expect("shootdown test lock");
    SHOOTDOWN_META.store(core::ptr::null_mut(), Ordering::Release);

    let metas = [FrameMeta::new()];
    let bitmap = [core::sync::atomic::AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(tx_hal::Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let map_pin = frame.try_map_pin().expect("map pin");
    drop(frame);

    let mut batch = KernelShootdownBatch::<_, 1>::new();
    batch
        .push_page_unmap_result(
            PmapUnmapResult::new(
                VirtAddr(0xffff_ffc0_4000_1000),
                PhysAddr(0),
                PmapReserveKind::Page4K,
            ),
            map_pin,
        )
        .expect("queued unmap accounting");

    let drop_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(batch)));

    assert!(drop_result.is_err());
    assert_eq!(metas[0].map_count_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);
}

#[test]
fn address_space_shootdown_uses_asid_and_releases_after_invalidation() {
    let _guard = SHOOTDOWN_TEST_LOCK.lock().expect("shootdown test lock");
    SHOOTDOWN_COUNT.store(0, Ordering::Release);
    MAP_COUNT_AT_SHOOTDOWN.store(usize::MAX, Ordering::Release);
    LAST_INVALIDATION_VIRT.store(0, Ordering::Release);
    LAST_ASID.store(usize::MAX, Ordering::Release);
    SHOOTDOWN_META.store(core::ptr::null_mut(), Ordering::Release);

    let metas = [FrameMeta::new()];
    let bitmap = [core::sync::atomic::AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(tx_hal::Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let map_pin = frame.try_map_pin().expect("map pin");
    drop(frame);

    SHOOTDOWN_META.store(
        &metas[0] as *const FrameMeta as *mut FrameMeta,
        Ordering::Release,
    );

    let mut batch = AddressSpaceShootdownBatch::<_, 4>::new(Asid(7));
    batch
        .push_page_unmap_result(
            PmapUnmapResult::new(VirtAddr(0x4000), PhysAddr(0), PmapReserveKind::Page4K),
            map_pin,
        )
        .expect("queued user unmap accounting");

    batch.issue_and_release::<TestPmap>();

    assert_eq!(SHOOTDOWN_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(LAST_ASID.load(Ordering::Acquire), 7);
    assert_eq!(LAST_INVALIDATION_VIRT.load(Ordering::Acquire), 0x4000);
    assert_eq!(MAP_COUNT_AT_SHOOTDOWN.load(Ordering::Acquire), 1);
    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 1);
}

#[test]
fn address_space_shootdown_releases_superpage_run_after_invalidation() {
    let _guard = SHOOTDOWN_TEST_LOCK.lock().expect("shootdown test lock");
    SHOOTDOWN_COUNT.store(0, Ordering::Release);
    LAST_ASID.store(usize::MAX, Ordering::Release);
    LAST_INVALIDATION_VIRT.store(0, Ordering::Release);
    SHOOTDOWN_META.store(core::ptr::null_mut(), Ordering::Release);

    let metas = [const { FrameMeta::new() }; 512];
    let bitmap = [
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
    ];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 512);
    for ppn in 0..512 {
        allocator.mark_free_for_test(tx_hal::Ppn(ppn));
    }

    let run = allocator
        .reserve_run(512, 512, ZeroPolicy::UninitFullOverwrite)
        .expect("superpage run")
        .commit();
    let map_pin_run = run.try_map_pin_run().expect("map pin run");
    drop(run);

    assert_eq!(metas[0].map_count_for_test(), 1);
    assert_eq!(metas[511].map_count_for_test(), 1);
    assert_eq!(allocator.free_count(), 0);

    let mut batch = AddressSpaceShootdownBatch::<_, 1>::new(Asid(11));
    batch
        .push_unmap_result(
            PmapUnmapResult::new(
                VirtAddr(0x20_0000),
                PhysAddr(0),
                PmapReserveKind::Superpage2M,
            ),
            map_pin_run,
        )
        .expect("queued superpage accounting");

    batch.issue_and_release::<TestPmap>();

    assert_eq!(SHOOTDOWN_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(LAST_ASID.load(Ordering::Acquire), 11);
    assert_eq!(LAST_INVALIDATION_VIRT.load(Ordering::Acquire), 0x20_0000);
    assert_eq!(metas[0].state_for_test(), 0);
    assert_eq!(metas[511].state_for_test(), 0);
    assert_eq!(allocator.free_count(), 512);
}

#[test]
fn shootdown_rejects_superpage_result_with_single_page_pin() {
    let _guard = SHOOTDOWN_TEST_LOCK.lock().expect("shootdown test lock");
    SHOOTDOWN_META.store(core::ptr::null_mut(), Ordering::Release);

    let metas = [FrameMeta::new()];
    let bitmap = [core::sync::atomic::AtomicU64::new(0)];
    let allocator = BitmapPageAllocator::new_for_test(&metas, &bitmap, 1);
    allocator.mark_free_for_test(tx_hal::Ppn(0));

    let frame = allocator
        .reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .expect("reservation")
        .commit();
    let map_pin = frame.try_map_pin().expect("map pin");
    drop(frame);

    let mut batch = KernelShootdownBatch::<_, 1>::new();
    let err = batch
        .push_page_unmap_result(
            PmapUnmapResult::new(
                VirtAddr(0x20_0000),
                PhysAddr(0),
                PmapReserveKind::Superpage2M,
            ),
            map_pin,
        )
        .expect_err("single-page pin cannot account for superpage");

    assert_eq!(err.reason(), ShootdownError::UnsupportedMapping);
    drop(err.into_map_pin());
}
