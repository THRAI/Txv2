//! HAL-facing time adapters.

use core::marker::PhantomData;

use tx_hal::{DeadlineTimerIf, MonotonicCounterIf, PersistentClockIf, VdsoCounterInfo};

use crate::driver::CurrentHartDeadlineTimer;
use crate::rtc::RtcDeviceOps;
use crate::types::TimeError;

/// Minimal monotonic HAL clock adapter.
pub struct HalMonotonicClock<P>(PhantomData<P>);

impl<P> HalMonotonicClock<P> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<P> Default for HalMonotonicClock<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: MonotonicCounterIf> HalMonotonicClock<P> {
    pub fn monotonic_now_ns(&self) -> u64 {
        P::read_ns()
    }

    pub fn vdso_counter_info(&self) -> Option<VdsoCounterInfo> {
        P::vdso_counter_info()
    }

    pub fn read_vdso_counter(&self) -> u64 {
        P::read_vdso_counter()
    }
}

/// Current-hart deadline timer adapter.
pub struct HalDeadlineTimer<P>(PhantomData<P>);

impl<P> HalDeadlineTimer<P> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<P> Default for HalDeadlineTimer<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: DeadlineTimerIf> HalDeadlineTimer<P> {
    pub fn set_deadline_ns(&self, deadline_ns: u64) {
        P::set_deadline_ns(deadline_ns);
    }

    pub fn cancel_deadline(&self) {
        P::cancel_deadline();
    }

    pub fn enable_timer_wakeups(&self) {
        P::enable_timer_wakeups();
    }
}

impl<P: DeadlineTimerIf> CurrentHartDeadlineTimer for HalDeadlineTimer<P> {
    fn set_current_hart_deadline_ns(&mut self, deadline_ns: u64) {
        self.set_deadline_ns(deadline_ns);
    }

    fn cancel_current_hart_deadline(&mut self) {
        self.cancel_deadline();
    }
}

/// Persistent-clock adapter for RTC typed device backends.
pub struct HalRtcDevice<P>(PhantomData<P>);

impl<P> HalRtcDevice<P> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<P> Default for HalRtcDevice<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: PersistentClockIf> RtcDeviceOps for HalRtcDevice<P> {
    fn read_time_ns(&self) -> Result<u64, TimeError> {
        P::read_realtime_ns().map_err(TimeError::from)
    }

    fn set_time_ns(&self, ns: u64) -> Result<(), TimeError> {
        P::set_realtime_ns(ns).map_err(TimeError::from)
    }

    fn set_alarm_ns(&self, ns: u64) -> Result<(), TimeError> {
        P::set_wake_alarm_ns(ns).map_err(TimeError::from)
    }

    fn clear_alarm(&self) -> Result<(), TimeError> {
        P::clear_wake_alarm().map_err(TimeError::from)
    }

    fn acknowledge_alarm_irq(&self) -> Result<(), TimeError> {
        P::acknowledge_wake_alarm_irq().map_err(TimeError::from)
    }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;
    use core::sync::atomic::{AtomicU64, Ordering};

    use tx_hal::{DeadlineTimerIf, PersistentClockError, PersistentClockIf};

    use super::{HalDeadlineTimer, HalRtcDevice, RtcDeviceOps};

    struct StaticPlatform;

    static DEADLINE_NS: AtomicU64 = AtomicU64::new(0);
    static RTC_NS: AtomicU64 = AtomicU64::new(0);

    impl DeadlineTimerIf for StaticPlatform {
        fn set_deadline_ns(deadline: u64) {
            DEADLINE_NS.store(deadline, Ordering::Release);
        }

        fn cancel_deadline() {
            DEADLINE_NS.store(0, Ordering::Release);
        }
    }

    impl PersistentClockIf for StaticPlatform {
        fn read_realtime_ns() -> Result<u64, PersistentClockError> {
            Ok(RTC_NS.load(Ordering::Acquire))
        }
    }

    #[test]
    fn deadline_timer_default_matches_new_static_adapter() {
        let default = HalDeadlineTimer::<StaticPlatform>::default();
        let new = HalDeadlineTimer::<StaticPlatform>::new();

        assert_eq!(size_of_val(&default), 0);
        assert_eq!(size_of_val(&new), 0);
        default.set_deadline_ns(11);
        assert_eq!(DEADLINE_NS.load(Ordering::Acquire), 11);
        new.cancel_deadline();
        assert_eq!(DEADLINE_NS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn rtc_device_default_matches_new_static_adapter() {
        RTC_NS.store(23, Ordering::Release);
        let default = HalRtcDevice::<StaticPlatform>::default();
        let new = HalRtcDevice::<StaticPlatform>::new();

        assert_eq!(size_of_val(&default), 0);
        assert_eq!(size_of_val(&new), 0);
        assert_eq!(default.read_time_ns(), Ok(23));
        assert_eq!(new.read_time_ns(), Ok(23));
    }
}
