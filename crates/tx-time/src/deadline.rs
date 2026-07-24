//! Consumer-facing deadline registration capability.

use alloc::sync::{Arc, Weak};

use tx_substrate::bus::RawQueue;
use tx_substrate::step::{DelegateTokenId, InterestMask, WaitSourceId};
use tx_substrate::wake::deadline::TimerGuardRole;
use tx_substrate::wake::TaskMailbox;

use crate::types::{DeadlineNs, TimeError};

/// Compatibility event identity from the substrate-backed registrar.
///
/// `TimerKey` is reserved for the future standalone `TimerEngine`; this token
/// remains the public event identity until the Phase 3 timer migration.
pub use tx_substrate::wake::deadline::TimerToken;

/// Device-owned timer callback descriptor exposed through the time service.
#[derive(Clone)]
pub struct DeviceTimerCallback {
    function: fn(u64),
    payload: u64,
    wait_source_wake: Option<(WaitSourceId, InterestMask)>,
    raw_queue_wake: Option<(RawQueue, u64)>,
}

impl DeviceTimerCallback {
    pub const fn new(function: fn(u64), payload: u64) -> Self {
        Self {
            function,
            payload,
            wait_source_wake: None,
            raw_queue_wake: None,
        }
    }

    pub fn with_wait_source_wake(mut self, source: WaitSourceId, interests: InterestMask) -> Self {
        self.wait_source_wake = Some((source, interests));
        self
    }

    pub fn with_raw_queue_wake(mut self, queue: RawQueue, interests: u64) -> Self {
        self.raw_queue_wake = Some((queue, interests));
        self
    }

    pub fn fire(&self) {
        (self.function)(self.payload);
    }

    pub fn wait_source_wake(&self) -> Option<(WaitSourceId, InterestMask)> {
        self.wait_source_wake
    }

    pub fn raw_queue_wake(&self) -> Option<(RawQueue, u64)> {
        self.raw_queue_wake.clone()
    }
}

/// Service-facing RAII registration guard.
#[must_use = "drop the guard to cancel the timer; binding to _ cancels immediately"]
pub struct TimerGuard {
    domain: Option<Arc<dyn DeadlineDomain>>,
    token: TimerToken,
}

impl TimerGuard {
    fn from_domain(domain: Arc<dyn DeadlineDomain>, token: TimerToken) -> Self {
        Self {
            domain: Some(domain),
            token,
        }
    }

    pub fn token(&self) -> TimerToken {
        self.token
    }

    pub fn forget(mut self) -> TimerToken {
        self.domain.take();
        self.token
    }
}

impl Drop for TimerGuard {
    fn drop(&mut self) {
        if let Some(domain) = self.domain.take() {
            let _ = domain.cancel_deadline(self.token);
        }
    }
}

/// Semantic role for a time-service deadline registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerRole {
    PrimarySleep,
    DeadlineAbort,
    TimerFd,
    ItimerReal,
    PosixTimer,
    FutexTimeout,
    PollTimeout,
    DelegateTimeout,
    DeviceEvent,
    RtcAlarm,
}

impl TimerRole {
    pub const fn guard_role(self) -> TimerGuardRole {
        match self {
            Self::PrimarySleep => TimerGuardRole::PrimarySleep,
            Self::DeadlineAbort
            | Self::TimerFd
            | Self::ItimerReal
            | Self::PosixTimer
            | Self::FutexTimeout
            | Self::PollTimeout
            | Self::RtcAlarm => TimerGuardRole::DeadlineAbort,
            Self::DelegateTimeout => TimerGuardRole::DelegateTimeout,
            Self::DeviceEvent => TimerGuardRole::DeviceEvent,
        }
    }
}

/// Wake target for a registered deadline.
pub enum TimerTarget {
    TaskMailbox(Weak<TaskMailbox>),
    SignalTarget {
        mailbox: Weak<TaskMailbox>,
    },
    DelegateToken(DelegateTokenId),
    WaitSource {
        source: WaitSourceId,
        interests: InterestMask,
    },
    DeviceCallback(DeviceTimerCallback),
}

/// Producer-facing deadline registration capability.
pub trait DeadlineRegistrar {
    fn register_deadline(
        &self,
        deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerGuard, TimeError>;

    fn rearm_deadline(
        &self,
        guard: &mut Option<TimerGuard>,
        deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<(), TimeError> {
        *guard = Some(self.register_deadline(deadline_ns, role, target)?);
        Ok(())
    }
}

/// Stable registration/cancellation boundary for an owning timer domain.
///
/// The domain owns deadline ordering and delivery routing. The compatibility
/// handle only retains the returned token so dropping its guard cancels in the
/// same domain that accepted the registration.
pub trait DeadlineDomain: Send + Sync {
    fn register_deadline(
        &self,
        deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError>;

    fn cancel_deadline(&self, token: TimerToken) -> bool;

    fn rearm_deadline(&self, _token: TimerToken, _deadline_ns: DeadlineNs) -> bool {
        false
    }
}

/// Cloneable capability supplied by a reactor-owned deadline domain.
#[derive(Clone)]
pub struct DeadlineRegistrarHandle {
    domain: Arc<dyn DeadlineDomain>,
}

impl DeadlineRegistrarHandle {
    pub fn from_domain(domain: Arc<dyn DeadlineDomain>) -> Self {
        Self { domain }
    }
}

impl DeadlineRegistrar for DeadlineRegistrarHandle {
    fn register_deadline(
        &self,
        deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerGuard, TimeError> {
        let token = self.domain.register_deadline(deadline_ns, role, target)?;
        Ok(TimerGuard::from_domain(Arc::clone(&self.domain), token))
    }

    fn rearm_deadline(
        &self,
        guard: &mut Option<TimerGuard>,
        deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<(), TimeError> {
        if let Some(existing) = guard.as_ref() {
            if self.domain.rearm_deadline(existing.token(), deadline_ns) {
                return Ok(());
            }
        }
        *guard = Some(self.register_deadline(deadline_ns, role, target)?);
        Ok(())
    }
}
