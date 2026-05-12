//! PR-7B reactor-side glue pin tests: timer-tick → registry callback.
//!
//! Pins the reactor-side wiring that PR-7B layers on top of PR-8's
//! `TimerWheel`:
//!
//! - [`TimerWheel::install_delegate_timeout`] tags an entry with a
//!   `DelegateTokenId` and returns a [`TimerGuard`] whose drop
//!   cancels the registration (composes with the existing PR-8
//!   forget / drop semantics).
//! - [`TimerWheel::fire_due_delegate_timeouts`] walks expired
//!   `DelegateTimeout` entries and calls
//!   `DelegateRegistry::mark_timed_out(...)` on each. Entries with
//!   other roles, or with deadlines in the future, are not
//!   touched.
//! - Late fire (state already terminal) is a no-op — DTOK-3
//!   reply-vs-timeout race is resolved on the substrate side; the
//!   wheel just retires the entry.
//! - Multiple ticks: a previously-fired entry is removed from the
//!   wheel and never re-fires.
//!
//! txdoc cross-refs:
//! - `docs/Txv3/05_DELEGATE_v1.md` §7 (DelegateTimeout fire path)
//! - `docs/Txv3/07_BLAST_RADIUS.md` §4 row H (TimerGuardRole catalog)

use std::sync::Arc;

use tx_reactor::{TimerGuardRole, TimerWheel};
use tx_substrate::step_v3::{
    AbortReason, AgentCancelPolicy, Deadline, DelegateRegistry, DelegateReply, DelegateRequest,
    DelegateState, DelegateTokenId, TokenDropPolicy, TransitionOutcome,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

// ---------------------------------------------------------------------------
// 1. install_delegate_timeout shape pin.
// ---------------------------------------------------------------------------

#[test]
fn install_delegate_timeout_returns_guard_with_delegate_timeout_role() {
    let wheel = TimerWheel::new();
    let guard = wheel.install_delegate_timeout(Deadline::from_raw(100), DelegateTokenId::new(7));
    assert_eq!(guard.role(), TimerGuardRole::DelegateTimeout);
    assert_eq!(guard.deadline(), Deadline::from_raw(100));
    assert_eq!(wheel.armed_count(), 1);
}

#[test]
fn dropping_delegate_timeout_guard_cancels_registration() {
    let wheel = TimerWheel::new();
    {
        let _g = wheel.install_delegate_timeout(Deadline::from_raw(100), DelegateTokenId::new(1));
        assert_eq!(wheel.armed_count(), 1);
    }
    assert_eq!(wheel.armed_count(), 0);
}

// ---------------------------------------------------------------------------
// 2. fire_due_delegate_timeouts routes to mark_timed_out.
// ---------------------------------------------------------------------------

#[test]
fn fire_due_delegate_timeouts_calls_mark_timed_out_on_expired_entries() {
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let agent_guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = agent_guard.id();
    let timer_guard = wheel.install_delegate_timeout(Deadline::from_raw(50), id);

    // Before the tick, token is Pending.
    assert_eq!(registry.state(id), Some(DelegateState::Pending));

    // Tick at a time past the deadline → entry fires.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(60), &registry);
    assert_eq!(fired, 1, "the expired entry should fire");
    assert_eq!(wheel.armed_count(), 0, "fired entry retired from wheel");

    // The registry observed mark_timed_out → token TimedOut.
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));

    // The bound mailbox observed an Abort(TimedOut) event.
    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().unwrap();
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::TimedOut,
        }
    );

    // The timer guard, when dropped, is a no-op against the
    // already-fired entry. Suppress drop-cancel by forgetting.
    let _ = timer_guard.forget();
    let _ = agent_guard.forget();
}

#[test]
fn fire_due_does_not_fire_future_deadlines() {
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    let agent_guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = agent_guard.id();
    let _timer = wheel.install_delegate_timeout(Deadline::from_raw(1000), id);

    // Tick well before the deadline.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(500), &registry);
    assert_eq!(fired, 0);
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
    assert_eq!(wheel.armed_count(), 1);
    let _ = agent_guard.forget();
}

#[test]
fn fire_due_ignores_non_delegate_timeout_roles() {
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    // Install other roles; fire_due_delegate_timeouts must skip them.
    let _g_primary = wheel.install(Deadline::from_raw(10), TimerGuardRole::PrimarySleep);
    let _g_abort = wheel.install(Deadline::from_raw(10), TimerGuardRole::DeadlineAbort);
    assert_eq!(wheel.armed_count(), 2);

    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(1_000_000), &registry);
    assert_eq!(
        fired, 0,
        "non-DelegateTimeout entries are not fired by this path"
    );
    assert_eq!(wheel.armed_count(), 2);
}

// ---------------------------------------------------------------------------
// 3. Late fire (state already terminal) is a no-op.
// ---------------------------------------------------------------------------

#[test]
fn late_fire_against_already_replied_token_is_no_op() {
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let agent_guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = agent_guard.id();
    let _timer = wheel.install_delegate_timeout(Deadline::from_raw(50), id);

    // Reply wins before the timer fires.
    assert_eq!(
        registry.mark_replied(id, DelegateReply::placeholder()),
        TransitionOutcome::Applied
    );
    assert_eq!(mailbox.len(), 1); // AgentReplied posted.

    // Now fire the late timer. The wheel retires the entry; the
    // registry CAS observes Replied and returns LateNoOp. The
    // mailbox queue does not grow.
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(100), &registry);
    assert_eq!(fired, 1, "wheel retires the expired entry");
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    assert_eq!(mailbox.len(), 1, "late timeout posts no second event");
    let _ = agent_guard.forget();
}

// ---------------------------------------------------------------------------
// 4. Multiple ticks: a fired entry never re-fires.
// ---------------------------------------------------------------------------

#[test]
fn fired_entry_does_not_re_fire_on_subsequent_tick() {
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    let agent_guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = agent_guard.id();
    let _timer = wheel.install_delegate_timeout(Deadline::from_raw(50), id);

    let fired_a = wheel.fire_due_delegate_timeouts(Deadline::from_raw(60), &registry);
    let fired_b = wheel.fire_due_delegate_timeouts(Deadline::from_raw(70), &registry);
    assert_eq!(fired_a, 1);
    assert_eq!(fired_b, 0);
    assert_eq!(wheel.armed_count(), 0);
    let _ = agent_guard.forget();
}

// ---------------------------------------------------------------------------
// 5. Walk multiple expired entries in one tick.
// ---------------------------------------------------------------------------

#[test]
fn fire_due_walks_all_expired_delegate_timeouts_in_one_tick() {
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    let g1 = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let g2 = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id1 = g1.id();
    let id2 = g2.id();

    let _t1 = wheel.install_delegate_timeout(Deadline::from_raw(10), id1);
    let _t2 = wheel.install_delegate_timeout(Deadline::from_raw(20), id2);
    // Add a future entry that must NOT fire.
    let g3 = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id3 = g3.id();
    let _t3 = wheel.install_delegate_timeout(Deadline::from_raw(1_000_000), id3);

    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(30), &registry);
    assert_eq!(fired, 2);
    assert_eq!(registry.state(id1), Some(DelegateState::TimedOut));
    assert_eq!(registry.state(id2), Some(DelegateState::TimedOut));
    assert_eq!(registry.state(id3), Some(DelegateState::Pending));
    assert_eq!(wheel.armed_count(), 1);

    let _ = g1.forget();
    let _ = g2.forget();
    let _ = g3.forget();
}

// ---------------------------------------------------------------------------
// 6. install (the generic constructor) issues entries with no delegate
//    tagging, so they are not consumed by fire_due_delegate_timeouts even
//    when the role happens to be DelegateTimeout (the tag is what routes,
//    not the role alone).
// ---------------------------------------------------------------------------

#[test]
fn generic_install_with_delegate_role_is_not_routed_by_fire_due() {
    // PR-7B contract: routing requires the per-entry token tag
    // (set by `install_delegate_timeout`). A bare
    // `install(deadline, DelegateTimeout)` does not get routed —
    // there's no `DelegateTokenId` to pass to `mark_timed_out`.
    let wheel = TimerWheel::new();
    let registry = DelegateRegistry::new();
    let _g = wheel.install(Deadline::from_raw(10), TimerGuardRole::DelegateTimeout);
    let fired = wheel.fire_due_delegate_timeouts(Deadline::from_raw(1_000_000), &registry);
    assert_eq!(fired, 0);
    assert_eq!(wheel.armed_count(), 1);
}
