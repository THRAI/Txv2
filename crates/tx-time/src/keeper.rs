//! Wall-clock state, VVAR payload generation, and service compatibility types.
//!
//! HAL [`MonotonicCounterIf`] remains monotonic-only. `CLOCK_REALTIME` is
//! represented as the monotonic counter plus a timekeeper-owned offset.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

use tx_hal::{MonotonicCounterIf, PersistentClockError, PersistentClockIf};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};
use tx_substrate::SpinMutex;

use crate::{
    vdso::counter_eligibility, ClockRead, DeadlineRegistrar, RealtimeControl, RealtimeSetPolicy,
    RealtimeSetReport, TimeError, VvarPublisher,
};

pub use crate::vdso::VvarSnapshot;

pub const DEFAULT_REALTIME_EPOCH_BASE_NS: u64 = 1_779_494_400_000_000_000;

pub type RealtimeTimerNotifier = fn(
    u64,
    Option<&dyn DeadlineRegistrar>,
    &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
) -> usize;

pub type VvarPublishHook = fn(VvarSnapshot);

static REALTIME_TIMER_NOTIFIER: AtomicUsize = AtomicUsize::new(0);
static VVAR_PUBLISH_HOOK: AtomicUsize = AtomicUsize::new(0);
/// Platform monotonic-ns reader for callers without a `P` bound. Page-backed
/// writeback stamps file mtimes below the platform generic, so it cannot
/// reach `timekeeper_clock::<P>()`. Zero until the boot path installs one.
static MONOTONIC_NS_SOURCE: AtomicUsize = AtomicUsize::new(0);

pub fn install_realtime_timer_notifier(notifier: RealtimeTimerNotifier) {
    REALTIME_TIMER_NOTIFIER.store(notifier as usize, Ordering::Release);
}

pub fn install_vvar_publish_hook(hook: VvarPublishHook) {
    VVAR_PUBLISH_HOOK.store(hook as usize, Ordering::Release);
}

/// Install the platform's monotonic clock reader so non-generic code can
/// read CLOCK_REALTIME via [`realtime_now_ns_hooked`].
pub fn install_monotonic_ns_source(source: fn() -> u64) {
    MONOTONIC_NS_SOURCE.store(source as usize, Ordering::Release);
}

/// CLOCK_REALTIME for callers without a `P` bound. `None` until the boot
/// path installs the monotonic source, so host unit tests (which never
/// install one) keep epoch timestamps instead of inventing a clock.
pub fn realtime_now_ns_hooked() -> Option<u64> {
    let raw = MONOTONIC_NS_SOURCE.load(Ordering::Acquire);
    if raw == 0 {
        return None;
    }
    // SAFETY: the only store is `install_monotonic_ns_source`, which writes a
    // valid `fn() -> u64`; fn pointers are non-null and never deallocated.
    let source: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    Some(add_signed_ns(source(), WALL_CLOCK.realtime_offset_ns()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WallClockError {
    Range,
    InvalidCalibration,
    PersistentClock(PersistentClockError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeSeedError {
    PersistentClock(PersistentClockError),
    WallClock(WallClockError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeWritebackPolicy {
    Disabled,
    BestEffort,
    Required,
}

struct WallClock {
    writer: SpinMutex<()>,
    realtime_offset_ns: AtomicI64,
    generation: AtomicU64,
    mult: AtomicU64,
    shift: AtomicU64,
    mask: AtomicU64,
}

/// Compatibility surface for callers that bind timekeeping to a static HAL
/// platform type.
pub trait TimekeeperIf {
    fn monotonic_now_ns<P: MonotonicCounterIf>(&self) -> u64;
    fn realtime_now_ns<P: MonotonicCounterIf>(&self) -> u64;
    fn set_realtime_ns<P: MonotonicCounterIf>(
        &self,
        realtime_ns: u64,
    ) -> Result<u64, WallClockError>;
    fn set_realtime_ns_with_persistent<P: MonotonicCounterIf + PersistentClockIf>(
        &self,
        realtime_ns: u64,
        policy: RealtimeWritebackPolicy,
    ) -> Result<RealtimeSetReport, WallClockError>;
    fn set_realtime_ns_with_persistent_and_timerfd_post<P, F>(
        &self,
        realtime_ns: u64,
        policy: RealtimeWritebackPolicy,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<RealtimeSetReport, WallClockError>
    where
        P: MonotonicCounterIf + PersistentClockIf,
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool;
    fn seed_realtime_ns<P: MonotonicCounterIf>(
        &self,
        realtime_ns: u64,
    ) -> Result<u64, WallClockError>;
    fn seed_realtime_from_persistent<P: MonotonicCounterIf + PersistentClockIf>(
        &self,
    ) -> Result<u64, RealtimeSeedError>;
    fn realtime_generation(&self) -> u64;
    fn realtime_offset_ns(&self) -> i64;
    fn monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> u64;
    fn try_monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> Result<u64, TimeError>;
    fn snapshot_for_vvar<P: MonotonicCounterIf>(&self) -> VvarSnapshot;
    fn publish_vvar<P: MonotonicCounterIf>(&self);
    fn set_clock_params(&self, mult: u64, shift: u64, mask: u64);
    fn try_set_clock_params(&self, mult: u64, shift: u64, mask: u64) -> Result<(), TimeError>;
}

/// Global timekeeper facade. Its concrete state is private to this module.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timekeeper;

pub const fn timekeeper() -> Timekeeper {
    Timekeeper
}

/// Static-platform adapter for the canonical time traits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimekeeperClock<P> {
    timekeeper: Timekeeper,
    _platform: PhantomData<fn() -> P>,
}

impl<P> TimekeeperClock<P> {
    pub const fn new(timekeeper: Timekeeper) -> Self {
        Self {
            timekeeper,
            _platform: PhantomData,
        }
    }

    pub const fn global() -> Self {
        Self::new(timekeeper())
    }
}

pub const fn timekeeper_clock<P>() -> TimekeeperClock<P> {
    TimekeeperClock::global()
}

impl<P: MonotonicCounterIf> ClockRead for TimekeeperClock<P> {
    fn monotonic_now_ns(&self) -> u64 {
        self.timekeeper.monotonic_now_ns::<P>()
    }

    fn realtime_now_ns(&self) -> u64 {
        self.timekeeper.realtime_now_ns::<P>()
    }
}

impl<P: MonotonicCounterIf + PersistentClockIf> RealtimeControl for TimekeeperClock<P> {
    fn set_realtime_ns(
        &self,
        ns: u64,
        policy: RealtimeSetPolicy,
    ) -> Result<RealtimeSetReport, TimeError> {
        self.set_realtime_with_post(ns, policy, None, |mailbox, event| mailbox.post(event))
    }

    fn set_realtime_ns_with_timerfd_post<F>(
        &self,
        ns: u64,
        policy: RealtimeSetPolicy,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<RealtimeSetReport, TimeError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.set_realtime_with_post(ns, policy, timer_registrar, post)
    }

    fn seed_realtime_from_persistent(&self) -> Result<RealtimeSetReport, TimeError> {
        let realtime_ns = P::read_realtime_ns().map_err(TimeError::from)?;
        let generation = self
            .timekeeper
            .seed_realtime_ns::<P>(realtime_ns)
            .map_err(TimeError::from)?;
        Ok(RealtimeSetReport {
            realtime_ns,
            generation,
            persistent_written: false,
        })
    }

    fn realtime_generation(&self) -> u64 {
        self.timekeeper.realtime_generation()
    }

    fn realtime_offset_ns(&self) -> i128 {
        self.timekeeper.realtime_offset_ns() as i128
    }
}

impl<P: MonotonicCounterIf + PersistentClockIf> TimekeeperClock<P> {
    fn set_realtime_with_post<F>(
        &self,
        ns: u64,
        policy: RealtimeSetPolicy,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<RealtimeSetReport, TimeError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let mutation = self
            .timekeeper
            .mutate_realtime_with_persistent_and_timerfd_post::<P, F>(
                ns,
                policy.into(),
                timer_registrar,
                post,
            )
            .map_err(TimeError::from)?;
        if let (RealtimeSetPolicy::Required, Some(Err(err))) = (policy, mutation.persistent_result)
        {
            return Err(TimeError::from(err));
        }
        Ok(mutation.report)
    }
}

impl<P: MonotonicCounterIf> VvarPublisher for TimekeeperClock<P> {
    fn publish_vvar(&self) {
        self.timekeeper.publish_vvar::<P>();
    }
}

impl From<RealtimeSetPolicy> for RealtimeWritebackPolicy {
    fn from(value: RealtimeSetPolicy) -> Self {
        match value {
            RealtimeSetPolicy::Disabled => Self::Disabled,
            RealtimeSetPolicy::BestEffort => Self::BestEffort,
            RealtimeSetPolicy::Required => Self::Required,
        }
    }
}

impl From<WallClockError> for TimeError {
    fn from(value: WallClockError) -> Self {
        match value {
            WallClockError::Range => Self::Range,
            WallClockError::InvalidCalibration => Self::Invalid,
            WallClockError::PersistentClock(err) => Self::from(err),
        }
    }
}

impl From<RealtimeSeedError> for TimeError {
    fn from(value: RealtimeSeedError) -> Self {
        match value {
            RealtimeSeedError::PersistentClock(err) => TimeError::from(err),
            RealtimeSeedError::WallClock(err) => TimeError::from(err),
        }
    }
}

impl TimekeeperIf for Timekeeper {
    fn monotonic_now_ns<P: MonotonicCounterIf>(&self) -> u64 {
        WALL_CLOCK.monotonic_now_ns::<P>()
    }

    fn realtime_now_ns<P: MonotonicCounterIf>(&self) -> u64 {
        WALL_CLOCK.realtime_now_ns::<P>()
    }

    fn set_realtime_ns<P: MonotonicCounterIf>(
        &self,
        realtime_ns: u64,
    ) -> Result<u64, WallClockError> {
        WALL_CLOCK.set_realtime_ns::<P>(realtime_ns)
    }

    fn set_realtime_ns_with_persistent<P: MonotonicCounterIf + PersistentClockIf>(
        &self,
        realtime_ns: u64,
        policy: RealtimeWritebackPolicy,
    ) -> Result<RealtimeSetReport, WallClockError> {
        self.set_realtime_ns_with_persistent_and_timerfd_post::<P, _>(
            realtime_ns,
            policy,
            None,
            |mailbox, event| mailbox.post(event),
        )
    }

    fn set_realtime_ns_with_persistent_and_timerfd_post<P, F>(
        &self,
        realtime_ns: u64,
        policy: RealtimeWritebackPolicy,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<RealtimeSetReport, WallClockError>
    where
        P: MonotonicCounterIf + PersistentClockIf,
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let mutation = self.mutate_realtime_with_persistent_and_timerfd_post::<P, F>(
            realtime_ns,
            policy,
            timer_registrar,
            post,
        )?;
        if let (RealtimeWritebackPolicy::Required, Some(Err(err))) =
            (policy, mutation.persistent_result)
        {
            return Err(WallClockError::PersistentClock(err));
        }
        Ok(mutation.report)
    }

    fn seed_realtime_ns<P: MonotonicCounterIf>(
        &self,
        realtime_ns: u64,
    ) -> Result<u64, WallClockError> {
        WALL_CLOCK.seed_realtime_ns::<P>(realtime_ns)
    }

    fn seed_realtime_from_persistent<P: MonotonicCounterIf + PersistentClockIf>(
        &self,
    ) -> Result<u64, RealtimeSeedError> {
        let realtime_ns = P::read_realtime_ns().map_err(RealtimeSeedError::PersistentClock)?;
        WALL_CLOCK
            .seed_realtime_ns::<P>(realtime_ns)
            .map_err(RealtimeSeedError::WallClock)
    }

    fn realtime_generation(&self) -> u64 {
        WALL_CLOCK.generation()
    }

    fn realtime_offset_ns(&self) -> i64 {
        WALL_CLOCK.realtime_offset_ns()
    }

    fn monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> u64 {
        WALL_CLOCK.monotonic_deadline_from_realtime_ns(realtime_ns)
    }

    fn try_monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> Result<u64, TimeError> {
        WALL_CLOCK.try_monotonic_deadline_from_realtime_ns(realtime_ns)
    }

    fn snapshot_for_vvar<P: MonotonicCounterIf>(&self) -> VvarSnapshot {
        WALL_CLOCK.snapshot_for_vvar::<P>()
    }

    fn publish_vvar<P: MonotonicCounterIf>(&self) {
        WALL_CLOCK.publish_vvar::<P>();
    }

    fn set_clock_params(&self, mult: u64, shift: u64, mask: u64) {
        WALL_CLOCK.set_clock_params(mult, shift, mask);
    }

    fn try_set_clock_params(&self, mult: u64, shift: u64, mask: u64) -> Result<(), TimeError> {
        WALL_CLOCK.try_set_clock_params(mult, shift, mask)
    }
}

struct RealtimeMutation {
    report: RealtimeSetReport,
    persistent_result: Option<Result<(), PersistentClockError>>,
}

#[derive(Clone, Copy)]
struct VvarCalibration {
    mask: u64,
    mult: u64,
    shift: u32,
}

impl Timekeeper {
    fn mutate_realtime_with_persistent_and_timerfd_post<P, F>(
        &self,
        realtime_ns: u64,
        policy: RealtimeWritebackPolicy,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<RealtimeMutation, WallClockError>
    where
        P: MonotonicCounterIf + PersistentClockIf,
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let (generation, _) = WALL_CLOCK.set_realtime_ns_with_timerfd_post::<P, F>(
            realtime_ns,
            timer_registrar,
            post,
        )?;
        let persistent_result = match policy {
            RealtimeWritebackPolicy::Disabled => None,
            RealtimeWritebackPolicy::BestEffort | RealtimeWritebackPolicy::Required => {
                Some(P::set_realtime_ns(realtime_ns))
            }
        };
        Ok(RealtimeMutation {
            report: RealtimeSetReport {
                realtime_ns,
                generation,
                persistent_written: matches!(persistent_result, Some(Ok(()))),
            },
            persistent_result,
        })
    }
}

impl WallClock {
    const fn new(default_offset_ns: i64) -> Self {
        Self {
            writer: SpinMutex::new(()),
            realtime_offset_ns: AtomicI64::new(default_offset_ns),
            generation: AtomicU64::new(0),
            mult: AtomicU64::new(1),
            shift: AtomicU64::new(0),
            mask: AtomicU64::new(!0),
        }
    }

    fn monotonic_now_ns<P: MonotonicCounterIf>(&self) -> u64 {
        P::read_ns()
    }

    fn realtime_now_ns<P: MonotonicCounterIf>(&self) -> u64 {
        add_signed_ns(P::read_ns(), self.realtime_offset_ns())
    }

    fn set_realtime_ns<P: MonotonicCounterIf>(
        &self,
        realtime_ns: u64,
    ) -> Result<u64, WallClockError> {
        self.set_realtime_ns_with_timerfd_post::<P, _>(realtime_ns, None, |mailbox, event| {
            mailbox.post(event)
        })
        .map(|(generation, _)| generation)
    }

    fn set_realtime_ns_with_timerfd_post<P, F>(
        &self,
        realtime_ns: u64,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<(u64, i64), WallClockError>
    where
        P: MonotonicCounterIf,
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.set_realtime_ns_inner::<P, F>(realtime_ns, true, timer_registrar, post)
    }

    fn seed_realtime_ns<P: MonotonicCounterIf>(
        &self,
        realtime_ns: u64,
    ) -> Result<u64, WallClockError> {
        self.set_realtime_ns_inner::<P, _>(realtime_ns, false, None, |_mailbox, _event| false)
            .map(|(generation, _)| generation)
    }

    fn set_realtime_ns_inner<P, F>(
        &self,
        realtime_ns: u64,
        notify_realtime_timers: bool,
        timer_registrar: Option<&dyn DeadlineRegistrar>,
        post: F,
    ) -> Result<(u64, i64), WallClockError>
    where
        P: MonotonicCounterIf,
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let (generation, offset, snapshot, publish, notifier) = {
            let _writer = self.writer.lock();
            let calibration = self
                .vvar_calibration_locked()
                .map_err(|_| WallClockError::InvalidCalibration)?;
            let mono_ns = P::read_ns();
            let offset = (realtime_ns as i128) - (mono_ns as i128);
            if offset < i64::MIN as i128 || offset > i64::MAX as i128 {
                return Err(WallClockError::Range);
            }
            self.realtime_offset_ns
                .store(offset as i64, Ordering::Release);
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            let snapshot = self.snapshot_for_vvar_locked::<P>(calibration);
            let publish = vvar_publish_hook();
            let notifier = notify_realtime_timers
                .then(realtime_timer_notifier)
                .flatten();
            (generation, offset as i64, snapshot, publish, notifier)
        };

        if let Some(publish) = publish {
            publish(snapshot);
        }
        if let Some(notifier) = notifier {
            let mut post = post;
            let _ = notifier(generation, timer_registrar, &mut post);
        }
        Ok((generation, offset))
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn realtime_offset_ns(&self) -> i64 {
        self.realtime_offset_ns.load(Ordering::Acquire)
    }

    fn set_clock_params(&self, mult: u64, shift: u64, mask: u64) {
        let _ = self.try_set_clock_params(mult, shift, mask);
    }

    fn try_set_clock_params(&self, mult: u64, shift: u64, mask: u64) -> Result<(), TimeError> {
        if !vvar_shift_is_valid(shift) {
            return Err(TimeError::Invalid);
        }
        let _writer = self.writer.lock();
        self.mult.store(mult, Ordering::Release);
        self.shift.store(shift, Ordering::Release);
        self.mask.store(mask, Ordering::Release);
        Ok(())
    }

    fn monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> u64 {
        self.try_monotonic_deadline_from_realtime_ns(realtime_ns)
            .unwrap_or_else(|_| {
                if self.realtime_offset_ns() > 0 {
                    0
                } else {
                    u64::MAX
                }
            })
    }

    fn try_monotonic_deadline_from_realtime_ns(&self, realtime_ns: u64) -> Result<u64, TimeError> {
        let deadline = (realtime_ns as i128) - (self.realtime_offset_ns() as i128);
        if deadline < 0 || deadline > u64::MAX as i128 {
            Err(TimeError::Range)
        } else {
            Ok(deadline as u64)
        }
    }

    fn snapshot_for_vvar<P: MonotonicCounterIf>(&self) -> VvarSnapshot {
        let _writer = self.writer.lock();
        let calibration = self
            .vvar_calibration_locked()
            .expect("timekeeper VVAR calibration must be validated before storage");
        self.snapshot_for_vvar_locked::<P>(calibration)
    }

    fn snapshot_for_vvar_locked<P: MonotonicCounterIf>(
        &self,
        calibration: VvarCalibration,
    ) -> VvarSnapshot {
        let (cycle_last, calibration) = match counter_eligibility::<P>().ready() {
            Some(counter) => (
                counter.read_counter::<P>(),
                VvarCalibration {
                    mask: counter.calibration().mask(),
                    mult: counter.calibration().mult(),
                    shift: counter.calibration().shift(),
                },
            ),
            None => (0, calibration),
        };
        let monotonic_ns = P::read_ns();
        let realtime_ns = add_signed_ns(monotonic_ns, self.realtime_offset_ns());
        let (monotonic_sec, monotonic_nsec) = split_ns(monotonic_ns);
        let (realtime_sec, realtime_nsec) = split_ns(realtime_ns);
        VvarSnapshot {
            realtime_generation: self.generation.load(Ordering::Acquire),
            cycle_last,
            mask: calibration.mask,
            mult: calibration.mult,
            shift: calibration.shift as u64,
            realtime_sec,
            realtime_nsec_shifted: realtime_nsec
                .checked_shl(calibration.shift)
                .expect("validated VVAR shift"),
            monotonic_sec,
            monotonic_nsec_shifted: monotonic_nsec
                .checked_shl(calibration.shift)
                .expect("validated VVAR shift"),
        }
    }

    fn publish_vvar<P: MonotonicCounterIf>(&self) {
        let (snapshot, publish) = {
            let _writer = self.writer.lock();
            let calibration = self
                .vvar_calibration_locked()
                .expect("timekeeper VVAR calibration must be validated before storage");
            (
                self.snapshot_for_vvar_locked::<P>(calibration),
                vvar_publish_hook(),
            )
        };
        if let Some(publish) = publish {
            publish(snapshot);
        }
    }

    fn vvar_calibration_locked(&self) -> Result<VvarCalibration, TimeError> {
        let shift = self.shift.load(Ordering::Acquire);
        if !vvar_shift_is_valid(shift) {
            return Err(TimeError::Invalid);
        }
        Ok(VvarCalibration {
            mask: self.mask.load(Ordering::Acquire),
            mult: self.mult.load(Ordering::Acquire),
            shift: shift as u32,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    fn reset_for_test(&self) {
        let _writer = self.writer.lock();
        self.realtime_offset_ns
            .store(DEFAULT_REALTIME_EPOCH_BASE_NS as i64, Ordering::Release);
        self.generation.store(0, Ordering::Release);
        self.mult.store(1, Ordering::Release);
        self.shift.store(0, Ordering::Release);
        self.mask.store(!0, Ordering::Release);
    }
}

static WALL_CLOCK: WallClock = WallClock::new(DEFAULT_REALTIME_EPOCH_BASE_NS as i64);

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    WALL_CLOCK.reset_for_test();
    REALTIME_TIMER_NOTIFIER.store(0, Ordering::Release);
    VVAR_PUBLISH_HOOK.store(0, Ordering::Release);
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

fn vvar_shift_is_valid(shift: u64) -> bool {
    let Ok(shift) = u32::try_from(shift) else {
        return false;
    };
    let Some(scale) = 1_u64.checked_shl(shift) else {
        return false;
    };
    (1_000_000_000_u64 - 1).checked_mul(scale).is_some()
}

fn realtime_timer_notifier() -> Option<RealtimeTimerNotifier> {
    let hook = REALTIME_TIMER_NOTIFIER.load(Ordering::Acquire);
    if hook == 0 {
        return None;
    }
    Some(unsafe { core::mem::transmute::<usize, RealtimeTimerNotifier>(hook) })
}

fn vvar_publish_hook() -> Option<VvarPublishHook> {
    let hook = VVAR_PUBLISH_HOOK.load(Ordering::Acquire);
    if hook == 0 {
        return None;
    }
    Some(unsafe { core::mem::transmute::<usize, VvarPublishHook>(hook) })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTime;
    struct FailingPersistentTime;

    static TEST_NS: AtomicU64 = AtomicU64::new(5_000_000_000);
    static TEST_PERSISTENT_SET_NS: AtomicU64 = AtomicU64::new(0);
    static PUBLISHED_REALTIME_SEC: AtomicU64 = AtomicU64::new(0);
    static PUBLISHED_MONOTONIC_SEC: AtomicU64 = AtomicU64::new(0);
    static TIMERFD_NOTIFICATION_GENERATION: AtomicU64 = AtomicU64::new(0);
    static REENTRANT_CALLBACKS: AtomicU64 = AtomicU64::new(0);
    static TEST_CLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl MonotonicCounterIf for TestTime {
        fn read_ns() -> u64 {
            TEST_NS.fetch_add(1, Ordering::Relaxed)
        }

        fn frequency_hz() -> u64 {
            1_000_000_000
        }
    }

    impl PersistentClockIf for TestTime {
        fn read_realtime_ns() -> Result<u64, PersistentClockError> {
            Ok(1_800_000_000_000_000_000)
        }

        fn set_realtime_ns(ns: u64) -> Result<(), PersistentClockError> {
            TEST_PERSISTENT_SET_NS.store(ns, Ordering::Release);
            Ok(())
        }
    }

    impl MonotonicCounterIf for FailingPersistentTime {
        fn read_ns() -> u64 {
            TestTime::read_ns()
        }

        fn frequency_hz() -> u64 {
            TestTime::frequency_hz()
        }
    }

    impl PersistentClockIf for FailingPersistentTime {
        fn set_realtime_ns(_ns: u64) -> Result<(), PersistentClockError> {
            Err(PersistentClockError::Hardware)
        }
    }

    fn publish_snapshot(snapshot: VvarSnapshot) {
        PUBLISHED_REALTIME_SEC.store(snapshot.realtime_sec, Ordering::Release);
        PUBLISHED_MONOTONIC_SEC.store(snapshot.monotonic_sec, Ordering::Release);
    }

    fn notify_timerfd(
        generation: u64,
        _timer_registrar: Option<&dyn DeadlineRegistrar>,
        _post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
    ) -> usize {
        TIMERFD_NOTIFICATION_GENERATION.store(generation, Ordering::Release);
        0
    }

    fn reentrant_publish_snapshot(_snapshot: VvarSnapshot) {
        timekeeper()
            .try_set_clock_params(3, 1, !0)
            .expect("VVAR hook must run after the writer lock is released");
        REENTRANT_CALLBACKS.fetch_add(1, Ordering::AcqRel);
    }

    fn reentrant_timerfd_notifier(
        _generation: u64,
        _timer_registrar: Option<&dyn DeadlineRegistrar>,
        _post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
    ) -> usize {
        timekeeper()
            .try_set_clock_params(5, 2, !0)
            .expect("timerfd hook must run after the writer lock is released");
        REENTRANT_CALLBACKS.fetch_add(1, Ordering::AcqRel);
        0
    }

    #[test]
    fn installed_hooks_round_trip_exact_function_pointer_behavior() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        PUBLISHED_REALTIME_SEC.store(0, Ordering::Release);
        TIMERFD_NOTIFICATION_GENERATION.store(0, Ordering::Release);
        install_vvar_publish_hook(publish_snapshot);
        install_realtime_timer_notifier(notify_timerfd);

        let publish = vvar_publish_hook().expect("installed VVAR hook must decode");
        let notifier = realtime_timer_notifier().expect("installed timer notifier must decode");
        assert!(core::ptr::fn_addr_eq(
            publish,
            publish_snapshot as VvarPublishHook
        ));
        assert!(core::ptr::fn_addr_eq(
            notifier,
            notify_timerfd as RealtimeTimerNotifier
        ));

        publish(VvarSnapshot {
            realtime_generation: 0,
            cycle_last: 0,
            mask: 0,
            mult: 0,
            shift: 0,
            realtime_sec: 37,
            realtime_nsec_shifted: 0,
            monotonic_sec: 41,
            monotonic_nsec_shifted: 0,
        });
        let mut post = |_mailbox: &TaskMailbox, _event: MailboxEvent| false;
        assert_eq!(notifier(43, None, &mut post), 0);
        assert_eq!(PUBLISHED_REALTIME_SEC.load(Ordering::Acquire), 37);
        assert_eq!(PUBLISHED_MONOTONIC_SEC.load(Ordering::Acquire), 41);
        assert_eq!(TIMERFD_NOTIFICATION_GENERATION.load(Ordering::Acquire), 43);
    }

    #[test]
    fn timekeeper_owner_publishes_matching_realtime_generation_and_offset() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(1_100, Ordering::Relaxed);

        let report = timekeeper()
            .set_realtime_ns_with_persistent::<TestTime>(2_000, RealtimeWritebackPolicy::Disabled)
            .expect("realtime update should fit the offset range");
        let snapshot = timekeeper().snapshot_for_vvar::<TestTime>();

        assert_eq!(report.generation, timekeeper().realtime_generation());
        assert_eq!(snapshot.realtime_generation, report.generation);
        assert_eq!(timekeeper().realtime_offset_ns(), 900);
        assert_eq!(snapshot.realtime_sec, 0);
        assert!(snapshot.realtime_nsec_shifted >= 2_000);
    }

    #[test]
    fn vvar_snapshot_publication_uses_timekeeper_owned_payload() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(8_000_000_000, Ordering::Relaxed);
        PUBLISHED_REALTIME_SEC.store(0, Ordering::Release);
        PUBLISHED_MONOTONIC_SEC.store(0, Ordering::Release);
        install_vvar_publish_hook(publish_snapshot);

        timekeeper().set_clock_params(7, 3, !0);
        timekeeper().publish_vvar::<TestTime>();

        assert!(PUBLISHED_REALTIME_SEC.load(Ordering::Acquire) >= 1_779_494_408);
        assert_eq!(PUBLISHED_MONOTONIC_SEC.load(Ordering::Acquire), 8);
    }

    #[test]
    fn realtime_set_preserves_timerfd_notification_generation() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(12_000_000_000, Ordering::Relaxed);
        TIMERFD_NOTIFICATION_GENERATION.store(0, Ordering::Release);
        install_realtime_timer_notifier(notify_timerfd);

        let report = timekeeper()
            .set_realtime_ns_with_persistent::<TestTime>(
                1_800_000_012_000_000_000,
                RealtimeWritebackPolicy::Disabled,
            )
            .expect("realtime update succeeds");

        assert_eq!(
            TIMERFD_NOTIFICATION_GENERATION.load(Ordering::Acquire),
            report.generation
        );
    }

    #[test]
    fn callbacks_reenter_after_timekeeper_writer_unlocks() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(14_000_000_000, Ordering::Relaxed);
        REENTRANT_CALLBACKS.store(0, Ordering::Release);
        install_vvar_publish_hook(reentrant_publish_snapshot);
        install_realtime_timer_notifier(reentrant_timerfd_notifier);

        timekeeper()
            .set_realtime_ns_with_persistent::<TestTime>(
                1_800_000_014_000_000_000,
                RealtimeWritebackPolicy::Disabled,
            )
            .expect("realtime update succeeds");

        assert_eq!(REENTRANT_CALLBACKS.load(Ordering::Acquire), 2);
    }

    #[test]
    fn persistent_writeback_preserves_best_effort_and_required_policy() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(20_000_000_000, Ordering::Relaxed);
        TEST_PERSISTENT_SET_NS.store(0, Ordering::Release);

        let target = 1_800_000_020_000_000_000;
        let best_effort = timekeeper()
            .set_realtime_ns_with_persistent::<TestTime>(
                target,
                RealtimeWritebackPolicy::BestEffort,
            )
            .expect("best-effort mutation succeeds");
        assert!(best_effort.persistent_written);
        assert_eq!(TEST_PERSISTENT_SET_NS.load(Ordering::Acquire), target);

        let required = timekeeper()
            .set_realtime_ns_with_persistent::<FailingPersistentTime>(
                target,
                RealtimeWritebackPolicy::Required,
            )
            .expect_err("required writeback failure must propagate");
        assert_eq!(
            required,
            WallClockError::PersistentClock(PersistentClockError::Hardware)
        );
    }

    #[test]
    fn inverse_realtime_deadline_handles_i64_min_without_losing_one_nanosecond() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store((i64::MAX as u64) + 1, Ordering::Relaxed);

        timekeeper()
            .set_realtime_ns_with_persistent::<TestTime>(0, RealtimeWritebackPolicy::Disabled)
            .expect("i64::MIN offset is representable");

        assert_eq!(
            timekeeper()
                .try_monotonic_deadline_from_realtime_ns(0)
                .expect("exact inverse is representable"),
            (i64::MAX as u64) + 1
        );
        assert_eq!(
            timekeeper().try_monotonic_deadline_from_realtime_ns(u64::MAX),
            Err(TimeError::Range)
        );
    }

    #[test]
    fn invalid_vvar_shift_is_rejected_without_replacing_calibration() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(1_000_000_000, Ordering::Relaxed);

        timekeeper()
            .try_set_clock_params(7, 3, !0)
            .expect("valid calibration accepted");
        assert_eq!(
            timekeeper().try_set_clock_params(7, 35, !0),
            Err(TimeError::Invalid)
        );
        assert_eq!(timekeeper().snapshot_for_vvar::<TestTime>().shift, 3);
    }

    #[test]
    fn timekeeper_clock_implements_canonical_time_traits() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(40_000_000_000, Ordering::Relaxed);

        let clock = timekeeper_clock::<TestTime>();
        let report = RealtimeControl::set_realtime_ns(
            &clock,
            1_800_000_040_000_000_000,
            RealtimeSetPolicy::BestEffort,
        )
        .expect("canonical realtime control succeeds");

        assert!(ClockRead::realtime_now_ns(&clock) >= report.realtime_ns);
        assert_eq!(
            RealtimeControl::realtime_generation(&clock),
            report.generation
        );
        VvarPublisher::publish_vvar(&clock);
    }

    #[test]
    fn required_realtime_writeback_maps_to_time_error() {
        let _guard = TEST_CLOCK_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        TEST_NS.store(50_000_000_000, Ordering::Relaxed);

        let clock = timekeeper_clock::<FailingPersistentTime>();
        let err = RealtimeControl::set_realtime_ns(
            &clock,
            1_800_000_050_000_000_000,
            RealtimeSetPolicy::Required,
        )
        .expect_err("required writeback failure should surface");

        assert_eq!(err, TimeError::Hardware);
    }
}
