//! Wall-clock timekeeping above the HAL monotonic clock.
//!
//! HAL `TimeIf` remains monotonic-only. CLOCK_REALTIME is represented
//! as `TimeIf::read_ns() + realtime_offset_ns`; setters replace that
//! offset and bump a generation counter that timer consumers can
//! observe.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use tx_hal::TimeIf;

pub const DEFAULT_REALTIME_EPOCH_BASE_NS: u64 = 1_779_494_400_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WallClockError {
    Range,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VvarSnapshot {
    pub cycle_last: u64,
    pub mask: u64,
    pub mult: u64,
    pub shift: u64,
    pub realtime_sec: u64,
    pub realtime_nsec_shifted: u64,
    pub monotonic_sec: u64,
    pub monotonic_nsec_shifted: u64,
}

pub struct WallClock {
    realtime_offset_ns: AtomicI64,
    generation: AtomicU64,
    mult: AtomicU64,
    shift: AtomicU64,
    mask: AtomicU64,
}

impl WallClock {
    pub const fn new(default_offset_ns: i64) -> Self {
        Self {
            realtime_offset_ns: AtomicI64::new(default_offset_ns),
            generation: AtomicU64::new(0),
            mult: AtomicU64::new(1),
            shift: AtomicU64::new(0),
            mask: AtomicU64::new(!0),
        }
    }

    pub fn monotonic_now_ns<P: TimeIf>(&self) -> u64 {
        P::read_ns()
    }

    pub fn realtime_now_ns<P: TimeIf>(&self) -> u64 {
        add_signed_ns(P::read_ns(), self.realtime_offset_ns())
    }

    pub fn set_realtime_ns<P: TimeIf>(&self, realtime_ns: u64) -> Result<u64, WallClockError> {
        let mono_ns = P::read_ns();
        let offset = (realtime_ns as i128) - (mono_ns as i128);
        if offset < i64::MIN as i128 || offset > i64::MAX as i128 {
            return Err(WallClockError::Range);
        }
        self.realtime_offset_ns
            .store(offset as i64, Ordering::Release);
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.publish_vvar::<P>();
        let _ = crate::timerfd::timerfd_clock_was_set(generation);
        Ok(generation)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn realtime_offset_ns(&self) -> i64 {
        self.realtime_offset_ns.load(Ordering::Acquire)
    }

    pub fn set_clock_params(&self, mult: u64, shift: u64, mask: u64) {
        self.mult.store(mult, Ordering::Release);
        self.shift.store(shift, Ordering::Release);
        self.mask.store(mask, Ordering::Release);
    }

    pub fn monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> u64 {
        add_signed_ns(realtime_ns, self.realtime_offset_ns().saturating_neg())
    }

    pub fn snapshot_for_vvar<P: TimeIf>(&self) -> VvarSnapshot {
        let cycle_last = read_cycle_counter();
        let monotonic_ns = P::read_ns();
        let realtime_ns = add_signed_ns(monotonic_ns, self.realtime_offset_ns());
        let shift = self.shift.load(Ordering::Acquire);
        let (monotonic_sec, monotonic_nsec) = split_ns(monotonic_ns);
        let (realtime_sec, realtime_nsec) = split_ns(realtime_ns);
        VvarSnapshot {
            cycle_last,
            mask: self.mask.load(Ordering::Acquire),
            mult: self.mult.load(Ordering::Acquire),
            shift,
            realtime_sec,
            realtime_nsec_shifted: realtime_nsec << shift,
            monotonic_sec,
            monotonic_nsec_shifted: monotonic_nsec << shift,
        }
    }

    pub fn publish_vvar<P: TimeIf>(&self) {
        if crate::vdso::vdso_available() {
            crate::vdso::vvar_page().update_from_snapshot(self.snapshot_for_vvar::<P>());
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reset_for_test(&self) {
        self.realtime_offset_ns
            .store(DEFAULT_REALTIME_EPOCH_BASE_NS as i64, Ordering::Release);
        self.generation.store(0, Ordering::Release);
        self.mult.store(1, Ordering::Release);
        self.shift.store(0, Ordering::Release);
        self.mask.store(!0, Ordering::Release);
    }
}

static WALL_CLOCK: WallClock = WallClock::new(DEFAULT_REALTIME_EPOCH_BASE_NS as i64);

pub fn monotonic_now_ns<P: TimeIf>() -> u64 {
    WALL_CLOCK.monotonic_now_ns::<P>()
}

pub fn realtime_now_ns<P: TimeIf>() -> u64 {
    WALL_CLOCK.realtime_now_ns::<P>()
}

pub fn set_realtime_ns<P: TimeIf>(realtime_ns: u64) -> Result<u64, WallClockError> {
    WALL_CLOCK.set_realtime_ns::<P>(realtime_ns)
}

pub fn generation() -> u64 {
    WALL_CLOCK.generation()
}

pub fn realtime_offset_ns() -> i64 {
    WALL_CLOCK.realtime_offset_ns()
}

pub fn set_clock_params(mult: u64, shift: u64, mask: u64) {
    WALL_CLOCK.set_clock_params(mult, shift, mask);
}

pub fn monotonic_deadline_from_realtime_ns(realtime_ns: u64) -> u64 {
    WALL_CLOCK.monotonic_deadline_from_realtime_ns(realtime_ns)
}

pub fn snapshot_for_vvar<P: TimeIf>() -> VvarSnapshot {
    WALL_CLOCK.snapshot_for_vvar::<P>()
}

pub fn publish_vvar<P: TimeIf>() {
    WALL_CLOCK.publish_vvar::<P>();
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    WALL_CLOCK.reset_for_test();
}

fn add_signed_ns(base: u64, delta: i64) -> u64 {
    let value = (base as i128) + (delta as i128);
    if value <= 0 {
        0
    } else if value > u64::MAX as i128 {
        u64::MAX
    } else {
        value as u64
    }
}

fn split_ns(ns: u64) -> (u64, u64) {
    (ns / 1_000_000_000, ns % 1_000_000_000)
}

#[cfg(target_arch = "riscv64")]
fn read_cycle_counter() -> u64 {
    let now: u64;
    unsafe {
        core::arch::asm!("rdtime {t}", t = out(reg) now, options(nomem, nostack));
    }
    now
}

#[cfg(not(target_arch = "riscv64"))]
fn read_cycle_counter() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTime;

    static TEST_NS: AtomicU64 = AtomicU64::new(5_000_000_000);

    impl TimeIf for TestTime {
        fn read_ns() -> u64 {
            TEST_NS.fetch_add(1, Ordering::Relaxed)
        }

        fn set_deadline_ns(_deadline: u64) {}

        fn cancel_deadline() {}

        fn frequency_hz() -> u64 {
            1_000_000_000
        }
    }

    #[test]
    fn realtime_is_monotonic_plus_offset_and_set_bumps_generation() {
        let clock = WallClock::new(DEFAULT_REALTIME_EPOCH_BASE_NS as i64);
        TEST_NS.store(5_000_000_000, Ordering::Relaxed);

        assert!(clock.realtime_now_ns::<TestTime>() >= DEFAULT_REALTIME_EPOCH_BASE_NS);
        let before_gen = clock.generation();
        clock
            .set_realtime_ns::<TestTime>(1_800_000_000_000_000_000)
            .expect("set realtime");

        assert_eq!(clock.generation(), before_gen + 1);
        assert!(clock.realtime_now_ns::<TestTime>() >= 1_800_000_000_000_000_000);
        assert!(clock.monotonic_now_ns::<TestTime>() < 6_000_000_000);
    }
}
