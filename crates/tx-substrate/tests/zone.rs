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
struct AllocationFailureObject;

static ALLOCATION_FAILURE_ZONE: Zone<AllocationFailureObject> = Zone::const_new();

#[derive(Debug)]
struct DropObject;

static DROP_ZONE: Zone<DropObject> = Zone::const_new();
static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    zone::register_zone_for::<DropObject>().expect("drop zone registration");
    let mut caps = Vec::new();
    while !caps
        .iter()
        .any(|cap: &Cap<DropObject>| cap.key().raw() == 0)
    {
        caps.push(zone::sign(DropObject).expect("allocation while seeking raw key zero"));
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
    assert_eq!(DROP_COUNT.load(Ordering::Acquire), 2);
    drop(caps);
}

#[test]
fn generation_max_quarantines_without_reuse() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<DropObject>().expect("drop zone registration");
    let cap = zone::sign(DropObject).expect("allocation");
    let key = cap.key();
    unsafe { zone::testing::force_generation::<DropObject>(key, u16::MAX) };
    drop(cap);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);
    let word =
        zone::testing::slot_word::<DropObject>(key).expect("quarantined slot remains resolvable");
    assert_eq!(word.state(), zone::SlotState::Free);
    assert_eq!(word.generation(), u16::MAX);
    assert!(word.generation_exhausted());

    let replacement = zone::sign(DropObject).expect("replacement allocation");
    assert_ne!(
        replacement.key(),
        key,
        "exhausted slot must not return to allocation"
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

    let selected = [
        slab_representatives[0].1,
        slab_representatives[slab_representatives.len() / 2].1,
        slab_representatives.last().expect("last slab").1,
    ];
    assert_eq!(
        slab_representatives[0].0 & 63,
        slab_representatives[64].0 & 63,
        "65 sequential slab ids must collide in the retired 64-entry cache"
    );
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
