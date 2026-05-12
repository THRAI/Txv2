use tx_substrate::epoch;
use tx_substrate::zone::{
    self, registered_zone_count, CoLocatedEntity, OperationalCapExt, OperationalRefExt, Zone,
    ZoneAllocated, ZoneError, ZoneId, ZoneMaintenanceBudget, Cap,
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
    guard
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

    let stats = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });
    assert!(stats.empty_slabs.scanned_zones >= 1);
    assert!(stats.empty_slabs.retired_slabs >= 1);

    let after = zone::lookup(LARGE_ZONE.id()).expect("zone info after trim");
    assert_eq!(after.slab_count, 1);
    assert!(after.allocated_slots < before.allocated_slots);
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
    assert!(cpu0.retired_count <= tx_substrate::epoch::RETIRED_NODE_POOL_CAPACITY);

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
    let view = cap
        .upgrade_operational()
        .expect("cap is live after sign");
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
