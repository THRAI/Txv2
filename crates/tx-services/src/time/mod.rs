//! Time service facade.
//!
//! This module is the public service boundary for time consumers. It exposes
//! role-shaped capabilities instead of the concrete HAL timer traits or the
//! concrete wake-substrate timer wheel.

pub mod clock;
pub mod deadline;
pub mod driver;
pub mod platform;
pub mod realtime;
pub mod rtc;
pub mod types;
pub mod wall_clock;

#[cfg(feature = "test-support")]
pub use tx_time::reset_for_test;
pub use tx_time::{
    install_realtime_timer_notifier, install_vvar_publish_hook, timekeeper, timekeeper_clock,
    ClockId, ClockRead, CurrentHartDeadlineTimer, DeadlineDomain, DeadlineNs, DeadlineRegistrar,
    DeadlineRegistrarHandle, DeviceTimerCallback, RealtimeControl, RealtimeSeedError,
    RealtimeSetPolicy, RealtimeSetReport, RealtimeTimerNotifier, RealtimeWritebackPolicy,
    RtcDeviceOps, TimeError, Timekeeper, TimekeeperClock, TimekeeperIf, TimerGuard, TimerKey,
    TimerRole, TimerTarget, TimerToken, VvarPublishHook, VvarPublisher, VvarSnapshot,
    WallClockError, DEFAULT_REALTIME_EPOCH_BASE_NS,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_facade_reexports_tx_time_types() {
        let _: tx_time::DeadlineNs = DeadlineNs::new(7);
        let _: Option<tx_time::TimerGuard> = None;
        let _: tx_time::TimerKey = TimerKey::new(11);
        assert_eq!(ClockId::Monotonic, tx_time::ClockId::Monotonic);
        assert_eq!(TimeError::Unsupported, tx_time::TimeError::Unsupported);
    }
}
