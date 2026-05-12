//! D6 §7 follow-up pin tests: `AgentTokenGuard.timer: Option<TimerGuard>`.
//!
//! Pins the integrated drop semantics introduced by the D6 §7
//! consolidation: `AgentTokenGuard` now owns an optional
//! `TimerGuard` directly, and `DelegateRegistry::install_request`
//! accepts an optional `(Deadline, &TimerWheel)` pair that mints
//! both registrations in a single call.
//!
//! Pins:
//!
//! - `install_request(deadline = None)`: the guard carries no
//!   timer; existing PR-7 / PR-7B drop semantics (CancelOnDrop CAS
//!   or Abandon no-op) are unchanged.
//! - `install_request(deadline = Some((d, &wheel)))`: the wheel
//!   gains a `DelegateTimeout` entry tagged with the freshly minted
//!   `DelegateTokenId`; the guard owns the resulting `TimerGuard`.
//! - **Drop order** — `TimerGuard` drops BEFORE the `mark_canceled`
//!   CAS:
//!     1. Drop the guard.
//!     2. Wheel entry is gone before step 3 runs.
//!     3. The state CAS to `Canceled` succeeds.
//!     4. A subsequent manual `fire_due_delegate_timeouts` walk
//!        observes no matching entry and is a no-op — it does NOT
//!        double-fire `mark_timed_out`.
//! - **DTOK-3 carry-through**: when the wheel fires first (before
//!   the guard drops), the registry CAS resolves the state as
//!   `TimedOut`; the guard's eventual `mark_canceled` returns
//!   `LateNoOp(TimedOut)`. Reply-vs-timeout race determinism is
//!   unchanged.
//!
//! txdoc cross-refs:
//! - `docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md` §7
//! - `docs/Txv3/05_DELEGATE_v1.md` §7 (DelegateTimeout fire path)
//! - `docs/Txv3/02_INVARIANTS_v5.md` DTOK-1, DTOK-2, DTOK-3

use std::sync::Arc;

use tx_substrate::step_v3::{
    AbortReason, AgentCancelPolicy, Deadline, DelegateRegistry, DelegateReply, DelegateRequest,
    DelegateState, TokenDropPolicy, TransitionOutcome,
};
use tx_substrate::wake::timer::{TimerGuardRole, TimerWheel};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

// ---------------------------------------------------------------------------
// 1. deadline = None: no timer guard installed.
// ---------------------------------------------------------------------------

#[test]
fn install_request_with_none_deadline_does_not_arm_a_timer() {
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    assert_eq!(wheel.armed_count(), 0, "no timer should be armed");
    assert_eq!(guard.state(), Some(DelegateState::Pending));
    let _ = guard.forget();
}

#[test]
fn install_request_with_none_deadline_drops_cleanly_on_cancel_on_drop() {
    let registry = DelegateRegistry::new();
    let id = {
        let guard = registry.install_request(
            DelegateRequest::Placeholder,
            0,
            AgentCancelPolicy::BestEffort,
            TokenDropPolicy::CancelOnDrop,
            std::sync::Weak::new(),
            None,
        );
        guard.id()
    };
    // CancelOnDrop with no paired timer behaves exactly as PR-7.
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
}

// ---------------------------------------------------------------------------
// 2. deadline = Some(...): timer is installed alongside the token.
// ---------------------------------------------------------------------------

#[test]
fn install_request_with_some_deadline_arms_the_paired_timer() {
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        Some((Deadline::from_raw(100), &wheel)),
    );
    assert_eq!(wheel.armed_count(), 1, "one DelegateTimeout entry armed");
    assert_eq!(guard.state(), Some(DelegateState::Pending));
    // `forget()` consumes the guard and suppresses the
    // registry-side cancel CAS, but the guard's `Drop` still
    // runs at end-of-`forget()` and the `timer` field's
    // `TimerGuard` drop retires the wheel entry. This is the
    // intentional drop ordering: the timer is bound to the
    // *guard's* lifetime, not the registry slot's.
    let _id = guard.forget();
    assert_eq!(
        wheel.armed_count(),
        0,
        "forget() runs Drop, which retires the paired wheel entry",
    );
}

#[test]
fn install_request_some_returns_guard_with_pending_state_and_token_id() {
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        Some((Deadline::from_raw(100), &wheel)),
    );
    let id = guard.id();
    // The wheel entry should be tagged with `id` — lookup by raw
    // token id requires walking, but we can pin via the fire path.
    drop(guard);
    // After Abandon-drop, the wheel entry is retired by the timer
    // guard's drop; the registry slot remains Pending.
    assert_eq!(wheel.armed_count(), 0);
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
}

// ---------------------------------------------------------------------------
// 3. Drop order — TimerGuard drops BEFORE the mark_canceled CAS.
// ---------------------------------------------------------------------------

#[test]
fn drop_retires_wheel_entry_before_mark_canceled_cas() {
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let id = {
        let guard = registry.install_request(
            DelegateRequest::Placeholder,
            0,
            AgentCancelPolicy::BestEffort,
            TokenDropPolicy::CancelOnDrop,
            Arc::downgrade(&mailbox),
            Some((Deadline::from_raw(100), &wheel)),
        );
        assert_eq!(wheel.armed_count(), 1, "timer is armed pre-drop");
        guard.id()
        // guard drops here:
        //   step 1: TimerGuard::drop  → wheel entry retired
        //   step 2: mark_canceled CAS → state = Canceled
    };

    // (a) state CAS to Canceled succeeded.
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
    // (b) wheel entry is gone *before* the CAS (so the wheel is
    //     now empty, and the CAS was the unambiguous last writer).
    assert_eq!(wheel.armed_count(), 0, "wheel entry retired");

    // (c) a subsequent manual fire walk MUST NOT double-fire
    //     mark_timed_out: there is no matching entry left.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(u64::MAX), &registry);
    assert_eq!(fired, 0, "no entry remains to fire");

    // State stays Canceled — Canceled → TimedOut is not a legal
    // transition (DTOK-1: terminal is terminal).
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));

    // The mailbox observed exactly one Abort{Canceled} from the
    // mark_canceled CAS (not a TimedOut from a stray late fire).
    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().unwrap();
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::Canceled,
        }
    );
}

#[test]
fn drop_with_abandon_policy_still_retires_wheel_entry() {
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let id = {
        let guard = registry.install_request(
            DelegateRequest::Placeholder,
            0,
            AgentCancelPolicy::Detached,
            TokenDropPolicy::Abandon,
            Arc::downgrade(&mailbox),
            Some((Deadline::from_raw(100), &wheel)),
        );
        assert_eq!(wheel.armed_count(), 1);
        guard.id()
    };

    // Abandon = no state CAS, but the timer guard still drops and
    // retires the wheel entry (drop-order step 1 always runs).
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
    assert_eq!(wheel.armed_count(), 0, "wheel entry retired by drop-step-1");
    assert!(mailbox.is_empty(), "Abandon posts no event");
}

// ---------------------------------------------------------------------------
// 4. DTOK-3 carry-through — wheel fires first, guard drops second.
// ---------------------------------------------------------------------------

#[test]
fn wheel_fires_before_drop_results_in_late_no_op_cancel() {
    // Scenario: fire_due_delegate_timeouts wins the race; the
    // guard's eventual drop must observe TimedOut and return
    // LateNoOp(TimedOut) from mark_canceled.
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        Arc::downgrade(&mailbox),
        Some((Deadline::from_raw(50), &wheel)),
    );
    let id = guard.id();

    // The wheel fires first.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(60), &registry);
    assert_eq!(fired, 1);
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));
    // Mailbox observed Abort{TimedOut} from the fire-applied CAS.
    assert_eq!(mailbox.len(), 1);

    // Drop the guard. Order: timer.take() is a no-op (wheel entry
    // already retired by the fire walk); mark_canceled CAS returns
    // LateNoOp(TimedOut) because the slot is already terminal.
    drop(guard);

    // State remains TimedOut — the CAS did not override it
    // (DTOK-3 race determinism: first writer wins).
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));

    // No extra mailbox event from the late-no-op cancel.
    assert_eq!(mailbox.len(), 1);
}

#[test]
fn wheel_fires_after_drop_finds_no_entry() {
    // Mirror scenario: guard drops first, fire walk happens later.
    // The walk must observe no matching entry.
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let id = {
        let guard = registry.install_request(
            DelegateRequest::Placeholder,
            0,
            AgentCancelPolicy::BestEffort,
            TokenDropPolicy::CancelOnDrop,
            Arc::downgrade(&mailbox),
            Some((Deadline::from_raw(50), &wheel)),
        );
        guard.id()
    };

    // Guard dropped: state = Canceled, wheel entry retired.
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));

    // A late fire walk past the (would-be) deadline finds nothing.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(60), &registry);
    assert_eq!(fired, 0);
    // State is unchanged.
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
}

// ---------------------------------------------------------------------------
// 5. DTOK-3 still holds: agent reply races wheel fire across paired install.
// ---------------------------------------------------------------------------

#[test]
fn agent_reply_wins_before_wheel_fire_carries_through_paired_install() {
    // The classic DTOK-3 scenario: mark_replied wins the race
    // before the wheel's fire walk runs. A subsequent fire walk
    // retires the entry but the mark_timed_out CAS is LateNoOp.
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        Some((Deadline::from_raw(50), &wheel)),
    );
    let id = guard.id();

    // Agent reply wins.
    let outcome = registry.mark_replied(id, DelegateReply::placeholder());
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    assert_eq!(mailbox.len(), 1, "AgentReplied posted on Applied");

    // Now fire the wheel — entry is still armed because the guard
    // hasn't dropped yet, but the state CAS will LateNoOp.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(60), &registry);
    assert_eq!(fired, 1, "entry was armed and retired");
    // State unchanged: Replied is terminal (DTOK-1).
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    // No extra event — late writer drops.
    assert_eq!(mailbox.len(), 1);

    // Drop the guard. Abandon: no state CAS. Timer slot is empty
    // (fire walk already retired it) so timer.take() drops a guard
    // whose wheel-cancel is a no-op (cancel() swap_remove on an
    // already-missing token is harmless — see TimerWheel::cancel).
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    assert_eq!(wheel.armed_count(), 0);
}

// ---------------------------------------------------------------------------
// 6. Role pin: paired timer carries the DelegateTimeout role.
// ---------------------------------------------------------------------------

#[test]
fn paired_timer_uses_delegate_timeout_role() {
    // The paired-install timer is tagged `DelegateTimeout` so the
    // wheel's `fire_due_delegate_timeouts` walk picks it up. We
    // pin the role by firing the entry while the guard (and thus
    // its TimerGuard) is still alive.
    let registry = DelegateRegistry::new();
    let wheel = TimerWheel::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        Some((Deadline::from_raw(100), &wheel)),
    );
    let id = guard.id();
    assert_eq!(wheel.armed_count(), 1);

    // Fire while the guard is alive — the fire walk only picks up
    // DelegateTimeout-role entries, so a non-zero `fired` count
    // proves the role tag.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(u64::MAX), &registry);
    assert_eq!(fired, 1, "DelegateTimeout-role entry fired");
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));

    // Drop the guard: timer.take() runs but the wheel entry was
    // already retired by the fire walk → TimerWheel::cancel is a
    // no-op (swap_remove on missing token). mark_canceled is not
    // called because policy is Abandon.
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));

    // Touch TimerGuardRole so the import isn't dead.
    let _: TimerGuardRole = TimerGuardRole::DelegateTimeout;
}
