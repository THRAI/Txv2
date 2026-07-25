//! Deadline delivery carriers independent of any queue algorithm.

/// Opaque identity carried between a deadline domain and mailbox routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerToken(u64);

impl TimerToken {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl From<crate::step::TimerId> for TimerToken {
    fn from(id: crate::step::TimerId) -> Self {
        Self(id.raw())
    }
}

impl From<TimerToken> for crate::step::TimerId {
    fn from(token: TimerToken) -> Self {
        crate::step::TimerId::new(token.raw())
    }
}

/// Semantic classification for deadline delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerGuardRole {
    PrimarySleep,
    DeadlineAbort,
    DelegateTimeout,
    DeviceEvent,
}
