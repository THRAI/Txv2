use tx_substrate::zone::{self, registered_zone_count, Zone, ZoneAllocated, ZoneError, ZoneId};

#[derive(Debug, Eq, PartialEq)]
struct Object {
    id: u32,
}

static TEST_ZONE: Zone<Object> = Zone::const_new();

unsafe impl ZoneAllocated for Object {
    fn zone() -> &'static Zone<Self> {
        &TEST_ZONE
    }
}

fn reset_zone_registry() {
    unsafe {
        zone::testing::reset_for_test();
    }
}

#[test]
fn static_zone_registration_is_idempotent() {
    reset_zone_registry();

    let first = zone::register_zone_for::<Object>().expect("first registration");
    let second = zone::register_zone_for::<Object>().expect("idempotent registration");

    assert_eq!(first.id, second.id);
    assert_eq!(first.id, ZoneId(1));
    assert_eq!(first.allocated_slots, 0);
    assert_eq!(registered_zone_count(), 1);
}

#[test]
fn reserve_requires_zone_runtime_initialization() {
    reset_zone_registry();

    let err = match zone::reserve_for::<Object>() {
        Ok(_) => panic!("reserve before init must fail"),
        Err(err) => err,
    };

    assert_eq!(err, ZoneError::NotInitialized);
}
