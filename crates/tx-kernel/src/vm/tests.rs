use super::*;
use crate::page_backed::PageContainer;
use alloc::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};

static COUNTING_PMAP_TEST_LOCK: Mutex<()> = Mutex::new(());
static COUNTING_PMAP_STATE: LazyLock<Mutex<CountingPmapState>> =
    LazyLock::new(|| Mutex::new(CountingPmapState::new()));

struct CountingPmap;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CountingPmapCounters {
    creates: usize,
    destroys: usize,
    reserves: usize,
    commits: usize,
    unmaps: usize,
    shoots: usize,
    last_asid: Option<Asid>,
}

struct CountingPmapState {
    next_root: usize,
    fail_reserve: Option<PmapError>,
    mappings: BTreeMap<(usize, usize), PhysAddr>,
    counters: CountingPmapCounters,
}

impl CountingPmapState {
    fn new() -> Self {
        Self {
            next_root: 1,
            fail_reserve: None,
            mappings: BTreeMap::new(),
            counters: CountingPmapCounters::default(),
        }
    }
}

fn reset_counting_pmap() {
    *COUNTING_PMAP_STATE.lock().expect("counting pmap lock") = CountingPmapState::new();
}

fn fail_counting_reserve(error: PmapError) {
    COUNTING_PMAP_STATE
        .lock()
        .expect("counting pmap lock")
        .fail_reserve = Some(error);
}

fn counting_pmap_counters() -> CountingPmapCounters {
    COUNTING_PMAP_STATE
        .lock()
        .expect("counting pmap lock")
        .counters
}

fn wait_for_counting_pmap_counters(expected: CountingPmapCounters) {
    for _ in 0..1024 {
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        if counting_pmap_counters() == expected {
            return;
        }
        std::thread::yield_now();
    }

    assert_eq!(counting_pmap_counters(), expected);
}

fn counting_root_key(root: &PmapRoot) -> usize {
    root.phys().0
}

impl PmapIf for CountingPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        let root_id = state.next_root;
        state.next_root += 1;
        state.counters.creates += 1;
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(root_id * USER_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(root: PmapRoot) {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.destroys += 1;
        let root_key = root.phys().0;
        state
            .mappings
            .retain(|(mapped_root, _), _| *mapped_root != root_key);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.reserves += 1;
        if let Some(error) = state.fail_reserve {
            return Err(error);
        }
        if state
            .mappings
            .contains_key(&(counting_root_key(root), virt.0))
        {
            return Err(PmapError::AlreadyMapped);
        }
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn commit_mapping(
        root: &PmapRoot,
        reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.commits += 1;
        state.mappings.insert(
            (counting_root_key(root), reservation.virt().0),
            reservation.phys(),
        );
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.unmaps += 1;
        let Some(phys) = state.mappings.remove(&(counting_root_key(root), virt.0)) else {
            return Ok(None);
        };
        Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
    }

    fn shootdown_mapping(asid: Asid, _invalidation: PmapInvalidation) {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.shoots += 1;
        state.counters.last_asid = Some(asid);
    }
}

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
}

fn page_backing(offset: u64) -> VmBacking {
    setup_host_substrate();
    VmBacking::Page {
        pc: PageContainer::new_cap(
            crate::page_backed::PageContainerKind::Anon {
                swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
            },
            16,
        )
        .expect("page container cap"),
        offset,
    }
}

fn page_backing_like(backing: &VmBacking, offset: u64) -> VmBacking {
    let VmBacking::Page { pc, .. } = backing else {
        panic!("expected page backing");
    };
    VmBacking::Page {
        pc: pc.clone(),
        offset,
    }
}

fn range(start: usize, pages: usize) -> UserRange {
    UserRange::new_aligned(UserVirtAddr(start), pages * USER_PAGE_SIZE).expect("valid range")
}

fn acquired(result: AcquireResult<'_>) -> RangeGuard<'_> {
    match result {
        AcquireResult::Acquired(guard) => guard,
        AcquireResult::WouldBlock(_) => panic!("expected acquired"),
    }
}

fn pair_acquired(result: AcquirePairResult<'_>) -> RangeGuardPair<'_> {
    match result {
        AcquirePairResult::Acquired(guards) => guards,
        AcquirePairResult::WouldBlock(_) => panic!("expected pair acquired"),
    }
}

fn map_reserved(result: MapReserveResult<'_>) -> MapReservation<'_> {
    match result {
        MapReserveResult::Reserved(reservation) => reservation,
        MapReserveResult::WouldBlock(_) => panic!("expected map reservation"),
        MapReserveResult::Err(error) => panic!("unexpected map reserve error: {error:?}"),
    }
}

fn map_error(result: MapReserveResult<'_>) -> VmMapError {
    match result {
        MapReserveResult::Reserved(_) => panic!("expected map reserve error"),
        MapReserveResult::WouldBlock(_) => panic!("expected map reserve error, got block"),
        MapReserveResult::Err(error) => error,
    }
}

fn would_block(result: AcquireResult<'_>) -> WouldBlock<'_> {
    match result {
        AcquireResult::Acquired(_) => panic!("expected would-block"),
        AcquireResult::WouldBlock(blocked) => blocked,
    }
}

#[test]
fn vm_user_range_rejects_zero_unaligned_and_overflow() {
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(0), 0),
        Err(UserRangeError::ZeroLength)
    );
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(1), USER_PAGE_SIZE),
        Err(UserRangeError::Unaligned)
    );
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(0), USER_PAGE_SIZE - 1),
        Err(UserRangeError::Unaligned)
    );
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(usize::MAX - 4095), USER_PAGE_SIZE),
        Err(UserRangeError::Overflow)
    );
}

#[test]
fn vm_user_range_iterates_pages_and_counts_them() {
    let pages = range(0x4000, 3);
    assert_eq!(pages.page_count(), 3);
    assert_eq!(pages.iter_pages().len(), 3);

    let mut iter = pages.iter_pages();
    assert_eq!(iter.next(), Some(UserPage(4)));
    assert_eq!(iter.next(), Some(UserPage(5)));
    assert_eq!(iter.next(), Some(UserPage(6)));
    assert_eq!(iter.next(), None);

    assert_eq!(UserVirtAddr(0x4123).containing_page(), UserPage(4));
    assert_eq!(
        UserRange::containing_page(UserVirtAddr(0x4123)),
        Ok(range(0x4000, 1))
    );
    assert_eq!(
        UserRange::containing_page(UserVirtAddr(usize::MAX)),
        Err(UserRangeError::Overflow)
    );
}

#[test]
fn vm_range_lock_conflict_matrix_matches_modes() {
    let lock = RangeLock::new();
    let first = range(0x1000, 2);
    let overlap = range(0x2000, 1);
    let disjoint = range(0x8000, 1);

    let writer = acquired(lock.acquire(first, LockMode::ExclusiveWriter));
    would_block(lock.acquire(overlap, LockMode::ExclusiveWriter));
    would_block(lock.acquire(overlap, LockMode::Materializer));
    let disjoint_writer = acquired(lock.acquire(disjoint, LockMode::ExclusiveWriter));
    drop(disjoint_writer);
    drop(writer);

    let materializer_a = acquired(lock.acquire(first, LockMode::Materializer));
    let materializer_b = acquired(lock.acquire(overlap, LockMode::Materializer));
    would_block(lock.acquire(overlap, LockMode::ExclusiveWriter));
    drop(materializer_b);
    drop(materializer_a);
}

#[test]
fn vm_range_lock_pending_writer_blocks_new_materializers() {
    let lock = RangeLock::new();
    let first = range(0x1000, 1);

    let materializer = acquired(lock.acquire(first, LockMode::Materializer));
    let pending = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("blocked writer should declare pending range");

    would_block(lock.acquire(first, LockMode::Materializer));
    drop(materializer);

    let writer = acquired(pending.try_acquire());
    would_block(lock.acquire(first, LockMode::Materializer));
    drop(writer);

    let materializer_after_drop = acquired(lock.acquire(first, LockMode::Materializer));
    drop(materializer_after_drop);
}

#[test]
fn vm_range_lock_overlapping_pending_writers_are_fifo() {
    let lock = RangeLock::new();
    let first = range(0x1000, 1);

    let materializer = acquired(lock.acquire(first, LockMode::Materializer));
    let pending_a = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("first writer queues");
    let pending_b = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("second writer queues");
    drop(materializer);

    let blocked_b = would_block(pending_b.try_acquire());
    let writer_a = acquired(pending_a.try_acquire());
    drop(writer_a);

    let writer_b = acquired(
        blocked_b
            .pending_writer()
            .expect("second writer remains queued")
            .try_acquire(),
    );
    drop(writer_b);
}

#[test]
fn vm_range_guard_drop_releases_reservation() {
    let lock = RangeLock::new();
    let first = range(0x1000, 1);

    {
        let writer = acquired(lock.acquire(first, LockMode::ExclusiveWriter));
        would_block(lock.acquire(first, LockMode::Materializer));
        drop(writer);
    }

    let materializer = acquired(lock.acquire(first, LockMode::Materializer));
    drop(materializer);
}

#[test]
fn vm_range_lock_acquires_two_ranges_atomically() {
    let lock = RangeLock::new();
    let a = range(0x1000, 1);
    let b = range(0x4000, 1);

    let pair = pair_acquired(lock.acquire_pair(
        (a, LockMode::ExclusiveWriter),
        (b, LockMode::ExclusiveWriter),
    ));
    would_block(lock.acquire(a, LockMode::Materializer));
    would_block(lock.acquire(b, LockMode::Materializer));
    drop(pair);

    let after = acquired(lock.acquire(a, LockMode::Materializer));
    drop(after);
}

#[test]
fn vm_range_lock_tree_removal_clears_only_removed_overlap() {
    let lock = RangeLock::new();
    let low = range(0x1000, 1);
    let middle = range(0x5000, 1);
    let high = range(0x9000, 1);

    let low_writer = acquired(lock.acquire(low, LockMode::ExclusiveWriter));
    let middle_writer = acquired(lock.acquire(middle, LockMode::ExclusiveWriter));
    let high_writer = acquired(lock.acquire(high, LockMode::ExclusiveWriter));

    would_block(lock.acquire(middle, LockMode::Materializer));
    drop(middle_writer);

    let middle_materializer = acquired(lock.acquire(middle, LockMode::Materializer));
    would_block(lock.acquire(low, LockMode::Materializer));
    would_block(lock.acquire(high, LockMode::Materializer));

    drop(middle_materializer);
    drop(low_writer);
    drop(high_writer);
}

#[test]
fn vm_range_lock_tree_keeps_disjoint_reservations_independent() {
    let lock = RangeLock::new();
    let a = range(0x1000, 1);
    let b = range(0x8000, 1);
    let c = range(0x10000, 1);

    let writer_a = acquired(lock.acquire(a, LockMode::ExclusiveWriter));
    let writer_b = acquired(lock.acquire(b, LockMode::ExclusiveWriter));
    let materializer_c = acquired(lock.acquire(c, LockMode::Materializer));

    would_block(lock.acquire(a, LockMode::Materializer));
    would_block(lock.acquire(b, LockMode::Materializer));
    let second_materializer_c = acquired(lock.acquire(c, LockMode::Materializer));

    drop(second_materializer_c);
    drop(materializer_c);
    drop(writer_b);
    drop(writer_a);
}

#[test]
fn vm_range_lock_tree_pending_fifo_is_range_scoped() {
    let lock = RangeLock::new();
    let first = range(0x2000, 1);
    let disjoint = range(0xa000, 1);

    let materializer = acquired(lock.acquire(first, LockMode::Materializer));
    let pending_a = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("first overlapping writer queues");
    let disjoint_writer = acquired(lock.acquire(disjoint, LockMode::ExclusiveWriter));
    let pending_b = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("second overlapping writer queues");

    drop(materializer);

    let blocked_b = would_block(pending_b.try_acquire());
    let writer_a = acquired(pending_a.try_acquire());
    drop(writer_a);

    let writer_b = acquired(
        blocked_b
            .pending_writer()
            .expect("second overlapping writer remains queued")
            .try_acquire(),
    );

    drop(writer_b);
    drop(disjoint_writer);
}

#[test]
fn vm_range_lock_tree_active_writer_blocks_materializer_overlap_only() {
    let lock = RangeLock::new();
    let writer_range = range(0x7000, 2);
    let overlap = range(0x8000, 1);
    let disjoint = range(0xb000, 1);

    let writer = acquired(lock.acquire(writer_range, LockMode::ExclusiveWriter));

    would_block(lock.acquire(overlap, LockMode::Materializer));
    let disjoint_materializer = acquired(lock.acquire(disjoint, LockMode::Materializer));

    drop(disjoint_materializer);
    drop(writer);
}

#[test]
fn vm_entry_split_for_unmap_preserves_survivors_and_offsets() {
    let entry = VmEntry::new(
        range(0x1000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        page_backing(10),
    );

    let rewrite = entry
        .split_for_unmap(range(0x2000, 2))
        .expect("hole is inside entry");

    assert_eq!(
        rewrite.before.expect("left survivor"),
        VmEntry::new(
            range(0x1000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            page_backing_like(&entry.backing, 10),
        )
    );
    assert_eq!(
        rewrite.after.expect("right survivor"),
        VmEntry::new(
            range(0x4000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            page_backing_like(&entry.backing, 10 + (3 * USER_PAGE_SIZE) as u64),
        )
    );
    assert_eq!(rewrite.target, None);
}

#[test]
fn vm_entry_split_for_protect_rewrites_middle_only() {
    let entry = VmEntry::new(
        range(0x1000, 3),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        VmBacking::PrivateAnon,
    );

    let rewrite = entry
        .split_for_protect(range(0x2000, 1), Prot::READ)
        .expect("target is inside entry");

    assert_eq!(rewrite.before.expect("before").prot, Prot::READ_WRITE);
    assert_eq!(rewrite.target.expect("target").prot, Prot::READ);
    assert_eq!(rewrite.after.expect("after").prot, Prot::READ_WRITE);
    assert_eq!(
        entry.split_for_protect(range(0x4000, 1), Prot::READ),
        Err(VmEntryError::RangeNotContained)
    );
}

#[test]
fn vm_address_space_map_reservation_publishes_recipe_on_commit() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );

    let reservation = map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree));
    assert_eq!(aspace.lookup(UserVirtAddr(0x4000)), None);
    assert_eq!(aspace.stats(), AddressSpaceStats::default());

    let commit = reservation.commit().expect("map commit");

    assert_eq!(commit.changed_pages, 2);
    assert_eq!(aspace.lookup(UserVirtAddr(0x4000)), Some(entry.clone()));
    assert_eq!(aspace.lookup(UserVirtAddr(0x5fff)), Some(entry));
    assert_eq!(aspace.lookup(UserVirtAddr(0x6000)), None);
    assert_eq!(
        aspace.stats(),
        AddressSpaceStats {
            recipe_count: 1,
            vm_size: 2 * USER_PAGE_SIZE,
        }
    );
}

#[test]
fn vm_address_space_dropped_map_reservation_rolls_back() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x8000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::None,
    );

    let reservation = map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree));
    drop(reservation);

    assert_eq!(aspace.lookup(UserVirtAddr(0x8000)), None);
    assert_eq!(aspace.stats(), AddressSpaceStats::default());
}

#[test]
fn vm_address_space_rejects_nonfixed_overlap() {
    let aspace = AddressSpace::new();
    let first = VmEntry::new(
        range(0x1000, 2),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(first.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("initial map");

    let overlap = VmEntry::new(
        range(0x2000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );

    assert_eq!(
        map_error(aspace.reserve_map(overlap, MapPlacement::RequireFree)),
        VmMapError::AlreadyMapped
    );
    assert_eq!(aspace.lookup(UserVirtAddr(0x1000)), Some(first));
}

#[test]
fn vm_address_space_fixed_map_replaces_overlap_and_preserves_survivors() {
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(original.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("initial map");

    let replacement = VmEntry::new(
        range(0x2000, 2),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    let commit = map_reserved(aspace.reserve_map(replacement.clone(), MapPlacement::FixedReplace))
        .commit()
        .expect("fixed replace");

    assert_eq!(commit.changed_pages, 4);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)),
        Some(VmEntry::new(
            range(0x1000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing_like(&original.backing, 0),
        ))
    );
    assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), Some(replacement));
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x4000)),
        Some(VmEntry::new(
            range(0x4000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing_like(&original.backing, (3 * USER_PAGE_SIZE) as u64),
        ))
    );
    assert_eq!(
        aspace.stats(),
        AddressSpaceStats {
            recipe_count: 3,
            vm_size: 4 * USER_PAGE_SIZE,
        }
    );
}

#[test]
fn vm_address_space_unmap_splits_recipe_and_updates_stats() {
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
        .commit()
        .expect("initial map");

    let commit = aspace.unmap(range(0x2000, 2)).expect("unmap commit");

    assert_eq!(commit.changed_pages, 2);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
        range(0x1000, 1)
    );
    assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), None);
    assert_eq!(aspace.lookup(UserVirtAddr(0x3000)), None);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x4000)).expect("right").range,
        range(0x4000, 1)
    );
    assert_eq!(
        aspace.stats(),
        AddressSpaceStats {
            recipe_count: 2,
            vm_size: 2 * USER_PAGE_SIZE,
        }
    );
}

#[test]
fn vm_address_space_protect_rewrites_only_declared_range() {
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 3),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
        .commit()
        .expect("initial map");

    let commit = aspace
        .protect(range(0x2000, 1), Prot::READ)
        .expect("protect commit");

    assert_eq!(commit.changed_pages, 1);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)).expect("left").prot,
        Prot::READ_WRITE
    );
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x2000)).expect("target").prot,
        Prot::READ
    );
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x3000)).expect("right").prot,
        Prot::READ_WRITE
    );
    assert_eq!(
        aspace.protect(range(0x8000, 1), Prot::READ),
        Err(VmMapError::MissingMapping)
    );
}

#[test]
fn vm_address_space_finds_first_gap_inside_search_window() {
    let aspace = AddressSpace::new();
    let first = VmEntry::new(
        range(0x1000, 2),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    let second = VmEntry::new(
        range(0x5000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(first, MapPlacement::RequireFree))
        .commit()
        .expect("first map");
    map_reserved(aspace.reserve_map(second, MapPlacement::RequireFree))
        .commit()
        .expect("second map");

    assert_eq!(
        aspace.find_free_range(range(0x1000, 6), 2),
        Some(range(0x3000, 2))
    );
    assert_eq!(aspace.find_free_range(range(0x1000, 6), 3), None);
}

#[test]
fn vm_address_space_lists_recipes_overlapping_declared_range_in_order() {
    let aspace = AddressSpace::new();
    let left = VmEntry::new(
        range(0x1000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    let right = VmEntry::new(
        range(0x3000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(left.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("left map");
    map_reserved(aspace.reserve_map(right.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("right map");

    assert_eq!(
        aspace.recipes_overlapping(range(0x1000, 3)),
        alloc::vec![left, right]
    );
}

#[test]
fn vm_checks_require_fault_recipe_matches_resolve_fault_outcome() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let fault = VmFault::new(UserVirtAddr(0x4008), AccessMode::Read);

    let checked = super::checks::require_fault_recipe(&aspace, fault).expect("checked fault");
    let resolved = aspace.resolve_fault(fault).expect("resolved fault");

    assert_eq!(checked, resolved);
    assert_eq!(checked.page_range, range(0x4000, 1));
    assert_eq!(checked.entry, entry);
}

#[test]
fn vm_checks_require_fault_recipe_reports_missing_and_permission_errors() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    assert_eq!(
        super::checks::require_fault_recipe(
            &aspace,
            VmFault::new(UserVirtAddr(0x8000), AccessMode::Read),
        ),
        Err(VmFaultError::NoRecipe)
    );
    assert_eq!(
        super::checks::require_fault_recipe(
            &aspace,
            VmFault::new(UserVirtAddr(0x4000), AccessMode::Write),
        ),
        Err(VmFaultError::ProtectionViolation)
    );
}

#[test]
fn vm_checks_require_fault_publication_rejects_stale_recipe_and_page() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x2000, 1),
        Prot::READ,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x2000), AccessMode::Read))
        .expect("fault resolves");

    assert_eq!(
        super::checks::require_fault_publication(
            &aspace,
            &outcome,
            crate::page_backed::PageIndex::new(99),
        ),
        Err(VmFaultError::StaleRecipe)
    );
    assert_eq!(
        super::checks::require_fault_publication(
            &aspace,
            &outcome,
            crate::page_backed::PageIndex::new(0),
        ),
        Ok(entry)
    );

    aspace
        .protect(range(0x2000, 1), Prot::NONE)
        .expect("stale permission");
    assert_eq!(
        super::checks::require_fault_publication(
            &aspace,
            &outcome,
            crate::page_backed::PageIndex::new(0),
        ),
        Err(VmFaultError::StaleRecipe)
    );

    aspace.unmap(range(0x2000, 1)).expect("stale recipe");
    assert_eq!(
        super::checks::require_fault_publication(
            &aspace,
            &outcome,
            crate::page_backed::PageIndex::new(0),
        ),
        Err(VmFaultError::StaleRecipe)
    );
}

#[test]
fn vm_checks_require_map_admission_preserves_placement_rules() {
    let aspace = AddressSpace::new();
    let first = VmEntry::new(
        range(0x1000, 2),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(first, MapPlacement::RequireFree))
        .commit()
        .expect("seed");

    let overlap = VmEntry::new(
        range(0x2000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );

    assert_eq!(
        super::checks::require_map_admission(&aspace, &overlap, MapPlacement::RequireFree),
        Err(VmMapError::AlreadyMapped)
    );
    assert_eq!(
        super::checks::require_map_admission(&aspace, &overlap, MapPlacement::FixedReplace),
        Ok(())
    );
}

#[test]
fn vm_checks_require_disjoint_remap_rejects_overlap_and_size_mismatch() {
    assert_eq!(
        super::checks::require_disjoint_remap(range(0x1000, 2), range(0x2000, 2)),
        Err(VmMapError::InvalidRange)
    );
    assert_eq!(
        super::checks::require_disjoint_remap(range(0x1000, 2), range(0x8000, 1)),
        Err(VmMapError::InvalidRange)
    );
    assert_eq!(
        super::checks::require_disjoint_remap(range(0x1000, 2), range(0x8000, 2)),
        Ok(())
    );
}

#[test]
fn vm_project_empty_address_space_has_empty_mapping_projection() {
    let aspace = AddressSpace::new();

    assert_eq!(
        project::project_address_space(&aspace),
        project::AddressSpaceProjection {
            stats: AddressSpaceStats::default(),
            mappings: alloc::vec![],
        }
    );
}

#[test]
fn vm_project_address_space_lists_mappings_in_start_order_and_hides_caps() {
    let aspace = AddressSpace::new();
    let high_page_backing = page_backing(2 * USER_PAGE_SIZE as u64);
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x9000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            high_page_backing,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("high map");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::None,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("low map");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x5000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("middle map");

    let before_stats = aspace.stats();
    let before_recipes = aspace.recipes_snapshot();
    let projection = project::project_address_space(&aspace);

    assert_eq!(aspace.stats(), before_stats);
    assert_eq!(aspace.recipes_snapshot(), before_recipes);
    assert_eq!(
        projection,
        project::AddressSpaceProjection {
            stats: AddressSpaceStats {
                recipe_count: 3,
                vm_size: 3 * USER_PAGE_SIZE,
            },
            mappings: alloc::vec![
                project::VmMappingProjection {
                    range: range(0x1000, 1),
                    prot: Prot::READ,
                    flags: VmEntryFlags::PRIVATE,
                    backing: project::VmBackingProjection::None,
                },
                project::VmMappingProjection {
                    range: range(0x5000, 1),
                    prot: Prot::READ,
                    flags: VmEntryFlags::PRIVATE,
                    backing: project::VmBackingProjection::PrivateAnon,
                },
                project::VmMappingProjection {
                    range: range(0x9000, 1),
                    prot: Prot::READ_WRITE,
                    flags: VmEntryFlags::SHARED,
                    backing: project::VmBackingProjection::Page {
                        offset: 2 * USER_PAGE_SIZE as u64,
                    },
                },
            ],
        }
    );
}

#[test]
fn vm_fault_resolution_requires_authoritative_recipe_and_permissions() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x4008), AccessMode::Read))
        .expect("read fault resolves");

    assert_eq!(outcome.page_range, range(0x4000, 1));
    assert_eq!(outcome.entry, entry);
    assert!(!outcome.pmap_materialization_deferred);
    assert_eq!(
        aspace.resolve_fault(VmFault::new(UserVirtAddr(0x4008), AccessMode::Write)),
        Err(VmFaultError::ProtectionViolation)
    );
    assert_eq!(
        aspace.resolve_fault(VmFault::new(UserVirtAddr(0x8000), AccessMode::Read)),
        Err(VmFaultError::NoRecipe)
    );
}

#[test]
fn vm_fault_resolution_waits_behind_overlapping_writer() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x1000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let _writer = acquired(
        aspace
            .range_lock()
            .acquire(range(0x1000, 1), LockMode::ExclusiveWriter),
    );

    assert_eq!(
        aspace.resolve_fault(VmFault::new(UserVirtAddr(0x1000), AccessMode::Read)),
        Err(VmFaultError::WouldBlock)
    );
}

#[test]
fn vm_fault_materializes_pagebacked_anon_page_from_recipe_offset() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x2000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(USER_PAGE_SIZE as u64),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x3000), AccessMode::Write))
        .expect("fault resolves");
    let materialized = outcome
        .materialize_pagebacked_anon()
        .expect("pagebacked materialization");

    assert_eq!(
        materialized.page_index,
        crate::page_backed::PageIndex::new(2)
    );
    assert!(materialized.page.newly_installed);
    assert!(materialized.page.dirty);
}

#[test]
fn address_space_cap_maps_cap_backed_page_container() {
    setup_host_substrate();
    let aspace = AddressSpace::new_cap().expect("address space cap");
    let pc = crate::page_backed::PageContainer::new_cap(
        crate::page_backed::PageContainerKind::Anon {
            swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
        },
        8,
    )
    .expect("page container cap");
    let range = range(0x41_0000, 1);

    let outcome = aspace
        .map_script(VmMapRequest::fixed(
            range,
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBacking::Page {
                pc: pc.clone(),
                offset: USER_PAGE_SIZE as u64,
            },
        ))
        .expect("map cap-backed page container");

    assert_eq!(outcome.range, range);
    let entry = aspace.lookup(range.start()).expect("mapped recipe");
    assert!(matches!(
        entry.backing,
        VmBacking::Page { offset, .. } if offset == USER_PAGE_SIZE as u64
    ));
}

#[test]
fn vm_fault_materialization_rejects_non_pagebacked_recipe() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x4000), AccessMode::Read))
        .expect("fault resolves");

    assert_eq!(
        outcome.materialize_pagebacked_anon().map(|_| ()),
        Err(VmFaultError::BackingMismatch)
    );
}

#[test]
fn vm_pmap_publish_revalidates_recipe_before_install() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x2000, 1),
        Prot::READ,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x2000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked_anon().expect("materialize");

    aspace.unmap(range(0x2000, 1)).expect("stale recipe");

    assert_eq!(
        aspace.publish_fault_materialization(outcome, materialized),
        Err(VmFaultError::StaleRecipe)
    );
    assert_eq!(aspace.pmap().lookup(UserPage(2)), None);
}

#[test]
fn vm_pmap_publish_installs_materialized_mapping_then_unmap_shoots_down() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x3000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x3000), AccessMode::Write))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked_anon().expect("materialize");
    let ppn = materialized.page.ppn;

    let publish = aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish");

    assert_eq!(publish.page, UserPage(3));
    assert_eq!(
        aspace.pmap().lookup(UserPage(3)),
        Some(PmapMappingSnapshot {
            ppn,
            prot: Prot::READ_WRITE,
        })
    );
    assert_eq!(aspace.pmap().stats().mapped_pages, 1);

    aspace.unmap(range(0x3000, 1)).expect("unmap");

    assert_eq!(aspace.pmap().lookup(UserPage(3)), None);
    assert_eq!(
        aspace.pmap().stats(),
        PmapStats {
            mapped_pages: 0,
            reservations: 1,
            commits: 1,
            rollbacks: 0,
            shootdowns: 1,
        }
    );
}

#[test]
fn vm_pmap_duplicate_publish_converges_on_existing_mapping() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0xa000, 1),
        Prot::READ,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0xa000), AccessMode::Read))
        .expect("fault resolves");
    let first = outcome.materialize_pagebacked_anon().expect("first page");
    let ppn = first.page.ppn;
    let second = outcome.materialize_pagebacked_anon().expect("second page");

    aspace
        .publish_fault_materialization(outcome.clone(), first)
        .expect("first publish");
    aspace
        .publish_fault_materialization(outcome, second)
        .expect("duplicate publish");

    assert_eq!(
        aspace.pmap().lookup(UserPage(10)),
        Some(PmapMappingSnapshot {
            ppn,
            prot: Prot::READ,
        })
    );
    assert_eq!(
        aspace.pmap().stats(),
        PmapStats {
            mapped_pages: 1,
            reservations: 1,
            commits: 1,
            rollbacks: 0,
            shootdowns: 0,
        }
    );
}

#[test]
fn vm_pmap_reserve_failure_is_reported_without_shadow_mapping() {
    let _guard = COUNTING_PMAP_TEST_LOCK.lock().expect("counting test lock");
    setup_host_substrate();
    reset_counting_pmap();
    fail_counting_reserve(PmapError::Exhausted);
    let aspace =
        AddressSpace::new_for_platform::<CountingPmap>().expect("counting pmap address space");
    let entry = VmEntry::new(
        range(0xb000, 1),
        Prot::READ,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0xb000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked_anon().expect("materialize");

    assert_eq!(
        aspace.publish_fault_materialization(outcome, materialized),
        Err(VmFaultError::Pmap(VmPmapError::Pmap(PmapError::Exhausted)))
    );
    assert_eq!(aspace.pmap().lookup(UserPage(11)), None);
    assert_eq!(aspace.pmap().stats().mapped_pages, 0);
    assert_eq!(counting_pmap_counters().reserves, 1);
}

#[test]
fn address_space_cap_drop_tears_down_pmap_before_destroying_root() {
    let _guard = COUNTING_PMAP_TEST_LOCK.lock().expect("counting test lock");
    setup_host_substrate();
    reset_counting_pmap();
    {
        let aspace =
            AddressSpace::new_cap_for_platform::<CountingPmap>().expect("address space cap");
        let entry = VmEntry::new(
            range(0xc000, 1),
            Prot::READ,
            VmEntryFlags::SHARED,
            page_backing(0),
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map");
        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(0xc000), AccessMode::Read))
            .expect("fault resolves");
        let materialized = outcome.materialize_pagebacked_anon().expect("materialize");
        aspace
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }

    wait_for_counting_pmap_counters(CountingPmapCounters {
        creates: 1,
        destroys: 1,
        reserves: 1,
        commits: 1,
        unmaps: 1,
        shoots: 1,
        last_asid: Some(Asid(1)),
    });
}

#[test]
fn vm_pmap_publish_uses_reserve_commit_sequence() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x9000, 1),
        Prot::READ,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x9000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked_anon().expect("materialize");

    aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish");

    assert_eq!(
        aspace.pmap().stats(),
        PmapStats {
            mapped_pages: 1,
            reservations: 1,
            commits: 1,
            rollbacks: 0,
            shootdowns: 0,
        }
    );
}

#[test]
fn vm_pmap_protect_tears_down_for_refault_not_in_place_retag() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x5000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x5000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked_anon().expect("materialize");
    aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish");

    aspace
        .protect(range(0x5000, 1), Prot::READ)
        .expect("protect");

    assert_eq!(aspace.pmap().lookup(UserPage(5)), None);
    assert_eq!(aspace.pmap().stats().shootdowns, 1);
}

#[test]
fn vm_map_script_places_nonfixed_mapping_in_first_recipe_gap() {
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed left");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x4000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed right");

    let outcome = aspace
        .map_script(VmMapRequest::anywhere(
            range(0x1000, 5),
            2,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("map into gap");

    assert_eq!(outcome.range, range(0x2000, 2));
    assert_eq!(outcome.commit.changed_pages, 2);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x2000)).expect("mapped").prot,
        Prot::READ_WRITE
    );
    assert_eq!(
        aspace.map_script(VmMapRequest::anywhere(
            range(0x1000, 5),
            2,
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        )),
        Err(VmMapError::NoFreeRange)
    );
}

#[test]
fn vm_map_script_fixed_replace_uses_declared_range() {
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 3),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(original.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("seed");

    let replacement = VmMapRequest::fixed(
        range(0x2000, 1),
        MapPlacement::FixedReplace,
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    let outcome = aspace.map_script(replacement).expect("fixed replace");

    assert_eq!(outcome.range, range(0x2000, 1));
    assert_eq!(outcome.commit.changed_pages, 2);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
        range(0x1000, 1)
    );
    assert_eq!(
        aspace
            .lookup(UserVirtAddr(0x2000))
            .expect("replacement")
            .prot,
        Prot::READ
    );
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x3000)).expect("right").range,
        range(0x3000, 1)
    );
}

#[test]
fn vm_remap_script_moves_disjoint_range_and_preserves_source_survivors() {
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(original.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("seed");

    let outcome = aspace
        .remap_script(VmRemapRequest::new(range(0x2000, 2), range(0x8000, 2)))
        .expect("remap");

    assert_eq!(outcome.old_range, range(0x2000, 2));
    assert_eq!(outcome.new_range, range(0x8000, 2));
    assert_eq!(outcome.commit.changed_pages, 4);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
        range(0x1000, 1)
    );
    assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), None);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x4000)).expect("right").range,
        range(0x4000, 1)
    );
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x8000)),
        Some(VmEntry::new(
            range(0x8000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing_like(&original.backing, USER_PAGE_SIZE as u64),
        ))
    );
}

#[test]
fn vm_remap_script_rejects_overlapping_or_occupied_destination() {
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 4),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x8000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed dest");

    assert_eq!(
        aspace.remap_script(VmRemapRequest::new(range(0x1000, 2), range(0x2000, 2))),
        Err(VmMapError::InvalidRange)
    );
    assert_eq!(
        aspace.remap_script(VmRemapRequest::new(range(0x1000, 1), range(0x8000, 1))),
        Err(VmMapError::AlreadyMapped)
    );
}
