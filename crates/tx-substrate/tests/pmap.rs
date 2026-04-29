use core::sync::atomic::{AtomicUsize, Ordering};
use tx_hal::pmap::{protect_page_range, reserve_page_range, unmap_page_range, PmapRangeError};
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};

static RESERVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static COMMIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static ROLLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
static UNMAP_COUNT: AtomicUsize = AtomicUsize::new(0);
static PROTECT_COUNT: AtomicUsize = AtomicUsize::new(0);
static FAIL_RESERVE_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
static PMAP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TestPmap;

impl PmapIf for TestPmap {
    fn reserve_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        let index = RESERVE_COUNT.fetch_add(1, Ordering::AcqRel);
        if FAIL_RESERVE_AT.load(Ordering::Acquire) == index {
            return Err(PmapError::Exhausted);
        }
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {
        ROLLBACK_COUNT.fetch_add(1, Ordering::AcqRel);
    }

    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
        COMMIT_COUNT.fetch_add(1, Ordering::AcqRel);
    }

    fn unmap_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        UNMAP_COUNT.fetch_add(1, Ordering::AcqRel);
        Ok(Some(PmapUnmapResult::new(virt, PhysAddr(virt.0), kind)))
    }

    fn protect_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        PROTECT_COUNT.fetch_add(1, Ordering::AcqRel);
        Ok(Some(PmapInvalidation::new(virt, kind.size())))
    }
}

fn reset() {
    RESERVE_COUNT.store(0, Ordering::Release);
    COMMIT_COUNT.store(0, Ordering::Release);
    ROLLBACK_COUNT.store(0, Ordering::Release);
    UNMAP_COUNT.store(0, Ordering::Release);
    PROTECT_COUNT.store(0, Ordering::Release);
    FAIL_RESERVE_AT.store(usize::MAX, Ordering::Release);
}

fn root() -> PmapRoot {
    PmapRoot::new(PtNode::boot_pool(PhysAddr(0x1000)), Asid(1))
}

#[test]
fn page_range_reservation_commits_all_pages() {
    let _guard = PMAP_TEST_LOCK.lock().expect("pmap test lock");
    reset();
    let root = root();

    let range = reserve_page_range::<TestPmap, 4>(&root, VirtAddr(0x4000), PhysAddr(0x8000), 3)
        .expect("reserve range");
    assert_eq!(range.len(), 3);

    assert_eq!(range.commit(PmapPermissions::KERNEL_RW), 3);
    assert_eq!(RESERVE_COUNT.load(Ordering::Acquire), 3);
    assert_eq!(COMMIT_COUNT.load(Ordering::Acquire), 3);
    assert_eq!(ROLLBACK_COUNT.load(Ordering::Acquire), 0);
}

#[test]
fn page_range_reservation_rolls_back_prefix_on_error() {
    let _guard = PMAP_TEST_LOCK.lock().expect("pmap test lock");
    reset();
    FAIL_RESERVE_AT.store(2, Ordering::Release);
    let root = root();

    let result = reserve_page_range::<TestPmap, 4>(&root, VirtAddr(0x4000), PhysAddr(0x8000), 3);

    assert_eq!(
        result.err(),
        Some(PmapRangeError::Pmap(PmapError::Exhausted))
    );
    assert_eq!(RESERVE_COUNT.load(Ordering::Acquire), 3);
    assert_eq!(COMMIT_COUNT.load(Ordering::Acquire), 0);
    assert_eq!(ROLLBACK_COUNT.load(Ordering::Acquire), 2);
}

#[test]
fn page_range_unmap_and_protect_collect_invalidations() {
    let _guard = PMAP_TEST_LOCK.lock().expect("pmap test lock");
    reset();
    let root = root();
    let mut unmaps = [None; 4];
    let mut invalidations = [None; 4];

    let unmapped =
        unmap_page_range::<TestPmap>(&root, VirtAddr(0x4000), 2, &mut unmaps).expect("unmap range");
    let protected = protect_page_range::<TestPmap>(
        &root,
        VirtAddr(0x4000),
        2,
        PmapPermissions::KERNEL_RO,
        &mut invalidations,
    )
    .expect("protect range");

    assert_eq!(unmapped, 2);
    assert_eq!(protected, 2);
    assert_eq!(unmaps[0].expect("first unmap").virt(), VirtAddr(0x4000));
    assert_eq!(unmaps[1].expect("second unmap").virt(), VirtAddr(0x5000));
    assert_eq!(
        invalidations[1].expect("second invalidation").virt(),
        VirtAddr(0x5000)
    );
    assert_eq!(UNMAP_COUNT.load(Ordering::Acquire), 2);
    assert_eq!(PROTECT_COUNT.load(Ordering::Acquire), 2);
}
