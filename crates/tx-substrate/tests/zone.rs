use tx_substrate::epoch;
use tx_substrate::zone::{
    self, registered_zone_count, Zone, ZoneAllocated, ZoneError, ZoneId, ZoneMaintenanceBudget,
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
fn maintenance_retries_pending_slot_retirement_after_epoch_pool_pressure() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    let guard = epoch::guard();
    let mut caps = Vec::new();
    let mut weaks = Vec::new();

    for id in 0..tx_substrate::epoch::RETIRED_NODE_POOL_CAPACITY {
        let reservation = zone::reserve_for::<Object>().expect("object reservation");
        let cap = zone::sign_for(reservation, Object { id: id as u32 });
        weaks.push(cap.downgrade());
        caps.push(cap);
    }

    for cap in caps.drain(..) {
        drop(cap);
    }

    let stuck_reservation = zone::reserve_for::<Object>().expect("stuck reservation");
    let stuck = zone::sign_for(stuck_reservation, Object { id: 9999 });
    let stuck_weak = stuck.downgrade();
    drop(stuck);

    assert!(
        stuck_weak.observe(&guard).is_none(),
        "pending-retire slot must already be non-upgradeable"
    );

    drop(guard);
    let first = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });
    let second = zone::maintenance_tick(ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    });

    assert!(
        first.retried_pending_slots + second.retried_pending_slots >= 1,
        "maintenance should retry at least one pending retirement"
    );

    let post_guard = epoch::guard();
    assert!(
        stuck_weak.observe(&post_guard).is_none(),
        "reclaimed slot must stay dead to the old weak handle"
    );

    let still_dead = weaks
        .iter()
        .filter(|weak| weak.observe(&post_guard).is_none())
        .count();
    assert!(
        still_dead >= tx_substrate::epoch::RETIRED_NODE_POOL_CAPACITY,
        "all saturated retirements should remain dead to old weak handles"
    );
}
