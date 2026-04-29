use tx_substrate::epoch;
use tx_substrate::zone::{self, Zone, ZoneError};

#[derive(Debug, Eq, PartialEq)]
struct Object {
    id: u32,
}

#[test]
fn dropped_reservation_rolls_back_without_publishing_slot() {
    let zone = Zone::<Object, 1>::new();

    {
        let reservation = zone::reserve(&zone).expect("first reservation");
        assert_eq!(reservation.slot_index(), 0);
        assert!(matches!(zone.reserve(), Err(ZoneError::Full)));
    }

    let reservation = zone.reserve().expect("rolled back slot is reusable");
    let cap = zone::sign(reservation, Object { id: 7 });
    let guard = epoch::guard();
    let ident = cap.ident_ref(&guard).expect("signed slot is observable");

    assert_eq!(ident.id, 7);
}

#[test]
fn weak_upgrade_is_generation_checked_and_guard_scoped() {
    let zone = Zone::<Object, 2>::new();
    let cap = zone.sign(zone.reserve().expect("reservation"), Object { id: 11 });
    let weak = cap.downgrade();
    let guard = epoch::guard();

    let ident = weak.upgrade(&guard).expect("weak observes live identity");

    assert_eq!(ident.id, 11);
    assert_eq!(ident.slot_index(), cap.slot_index());
    assert_eq!(ident.generation(), cap.generation());
}

#[test]
fn epoch_domain_collects_deferred_work_only_after_guards_exit() {
    let domain = epoch::Domain::new();
    let guard = domain.guard();

    domain.defer_retired();

    assert_eq!(domain.active_guards(), 1);
    assert_eq!(domain.collect(), 0);
    drop(guard);
    assert_eq!(domain.collect(), 1);
    assert_eq!(domain.deferred_count(), 0);
}
