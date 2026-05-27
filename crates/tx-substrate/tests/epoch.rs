use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{EntropyIf, IrqIf, PercpuIf, SmpIf};
use tx_substrate::epoch::{self, testing, EpochError};

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static RECLAIM_COUNT: AtomicUsize = AtomicUsize::new(0);

struct TestPlatform;

impl PercpuIf for TestPlatform {}
impl IrqIf for TestPlatform {}
impl EntropyIf for TestPlatform {}
impl SmpIf for TestPlatform {}

unsafe fn count_reclaim(_ptr: *mut u8) {
    RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn reset_epoch() {
    unsafe {
        testing::reset_for_test();
    }
    RECLAIM_COUNT.store(0, Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("epoch init");
}

fn retired_ptr() -> *mut u8 {
    NonNull::<u8>::dangling().as_ptr()
}

#[test]
fn guard_delays_reclaim_until_drop() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();

    let epoch_guard = epoch::guard();
    unsafe {
        testing::retire_raw_for_test(retired_ptr(), count_reclaim).expect("retire");
    }

    let blocked = epoch::try_drain(usize::MAX);
    assert_eq!(blocked.reclaimed, 0);
    assert_eq!(blocked.active_guards, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(epoch_guard);

    let drained = epoch::try_drain(usize::MAX);
    assert_eq!(drained.reclaimed, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn drain_budget_limits_reclaim_work() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();

    for _ in 0..3 {
        unsafe {
            testing::retire_raw_for_test(retired_ptr(), count_reclaim).expect("retire");
        }
    }

    let not_yet = epoch::try_drain(2);
    assert_eq!(not_yet.reclaimed, 0);

    let first = epoch::try_drain(2);
    assert_eq!(first.reclaimed, 2);
    assert_eq!(first.remaining, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 2);

    let second = epoch::try_drain(2);
    assert_eq!(second.reclaimed, 1);
    assert_eq!(second.remaining, 0);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 3);
}

#[test]
fn retired_node_pool_exhaustion_is_reported() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();

    let epoch_guard = epoch::guard();
    for _ in 0..testing::RETIRED_NODE_POOL_CAPACITY {
        unsafe {
            testing::retire_raw_for_test(retired_ptr(), count_reclaim).expect("retire");
        }
    }

    let error = unsafe { testing::retire_raw_for_test(retired_ptr(), count_reclaim) }
        .expect_err("retired node pool must be exhausted");
    assert_eq!(error, EpochError::RetiredNodePoolExhausted);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(epoch_guard);
    let first = epoch::try_drain(usize::MAX);
    let second = epoch::try_drain(usize::MAX);
    assert_eq!(
        first.reclaimed + second.reclaimed,
        testing::RETIRED_NODE_POOL_CAPACITY
    );
}

#[test]
fn retire_requires_initialization_and_non_null_pointer() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    unsafe {
        testing::reset_for_test();
    }

    let not_initialized = unsafe { testing::retire_raw_for_test(retired_ptr(), count_reclaim) }
        .expect_err("retire before init should fail");
    assert_eq!(not_initialized, EpochError::NotInitialized);

    epoch::init_on_bsp::<TestPlatform>().expect("epoch init");
    let null_pointer =
        unsafe { testing::retire_raw_for_test(core::ptr::null_mut(), count_reclaim) }
            .expect_err("null retire pointer should fail");
    assert_eq!(null_pointer, EpochError::NullPointer);
}
