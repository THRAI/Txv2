//! `timerfd_create(2)` — timers that notify via file descriptors.
//!
//! Spec: `man 2 timerfd_create`, `man 2 timerfd_settime`,
//! `man 2 timerfd_gettime`.  v1: caller-driven time model — the
//! subsystem stores deadline/interval; the syscall shim provides
//! `now_ns` and handles blocking via `timer_sleep::sleep_until_ns`.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;

#[cfg(test)]
use adapter::step_engine::V3Errno;
use adapter::step_engine::{
    eagain, sign, yield_until_readable, ByteOutcome, Cap, NoProgress, OneShotStepOp, ScriptCtx,
    StepOp, StepOutcome, SubjectIdentity, WaitSource, Zone, ZoneAllocated, ZoneError,
};
use adapter::wait_routing::{self, Channel};

use crate::wait_source;

pub const TFD_CLOEXEC: u32 = 0o2000000;
pub const TFD_NONBLOCK: u32 = 0o4000;
pub const TFD_TIMER_ABSTIME: u32 = 0x1;
pub const TIMERFD_READABLE: u64 = 0x1;
pub const ITIMERSPEC_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ItimerSpec {
    pub it_interval_ns: u64,
    pub it_value_ns: u64,
}

impl ItimerSpec {
    pub fn to_bytes(&self) -> [u8; ITIMERSPEC_BYTES] {
        let mut buf = [0u8; ITIMERSPEC_BYTES];
        let ival_sec = (self.it_interval_ns / 1_000_000_000) as i64;
        let ival_nsec = (self.it_interval_ns % 1_000_000_000) as i64;
        let val_sec = (self.it_value_ns / 1_000_000_000) as i64;
        let val_nsec = (self.it_value_ns % 1_000_000_000) as i64;
        buf[0..8].copy_from_slice(&ival_sec.to_le_bytes());
        buf[8..16].copy_from_slice(&ival_nsec.to_le_bytes());
        buf[16..24].copy_from_slice(&val_sec.to_le_bytes());
        buf[24..32].copy_from_slice(&val_nsec.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8; ITIMERSPEC_BYTES]) -> Self {
        Self::try_from_bytes(bytes).unwrap_or_default()
    }

    pub fn try_from_bytes(bytes: &[u8; ITIMERSPEC_BYTES]) -> Option<Self> {
        let ival_sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let ival_nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let val_sec = i64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let val_nsec = i64::from_le_bytes(bytes[24..32].try_into().unwrap());
        if ival_sec < 0
            || ival_nsec < 0
            || ival_nsec >= 1_000_000_000
            || val_sec < 0
            || val_nsec < 0
            || val_nsec >= 1_000_000_000
        {
            return None;
        }
        let it_interval_ns = (ival_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ival_nsec as u64);
        let it_value_ns = (val_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(val_nsec as u64);
        Some(ItimerSpec {
            it_interval_ns,
            it_value_ns,
        })
    }
}

pub struct TimerFd {
    clockid: u32,
    deadline_ns: AtomicU64,
    interval_ns: AtomicU64,
    expiration_count: AtomicU64,
    source_id: u64,
    channel: Option<Channel>,
    source: Option<Arc<WaitSource>>,
    flags: AtomicU64,
}

impl TimerFd {
    pub fn new(clockid: u32, flags: u32) -> Self {
        let channel = Channel::new();
        let source_id = wait_source::register_wait_channel(channel.clone());
        let source = Some(wait_routing::new_wait_source(source_id));
        TimerFd {
            clockid,
            deadline_ns: AtomicU64::new(0),
            interval_ns: AtomicU64::new(0),
            expiration_count: AtomicU64::new(0),
            source_id,
            channel: Some(channel),
            source,
            flags: AtomicU64::new(flags as u64),
        }
    }

    pub fn clockid(&self) -> u32 {
        self.clockid
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns.load(Ordering::Acquire)
    }
    pub fn interval_ns(&self) -> u64 {
        self.interval_ns.load(Ordering::Acquire)
    }
    pub fn source_id(&self) -> u64 {
        self.source_id
    }
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire) as u32
    }
    #[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
    fn is_nonblocking(&self) -> bool {
        (self.flags() & TFD_NONBLOCK) != 0
    }

    pub fn arm(&self, deadline_ns: u64, interval_ns: u64) {
        self.deadline_ns.store(deadline_ns, Ordering::Release);
        self.interval_ns.store(interval_ns, Ordering::Release);
        self.expiration_count.store(0, Ordering::Release);
    }

    pub fn disarm(&self) {
        self.deadline_ns.store(0, Ordering::Release);
        self.interval_ns.store(0, Ordering::Release);
    }

    pub fn expiration_count(&self) -> u64 {
        self.expiration_count.load(Ordering::Acquire)
    }

    pub fn remaining_value_ns(&self, now_ns: u64) -> u64 {
        let deadline = self.deadline_ns();
        if deadline == 0 {
            0
        } else {
            deadline.saturating_sub(now_ns)
        }
    }

    fn bump_expirations(&self, now_ns: u64) -> u64 {
        let deadline = self.deadline_ns.load(Ordering::Acquire);
        if deadline == 0 {
            return 0;
        }
        if now_ns < deadline {
            return 0;
        }
        let interval = self.interval_ns.load(Ordering::Acquire);
        if interval == 0 {
            self.deadline_ns.store(0, Ordering::Release);
            let prev = self.expiration_count.fetch_add(1, Ordering::AcqRel);
            return prev + 1;
        }
        let elapsed = now_ns - deadline;
        let count = elapsed / interval + 1;
        let new_deadline = deadline + count * interval;
        self.deadline_ns.store(new_deadline, Ordering::Release);
        let prev = self.expiration_count.fetch_add(count, Ordering::AcqRel);
        prev + count
    }

    fn drain_count(&self) -> u64 {
        self.expiration_count.swap(0, Ordering::AcqRel)
    }

    fn fire_readable(&self) {
        if let Some(ref ch) = self.channel {
            wait_routing::fire_legacy_channel(ch, TIMERFD_READABLE);
        }
        if let Some(ref src) = self.source {
            wait_routing::notify_v3_source(src, TIMERFD_READABLE);
        }
    }
}

static TIMERFD_ZONE: Zone<TimerFd> = Zone::const_new();

unsafe impl ZoneAllocated for TimerFd {
    fn zone() -> &'static Zone<Self> {
        &TIMERFD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    adapter::step_engine::register_zone_for::<TimerFd>()?;
    Ok(())
}

pub fn timerfd_create(clockid: u32, flags: u32) -> Result<Cap<TimerFd>, ZoneError> {
    let tfd = TimerFd::new(clockid, flags);
    sign(tfd)
}

pub fn timerfd_settime(
    tfd: &TimerFd,
    abstime: bool,
    now_ns: u64,
    new_value: ItimerSpec,
    old_value: Option<&mut ItimerSpec>,
) {
    if let Some(old) = old_value {
        *old = ItimerSpec {
            it_interval_ns: tfd.interval_ns(),
            it_value_ns: tfd.remaining_value_ns(now_ns),
        };
    }
    let it_value = new_value.it_value_ns;
    if it_value == 0 {
        tfd.disarm();
        return;
    }
    let deadline = if abstime {
        it_value
    } else {
        now_ns.saturating_add(it_value)
    };
    tfd.arm(deadline, new_value.it_interval_ns);
    let count = tfd.bump_expirations(now_ns);
    if count > 0 {
        tfd.fire_readable();
    }
}

pub fn step_timerfd_read(
    tfd: &TimerFd,
    now_ns: u64,
    out: &mut [u8; 8],
    nonblocking: bool,
) -> ByteOutcome {
    let count = tfd.bump_expirations(now_ns);
    if count > 0 {
        let drained = tfd.drain_count();
        if tfd.interval_ns() > 0 {
            let _ = tfd.bump_expirations(now_ns);
        }
        out.copy_from_slice(&drained.to_le_bytes());
        return ByteOutcome::done(8);
    }
    if nonblocking {
        return eagain();
    }
    yield_until_readable(tfd.source_id, TIMERFD_READABLE)
}

pub struct TimerfdCreateOp {
    pub clockid: u32,
    pub flags: u32,
}
impl<I: SubjectIdentity> StepOp<I> for TimerfdCreateOp {
    type Output = Result<Cap<TimerFd>, ZoneError>;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(timerfd_create(self.clockid, self.flags))
    }
}
impl<I: SubjectIdentity> OneShotStepOp<I> for TimerfdCreateOp {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        guard
    }

    #[test]
    fn create_disarmed_read_returns_eagain() {
        let _g = setup();
        let cap = timerfd_create(1, TFD_NONBLOCK).expect("create");
        let mut buf = [0u8; 8];
        match step_timerfd_read(&cap, 0, &mut buf, true) {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            other => panic!("expected EAGAIN, got {other:?}"),
        }
    }

    #[test]
    fn arm_and_past_deadline_reads_one() {
        let _g = setup();
        let cap = timerfd_create(1, 0).expect("create");
        let now = 1_000_000_000;
        timerfd_settime(
            &cap,
            false,
            now,
            ItimerSpec {
                it_interval_ns: 0,
                it_value_ns: 100_000_000,
            },
            None,
        );
        let mut buf = [0u8; 8];
        match step_timerfd_read(&cap, now, &mut buf, false) {
            StepOutcome::Yield { .. } => {}
            other => panic!("expected Yield, got {other:?}"),
        }
        let now2 = now + 200_000_000;
        match step_timerfd_read(&cap, now2, &mut buf, false) {
            StepOutcome::Done(8) => assert_eq!(u64::from_le_bytes(buf), 1),
            other => panic!("expected Done(8), got {other:?}"),
        }
    }

    #[test]
    fn disarm_zero_it_value() {
        let _g = setup();
        let cap = timerfd_create(1, 0).expect("create");
        let now = 1_000_000_000;
        timerfd_settime(
            &cap,
            false,
            now,
            ItimerSpec {
                it_interval_ns: 0,
                it_value_ns: 100_000_000,
            },
            None,
        );
        assert_eq!(cap.deadline_ns(), now + 100_000_000);
        timerfd_settime(
            &cap,
            false,
            now,
            ItimerSpec {
                it_interval_ns: 0,
                it_value_ns: 0,
            },
            None,
        );
        assert_eq!(cap.deadline_ns(), 0);
    }

    #[test]
    fn itimerspec_roundtrip() {
        let spec = ItimerSpec {
            it_interval_ns: 1_500_000_000,
            it_value_ns: 500_000_000,
        };
        let bytes = spec.to_bytes();
        let parsed = ItimerSpec::from_bytes(&bytes);
        assert_eq!(parsed.it_interval_ns, 1_500_000_000);
        assert_eq!(parsed.it_value_ns, 500_000_000);
    }
}
