use core::sync::atomic::{AtomicUsize, Ordering};
use tx_substrate::epoch;
use tx_substrate::zone::{
    self, registered_zone_count, Cap, CoLocatedEntity, OperationalCapExt, OperationalRefExt, Zone,
    ZoneAllocated, ZoneError, ZoneId, ZoneMaintenanceBudget,
};

static ZONE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Eq, PartialEq)]
struct Object {
    id: u32,
}

static TEST_ZONE: Zone<Object> = Zone::const_new();

#[derive(Debug, Eq, PartialEq)]
struct LargeObject {
    bytes: [u8; 1024],
}

static LARGE_ZONE: Zone<LargeObject> = Zone::const_new();

#[derive(Debug)]
struct SingleSlotObject {
    bytes: [u8; 3000],
}

static SINGLE_SLOT_ZONE: Zone<SingleSlotObject> = Zone::const_new();

#[derive(Debug)]
struct SmpSingleSlotObject {
    bytes: [u8; 3000],
}

static SMP_SINGLE_SLOT_ZONE: Zone<SmpSingleSlotObject> = Zone::const_new();

#[derive(Debug)]
struct AllocationFailureObject;

static ALLOCATION_FAILURE_ZONE: Zone<AllocationFailureObject> = Zone::const_new();

#[derive(Debug)]
struct DropObject;

static DROP_ZONE: Zone<DropObject> = Zone::const_new();
static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct ReclaimOnceNode {
    value: u32,
}

impl Drop for ReclaimOnceNode {
    fn drop(&mut self) {
        RECLAIM_ONCE_NODE_DROPS.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct ReclaimOnceObject {
    nodes: std::collections::BTreeMap<u32, ReclaimOnceNode>,
    _single_slot_padding: [u8; 3000],
}

static RECLAIM_ONCE_ZONE: Zone<ReclaimOnceObject> = Zone::const_new();
static RECLAIM_ONCE_NODE_DROPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct GenerationMaxObject;
static GENERATION_MAX_ZONE: Zone<GenerationMaxObject> = Zone::const_new();

#[derive(Debug)]
struct GenerationExhaustedObject;
static GENERATION_EXHAUSTED_ZONE: Zone<GenerationExhaustedObject> = Zone::const_new();

#[derive(Debug)]
struct EmptyRotationObject;
static EMPTY_ROTATION_ZONE: Zone<EmptyRotationObject> = Zone::const_new();

#[derive(Debug)]
struct ZeroKeyObject;
static ZERO_KEY_ZONE: Zone<ZeroKeyObject> = Zone::const_new();

#[derive(Debug)]
struct MixedA;
#[derive(Debug)]
struct MixedB;
static MIXED_A_ZONE: Zone<MixedA> = Zone::const_new();
static MIXED_B_ZONE: Zone<MixedB> = Zone::const_new();
unsafe impl ZoneAllocated for MixedA {
    fn zone() -> &'static Zone<Self> {
        &MIXED_A_ZONE
    }
}
unsafe impl ZoneAllocated for MixedB {
    fn zone() -> &'static Zone<Self> {
        &MIXED_B_ZONE
    }
}

impl Drop for DropObject {
    fn drop(&mut self) {
        DROP_COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

unsafe impl ZoneAllocated for DropObject {
    fn zone() -> &'static Zone<Self> {
        &DROP_ZONE
    }
}

unsafe impl ZoneAllocated for ReclaimOnceObject {
    fn zone() -> &'static Zone<Self> {
        &RECLAIM_ONCE_ZONE
    }
}

unsafe impl ZoneAllocated for GenerationMaxObject {
    fn zone() -> &'static Zone<Self> {
        &GENERATION_MAX_ZONE
    }
}

unsafe impl ZoneAllocated for GenerationExhaustedObject {
    fn zone() -> &'static Zone<Self> {
        &GENERATION_EXHAUSTED_ZONE
    }
}

unsafe impl ZoneAllocated for EmptyRotationObject {
    fn zone() -> &'static Zone<Self> {
        &EMPTY_ROTATION_ZONE
    }
}

unsafe impl ZoneAllocated for ZeroKeyObject {
    fn zone() -> &'static Zone<Self> {
        &ZERO_KEY_ZONE
    }
}

unsafe impl ZoneAllocated for Object {
    fn zone() -> &'static Zone<Self> {
        &TEST_ZONE
    }
}

unsafe impl ZoneAllocated for LargeObject {
    fn zone() -> &'static Zone<Self> {
        &LARGE_ZONE
    }
}

unsafe impl ZoneAllocated for SingleSlotObject {
    fn zone() -> &'static Zone<Self> {
        &SINGLE_SLOT_ZONE
    }
}

unsafe impl ZoneAllocated for SmpSingleSlotObject {
    fn zone() -> &'static Zone<Self> {
        &SMP_SINGLE_SLOT_ZONE
    }
}

unsafe impl ZoneAllocated for AllocationFailureObject {
    fn zone() -> &'static Zone<Self> {
        &ALLOCATION_FAILURE_ZONE
    }
}

impl CoLocatedEntity for Object {}

fn reset_zone_registry() {
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
}

fn reset_zone_and_epoch() -> std::sync::MutexGuard<'static, ()> {
    let guard = ZONE_TEST_LOCK.lock().expect("zone test lock");
    tx_substrate::testing::init_host_for_test_once();
    reset_zone_registry();
    epoch::testing::init_for_test();
    zone::testing::init_for_test(
        4096,
        tx_substrate::page_allocator::testing::direct_map_base_for_test(),
    )
    .expect("test zone runtime init");
    DROP_COUNT.store(0, Ordering::Release);
    guard
}

#[test]
fn slot_lifecycle_is_four_state_and_reclaims_after_grace() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<DropObject>().expect("drop zone registration");
    let cap = zone::sign(DropObject).expect("drop object allocation");
    let key = cap.key();
    assert_eq!(
        zone::testing::slot_word::<DropObject>(key).unwrap().state(),
        zone::SlotState::Live
    );

    drop(cap);
    assert_eq!(
        zone::testing::slot_word::<DropObject>(key).unwrap().state(),
        zone::SlotState::Retiring
    );
    assert_eq!(DROP_COUNT.load(Ordering::Acquire), 0);
    let first = epoch::drain_with_budget(usize::MAX);
    assert_eq!(first.bag_reclaimed, 0);
    let second = epoch::drain_with_budget(usize::MAX);
    assert_eq!(second.bag_reclaimed, 1);
    assert_eq!(
        zone::testing::slot_word::<DropObject>(key).unwrap().state(),
        zone::SlotState::Free
    );
    assert_eq!(DROP_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn stale_reclaim_key_cannot_drop_free_or_reused_btree_occupant() {
    const NODE_COUNT: usize = 24;

    fn value(seed: u32) -> ReclaimOnceObject {
        let nodes = (0..NODE_COUNT as u32)
            .map(|index| {
                (
                    index,
                    ReclaimOnceNode {
                        value: seed + index,
                    },
                )
            })
            .collect();
        ReclaimOnceObject {
            nodes,
            _single_slot_padding: [seed as u8; 3000],
        }
    }

    fn assert_value(cap: &Cap<ReclaimOnceObject>, seed: u32) {
        assert_eq!(cap.nodes.len(), NODE_COUNT);
        for (index, node) in &cap.nodes {
            assert_eq!(node.value, seed + *index);
        }
    }

    let _guard = reset_zone_and_epoch();
    RECLAIM_ONCE_NODE_DROPS.store(0, Ordering::Release);
    zone::register_zone_for::<ReclaimOnceObject>().expect("reclaim-once zone registration");

    let original = zone::sign(value(1000)).expect("original BTreeMap allocation");
    let stale_key = original.key();
    let stale_generation = original.binding_token().generation();
    assert_value(&original, 1000);
    drop(original);

    // First let the real callback complete normally, then replay its exact
    // key+generation token while the physical storage is Free.
    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    assert_eq!(RECLAIM_ONCE_NODE_DROPS.load(Ordering::Acquire), NODE_COUNT);
    unsafe { zone::testing::reclaim_retired_slot_now(stale_key, stale_generation) };
    assert_eq!(
        RECLAIM_ONCE_NODE_DROPS.load(Ordering::Acquire),
        NODE_COUNT,
        "a repeated callback must not destruct uninitialized Free storage"
    );

    // The padded value gives this zone one slot per slab, making reuse of the
    // exact physical SlotKey deterministic.
    let replacement = zone::sign(value(2000)).expect("replacement BTreeMap allocation");
    assert_eq!(replacement.key(), stale_key);
    assert_ne!(replacement.binding_token().generation(), stale_generation);
    assert_value(&replacement, 2000);

    // Retire the replacement, then replay the old generation while the same
    // key is Retiring again. State-only validation would destruct the new
    // BTreeMap here; generation-bearing validation must reject it.
    drop(replacement);
    unsafe { zone::testing::reclaim_retired_slot_now(stale_key, stale_generation) };
    assert_eq!(
        RECLAIM_ONCE_NODE_DROPS.load(Ordering::Acquire),
        NODE_COUNT,
        "an old generation must not destruct the reused Retiring occupant"
    );

    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    assert_eq!(
        RECLAIM_ONCE_NODE_DROPS.load(Ordering::Acquire),
        NODE_COUNT * 2,
        "each BTreeMap node must be dropped exactly once per real occupant"
    );
}

#[test]
fn weak_observation_survives_retirement_until_guard_drop() {
    let _isolation = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");
    let cap = zone::sign(Object { id: 77 }).expect("object allocation");
    let weak = cap.downgrade();
    let guard = epoch::guard();
    let ident = weak.observe(&guard).expect("guarded observation");
    drop(cap);

    assert_eq!(ident.id, 77);
    assert!(ident.to_cap().is_err(), "Retiring blocks new retention");
    let blocked = epoch::drain_with_budget(usize::MAX);
    assert_eq!(blocked.bag_reclaimed, 0);
    drop(ident);
    drop(guard);
    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.bag_reclaimed, 1);
    assert!(weak.observe(&epoch::guard()).is_none());
}

#[test]
fn borrowed_guard_outliving_outer_blocks_zone_reclaim() {
    let _isolation = reset_zone_and_epoch();
    zone::register_zone_for::<DropObject>().expect("drop zone registration");
    let cap = zone::sign(DropObject).expect("drop object allocation");
    let generation = cap.binding_token().generation();
    let outer = epoch::guard();
    let borrowed = epoch::borrow_current_guard().expect("nested epoch guard");
    let ident = cap.ident_ref(&borrowed);

    assert_eq!(epoch::summary().active_guards, 2);
    drop(outer);
    drop(cap);

    for _ in 0..3 {
        let drained = epoch::drain_with_budget(usize::MAX);
        assert_eq!(
            drained.bag_reclaimed, 0,
            "a live borrowed guard must keep its IdentRef storage intact"
        );
    }
    assert_eq!(ident.binding_token().generation(), generation);
    assert_eq!(DROP_COUNT.load(Ordering::Acquire), 0);

    drop(ident);
    drop(borrowed);
    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.bag_reclaimed, 1);
    assert_eq!(DROP_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn mixed_zone_bag_dispatches_typed_reclaim() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<MixedA>().expect("mixed A registration");
    zone::register_zone_for::<MixedB>().expect("mixed B registration");
    let object = zone::sign(MixedA).expect("mixed A allocation");
    let drop_object = zone::sign(MixedB).expect("mixed B allocation");
    let object_key = object.key();
    let drop_key = drop_object.key();
    drop(object);
    drop(drop_object);
    let _ = epoch::drain_with_budget(usize::MAX);
    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.bag_reclaimed, 2);
    assert_eq!(
        zone::testing::slot_word::<MixedA>(object_key)
            .unwrap()
            .state(),
        zone::SlotState::Free
    );
    assert_eq!(
        zone::testing::slot_word::<MixedB>(drop_key)
            .unwrap()
            .state(),
        zone::SlotState::Free
    );
}

#[test]
fn slot_key_zero_is_a_real_intrusive_member() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<ZeroKeyObject>().expect("zero-key zone registration");
    let mut caps: Vec<Cap<ZeroKeyObject>> = Vec::new();
    while !caps
        .iter()
        .any(|cap: &Cap<ZeroKeyObject>| cap.key().raw() == 0)
    {
        caps.push(zone::sign(ZeroKeyObject).expect("allocation while seeking raw key zero"));
        assert!(caps.len() <= 64, "first slab must contain raw key zero");
    }
    let zero_index = caps
        .iter()
        .position(|cap| cap.key().raw() == 0)
        .expect("raw key zero cap");
    let zero = caps.swap_remove(zero_index);
    let other = caps.pop().expect("second intrusive member");
    drop(zero);
    drop(other);
    let _ = epoch::drain_with_budget(usize::MAX);
    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.bag_reclaimed, 2);
    drop(caps);
}

#[test]
fn generation_max_quarantines_without_reuse() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<GenerationMaxObject>().expect("generation-max zone registration");
    let cap = zone::sign(GenerationMaxObject).expect("allocation");
    let key = cap.key();
    unsafe { zone::testing::force_generation::<GenerationMaxObject>(key, u16::MAX) };
    drop(cap);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);
    let word = zone::testing::slot_word::<GenerationMaxObject>(key)
        .expect("quarantined slot remains resolvable");
    assert_eq!(word.state(), zone::SlotState::Free);
    assert_eq!(word.generation(), u16::MAX);
    assert!(word.generation_exhausted());

    let replacement = zone::sign(GenerationMaxObject).expect("replacement allocation");
    assert_ne!(
        replacement.key(),
        key,
        "exhausted slot must not return to allocation"
    );
}

#[test]
fn generation_exhausted_full_slab_is_retired_and_replaced() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<GenerationExhaustedObject>()
        .expect("generation-exhausted zone registration");

    let mut caps: Vec<Cap<GenerationExhaustedObject>> = Vec::new();
    for _ in 0..64 {
        let cap = zone::sign(GenerationExhaustedObject).expect("first slab allocation");
        if let Some(first) = caps.first() {
            assert_eq!(cap.key().slab_id(), first.key().slab_id());
        }
        unsafe {
            zone::testing::force_generation::<GenerationExhaustedObject>(cap.key(), u16::MAX)
        };
        caps.push(cap);
    }

    let old_key = caps[0].key();
    let old_weak = caps[0].downgrade();
    let old_slab_id = old_key.slab_id();
    drop(caps);

    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);

    assert!(
        zone::testing::slot_word::<GenerationExhaustedObject>(old_key).is_none(),
        "a fully generation-exhausted slab must unpublish instead of trapping the zone"
    );
    assert!(
        old_weak.observe(&epoch::guard()).is_none(),
        "old weak handles must not resolve after exhausted slab retirement"
    );

    let replacement =
        zone::sign(GenerationExhaustedObject).expect("replacement after exhausted slab");
    assert_ne!(
        replacement.key().slab_id(),
        old_slab_id,
        "replacement allocation must use a fresh slab id"
    );
}

#[test]
fn empty_slab_reuse_rotates_slot_start_after_maintenance_flush() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<EmptyRotationObject>().expect("empty-rotation zone registration");

    let mut slot_indices = [0usize; 3];
    for slot_index in &mut slot_indices {
        let cap = zone::sign(EmptyRotationObject).expect("allocation");
        *slot_index = cap.key().slot_index();
        drop(cap);

        // Final-cap reclamation is two-phase.  Flush the per-CPU bucket after
        // the slot is free so the next allocation re-enters the central Keg
        // and exercises the empty-slab starting-point policy.
        let _ = epoch::drain_with_budget(usize::MAX);
        let _ = epoch::drain_with_budget(usize::MAX);
        let _ = zone::maintenance_tick(ZoneMaintenanceBudget {
            epoch_reclaim_budget: usize::MAX,
            empty_slab_budget: 0,
        });
    }

    assert_ne!(
        slot_indices[0], slot_indices[1],
        "an empty slab must not restart every reuse at the same hot slot"
    );
    assert_ne!(
        slot_indices[1], slot_indices[2],
        "successive empty-slab cycles should keep spreading slot wear"
    );
}

#[test]
fn single_slot_slab_refill_does_not_allocate_bucket_capacity_of_new_slabs() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<SingleSlotObject>().expect("single-slot zone registration");

    let cap = zone::sign(SingleSlotObject {
        bytes: [0x5a; 3000],
    })
    .expect("single-slot allocation");
    assert_eq!(cap.bytes[0], 0x5a);
    let first_key = cap.key();
    let after_first = zone::lookup(SINGLE_SLOT_ZONE.id()).expect("single-slot info");
    assert_eq!(
        after_first.slab_count, 1,
        "one single-slot allocation must allocate one slab, not a full bucket"
    );
    assert_eq!(after_first.allocated_slots, 1);
    drop(cap);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });

    let replacement = zone::sign(SingleSlotObject {
        bytes: [0xa5; 3000],
    })
    .expect("single-slot replacement allocation");
    assert_eq!(
        replacement.key().slab_id(),
        first_key.slab_id(),
        "non-exhausted single-slot slabs should be reused instead of churned"
    );
}

#[test]
fn smp_single_slot_refill_returns_once_and_defers_slab_frame_reuse_until_grace() {
    const WORKERS: usize = 4;

    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<SmpSingleSlotObject>().expect("SMP single-slot zone registration");
    let phase = std::sync::Barrier::new(WORKERS + 1);
    let drain_lock = std::sync::Mutex::new(());
    let mut claimed_ppns = [None; WORKERS];

    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            let phase = &phase;
            let drain_lock = &drain_lock;
            scope.spawn(move || {
                let mut bucket = zone::ZoneBucket::<SmpSingleSlotObject, 8>::new();
                phase.wait();
                SMP_SINGLE_SLOT_ZONE
                    .refill_bucket(&mut bucket)
                    .expect("concurrent single-slot refill");
                assert_eq!(
                    bucket.len(),
                    1,
                    "a refill may allocate at most one new slab when no reusable slot exists"
                );
                phase.wait();
                phase.wait();

                // The host test hook represents one logical CPU, so serialize
                // the retire-guard portion while retaining concurrent Keg
                // allocation above. Real SMP coverage runs this path with one
                // local retire state per CPU.
                let _drain = drain_lock.lock().expect("serialized host bucket drain");
                SMP_SINGLE_SLOT_ZONE.drain_bucket_to_keg(&mut bucket);
                assert!(bucket.is_empty());
            });
        }

        phase.wait();
        phase.wait();
        let claimed = zone::lookup(SMP_SINGLE_SLOT_ZONE.id()).expect("SMP zone after claims");
        assert_eq!(claimed.slab_count, WORKERS);
        assert_eq!(claimed.allocated_slots, WORKERS);
        assert_eq!(
            zone::testing::slab_backing_ppns(&SMP_SINGLE_SLOT_ZONE, &mut claimed_ppns),
            WORKERS
        );
        claimed_ppns.sort_unstable();
        assert!(
            claimed_ppns.windows(2).all(|pair| pair[0] != pair[1]),
            "four simultaneous claims must own four distinct single-slot frames"
        );
        phase.wait();
    });

    let free_after_claims =
        tx_substrate::page_allocator::free_count().expect("free after SMP claims");
    let after_returns = zone::lookup(SMP_SINGLE_SLOT_ZONE.id()).expect("SMP zone after returns");
    assert_eq!(after_returns.slab_count, 1);
    assert_eq!(after_returns.allocated_slots, 1);
    assert_eq!(
        tx_substrate::page_allocator::free_count().expect("free before slab grace"),
        free_after_claims,
        "logical slab retirement must not make a backing frame reusable before grace"
    );

    let mut retained_ppn = [None; 1];
    assert_eq!(
        zone::testing::slab_backing_ppns(&SMP_SINGLE_SLOT_ZONE, &mut retained_ppn),
        1
    );

    // The retained low-water slab supplies one slot. The other three objects
    // must allocate fresh frames because the three retired frames have not yet
    // crossed an epoch grace period.
    let mut caps = Vec::new();
    for byte in 0..WORKERS as u8 {
        caps.push(
            zone::sign(SmpSingleSlotObject {
                bytes: [byte; 3000],
            })
            .expect("allocation while retired frames await grace"),
        );
    }
    assert_eq!(caps[3].bytes[0], 3);
    let mut live_ppns = [None; WORKERS];
    assert_eq!(
        zone::testing::slab_backing_ppns(&SMP_SINGLE_SLOT_ZONE, &mut live_ppns),
        WORKERS
    );
    assert!(live_ppns.contains(&retained_ppn[0]));
    for retired in claimed_ppns
        .iter()
        .copied()
        .filter(|ppn| *ppn != retained_ppn[0])
    {
        assert!(
            !live_ppns.contains(&retired),
            "a logically retired slab frame was reused before its grace period"
        );
    }

    drop(caps);
    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    let stats = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });
    assert_eq!(stats.empty_slabs.retired_slabs, WORKERS - 1);
    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }

    let cleaned = zone::lookup(SMP_SINGLE_SLOT_ZONE.id()).expect("SMP zone after cleanup");
    assert_eq!(cleaned.slab_count, 1);
    assert_eq!(cleaned.allocated_slots, 1);
    let mut final_ppn = [None; 1];
    assert_eq!(
        zone::testing::slab_backing_ppns(&SMP_SINGLE_SLOT_ZONE, &mut final_ppn),
        1
    );
}

#[test]
fn static_zone_registration_is_idempotent() {
    let _guard = ZONE_TEST_LOCK.lock().expect("zone test lock");
    reset_zone_registry();

    let first = zone::register_zone_for::<Object>().expect("first registration");
    let second = zone::register_zone_for::<Object>().expect("idempotent registration");

    assert_eq!(first.id, second.id);
    assert_eq!(first.id, ZoneId(1));
    assert_eq!(registered_zone_count(), 1);
}

#[test]
fn reserve_requires_zone_runtime_initialization() {
    let _guard = ZONE_TEST_LOCK.lock().expect("zone test lock");
    reset_zone_registry();

    let err = match zone::reserve_for::<Object>() {
        Ok(_) => panic!("reserve before init must fail"),
        Err(err) => err,
    };

    assert_eq!(err, ZoneError::NotInitialized);
}

#[test]
fn reserve_requires_boot_time_registration_after_runtime_init() {
    let _guard = reset_zone_and_epoch();

    let err = match zone::reserve_for::<Object>() {
        Ok(_) => panic!("reserve before zone registration must fail"),
        Err(err) => err,
    };

    assert_eq!(err, ZoneError::NotRegistered);
}

#[test]
fn slab_allocation_rolls_back_committed_frame_when_direct_map_resolution_fails() {
    let _guard = ZONE_TEST_LOCK.lock().expect("zone test lock");
    tx_substrate::testing::init_host_for_test_once();
    reset_zone_registry();
    epoch::testing::init_for_test();

    let held = tx_substrate::page_allocator::reserve_frame(
        tx_substrate::page_allocator::ZeroPolicy::UninitFullOverwrite,
    )
    .expect("reserve sentinel frame")
    .commit();
    zone::testing::init_for_test(4096, usize::MAX).expect("install failing direct map");
    zone::register_zone_for::<AllocationFailureObject>().expect("failure zone registration");
    let before = tx_substrate::page_allocator::free_count().expect("free count before");

    let result = zone::reserve_for::<AllocationFailureObject>();

    assert!(matches!(result, Err(ZoneError::AllocationFailed)));
    assert_eq!(
        tx_substrate::page_allocator::free_count().expect("free count after"),
        before,
        "failed slab allocation must return its committed frame"
    );
    drop(held);
}

#[test]
fn maintenance_tick_retires_surplus_empty_slabs() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<LargeObject>().expect("large zone registration");

    let mut caps = Vec::new();
    for i in 0..16u8 {
        let reservation = zone::reserve_for::<LargeObject>().expect("large object reservation");
        caps.push(zone::sign_for(
            reservation,
            LargeObject { bytes: [i; 1024] },
        ));
    }

    let allocated = zone::lookup(LARGE_ZONE.id()).expect("large zone info");
    assert!(
        allocated.slab_count > 1,
        "test must allocate multiple slabs"
    );

    drop(caps);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);

    let before = zone::lookup(LARGE_ZONE.id()).expect("zone info before trim");
    assert!(
        before.slab_count > 1,
        "reclaimed slots should leave multiple empty slabs"
    );

    let free_before_trim = tx_substrate::page_allocator::free_count().expect("free before trim");
    let stats = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });
    assert!(stats.empty_slabs.scanned_zones >= 1);
    assert!(stats.empty_slabs.retired_slabs >= 1);

    let after = zone::lookup(LARGE_ZONE.id()).expect("zone info after trim");
    assert_eq!(after.slab_count, 1);
    assert!(after.allocated_slots < before.allocated_slots);
    assert_eq!(
        tx_substrate::page_allocator::free_count().expect("free before slab grace"),
        free_before_trim,
        "logical slab retirement must not release frames before grace"
    );
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);
    assert!(
        tx_substrate::page_allocator::free_count().expect("free after slab grace")
            > free_before_trim,
        "intrusive slab callback must release frames after grace"
    );
}

#[test]
fn zone_lookup_is_lock_free_across_cache_collisions() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<LargeObject>().expect("large zone registration");

    let mut caps = Vec::new();
    let mut slab_representatives = Vec::new();
    while slab_representatives.len() <= 64 {
        let value = caps.len() as u8;
        let cap = zone::sign(LargeObject {
            bytes: [value; 1024],
        })
        .expect("large object allocation");
        let slab_id = cap.key().slab_id();
        if !slab_representatives
            .iter()
            .any(|(existing_id, _)| *existing_id == slab_id)
        {
            slab_representatives.push((slab_id, caps.len()));
        }
        caps.push(cap);
        assert!(caps.len() < 2048, "test must reach 65 distinct slabs");
    }

    let mut colliding = None;
    'outer: for (left_pos, (left_id, left_index)) in slab_representatives.iter().enumerate() {
        for (right_id, right_index) in slab_representatives.iter().skip(left_pos + 1) {
            if (left_id & 63) == (right_id & 63) {
                colliding = Some((*left_index, *right_index));
                break 'outer;
            }
        }
    }
    let (collision_a, collision_b) =
        colliding.expect("65 slab ids must contain a directory-index collision");
    let selected = [slab_representatives[0].1, collision_a, collision_b];
    let weaks = selected.map(|index| caps[index].downgrade());

    zone::testing::begin_keg_lock_counting();
    let epoch_guard = epoch::guard();
    for weak in &weaks {
        assert!(weak.observe(&epoch_guard).is_some());
    }
    for index in selected {
        let expected = index as u8;
        assert_eq!(caps[index].bytes[0], expected);
        let clone = caps[index].clone();
        drop(clone);
    }

    assert_eq!(
        zone::testing::finish_keg_lock_counting(),
        0,
        "observe, deref, clone, and non-final drop must not acquire the Keg lock"
    );

    drop(epoch_guard);
    drop(caps);
    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    let _ = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });
    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    assert_eq!(
        zone::lookup(LARGE_ZONE.id())
            .expect("large zone after cleanup")
            .slab_count,
        1,
        "cleanup must leave only the reusable low-water slab"
    );
}

#[test]
fn reserve_is_rejected_after_shutdown_freeze() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    assert_eq!(zone::state(), zone::ZoneRuntimeState::Running);
    zone::freeze_for_shutdown().expect("freeze zone runtime");
    assert_eq!(zone::state(), zone::ZoneRuntimeState::FrozenForShutdown);

    let err = match zone::reserve_for::<Object>() {
        Ok(_) => panic!("reserve after freeze must fail"),
        Err(err) => err,
    };
    assert_eq!(err, ZoneError::FrozenForShutdown);
}

#[test]
fn zone_and_epoch_summary_report_registered_state() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    let epoch_summary = epoch::summary();
    assert!(epoch_summary.initialized);
    assert_eq!(epoch_summary.active_guards, 0);
    assert!(epoch_summary.global_epoch >= 1);

    let cpu0 = epoch::cpu_summary(tx_hal::CpuId(0)).expect("cpu0 summary");
    assert!(cpu0.initialized);
    assert_eq!(cpu0.bag_retired, 0);
    assert_eq!(cpu0.publication_pending, 0);

    let mut zones = [None; 8];
    let written = zone::snapshot(&mut zones);
    assert!(written >= 1);
    let info = zones[..written]
        .iter()
        .flatten()
        .find(|info| info.id == TEST_ZONE.id())
        .expect("test zone snapshot");
    assert!(info.slab_count >= info.empty_slab_count);
}

#[test]
fn colocated_entity_operational_upgrade_matches_identity_cap() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    let reservation = zone::reserve_for::<Object>().expect("object reservation");
    let cap = zone::sign_for(reservation, Object { id: 7 });
    let guard = epoch::guard();
    let ident = cap.ident_ref(&guard);

    let op_from_cap = cap.upgrade_operational().expect("cap operational upgrade");
    let op_from_ref = ident
        .upgrade_operational()
        .expect("ident operational upgrade");

    assert_eq!(op_from_cap.id, 7);
    assert_eq!(op_from_ref.id, 7);
    assert_eq!(op_from_cap.key(), cap.key());
    assert_eq!(op_from_ref.key(), cap.key());
}

// === zone::sign tests ===================================================

#[test]
fn sign_round_trips_a_value() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    let cap: Cap<Object> = zone::sign(Object { id: 42 }).expect("sign succeeds");
    let epoch_guard = epoch::guard();
    let view = cap.upgrade_operational().expect("cap is live after sign");
    assert_eq!(view.id, 42);
    drop(epoch_guard);
}

#[test]
fn sign_propagates_not_registered_error() {
    // reserve_for fails with NotRegistered when the zone is not registered.
    // zone::sign wraps reserve_for, so the same error propagates.
    let _guard = reset_zone_and_epoch();
    // Deliberately skip zone::register_zone_for::<Object>() so the zone is unregistered.
    let err = zone::sign(Object { id: 1 }).expect_err("sign must fail on unregistered zone");
    assert_eq!(err, ZoneError::NotRegistered);
}

#[test]
fn sign_result_matches_reserve_then_sign_for() {
    // Both paths should produce live, operationally-accessible caps with
    // the same value; sign is the canonical one-step form.
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    let one_step: Cap<Object> = zone::sign(Object { id: 10 }).expect("one-step sign");
    let reservation = zone::reserve_for::<Object>().expect("reserve");
    let two_step: Cap<Object> = zone::sign_for(reservation, Object { id: 20 });

    let epoch_guard = epoch::guard();
    let v1 = one_step.upgrade_operational().expect("one_step live");
    let v2 = two_step.upgrade_operational().expect("two_step live");
    assert_eq!(v1.id, 10);
    assert_eq!(v2.id, 20);
    drop(epoch_guard);
}
