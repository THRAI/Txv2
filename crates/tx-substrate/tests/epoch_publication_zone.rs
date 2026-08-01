use tx_substrate::zone::{self, CoLocatedEntity, Zone, ZoneAllocated};
use tx_substrate::{epoch, Published};

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Object;

static TEST_ZONE: Zone<Object> = Zone::const_new();

unsafe impl ZoneAllocated for Object {
    fn zone() -> &'static Zone<Self> {
        &TEST_ZONE
    }
}

impl CoLocatedEntity for Object {}

fn reset_zone_and_epoch() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().expect("test lock");
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    epoch::testing::init_for_test();
    zone::testing::init_for_test(
        4096,
        tx_substrate::page_allocator::testing::direct_map_base_for_test(),
    )
    .expect("test zone runtime init");
    guard
}

#[test]
fn publication_preflight_tags_empty_bags_before_zone_retire() {
    let _guard = reset_zone_and_epoch();
    zone::register_zone_for::<Object>().expect("object zone registration");

    let reservation = zone::reserve_for::<Object>().expect("object reservation");
    let cap = zone::sign_for(reservation, Object);
    let published = Published::try_new(1usize).expect("initial publication");

    published
        .prepare_replace(2)
        .expect("publication reservation")
        .commit();
    drop(cap);

    for _ in 0..3 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    drop(published);
}
