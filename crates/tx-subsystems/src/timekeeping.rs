//! Kernel-wide timekeeping service above HAL monotonic time.
//!
//! HAL `TimeIf` remains the monotonic clock/deadline source. This
//! module owns the Linux-visible civil-time projections and the v1
//! `adjtimex` bookkeeping state. The existing `wall_clock` module
//! remains the low-level realtime-offset/VVAR publisher during the
//! migration; syscall users should enter through this service.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, Ordering};

use tx_hal::TimeIf;

use crate::wall_clock::{self, WallClockError};

pub const TIME_OK: i32 = 0;

const NSEC_PER_SEC: i64 = 1_000_000_000;
const USEC_PER_SEC: i64 = 1_000_000;
const USER_HZ: i64 = 100;
const USER_TICK_USEC: i64 = USEC_PER_SEC / USER_HZ;
const MAX_TAI_OFFSET: i64 = 100_000;

pub const ADJ_OFFSET: u32 = 0x0001;
pub const ADJ_FREQUENCY: u32 = 0x0002;
pub const ADJ_MAXERROR: u32 = 0x0004;
pub const ADJ_ESTERROR: u32 = 0x0008;
pub const ADJ_STATUS: u32 = 0x0010;
pub const ADJ_TIMECONST: u32 = 0x0020;
pub const ADJ_TAI: u32 = 0x0080;
pub const ADJ_SETOFFSET: u32 = 0x0100;
pub const ADJ_MICRO: u32 = 0x1000;
pub const ADJ_NANO: u32 = 0x2000;
pub const ADJ_TICK: u32 = 0x4000;
pub const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
pub const ADJ_OFFSET_SS_READ: u32 = 0xa001;

pub const STA_NANO: u32 = 0x2000;
pub const STA_RONLY: u32 = 0xff00;

const BOOKKEEPING_MODES: u32 =
    ADJ_MAXERROR | ADJ_ESTERROR | ADJ_STATUS | ADJ_TAI | ADJ_MICRO | ADJ_NANO;
const STEP_MODES: u32 = ADJ_SETOFFSET;
const UNSUPPORTED_DISCIPLINE_MODES: u32 = ADJ_OFFSET
    | ADJ_FREQUENCY
    | ADJ_TICK
    | ADJ_TIMECONST
    | ADJ_OFFSET_SINGLESHOT
    | ADJ_OFFSET_SS_READ;
const KNOWN_MODES: u32 = BOOKKEEPING_MODES | STEP_MODES | UNSUPPORTED_DISCIPLINE_MODES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockId {
    Realtime,
    Monotonic,
    ProcessCpuTime,
    ThreadCpuTime,
    MonotonicRaw,
    RealtimeCoarse,
    MonotonicCoarse,
    Boottime,
    Tai,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimekeepingError {
    Invalid,
    Permission,
    Unsupported,
    Range,
}

impl From<WallClockError> for TimekeepingError {
    fn from(value: WallClockError) -> Self {
        match value {
            WallClockError::Range => TimekeepingError::Range,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimexState {
    pub modes: u32,
    pub offset: i64,
    pub freq: i64,
    pub maxerror: i64,
    pub esterror: i64,
    pub status: u32,
    pub constant: i64,
    pub precision: i64,
    pub tolerance: i64,
    pub time_sec: i64,
    pub time_subsec: i64,
    pub tick: i64,
    pub ppsfreq: i64,
    pub jitter: i64,
    pub shift: i32,
    pub stabil: i64,
    pub jitcnt: i64,
    pub calcnt: i64,
    pub errcnt: i64,
    pub stbcnt: i64,
    pub tai: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntervalTimerSpec {
    pub interval_ns: u64,
    pub value_ns: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessIntervalTimer {
    deadline_ns: u64,
    interval_ns: u64,
}

impl ProcessIntervalTimer {
    pub fn snapshot(self, now_ns: u64) -> IntervalTimerSpec {
        IntervalTimerSpec {
            interval_ns: self.interval_ns,
            value_ns: self.deadline_ns.saturating_sub(now_ns),
        }
    }

    pub fn replace(&mut self, now_ns: u64, new_value: IntervalTimerSpec) -> IntervalTimerSpec {
        let old = self.snapshot(now_ns);
        if new_value.value_ns == 0 {
            self.deadline_ns = 0;
            self.interval_ns = 0;
        } else {
            self.deadline_ns = now_ns.saturating_add(new_value.value_ns);
            self.interval_ns = new_value.interval_ns;
        }
        old
    }

    pub fn consume_expired(&mut self, now_ns: u64) -> bool {
        let deadline = self.deadline_ns;
        if deadline == 0 || now_ns < deadline {
            return false;
        }
        if self.interval_ns == 0 {
            self.deadline_ns = 0;
        } else {
            let elapsed = now_ns.saturating_sub(deadline);
            let count = elapsed / self.interval_ns + 1;
            self.deadline_ns = deadline.saturating_add(count.saturating_mul(self.interval_ns));
        }
        true
    }

    pub fn next_deadline_ns(self) -> Option<u64> {
        (self.deadline_ns != 0).then_some(self.deadline_ns)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PosixTimerClock {
    Realtime,
    Monotonic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PosixTimerNotify {
    None,
    Signal { signum: u32, sigval: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PosixTimerSnapshot {
    pub interval_ns: u64,
    pub value_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiredTimerSignal {
    pub signum: u32,
    pub sigval: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PosixTimer {
    clock: PosixTimerClock,
    notify: PosixTimerNotify,
    deadline_mono_ns: u64,
    interval_ns: u64,
    overrun: i32,
}

#[derive(Debug, Default)]
pub struct ProcessPosixTimers {
    next_id: u32,
    timers: BTreeMap<u32, PosixTimer>,
}

impl ProcessPosixTimers {
    pub fn create(
        &mut self,
        clock: PosixTimerClock,
        notify: PosixTimerNotify,
    ) -> Result<u32, TimekeepingError> {
        for _ in 0..=u32::MAX {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if let alloc::collections::btree_map::Entry::Vacant(slot) = self.timers.entry(id) {
                slot.insert(PosixTimer {
                    clock,
                    notify,
                    deadline_mono_ns: 0,
                    interval_ns: 0,
                    overrun: 0,
                });
                return Ok(id);
            }
        }
        Err(TimekeepingError::Range)
    }

    pub fn settime(
        &mut self,
        timer_id: u32,
        deadline_mono_ns: u64,
        interval_ns: u64,
        new_value_ns: u64,
        now_mono_ns: u64,
    ) -> Result<PosixTimerSnapshot, TimekeepingError> {
        let timer = self
            .timers
            .get_mut(&timer_id)
            .ok_or(TimekeepingError::Invalid)?;
        let old = timer.snapshot(now_mono_ns);
        if new_value_ns == 0 {
            timer.deadline_mono_ns = 0;
            timer.interval_ns = 0;
        } else {
            timer.deadline_mono_ns = deadline_mono_ns;
            timer.interval_ns = interval_ns;
        }
        timer.overrun = 0;
        Ok(old)
    }

    pub fn gettime(
        &self,
        timer_id: u32,
        now_mono_ns: u64,
    ) -> Result<PosixTimerSnapshot, TimekeepingError> {
        self.timers
            .get(&timer_id)
            .map(|timer| timer.snapshot(now_mono_ns))
            .ok_or(TimekeepingError::Invalid)
    }

    pub fn getoverrun(&self, timer_id: u32) -> Result<i32, TimekeepingError> {
        self.timers
            .get(&timer_id)
            .map(|timer| timer.overrun)
            .ok_or(TimekeepingError::Invalid)
    }

    pub fn clock(&self, timer_id: u32) -> Result<PosixTimerClock, TimekeepingError> {
        self.timers
            .get(&timer_id)
            .map(|timer| timer.clock)
            .ok_or(TimekeepingError::Invalid)
    }

    pub fn delete(&mut self, timer_id: u32) -> Result<(), TimekeepingError> {
        self.timers
            .remove(&timer_id)
            .map(|_| ())
            .ok_or(TimekeepingError::Invalid)
    }

    pub fn consume_expired(&mut self, now_mono_ns: u64) -> Vec<ExpiredTimerSignal> {
        let mut expired = Vec::new();
        for timer in self.timers.values_mut() {
            let deadline = timer.deadline_mono_ns;
            if deadline == 0 || now_mono_ns < deadline {
                continue;
            }
            let count = if timer.interval_ns == 0 {
                timer.deadline_mono_ns = 0;
                1
            } else {
                let elapsed = now_mono_ns.saturating_sub(deadline);
                let count = elapsed / timer.interval_ns + 1;
                timer.deadline_mono_ns =
                    deadline.saturating_add(count.saturating_mul(timer.interval_ns));
                count
            };
            timer.overrun = count.saturating_sub(1).min(i32::MAX as u64) as i32;
            if let PosixTimerNotify::Signal { signum, sigval } = timer.notify {
                expired.push(ExpiredTimerSignal { signum, sigval });
            }
        }
        expired
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.timers
            .values()
            .filter_map(|timer| (timer.deadline_mono_ns != 0).then_some(timer.deadline_mono_ns))
            .min()
    }
}

impl PosixTimer {
    fn snapshot(self, now_mono_ns: u64) -> PosixTimerSnapshot {
        PosixTimerSnapshot {
            interval_ns: self.interval_ns,
            value_ns: if self.deadline_mono_ns == 0 {
                0
            } else {
                self.deadline_mono_ns.saturating_sub(now_mono_ns)
            },
        }
    }
}

pub struct Timekeeper {
    tai_offset_sec: AtomicI32,
    status: AtomicU32,
    maxerror_us: AtomicI64,
    esterror_us: AtomicI64,
    tick_us: AtomicI64,
}

impl Timekeeper {
    pub const fn new() -> Self {
        Self {
            tai_offset_sec: AtomicI32::new(0),
            status: AtomicU32::new(STA_NANO),
            maxerror_us: AtomicI64::new(0),
            esterror_us: AtomicI64::new(0),
            tick_us: AtomicI64::new(USER_TICK_USEC),
        }
    }

    pub fn clock_now_ns<P: TimeIf>(&self, clock: ClockId) -> u64 {
        match clock {
            ClockId::Realtime | ClockId::RealtimeCoarse => wall_clock::realtime_now_ns::<P>(),
            ClockId::Tai => {
                let tai_ns = (self.tai_offset_sec.load(Ordering::Acquire) as i128)
                    .saturating_mul(NSEC_PER_SEC as i128);
                add_signed_i128(wall_clock::realtime_now_ns::<P>(), tai_ns)
            }
            ClockId::Monotonic
            | ClockId::ProcessCpuTime
            | ClockId::ThreadCpuTime
            | ClockId::MonotonicRaw
            | ClockId::MonotonicCoarse
            | ClockId::Boottime => P::read_ns(),
        }
    }

    pub fn set_realtime_ns<P: TimeIf>(&self, realtime_ns: u64) -> Result<u64, TimekeepingError> {
        wall_clock::set_realtime_ns::<P>(realtime_ns).map_err(Into::into)
    }

    pub fn monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> u64 {
        wall_clock::monotonic_deadline_from_realtime_ns(realtime_ns)
    }

    pub fn generation(&self) -> u64 {
        wall_clock::generation()
    }

    pub fn adjtimex<P: TimeIf>(
        &self,
        clock: ClockId,
        privileged: bool,
        tx: &mut TimexState,
    ) -> Result<i32, TimekeepingError> {
        if clock != ClockId::Realtime {
            return Err(TimekeepingError::Unsupported);
        }
        let modes = tx.modes;
        validate_modes(modes)?;
        if modes != 0 && !privileged {
            return Err(TimekeepingError::Permission);
        }
        if modes & UNSUPPORTED_DISCIPLINE_MODES != 0 {
            return Err(TimekeepingError::Unsupported);
        }
        if modes & ADJ_STATUS != 0 && (tx.status & STA_RONLY) != 0 {
            return Err(TimekeepingError::Invalid);
        }

        if modes & ADJ_SETOFFSET != 0 {
            let subsec_limit = if modes & ADJ_NANO != 0 {
                NSEC_PER_SEC
            } else {
                USEC_PER_SEC
            };
            if tx.time_subsec < 0 || tx.time_subsec >= subsec_limit {
                return Err(TimekeepingError::Invalid);
            }
            let scale = if modes & ADJ_NANO != 0 { 1 } else { 1_000 };
            let delta = (tx.time_sec as i128)
                .saturating_mul(NSEC_PER_SEC as i128)
                .saturating_add((tx.time_subsec as i128).saturating_mul(scale));
            let now = wall_clock::realtime_now_ns::<P>();
            let new_realtime = add_checked_i128(now, delta)?;
            self.set_realtime_ns::<P>(new_realtime)?;
        }

        if modes & ADJ_TAI != 0 {
            if tx.constant < 0 || tx.constant > MAX_TAI_OFFSET {
                return Err(TimekeepingError::Invalid);
            }
            self.tai_offset_sec
                .store(tx.constant as i32, Ordering::Release);
        }
        if modes & ADJ_MAXERROR != 0 {
            self.maxerror_us
                .store(tx.maxerror.max(0), Ordering::Release);
        }
        if modes & ADJ_ESTERROR != 0 {
            self.esterror_us
                .store(tx.esterror.max(0), Ordering::Release);
        }
        if modes & ADJ_STATUS != 0 {
            let readonly = self.status.load(Ordering::Acquire) & STA_RONLY;
            self.status
                .store(readonly | (tx.status & !STA_RONLY), Ordering::Release);
        }
        if modes & ADJ_NANO != 0 {
            self.status.fetch_or(STA_NANO, Ordering::AcqRel);
        }
        if modes & ADJ_MICRO != 0 {
            self.status.fetch_and(!STA_NANO, Ordering::AcqRel);
        }

        self.fill_timex::<P>(tx);
        Ok(TIME_OK)
    }

    fn fill_timex<P: TimeIf>(&self, tx: &mut TimexState) {
        let realtime = wall_clock::realtime_now_ns::<P>();
        let status = self.status.load(Ordering::Acquire);
        tx.modes = 0;
        tx.offset = 0;
        tx.freq = 0;
        tx.maxerror = self.maxerror_us.load(Ordering::Acquire);
        tx.esterror = self.esterror_us.load(Ordering::Acquire);
        tx.status = status;
        tx.constant = 0;
        tx.precision = 1;
        tx.tolerance = 0;
        tx.time_sec = (realtime / NSEC_PER_SEC as u64) as i64;
        tx.time_subsec = if status & STA_NANO != 0 {
            (realtime % NSEC_PER_SEC as u64) as i64
        } else {
            ((realtime % NSEC_PER_SEC as u64) / 1_000) as i64
        };
        tx.tick = self.tick_us.load(Ordering::Acquire);
        tx.ppsfreq = 0;
        tx.jitter = 0;
        tx.shift = 0;
        tx.stabil = 0;
        tx.jitcnt = 0;
        tx.calcnt = 0;
        tx.errcnt = 0;
        tx.stbcnt = 0;
        tx.tai = self.tai_offset_sec.load(Ordering::Acquire);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reset_for_test(&self) {
        wall_clock::reset_for_test();
        self.tai_offset_sec.store(0, Ordering::Release);
        self.status.store(STA_NANO, Ordering::Release);
        self.maxerror_us.store(0, Ordering::Release);
        self.esterror_us.store(0, Ordering::Release);
        self.tick_us.store(USER_TICK_USEC, Ordering::Release);
    }
}

static TIMEKEEPER: Timekeeper = Timekeeper::new();

pub fn clock_now_ns<P: TimeIf>(clock: ClockId) -> u64 {
    TIMEKEEPER.clock_now_ns::<P>(clock)
}

pub fn set_realtime_ns<P: TimeIf>(realtime_ns: u64) -> Result<u64, TimekeepingError> {
    TIMEKEEPER.set_realtime_ns::<P>(realtime_ns)
}

pub fn monotonic_deadline_from_realtime_ns(realtime_ns: u64) -> u64 {
    TIMEKEEPER.monotonic_deadline_from_realtime_ns(realtime_ns)
}

pub fn generation() -> u64 {
    TIMEKEEPER.generation()
}

pub fn adjtimex<P: TimeIf>(
    clock: ClockId,
    privileged: bool,
    tx: &mut TimexState,
) -> Result<i32, TimekeepingError> {
    TIMEKEEPER.adjtimex::<P>(clock, privileged, tx)
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    TIMEKEEPER.reset_for_test();
}

fn validate_modes(modes: u32) -> Result<(), TimekeepingError> {
    if modes & !KNOWN_MODES != 0 {
        return Err(TimekeepingError::Invalid);
    }
    if modes & ADJ_NANO != 0 && modes & ADJ_MICRO != 0 {
        return Err(TimekeepingError::Invalid);
    }
    Ok(())
}

fn add_checked_i128(base: u64, delta: i128) -> Result<u64, TimekeepingError> {
    let value = (base as i128).saturating_add(delta);
    if value < 0 || value > u64::MAX as i128 {
        Err(TimekeepingError::Range)
    } else {
        Ok(value as u64)
    }
}

fn add_signed_i128(base: u64, delta: i128) -> u64 {
    let value = (base as i128).saturating_add(delta);
    if value <= 0 {
        0
    } else if value > u64::MAX as i128 {
        u64::MAX
    } else {
        value as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64;

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
    fn setoffset_steps_realtime_without_moving_monotonic() {
        let tk = Timekeeper::new();
        let mut tx = TimexState {
            modes: ADJ_SETOFFSET,
            time_sec: 1,
            time_subsec: 500_000,
            ..TimexState::default()
        };
        let before = tk.clock_now_ns::<TestTime>(ClockId::Monotonic);

        assert_eq!(
            tk.adjtimex::<TestTime>(ClockId::Realtime, true, &mut tx),
            Ok(TIME_OK)
        );

        let after = tk.clock_now_ns::<TestTime>(ClockId::Monotonic);
        assert!(after >= before);
        assert!(tk.clock_now_ns::<TestTime>(ClockId::Realtime) >= 1_500_000_000);
    }
}
