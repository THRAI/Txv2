use tx_hal::{IrqIf, PercpuIf, SmpIf};
use tx_substrate::epoch;
use tx_substrate::index::{Index, IndexError};

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TestPlatform;

impl PercpuIf for TestPlatform {}
impl IrqIf for TestPlatform {}
impl SmpIf for TestPlatform {}

fn reset_epoch() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    unsafe {
        epoch::testing::reset_for_test();
    }
    epoch::init_on_bsp::<TestPlatform>().expect("epoch init");
    guard
}

#[derive(Debug, Eq, PartialEq)]
struct NonCloneValue {
    number: u32,
}

#[test]
fn duplicate_reservations_fail_and_dropped_reservation_rolls_back() {
    let index = Index::<u32, NonCloneValue, 1>::new();

    {
        let reservation = index.reserve(4).expect("reserve key");
        assert_eq!(reservation.key(), &4);
        assert!(matches!(index.reserve(4), Err(IndexError::Duplicate)));
    }

    let reservation = index.reserve(4).expect("rollback makes key reservable");
    reservation.commit(NonCloneValue { number: 9 });

    assert!(matches!(index.reserve(4), Err(IndexError::Duplicate)));
    assert!(matches!(index.reserve(5), Err(IndexError::Full)));
}

#[test]
fn committed_lookup_is_guard_observed_without_cloning_value() {
    let _epoch = reset_epoch();
    let index = Index::<u32, NonCloneValue, 2>::new();
    index
        .reserve(8)
        .expect("reserve")
        .commit(NonCloneValue { number: 12 });

    let guard = epoch::guard();
    let observed = index.lookup(&8, &guard).expect("committed entry");

    assert_eq!(observed.key(), &8);
    assert_eq!(observed.value().number, 12);
    assert!(index.lookup(&9, &guard).is_none());
}
