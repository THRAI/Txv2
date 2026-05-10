//! v3 wait-protocol catalog pin tests.
//!
//! These tests pin the closed-catalog shapes for `WaitProtocol` and
//! `WaitOutcome`, which are referenced by the reactor's yield-resolution
//! machinery (later PRs wire them into `YieldShape` resolution). Wave 2
//! lands the catalogs themselves so subsequent migration PRs cannot
//! silently widen them or shift the helper-method semantics.
//!
//! txdoc cross-refs (canonical anchors from `docs/Txv3/03_STEP_MODEL_v2.md`):
//! - txdoc:TXV3-STEP-MODEL-V2 (entire algebra; wait protocol is referenced
//!   by the yield-resolution context)

use tx_substrate::step_v3::{WaitOutcome, WaitProtocol};

// -- WaitProtocol closed catalog ---------------------------------------------

#[test]
fn wait_protocol_has_exactly_five_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a sixth variant
    // appears later without an ARCH-3 review, this stops compiling.
    let cases: [WaitProtocol; 5] = [
        WaitProtocol::Uninterruptible,
        WaitProtocol::Interruptible,
        WaitProtocol::Killable,
        WaitProtocol::InterruptibleTimeout,
        WaitProtocol::KillableTimeout,
    ];

    for protocol in cases {
        match protocol {
            WaitProtocol::Uninterruptible => {}
            WaitProtocol::Interruptible => {}
            WaitProtocol::Killable => {}
            WaitProtocol::InterruptibleTimeout => {}
            WaitProtocol::KillableTimeout => {}
        }
    }
}

// -- WaitOutcome closed catalog ----------------------------------------------

#[test]
fn wait_outcome_has_exactly_four_variants_via_exhaustive_match() {
    let cases: [WaitOutcome; 4] = [
        WaitOutcome::Ready,
        WaitOutcome::Interrupted,
        WaitOutcome::Killed,
        WaitOutcome::TimedOut,
    ];

    for outcome in cases {
        match outcome {
            WaitOutcome::Ready => {}
            WaitOutcome::Interrupted => {}
            WaitOutcome::Killed => {}
            WaitOutcome::TimedOut => {}
        }
    }
}

// -- WaitProtocol helper-method tables ---------------------------------------

#[test]
fn wait_protocol_permits_signals_table() {
    // Uninterruptible blocks all signal-driven wakeups; everything else
    // permits at least some signal-driven wakeup (Killable: SIGKILL only,
    // but the per-signal filter is downstream).
    let cases: &[(WaitProtocol, bool)] = &[
        (WaitProtocol::Uninterruptible, false),
        (WaitProtocol::Interruptible, true),
        (WaitProtocol::Killable, true),
        (WaitProtocol::InterruptibleTimeout, true),
        (WaitProtocol::KillableTimeout, true),
    ];
    for &(protocol, expected) in cases {
        assert_eq!(
            protocol.permits_signals(),
            expected,
            "permits_signals broke for {protocol:?}",
        );
    }
}

#[test]
fn wait_protocol_permits_kill_table() {
    // Every protocol except Uninterruptible can be killed.
    let cases: &[(WaitProtocol, bool)] = &[
        (WaitProtocol::Uninterruptible, false),
        (WaitProtocol::Interruptible, true),
        (WaitProtocol::Killable, true),
        (WaitProtocol::InterruptibleTimeout, true),
        (WaitProtocol::KillableTimeout, true),
    ];
    for &(protocol, expected) in cases {
        assert_eq!(
            protocol.permits_kill(),
            expected,
            "permits_kill broke for {protocol:?}",
        );
    }
}

#[test]
fn wait_protocol_has_deadline_table() {
    // Only the *Timeout variants carry a deadline.
    let cases: &[(WaitProtocol, bool)] = &[
        (WaitProtocol::Uninterruptible, false),
        (WaitProtocol::Interruptible, false),
        (WaitProtocol::Killable, false),
        (WaitProtocol::InterruptibleTimeout, true),
        (WaitProtocol::KillableTimeout, true),
    ];
    for &(protocol, expected) in cases {
        assert_eq!(
            protocol.has_deadline(),
            expected,
            "has_deadline broke for {protocol:?}",
        );
    }
}

// -- WaitOutcome helper-method table -----------------------------------------

#[test]
fn wait_outcome_is_terminal_table() {
    // Killed and TimedOut are terminal; Ready completes the wait
    // successfully, Interrupted means the wait was disturbed and the
    // operation can be retried.
    let cases: &[(WaitOutcome, bool)] = &[
        (WaitOutcome::Ready, false),
        (WaitOutcome::Interrupted, false),
        (WaitOutcome::Killed, true),
        (WaitOutcome::TimedOut, true),
    ];
    for &(outcome, expected) in cases {
        assert_eq!(
            outcome.is_terminal(),
            expected,
            "is_terminal broke for {outcome:?}",
        );
    }
}
