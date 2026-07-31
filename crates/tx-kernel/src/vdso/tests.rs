use core::sync::atomic::{AtomicU64, Ordering};

use super::*;

use tx_hal::{MonotonicCounterIf, PersistentClockIf};
use tx_services::time::{reset_for_test, timekeeper_clock, ClockRead};

const SEED_REALTIME_NS: u64 = 1_800_000_000_000_000_000;

struct SeedPlatform;

static TEST_MONOTONIC_NS: AtomicU64 = AtomicU64::new(10_000_000_000);

impl MonotonicCounterIf for SeedPlatform {
    fn read_ns() -> u64 {
        TEST_MONOTONIC_NS.fetch_add(1, Ordering::Relaxed)
    }

    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl PersistentClockIf for SeedPlatform {
    fn read_realtime_ns() -> Result<u64, tx_hal::PersistentClockError> {
        Ok(SEED_REALTIME_NS)
    }
}

#[test]
fn boot_seed_helper_uses_persistent_clock_before_publish_path() {
    let _guard = crate::test_serialise::KERNEL_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    TEST_MONOTONIC_NS.store(10_000_000_000, Ordering::Relaxed);

    let generation =
        seed_realtime_from_persistent::<SeedPlatform>().expect("seed persistent realtime");

    assert_eq!(generation, 1);
    assert!(
        timekeeper_clock::<SeedPlatform>().realtime_now_ns() >= SEED_REALTIME_NS,
        "realtime should derive from persistent seed after boot helper"
    );
}
