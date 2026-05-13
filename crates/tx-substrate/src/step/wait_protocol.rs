//! Closed catalogs for the reactor wait machinery.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md`: the protocol governing how a
//! `YieldShape` resolution behaves under signal delivery / cancellation
//! is one of five members; the resolution outcome is one of four. Both
//! catalogs are ARCH-3-gated; extension requires architecture review.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (referenced by yield resolution)

/// Closed catalog of wait protocols. Selected per yield-resolution
/// site by the driver; the reactor honours the protocol when delivering
/// signals or deadline expiry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitProtocol {
    /// No signal delivery, no deadline. The wait can only complete by
    /// the carrier becoming ready (or, for `OnAgent`, the agent reply).
    Uninterruptible,
    /// Any signal disturbs the wait — the resumer sees `Interrupted`.
    Interruptible,
    /// Only SIGKILL disturbs the wait; other signals are deferred until
    /// the wait resolves naturally.
    Killable,
    /// `Interruptible` plus a deadline; expiry yields `TimedOut`.
    InterruptibleTimeout,
    /// `Killable` plus a deadline; expiry yields `TimedOut`.
    KillableTimeout,
}

impl WaitProtocol {
    /// Returns `true` if the protocol permits at least some
    /// signal-driven wakeup. The per-signal filter (which signals
    /// actually disturb the wait) is downstream.
    pub const fn permits_signals(&self) -> bool {
        !matches!(self, WaitProtocol::Uninterruptible)
    }

    /// Returns `true` if the protocol permits SIGKILL to disturb the
    /// wait. Every protocol except `Uninterruptible` permits kill.
    pub const fn permits_kill(&self) -> bool {
        !matches!(self, WaitProtocol::Uninterruptible)
    }

    /// Returns `true` if the protocol carries a deadline; only the
    /// `*Timeout` members do.
    pub const fn has_deadline(&self) -> bool {
        matches!(
            self,
            WaitProtocol::InterruptibleTimeout | WaitProtocol::KillableTimeout
        )
    }
}

/// Closed catalog of wait resolution outcomes. The driver receives
/// exactly one of these when a `YieldShape` resolution returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    /// Carrier (or agent reply) signalled readiness.
    Ready,
    /// Wait was disturbed by a non-kill signal; the operation may be
    /// retried.
    Interrupted,
    /// Wait was disturbed by SIGKILL; the operation must terminate.
    Killed,
    /// Deadline expired before the wait resolved.
    TimedOut,
}

impl WaitOutcome {
    /// Returns `true` if the outcome ends the operation. `Killed` and
    /// `TimedOut` are terminal; `Ready` completes successfully and
    /// `Interrupted` means the operation can be retried.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, WaitOutcome::Killed | WaitOutcome::TimedOut)
    }
}
