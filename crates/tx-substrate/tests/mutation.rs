use tx_hal::{EntropyIf, IrqIf, PercpuIf, SmpIf};
use tx_substrate::epoch;
use tx_substrate::index::Index;
use tx_substrate::mutation::{self, MutationError};

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TestPlatform;

impl PercpuIf for TestPlatform {}
unsafe fn restore_test_local_execution(_saved_state: usize) {}

impl IrqIf for TestPlatform {
    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        unsafe { tx_hal::LocalExecutionGuard::new(0, restore_test_local_execution) }
    }
}
impl EntropyIf for TestPlatform {}
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
fn install_if_absent_commits_only_when_key_is_free() {
    let _epoch = reset_epoch();
    let index = Index::<u32, NonCloneValue, 2>::new();

    mutation::install_if_absent(&index, 1, NonCloneValue { number: 10 }).expect("initial install");

    let err = mutation::install_if_absent(&index, 1, NonCloneValue { number: 20 })
        .expect_err("duplicate install fails");
    assert_eq!(err, MutationError::AlreadyPresent);

    let guard = epoch::guard();
    let observed = index.lookup(&1, &guard).expect("original value remains");
    assert_eq!(observed.value().number, 10);
}

#[test]
fn withdraw_and_swap_move_values_without_clone_bounds() {
    let _epoch = reset_epoch();
    let index = Index::<u32, NonCloneValue, 2>::new();
    mutation::install_if_absent(&index, 2, NonCloneValue { number: 30 }).expect("install");

    let old = mutation::swap(&index, &2, NonCloneValue { number: 31 }).expect("swap");
    assert_eq!(old, NonCloneValue { number: 30 });

    let removed = mutation::withdraw(&index, &2).expect("withdraw");
    assert_eq!(removed, NonCloneValue { number: 31 });

    let guard = epoch::guard();
    assert!(index.lookup(&2, &guard).is_none());
    assert_eq!(
        mutation::withdraw::<_, _, 2>(&index, &2),
        Err(MutationError::Missing)
    );
}
