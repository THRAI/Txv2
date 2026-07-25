//! Shared time facade types.

/// Kernel-supported clock ids at the time-service facade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockId {
    Realtime,
    Monotonic,
    Boottime,
    Tai,
    ProcessCpuTime,
    ThreadCpuTime,
}

/// Monotonic deadline in nanoseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DeadlineNs(u64);

impl DeadlineNs {
    pub const fn new(ns: u64) -> Self {
        Self(ns)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Opaque timer identity reserved for the future standalone `TimerEngine`.
///
/// Phase 1 keeps this separate from [`crate::TimerToken`]. `TimerToken` is
/// still the compatibility event identity exposed by the substrate-backed
/// registrar; the two identities are not interchangeable until the later
/// timer-engine migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerKey(u64);

impl TimerKey {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Typed error for time facade operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeError {
    Unsupported,
    Invalid,
    Range,
    Hardware,
    Unavailable,
}

impl From<tx_hal::PersistentClockError> for TimeError {
    fn from(value: tx_hal::PersistentClockError) -> Self {
        match value {
            tx_hal::PersistentClockError::Unsupported => Self::Unsupported,
            tx_hal::PersistentClockError::Invalid => Self::Invalid,
            tx_hal::PersistentClockError::Range => Self::Range,
            tx_hal::PersistentClockError::Hardware => Self::Hardware,
        }
    }
}
