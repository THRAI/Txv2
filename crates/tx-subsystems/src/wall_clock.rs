//! Type-erased wall-clock bridge for subsystems that are not generic over the
//! concrete HAL platform.
//!
//! `tx-time` remains the single owner of realtime offset, generation, timerfd
//! notification, and VVAR publication.  The kernel installs a platform-bound
//! read function after seeding the canonical timekeeper; filesystem backends
//! use this module without introducing a second wall-clock state machine.

use core::sync::atomic::{AtomicPtr, Ordering};

use tx_services::time::{timekeeper, TimekeeperIf, DEFAULT_REALTIME_EPOCH_BASE_NS};

type RealtimeNowFn = fn() -> u64;

static REALTIME_NOW_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn install_realtime_source(source: RealtimeNowFn) {
    REALTIME_NOW_FN.store(source as *mut (), Ordering::Release);
}

/// Realtime clock for non-platform-generic kernel subsystems.
///
/// The fallback keeps early filesystem construction deterministic until the
/// kernel installs its platform-bound source.
pub fn current_realtime_ns() -> u64 {
    let raw = REALTIME_NOW_FN.load(Ordering::Acquire);
    if raw.is_null() {
        DEFAULT_REALTIME_EPOCH_BASE_NS
    } else {
        // SAFETY: `install_realtime_source` is the sole writer and stores a
        // `RealtimeNowFn` through the inverse pointer cast.
        let source = unsafe { core::mem::transmute::<*mut (), RealtimeNowFn>(raw) };
        source()
    }
}

pub fn current_realtime_sec() -> u64 {
    current_realtime_ns() / 1_000_000_000
}

/// Compatibility bridge used by the final-smp VM timeout path. The conversion
/// itself is platform independent and therefore delegates directly to the
/// canonical `tx-time` timekeeper.
pub fn monotonic_deadline_from_realtime_ns(realtime_ns: u64) -> u64 {
    timekeeper().monotonic_deadline_from_realtime_ns(realtime_ns)
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    REALTIME_NOW_FN.store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_source_drives_filesystem_realtime() {
        fn test_now() -> u64 {
            7_000_000_123
        }

        reset_for_test();
        assert_eq!(current_realtime_ns(), DEFAULT_REALTIME_EPOCH_BASE_NS);
        install_realtime_source(test_now);
        assert_eq!(current_realtime_ns(), 7_000_000_123);
        assert_eq!(current_realtime_sec(), 7);
        reset_for_test();
    }
}
