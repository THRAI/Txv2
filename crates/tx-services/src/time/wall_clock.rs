//! Compatibility path for the moved `tx-time` timekeeper.
//!
//! New code should import these names from `tx_time`. This module remains so
//! existing `tx_services::time::wall_clock::*` consumers do not need to move
//! during Phase 2.

pub use tx_time::{
    install_realtime_timer_notifier, install_vvar_publish_hook, timekeeper, timekeeper_clock,
    RealtimeSeedError, RealtimeSetReport, RealtimeTimerNotifier, RealtimeWritebackPolicy,
    Timekeeper, TimekeeperClock, TimekeeperIf, VvarPublishHook, VvarSnapshot, WallClockError,
    DEFAULT_REALTIME_EPOCH_BASE_NS,
};

#[cfg(feature = "test-support")]
pub use tx_time::reset_for_test;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_wall_clock_paths_forward_to_tx_time() {
        let _: tx_time::Timekeeper = timekeeper();
        let _: tx_time::TimekeeperClock<()> = timekeeper_clock();
        let _: Option<VvarSnapshot> = None;
        assert_eq!(
            DEFAULT_REALTIME_EPOCH_BASE_NS,
            tx_time::DEFAULT_REALTIME_EPOCH_BASE_NS
        );
    }
}
