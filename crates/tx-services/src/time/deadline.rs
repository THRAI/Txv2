//! Compatibility re-exports for the standalone `tx-time` API.

pub use tx_time::deadline::{
    DeadlineDomain, DeadlineRegistrar, DeadlineRegistrarHandle, DeviceTimerCallback, TimerGuard,
    TimerRole, TimerTarget, TimerToken,
};
