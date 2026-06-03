use super::*;
use crate::page_backed::PageContainer;
use crate::test_support::EPOCH_TEST_LOCK;
use alloc::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::thread_local;
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};

mod execution_scripts;
mod fault_materialization;
mod observation;
mod range_locks;
mod script_async;
mod user_access;

static COUNTING_PMAP_TEST_LOCK: Mutex<()> = Mutex::new(());
static COUNTING_PMAP_STATE: LazyLock<Mutex<CountingPmapState>> =
    LazyLock::new(|| Mutex::new(CountingPmapState::new()));
thread_local! {
    static VM_TEST_EPOCH_GUARD: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
        const { std::cell::RefCell::new(None) };
}

struct CountingPmap;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CountingPmapCounters {
    creates: usize,
    destroys: usize,
    activates: usize,
    reserves: usize,
    commits: usize,
    unmaps: usize,
    shoots: usize,
    shoot_batches: usize,
    shoot_invalidations: usize,
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
        let _ = tx_test_support::drain_once_unbounded();
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

    fn activate_pmap(root: &PmapRoot) -> Result<(), PmapError> {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.activates += 1;
        state.counters.last_asid = Some(root.asid());
        Ok(())
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
        state.counters.shoot_batches += 1;
        state.counters.shoot_invalidations += 1;
        state.counters.last_asid = Some(asid);
    }

    fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        let mut state = COUNTING_PMAP_STATE.lock().expect("counting pmap lock");
        state.counters.shoots += invalidations.len();
        state.counters.shoot_batches += 1;
        state.counters.shoot_invalidations += invalidations.len();
        state.counters.last_asid = Some(asid);
    }
}

fn setup_host_substrate() {
    VM_TEST_EPOCH_GUARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
        }
    });
    tx_test_support::init_host();
    crate::zones::register_all().expect("kernel zones");
    match crate::vm::adapter::step_engine::page_allocator::claim_zero_frame() {
        Ok(_)
        | Err(crate::vm::adapter::step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for VM tests: {error:?}"),
    }
    tx_test_support::drain_to_quiescence();
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
        .expect("page container cap")
        .into(),
        offset,
    }
}

fn page_backing_like_entry(entry: &VmEntry, offset: u64) -> VmBacking {
    let VmBacking::Page { pc, .. } = entry.backing() else {
        panic!("expected page backing");
    };
    VmBacking::Page { pc, offset }
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
        MapReserveResult::Blocked(_) => panic!("expected map reservation"),
        MapReserveResult::Err(error) => panic!("unexpected map reserve error: {error:?}"),
    }
}

fn map_error(result: MapReserveResult<'_>) -> VmMapError {
    match result {
        MapReserveResult::Reserved(_) => panic!("expected map reserve error"),
        MapReserveResult::Blocked(_) => panic!("expected map reserve error, got block"),
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
fn vm_range_guard_drop_releases_reservation() {
    let lock = RangeLock::new();
    let first = range(0x1000, 1);

    {
        let writer = acquired(lock.acquire_step_rich(first, LockMode::ExclusiveWriter));
        would_block(lock.acquire_step_rich(first, LockMode::Materializer));
        drop(writer);
    }

    let materializer = acquired(lock.acquire_step_rich(first, LockMode::Materializer));
    drop(materializer);
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
            page_backing_like_entry(&entry, 10),
        )
    );
    assert_eq!(
        rewrite.after.expect("right survivor"),
        VmEntry::new(
            range(0x4000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            page_backing_like_entry(&entry, 10 + (3 * USER_PAGE_SIZE) as u64),
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
fn vm_entry_split_without_private_pages_reuses_owner_bundle() {
    let entry = VmEntry::new(
        range(0x1000, 3),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    let (pc, _) = entry.page_backing().expect("expected page backing");
    let retain_before = pc.retain_count();
    let owner_count_before = entry.debug_owner_strong_count();

    let rewrite = entry
        .split_for_protect(range(0x2000, 1), Prot::READ)
        .expect("target is inside entry");

    let before = rewrite.before.as_ref().expect("before");
    let target = rewrite.target.as_ref().expect("target");
    let after = rewrite.after.as_ref().expect("after");
    let expected_owner_count = owner_count_before + 3;
    assert_eq!(entry.debug_owner_strong_count(), expected_owner_count);
    assert_eq!(before.debug_owner_strong_count(), expected_owner_count);
    assert_eq!(target.debug_owner_strong_count(), expected_owner_count);
    assert_eq!(after.debug_owner_strong_count(), expected_owner_count);
    assert_eq!(
        pc.retain_count(),
        retain_before,
        "split rewrites should copy hot metadata without retaining the heavy PageContainer cap"
    );
}

#[test]
fn vm_entry_split_with_private_pages_keeps_isolated_owner_bundles() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("private map");
    let entry = aspace.lookup(UserVirtAddr(0x1000)).expect("mapped entry");
    let original_private = entry.private_identity().expect("private set");
    let owner_count_before = entry.debug_owner_strong_count();

    let rewrite = entry
        .split_for_protect(range(0x2000, 1), Prot::READ)
        .expect("target is inside entry");

    let before = rewrite.before.as_ref().expect("before");
    let target = rewrite.target.as_ref().expect("target");
    let after = rewrite.after.as_ref().expect("after");
    assert_eq!(
        entry.debug_owner_strong_count(),
        owner_count_before,
        "private splits must not share the parent owner bundle"
    );
    assert_eq!(before.debug_owner_strong_count(), 1);
    assert_eq!(target.debug_owner_strong_count(), 1);
    assert_eq!(after.debug_owner_strong_count(), 1);
    assert_ne!(before.private_identity(), Some(original_private));
    assert_eq!(target.private_identity(), None);
    assert_ne!(after.private_identity(), Some(original_private));
}

#[test]
fn vm_entry_split_with_private_page_backing_does_not_retain_page_cap() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("private map");
    let entry = aspace.lookup(UserVirtAddr(0x1000)).expect("mapped entry");
    let (pc, _) = entry.page_backing().expect("expected page backing");
    let retain_before = pc.retain_count();

    let rewrite = entry
        .split_for_protect(range(0x2000, 1), Prot::READ)
        .expect("target is inside entry");

    assert!(rewrite.before.is_some());
    assert!(rewrite.target.is_some());
    assert!(rewrite.after.is_some());
    assert_eq!(
        pc.retain_count(),
        retain_before,
        "private page-backed splits should not retain the heavy PageContainer cap for each replacement"
    );
}

#[test]
fn vm_entry_clone_does_not_retain_page_backing_cap() {
    let entry = VmEntry::new(
        range(0x1000, 1),
        Prot::READ,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    let (pc, _) = entry.page_backing().expect("expected page backing");
    let retain_before = pc.retain_count();

    let cloned = entry.clone();

    assert_eq!(
        pc.retain_count(),
        retain_before,
        "VmEntry clone should copy hot metadata without retaining the heavy PageContainer cap"
    );
    drop(cloned);
    assert_eq!(pc.retain_count(), retain_before);
}

#[test]
fn vm_entry_clone_does_not_retain_private_page_set_cap() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x1000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("private map");
    let mapped = aspace.lookup(UserVirtAddr(0x1000)).expect("mapped entry");
    let private = mapped.private().expect("private set");
    let retain_before = private.retain_count();

    let cloned = mapped.clone();

    assert_eq!(
        private.retain_count(),
        retain_before,
        "VmEntry clone should copy hot metadata without retaining the heavy PrivatePageSet cap"
    );
    drop(cloned);
    assert_eq!(private.retain_count(), retain_before);
}

#[test]
fn vm_entry_clone_shares_one_owner_handle_for_heavy_caps() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x1000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("private page-backed map");
    let mapped = aspace.lookup(UserVirtAddr(0x1000)).expect("mapped entry");
    let (pc, _) = mapped.page_backing().expect("expected page backing");
    let private = mapped.private().expect("private set");
    let pc_retain_before = pc.retain_count();
    let private_retain_before = private.retain_count();
    let owner_count_before = mapped.debug_owner_strong_count();

    let cloned = mapped.clone();

    assert_eq!(
        mapped.debug_owner_strong_count(),
        owner_count_before + 1,
        "VmEntry clone should add one shared owner-bundle reference instead of cloning each heavy capability slot"
    );
    assert_eq!(cloned.debug_owner_strong_count(), owner_count_before + 1);
    assert_eq!(pc.retain_count(), pc_retain_before);
    assert_eq!(private.retain_count(), private_retain_before);
    drop(cloned);
    assert_eq!(mapped.debug_owner_strong_count(), owner_count_before);
    assert_eq!(pc.retain_count(), pc_retain_before);
    assert_eq!(private.retain_count(), private_retain_before);
}

#[test]
fn vm_address_space_map_reservation_publishes_recipe_on_commit() {
    setup_host_substrate();
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
fn vm_recipe_reclaim_defers_old_root_destruction_until_vm_drain() {
    setup_host_substrate();
    crate::vm::reset_debug_phase_totals();
    while crate::vm::structure::drain_deferred_recipe_reclaims(usize::MAX) != 0 {}

    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x4000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("initial map");
    let before = crate::vm::structure::recipe_debug_totals();

    for _ in 0..8 {
        let _ = tx_test_support::drain_once_unbounded();
        if crate::vm::structure::deferred_recipe_reclaim_len_for_test() != 0 {
            break;
        }
    }

    assert_eq!(
        crate::vm::structure::deferred_recipe_reclaim_len_for_test(),
        1,
        "EBR callback should enqueue the old recipe root instead of dropping it inline"
    );
    let after_ebr = crate::vm::structure::recipe_debug_totals();
    assert_eq!(
        after_ebr.deferred_reclaim_enqueued,
        before.deferred_reclaim_enqueued + 1
    );
    assert_eq!(
        after_ebr.deferred_reclaim_drained,
        before.deferred_reclaim_drained
    );
    assert_eq!(
        after_ebr.deferred_reclaim_inline_fallback,
        before.deferred_reclaim_inline_fallback
    );

    assert_eq!(crate::vm::drain_deferred_recipe_reclaims(1), 1);
    assert_eq!(
        crate::vm::structure::deferred_recipe_reclaim_len_for_test(),
        0
    );
    let after_drain = crate::vm::structure::recipe_debug_totals();
    assert_eq!(
        after_drain.deferred_reclaim_drained,
        after_ebr.deferred_reclaim_drained + 1
    );
}

#[test]
fn vm_address_space_dropped_map_reservation_rolls_back() {
    setup_host_substrate();
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
    setup_host_substrate();
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
fn vm_recipe_disjoint_publish_touches_bounded_path() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..2048 {
        let entry = VmEntry::new(
            range(0x1000 + i * 0x4000, 1),
            Prot::NONE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("disjoint map commit");
    }

    assert_eq!(aspace.stats().recipe_count, 2048);
    assert!(
        aspace.recipes.debug_last_publish_touched_entries() <= 64,
        "disjoint immutable recipe publication must path-copy, not clone every existing recipe"
    );
}

#[test]
fn vm_recipe_wide_sparse_unmap_uses_batched_persistent_tree_splice() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..2048 {
        let entry = VmEntry::new(
            range(0x1000 + i * 0x4000, 1),
            Prot::NONE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("disjoint map commit");
    }

    let full_sparse_window = range(0x1000, (2047 * 4) + 1);
    let commit = aspace
        .try_munmap(full_sparse_window)
        .expect("wide sparse unmap");

    assert_eq!(commit.changed_pages, 2048);
    assert_eq!(aspace.stats().recipe_count, 0);
    assert!(
        aspace.recipes.debug_last_publish_touched_entries() <= 64,
        "wide sparse munmap should split/splice the persistent tree, not remove every VMA separately"
    );
}

#[test]
fn vm_recipe_sparse_unmap_splice_preserves_boundary_survivors() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    let entries = [
        VmEntry::new(
            range(0x1000, 4),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        VmEntry::new(
            range(0x7000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        VmEntry::new(
            range(0x9000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        VmEntry::new(
            range(0xc000, 4),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
    ];
    for entry in entries {
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("sparse map commit");
    }

    let commit = aspace
        .try_munmap(range(0x3000, 11))
        .expect("sparse boundary unmap");

    assert_eq!(commit.changed_pages, 6);
    assert_eq!(
        aspace.recipes_snapshot(),
        alloc::vec![
            VmEntry::new(
                range(0x1000, 2),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ),
            VmEntry::new(
                range(0xe000, 2),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ),
        ]
    );
    assert_eq!(
        aspace.stats(),
        AddressSpaceStats {
            recipe_count: 2,
            vm_size: 4 * USER_PAGE_SIZE,
        }
    );
    assert!(
        aspace.recipes.debug_last_publish_touched_entries() <= 64,
        "sparse boundary munmap should splice by persistent subtree metadata"
    );
}

#[test]
fn vm_recipe_mid_vma_protect_uses_single_splice_path() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..2048 {
        let entry = VmEntry::new(
            range(0x1000 + i * 0x4000, 1),
            Prot::NONE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("disjoint map commit");
    }
    let wide = VmEntry::new(
        range(0x4000_0000, 16),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(wide.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("wide map");

    let commit = aspace
        .try_mprotect(range(0x4000_5000, 5), Prot::READ)
        .expect("mid-vma protect");

    assert_eq!(commit.changed_pages, 5);
    let recipes = aspace.recipes_snapshot();
    assert_eq!(recipes.len(), 2051);
    for i in 0..2048 {
        assert_eq!(
            recipes[i],
            VmEntry::new(
                range(0x1000 + i * 0x4000, 1),
                Prot::NONE,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            )
        );
    }
    let split = &recipes[2048..];
    assert_eq!(
        split
            .iter()
            .map(|entry| (entry.range, entry.prot, entry.flags))
            .collect::<alloc::vec::Vec<_>>(),
        alloc::vec![
            (
                range(0x4000_0000, 5),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED
            ),
            (range(0x4000_5000, 5), Prot::READ, VmEntryFlags::SHARED),
            (
                range(0x4000_a000, 6),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED
            ),
        ]
    );
    assert!(matches!(
        split[0].backing_kind(),
        VmEntryBacking::Page { offset: 0 }
    ));
    match split[1].backing_kind() {
        VmEntryBacking::Page { offset } => assert_eq!(offset, 5 * USER_PAGE_SIZE as u64),
        _ => panic!("target backing should stay page-backed"),
    }
    match split[2].backing_kind() {
        VmEntryBacking::Page { offset } => assert_eq!(offset, 10 * USER_PAGE_SIZE as u64),
        _ => panic!("tail backing should stay page-backed"),
    }
    let touched = aspace.recipes.debug_last_publish_touched_entries();
    assert!(
        touched <= 24,
        "mid-VMA mprotect should split/splice once instead of path-copying before/target/after independently; touched={touched}"
    );
}

#[test]
fn vm_recipe_lookup_view_borrows_published_entry() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x6000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("initial map");

    let guard = crate::vm::adapter::step_engine::guard();
    let view = aspace
        .recipes
        .lookup_view(UserVirtAddr(0x7000), &guard)
        .expect("borrowed recipe view");

    assert_eq!(view.range, entry.range);
    assert_eq!(view.prot, entry.prot);
    assert_eq!(view.flags, entry.flags);
    assert_eq!(view.backing, entry.backing_kind());
    assert_eq!(view.ufd_registration, entry.ufd_registration);
    assert!(view.private.is_none());
    assert!(view.page.is_some());
    assert!(view.permits_fault(AccessMode::Read));
}

#[test]
fn vm_recipe_backend_name_matches_cfg() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    #[cfg(tx_vm_recipe_bplus)]
    assert_eq!(aspace.recipes.debug_backend_name(), "bplus");
    #[cfg(not(tx_vm_recipe_bplus))]
    assert_eq!(aspace.recipes.debug_backend_name(), "treap");
}

#[test]
#[cfg(tx_vm_recipe_bplus)]
fn vm_recipe_bplus_leaf_split_preserves_lookup_order() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..40 {
        let entry = VmEntry::new(
            range(0x10_0000 + i * 0x4000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map commit");
    }

    assert_eq!(aspace.stats().recipe_count, 40);
    assert!(aspace.recipes.debug_last_publish_leaf_splits() > 0);
    for i in 0..40 {
        let addr = UserVirtAddr(0x10_0000 + i * 0x4000);
        assert_eq!(aspace.lookup(addr).expect("mapped").range, range(addr.0, 1));
    }
    assert_eq!(aspace.recipes_snapshot().len(), 40);
}

#[test]
#[cfg(tx_vm_recipe_bplus)]
fn vm_recipe_bplus_spanning_unmap_trims_boundary_leaves() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..40 {
        let entry = VmEntry::new(
            range(0x20_0000 + i * 0x2000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map commit");
    }

    let commit = aspace
        .try_munmap(range(0x20_0000 + 8 * 0x2000, 24 * 2 - 1))
        .expect("spanning unmap");

    assert_eq!(commit.changed_pages, 24);
    assert_eq!(aspace.stats().recipe_count, 16);
    let snapshot = aspace.recipes_snapshot();
    assert_eq!(snapshot.len(), 16);
    assert_eq!(snapshot[0].range, range(0x20_0000, 1));
    assert_eq!(snapshot[7].range, range(0x20_0000 + 7 * 0x2000, 1));
    assert_eq!(snapshot[8].range, range(0x20_0000 + 32 * 0x2000, 1));
    assert_eq!(snapshot[15].range, range(0x20_0000 + 39 * 0x2000, 1));
}

#[test]
#[cfg(tx_vm_recipe_bplus)]
fn vm_recipe_bplus_exact_boundary_unmap_preserves_neighbors() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..24 {
        let entry = VmEntry::new(
            range(0x30_0000 + i * 2 * USER_PAGE_SIZE, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map commit");
    }

    let commit = aspace
        .try_munmap(range(0x30_0000 + 8 * 2 * USER_PAGE_SIZE, 8 * 2 - 1))
        .expect("exact-boundary unmap");

    assert_eq!(commit.changed_pages, 8);
    let snapshot = aspace.recipes_snapshot();
    assert_eq!(snapshot.len(), 16);
    assert_eq!(
        snapshot[7].range,
        range(0x30_0000 + 7 * 2 * USER_PAGE_SIZE, 1)
    );
    assert_eq!(
        snapshot[8].range,
        range(0x30_0000 + 16 * 2 * USER_PAGE_SIZE, 1)
    );
}

#[test]
#[cfg(tx_vm_recipe_bplus)]
fn vm_recipe_bplus_whole_leaf_covering_unmap_drops_middle_leaf() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..48 {
        let entry = VmEntry::new(
            range(0x40_0000 + i * 2 * USER_PAGE_SIZE, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map commit");
    }

    let commit = aspace
        .try_munmap(range(0x40_0000 + 16 * 2 * USER_PAGE_SIZE, 16 * 2 - 1))
        .expect("whole-leaf unmap");

    assert_eq!(commit.changed_pages, 16);
    let snapshot = aspace.recipes_snapshot();
    assert_eq!(snapshot.len(), 32);
    assert_eq!(
        snapshot[15].range,
        range(0x40_0000 + 15 * 2 * USER_PAGE_SIZE, 1)
    );
    assert_eq!(
        snapshot[16].range,
        range(0x40_0000 + 32 * 2 * USER_PAGE_SIZE, 1)
    );
}

#[test]
#[cfg(tx_vm_recipe_bplus)]
fn vm_recipe_bplus_map_coalesces_across_leaf_boundary() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    for i in 0..15 {
        let entry = VmEntry::new(
            range(0x50_0000 + i * 2 * USER_PAGE_SIZE, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("prefix sparse map");
    }
    let left = VmEntry::new(
        range(0x60_0000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(left, MapPlacement::RequireFree))
        .commit()
        .expect("left map");
    let right = VmEntry::new(
        range(0x60_2000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(right, MapPlacement::RequireFree))
        .commit()
        .expect("right map");

    let bridge = VmEntry::new(
        range(0x60_1000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(bridge, MapPlacement::RequireFree))
        .commit()
        .expect("bridge map");

    assert_eq!(
        aspace
            .lookup(UserVirtAddr(0x60_0000))
            .expect("merged")
            .range,
        range(0x60_0000, 3)
    );
}

#[test]
fn vm_address_space_fixed_map_replaces_overlap_and_preserves_survivors() {
    setup_host_substrate();
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
            page_backing_like_entry(&original, 0),
        ))
    );
    assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), Some(replacement));
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x4000)),
        Some(VmEntry::new(
            range(0x4000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing_like_entry(&original, (3 * USER_PAGE_SIZE) as u64),
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
fn vm_recipe_snapshot_reader_survives_split_rewrite_publication() {
    setup_host_substrate();
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

    let before = {
        let guard = crate::vm::adapter::step_engine::guard();
        aspace.recipes.snapshot_reader(&guard)
    };

    let replacement = VmEntry::new(
        range(0x2000, 2),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(replacement.clone(), MapPlacement::FixedReplace))
        .commit()
        .expect("fixed replace");

    assert_eq!(before.snapshot(), alloc::vec![original.clone()]);
    assert_eq!(
        before.lookup(UserVirtAddr(0x2000)).expect("old view").prot,
        Prot::READ_WRITE
    );
    assert_eq!(
        aspace.recipes_snapshot(),
        alloc::vec![
            VmEntry::new(
                range(0x1000, 1),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED,
                page_backing_like_entry(&original, 0),
            ),
            replacement,
            VmEntry::new(
                range(0x4000, 1),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED,
                page_backing_like_entry(&original, (3 * USER_PAGE_SIZE) as u64),
            ),
        ]
    );
}

#[test]
fn vm_address_space_unmap_splits_recipe_and_updates_stats() {
    setup_host_substrate();
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

    let commit = aspace.try_munmap(range(0x2000, 2)).expect("unmap commit");

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
fn vm_address_space_unmap_sparse_large_range_tears_down_resident_pmap_entries() {
    let _guard = COUNTING_PMAP_TEST_LOCK.lock().expect("counting test lock");
    setup_host_substrate();
    reset_counting_pmap();
    let aspace =
        AddressSpace::new_for_platform::<CountingPmap>().expect("counting pmap address space");
    let mapped = range(0x1000_0000, 65_536);
    let entry = VmEntry::new(
        mapped,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("large map");

    for fault_addr in [0x1000_0000, 0x1200_0000, 0x1fff_f000] {
        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(fault_addr), AccessMode::Write))
            .expect("fault resolves");
        let materialized = outcome.materialize_pagebacked().expect("materialize");
        aspace
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }
    assert_eq!(aspace.pmap().stats().mapped_pages, 3);

    let commit = aspace.try_munmap(mapped).expect("unmap large sparse range");

    assert_eq!(commit.changed_pages, 65_536);
    assert_eq!(aspace.pmap().stats().mapped_pages, 0);
    assert_eq!(
        counting_pmap_counters(),
        CountingPmapCounters {
            creates: 1,
            reserves: 3,
            commits: 3,
            unmaps: 3,
            shoots: 3,
            shoot_batches: 1,
            shoot_invalidations: 3,
            last_asid: Some(Asid(1)),
            ..CountingPmapCounters::default()
        }
    );
}

#[test]
fn vm_address_space_protect_rewrites_only_declared_range() {
    setup_host_substrate();
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
        .try_mprotect(range(0x2000, 1), Prot::READ)
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
        aspace.try_mprotect(range(0x8000, 1), Prot::READ),
        Err(VmMapError::MissingMapping)
    );
}

#[test]
fn vm_private_anon_prot_none_allocates_private_set_only_when_made_writable() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 4),
        Prot::NONE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
        .commit()
        .expect("prot-none map");

    assert!(
        aspace
            .lookup(UserVirtAddr(0x1000))
            .expect("prot-none entry")
            .private()
            .is_none(),
        "PROT_NONE private anon mappings should not allocate an empty private set"
    );

    let commit = aspace
        .try_mprotect(range(0x2000, 2), Prot::READ_WRITE)
        .expect("make stack body writable");

    assert_eq!(commit.changed_pages, 2);
    assert!(
        aspace
            .lookup(UserVirtAddr(0x1000))
            .expect("left guard")
            .private()
            .is_none(),
        "left guard remains set-less"
    );
    assert!(
        aspace
            .lookup(UserVirtAddr(0x2000))
            .expect("writable body")
            .private()
            .is_some(),
        "writable private anon body gets a private set before write faults"
    );
    assert!(
        aspace
            .lookup(UserVirtAddr(0x4000))
            .expect("right guard")
            .private()
            .is_none(),
        "right guard remains set-less"
    );
}

#[test]
fn vm_address_space_finds_first_gap_inside_search_window() {
    setup_host_substrate();
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
fn vm_try_mmap_anywhere_continues_from_recent_success_hint() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let window = range(0x1000, 8);
    let request = || {
        VmMapRequest::anywhere(
            window,
            1,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        )
    };

    let first = aspace.try_mmap(request()).expect("first mmap");
    let second = aspace.try_mmap(request()).expect("second mmap");
    aspace.try_munmap(first.range).expect("free first mapping");
    let third = aspace.try_mmap(request()).expect("third mmap");

    assert_eq!(first.range, range(0x1000, 1));
    assert_eq!(second.range, range(0x2000, 1));
    assert_eq!(
        third.range,
        range(0x3000, 1),
        "non-fixed mmap should continue forward instead of rescanning low freed gaps first"
    );
}

#[test]
fn vm_address_space_lists_recipes_overlapping_declared_range_in_order() {
    setup_host_substrate();
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
    setup_host_substrate();
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
    setup_host_substrate();
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
    setup_host_substrate();
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
    let mut wrong_page = outcome
        .materialize_pagebacked_anon()
        .expect("wrong page materialization");
    wrong_page.page_index = crate::page_backed::PageIndex::new(99);
    let materialized = outcome
        .materialize_pagebacked_anon()
        .expect("materialization");

    assert_eq!(
        super::checks::require_fault_publication(&aspace, &outcome, &wrong_page),
        Err(VmFaultError::StaleRecipe)
    );
    assert_eq!(
        super::checks::require_fault_publication(&aspace, &outcome, &materialized),
        Ok(())
    );

    aspace
        .try_mprotect(range(0x2000, 1), Prot::NONE)
        .expect("stale permission");
    assert_eq!(
        super::checks::require_fault_publication(&aspace, &outcome, &materialized),
        Err(VmFaultError::StaleRecipe)
    );

    aspace.try_munmap(range(0x2000, 1)).expect("stale recipe");
    assert_eq!(
        super::checks::require_fault_publication(&aspace, &outcome, &materialized),
        Err(VmFaultError::StaleRecipe)
    );
}

#[test]
fn vm_checks_require_fault_publication_does_not_clone_recipe_entry() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x3000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x3000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome
        .materialize_pagebacked()
        .expect("private anon materialization");
    let owner_count_before = outcome.entry.debug_owner_strong_count();

    super::checks::require_fault_publication(&aspace, &outcome, &materialized)
        .expect("publication still valid");

    assert_eq!(
        outcome.entry.debug_owner_strong_count(),
        owner_count_before,
        "publication validation should borrow the current recipe instead of cloning its owner bundle"
    );
}

#[test]
fn vm_checks_require_fault_publication_rejects_replaced_private_set_identity() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x4000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("map");

    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x4000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome
        .materialize_pagebacked()
        .expect("private anon materialization");

    aspace
        .try_mmap(VmMapRequest::fixed(
            range(0x4000, 1),
            MapPlacement::FixedReplace,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("replace with semantically equivalent private mapping");

    assert_eq!(
        super::checks::require_fault_publication(&aspace, &outcome, &materialized),
        Err(VmFaultError::StaleRecipe)
    );
}

#[test]
fn vm_checks_require_map_admission_preserves_placement_rules() {
    setup_host_substrate();
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
fn vm_checks_require_disjoint_remap_rejects_overlap_only() {
    assert_eq!(
        super::checks::require_disjoint_remap(range(0x1000, 2), range(0x2000, 2)),
        Err(VmMapError::InvalidRange)
    );
    assert_eq!(
        super::checks::require_disjoint_remap(range(0x1000, 2), range(0x8000, 1)),
        Ok(())
    );
    assert_eq!(
        super::checks::require_disjoint_remap(range(0x1000, 2), range(0x8000, 2)),
        Ok(())
    );
}

#[test]
fn vm_project_empty_address_space_has_empty_mapping_projection() {
    setup_host_substrate();
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
    setup_host_substrate();
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
        .try_mmap(VmMapRequest::fixed(
            range,
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBacking::Page {
                pc: pc.clone().into(),
                offset: USER_PAGE_SIZE as u64,
            },
        ))
        .expect("map cap-backed page container");

    assert_eq!(outcome.range, range);
    let entry = aspace.lookup(range.start()).expect("mapped recipe");
    assert!(matches!(
        entry.backing_kind(),
        VmEntryBacking::Page { offset } if offset == USER_PAGE_SIZE as u64
    ));
}

#[test]
fn vm_pmap_publish_revalidates_recipe_before_install() {
    setup_host_substrate();
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

    aspace.try_munmap(range(0x2000, 1)).expect("stale recipe");

    assert_eq!(
        aspace.publish_fault_materialization(outcome, materialized),
        Err(VmFaultError::StaleRecipe)
    );
    assert_eq!(aspace.pmap().lookup(UserPage(2)), None);
}

#[test]
fn vm_pmap_publish_installs_materialized_mapping_then_unmap_shoots_down() {
    setup_host_substrate();
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

    aspace.try_munmap(range(0x3000, 1)).expect("unmap");

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
    setup_host_substrate();
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
fn address_space_cap_drop_destroys_dead_pmap_without_per_page_unmap() {
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
        assert_eq!(aspace.pmap().stats().mapped_pages, 1);
    }

    wait_for_counting_pmap_counters(CountingPmapCounters {
        creates: 1,
        destroys: 1,
        activates: 0,
        reserves: 1,
        commits: 1,
        unmaps: 0,
        shoots: 0,
        shoot_batches: 0,
        shoot_invalidations: 0,
        last_asid: None,
    });
}

#[test]
fn address_space_activate_pmap_installs_owned_root() {
    let _guard = COUNTING_PMAP_TEST_LOCK.lock().expect("counting test lock");
    setup_host_substrate();
    reset_counting_pmap();

    let aspace =
        AddressSpace::new_for_platform::<CountingPmap>().expect("counting pmap address space");
    aspace.activate_pmap().expect("activate pmap");

    assert_eq!(
        counting_pmap_counters(),
        CountingPmapCounters {
            creates: 1,
            activates: 1,
            last_asid: Some(Asid(1)),
            ..CountingPmapCounters::default()
        }
    );
}

#[test]
fn vm_pmap_publish_uses_reserve_commit_sequence() {
    setup_host_substrate();
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
    setup_host_substrate();
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
        .try_mprotect(range(0x5000, 1), Prot::READ)
        .expect("protect");

    assert_eq!(aspace.pmap().lookup(UserPage(5)), None);
    assert_eq!(aspace.pmap().stats().shootdowns, 1);
}

// =====================================================================
// Part A — `reserve_user_range_for_access` and PrivateAnon
// cross-call consistency. The eager-walk path must publish each
// materialised page through the pmap so subsequent `copy_*_user`
// calls find the same frame; without that publish, a brk-backed
// `PrivateAnon` page would re-zero on every read and writes would
// be invisible to subsequent reads. See `vm/user_access.rs`'s
// module header for the design.
// =====================================================================

#[test]
fn vm_aspace_reserve_user_range_for_access_publishes_private_anon_pages() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x10000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("anon map");

    let outcome =
        aspace.reserve_user_range_for_access(range(0x10000, 3), crate::vm::UserAccessKind::Write);
    assert!(matches!(
        outcome,
        crate::vm::adapter::step_engine::StepOutcome::Done(())
    ));
    for page in [UserPage(0x10), UserPage(0x11), UserPage(0x12)] {
        let snap = aspace
            .pmap()
            .lookup(page)
            .expect("page published after reserve");
        assert!(snap.prot.write, "write reserve publishes writable mapping");
    }
}

#[test]
fn vm_aspace_reserve_user_range_for_access_skips_already_published() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x20000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("anon map");

    let _ =
        aspace.reserve_user_range_for_access(range(0x20000, 2), crate::vm::UserAccessKind::Write);
    let mapped_after_first = aspace.pmap().stats().mapped_pages;
    assert_eq!(mapped_after_first, 2);
    let commits_after_first = aspace.pmap().stats().commits;

    // Second call: every page already permits Write; no new commits.
    let _ =
        aspace.reserve_user_range_for_access(range(0x20000, 2), crate::vm::UserAccessKind::Write);
    assert_eq!(aspace.pmap().stats().mapped_pages, 2);
    assert_eq!(aspace.pmap().stats().commits, commits_after_first);
}

#[test]
fn vm_aspace_reserve_user_range_for_access_upgrades_readonly_private_cow_page() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let user_va = 0x26000usize;
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(user_va, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("anon map");

    let read_reserve =
        aspace.reserve_user_range_for_access(range(user_va, 1), crate::vm::UserAccessKind::Read);
    assert!(matches!(
        read_reserve,
        crate::vm::adapter::step_engine::StepOutcome::Done(())
    ));
    assert_eq!(
        aspace
            .pmap()
            .lookup(UserPage(user_va / crate::vm::USER_PAGE_SIZE))
            .expect("read reserve published page")
            .prot,
        Prot::READ
    );

    let write_reserve =
        aspace.reserve_user_range_for_access(range(user_va, 1), crate::vm::UserAccessKind::Write);
    assert!(matches!(
        write_reserve,
        crate::vm::adapter::step_engine::StepOutcome::Done(())
    ));
    assert_eq!(
        aspace
            .pmap()
            .lookup(UserPage(user_va / crate::vm::USER_PAGE_SIZE))
            .expect("write reserve upgraded page")
            .prot,
        Prot::READ_WRITE
    );
}

#[test]
fn vm_aspace_reserve_user_range_for_access_returns_efault_for_unmapped() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    let outcome =
        aspace.reserve_user_range_for_access(range(0x30000, 1), crate::vm::UserAccessKind::Read);
    assert_eq!(
        outcome,
        crate::vm::adapter::step_engine::StepOutcome::err(crate::execution::Errno::EFAULT.into())
    );
    assert_eq!(aspace.pmap().stats().mapped_pages, 0);
}

#[test]
fn vm_aspace_reserve_user_range_for_access_propagates_prot_mismatch_efault() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x40000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("read-only map");

    let outcome =
        aspace.reserve_user_range_for_access(range(0x40000, 1), crate::vm::UserAccessKind::Write);
    assert_eq!(
        outcome,
        crate::vm::adapter::step_engine::StepOutcome::err(crate::execution::Errno::EFAULT.into())
    );
}

#[test]
fn vm_aspace_copy_from_user_consistent_with_prior_copy_to_user_for_private_anon() {
    // Regression test for the PrivateAnon zero-frame consistency bug.
    // Without the pmap-first probe + publish in `resolve_user_page_addr`,
    // each call to `copy_*_user` on an unpublished anon page allocated a
    // fresh zero frame, so `copy_to_user` writes were invisible to a
    // subsequent `copy_from_user` read on the same page.
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let user_va = 0x50_0000usize;
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(user_va, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("anon map");

    let guard = crate::vm::adapter::step_engine::guard();
    let dst = tx_hal::UserPtr::<u8>::new(user_va);
    let payload: alloc::vec::Vec<u8> = (0u8..200).collect();
    match aspace.copy_to_user(dst, &payload, &guard) {
        crate::vm::adapter::step_engine::StepOutcome::Done(n) => assert_eq!(n, payload.len()),
        other => panic!("copy_to_user expected Done, got {other:?}"),
    }

    // The pmap must now have a published mapping for this page —
    // otherwise the readback below would observe a fresh zero frame.
    let user_page = UserVirtAddr(user_va).containing_page();
    assert!(
        aspace.pmap().lookup(user_page).is_some(),
        "copy_to_user must publish the materialised frame"
    );

    let mut readback = alloc::vec![0u8; payload.len()];
    let src = tx_hal::UserPtr::<u8>::new(user_va);
    match aspace.copy_from_user(&mut readback, src, &guard) {
        crate::vm::adapter::step_engine::StepOutcome::Done(n) => assert_eq!(n, payload.len()),
        other => panic!("copy_from_user expected Done, got {other:?}"),
    }
    assert_eq!(readback, payload, "readback must match prior write");
}
