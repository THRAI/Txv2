//! PR-7 OnAgent delegate-runtime pin tests.
//!
//! Pins the runtime state machine introduced by PR-7 of the v3 TDD
//! migration plan:
//!
//! - [`DelegateState`] catalog and `is_terminal` discipline
//! - [`DelegateRegistry::install_request`] mints a `Pending` token
//!   that carries the requested [`AgentCancelPolicy`] /
//!   [`TokenDropPolicy`].
//! - State-transition graph CAS-only: `Pending → ReplyInstalling →
//!   Replied`, `Pending → Canceled`, `Pending → AgentDied`,
//!   `Pending → TimedOut`. **`Replied → TimedOut` is NOT
//!   permitted** (DTOK-1, DTOK-3).
//! - DTOK-3 reply-vs-timeout race determinism: first writer wins,
//!   the loser observes `LateNoOp(<winner>)`.
//! - [`AgentTokenGuard`] drop semantics matrix: `CancelOnDrop`
//!   triggers `delegate cancel transition`; `Abandon` is a pure unbind.
//! - `delegate endpoint-death transition` walks all tokens with a matching marker
//!   (DELEGATE-3, DTOK-2).
//! - [`TokenDropPolicy::from_agent_cancel`] derivation: BestEffort
//!   / Synchronous → CancelOnDrop; Detached → Abandon.
//!
//! txdoc cross-refs:
//! - `docs/Txv3/05_DELEGATE_v1.md` §4, §6, §7
//! - `docs/Txv3/02_INVARIANTS_v5.md` DTOK-1, DTOK-2, DTOK-3,
//!   DELEGATE-3, DELEGATE-6

use tx_substrate::step::{
    AgentCancelPolicy, DelegateRegistry, DelegateReply, DelegateRequest, DelegateState,
    DelegateTokenId, TokenDropPolicy, TransitionOutcome,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

fn direct_delegate_mailbox_post(mailbox: std::sync::Weak<TaskMailbox>, event: MailboxEvent) {
    if let Some(mailbox) = mailbox.upgrade() {
        let _ = mailbox.post(event);
    }
}

// ---------------------------------------------------------------------------
// 1. DelegateState catalog and terminal classification.
// ---------------------------------------------------------------------------

#[test]
fn delegate_state_has_exactly_six_variants_via_exhaustive_match() {
    // Closed catalog per docs/Txv3/05_DELEGATE_v1.md §4. The
    // absence of a wildcard arm is the test: if a seventh variant
    // appears without an ARCH-3 review, this stops compiling.
    let cases: [DelegateState; 6] = [
        DelegateState::Pending,
        DelegateState::ReplyInstalling,
        DelegateState::Replied,
        DelegateState::Canceled,
        DelegateState::AgentDied,
        DelegateState::TimedOut,
    ];
    for state in cases {
        match state {
            DelegateState::Pending => {}
            DelegateState::ReplyInstalling => {}
            DelegateState::Replied => {}
            DelegateState::Canceled => {}
            DelegateState::AgentDied => {}
            DelegateState::TimedOut => {}
        }
    }
}

#[test]
fn pending_and_reply_installing_are_non_terminal() {
    assert!(!DelegateState::Pending.is_terminal());
    assert!(!DelegateState::ReplyInstalling.is_terminal());
}

#[test]
fn replied_canceled_agent_died_timed_out_are_terminal() {
    assert!(DelegateState::Replied.is_terminal());
    assert!(DelegateState::Canceled.is_terminal());
    assert!(DelegateState::AgentDied.is_terminal());
    assert!(DelegateState::TimedOut.is_terminal());
}

// ---------------------------------------------------------------------------
// 2. install_request mints a Pending token and round-trips policy.
// ---------------------------------------------------------------------------

#[test]
fn install_request_returns_a_pending_token() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    assert_eq!(guard.state(), Some(DelegateState::Pending));
    assert_eq!(registry.tracked_count(), 1);
    // Bind to a name to suppress the must-use drop-immediately
    // semantics; we want the guard alive across the assertion.
    drop(guard);
}

#[test]
fn install_request_round_trips_policies() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::Synchronous,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
        None,
    );
    assert_eq!(guard.cancel_policy(), AgentCancelPolicy::Synchronous);
    assert_eq!(guard.drop_policy(), TokenDropPolicy::CancelOnDrop);
    let _ = guard.forget();
}

#[test]
fn install_request_issues_monotonic_ids() {
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
    assert_ne!(g1.id(), g2.id());
    assert!(g2.id().raw() > g1.id().raw());
    drop(g1);
    drop(g2);
}

// ---------------------------------------------------------------------------
// 3. State-transition graph: legal paths.
// ---------------------------------------------------------------------------

#[test]
fn pending_to_replied_path_installs_reply() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();

    let outcome = registry.mark_replied_with_post(
        id,
        DelegateReply::placeholder(),
        direct_delegate_mailbox_post,
    );
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(id), Some(DelegateState::Replied));

    // Driver takes the reply (DELEGATE-V1 §7 step 5).
    let reply = registry.take_reply(id);
    assert_eq!(reply, Some(DelegateReply::placeholder()));
    // take_reply consumes — subsequent calls return None.
    assert_eq!(registry.take_reply(id), None);
    // State remains Replied.
    assert_eq!(registry.state(id), Some(DelegateState::Replied));

    let _ = guard.forget();
}

#[test]
fn pending_to_timed_out_is_legal() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    let outcome = registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post);
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));
    let _ = guard.forget();
}

#[test]
fn pending_to_canceled_is_legal() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    let outcome = registry.mark_canceled_with_post(id, direct_delegate_mailbox_post);
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
    let _ = guard.forget();
}

#[test]
fn pending_to_agent_died_is_legal() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    let outcome = registry.mark_agent_died_with_post(id, direct_delegate_mailbox_post);
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(id), Some(DelegateState::AgentDied));
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 4. State-transition graph: illegal paths land as LateNoOp (DTOK-1).
// ---------------------------------------------------------------------------

#[test]
fn replied_to_timed_out_is_not_permitted() {
    // The load-bearing DTOK-1 / DTOK-3 invariant: once Replied,
    // the token is terminal. A late timer fire CASes against
    // Pending, observes Replied, and reports LateNoOp(Replied).
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();

    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied
    );
    assert_eq!(
        registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::LateNoOp(DelegateState::Replied)
    );
    // State unchanged.
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    let _ = guard.forget();
}

#[test]
fn timed_out_blocks_subsequent_reply_as_late() {
    // The mirror direction of DTOK-1: once TimedOut wins, a later
    // reply is rejected as LateReply.
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();

    assert_eq!(
        registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::Applied
    );
    let outcome = registry.mark_replied_with_post(
        id,
        DelegateReply::placeholder(),
        direct_delegate_mailbox_post,
    );
    assert_eq!(
        outcome,
        TransitionOutcome::LateNoOp(DelegateState::TimedOut)
    );
    // No reply slot installed.
    assert_eq!(registry.take_reply(id), None);
    let _ = guard.forget();
}

#[test]
fn canceled_blocks_subsequent_terminal_transitions() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    assert_eq!(
        registry.mark_canceled_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::Applied
    );
    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::LateNoOp(DelegateState::Canceled)
    );
    assert_eq!(
        registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::LateNoOp(DelegateState::Canceled)
    );
    assert_eq!(
        registry.mark_agent_died_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::LateNoOp(DelegateState::Canceled)
    );
    let _ = guard.forget();
}

#[test]
fn agent_died_blocks_subsequent_terminal_transitions() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    assert_eq!(
        registry.mark_agent_died_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::Applied
    );
    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::LateNoOp(DelegateState::AgentDied)
    );
    assert_eq!(
        registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::LateNoOp(DelegateState::AgentDied)
    );
    assert_eq!(
        registry.mark_canceled_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::LateNoOp(DelegateState::AgentDied)
    );
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 5. DTOK-3: reply-vs-timeout race determinism — first writer wins.
// ---------------------------------------------------------------------------

#[test]
fn reply_wins_when_reply_fires_first_then_timeout_late() {
    // Models the OnAgent deadline-race PR-7 risk row in
    // 07_BLAST_RADIUS.md §6. Reply CAS lands first; the timer's
    // late fire is a no-op (DTOK-3).
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();

    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied
    );
    assert_eq!(
        registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::LateNoOp(DelegateState::Replied)
    );
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    assert_eq!(registry.take_reply(id), Some(DelegateReply::placeholder()));
    let _ = guard.forget();
}

#[test]
fn timeout_wins_when_timeout_fires_first_then_reply_late() {
    // The mirror direction. Timer CAS lands first; the agent's
    // late reply is rejected as LateReply (DTOK-1 / DTOK-3).
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();

    assert_eq!(
        registry.mark_timed_out_with_post(id, direct_delegate_mailbox_post),
        TransitionOutcome::Applied
    );
    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::LateNoOp(DelegateState::TimedOut)
    );
    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));
    // The reply was never installed.
    assert_eq!(registry.take_reply(id), None);
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 6. AgentTokenGuard drop semantics matrix.
// ---------------------------------------------------------------------------

#[test]
fn drop_with_cancel_on_drop_triggers_mark_canceled() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
}

#[test]
fn drop_with_cancel_on_drop_synchronous_still_triggers_cancel_cas() {
    // CancelOnDrop is what drives the CAS regardless of the
    // AgentCancelPolicy. Synchronous shapes the agent-facing
    // protocol the caller drives *after* the CAS, not the CAS
    // itself.
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::Synchronous,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
}

#[test]
fn drop_with_cancel_on_drop_detached_still_triggers_cancel_cas() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::Detached,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
}

#[test]
fn drop_with_abandon_leaves_state_pending() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    drop(guard);
    // Pure unbind: state remains Pending; the agent's eventual
    // reply lands on a dead waiter (reply-routing layer drops it).
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
}

#[test]
fn drop_with_abandon_synchronous_leaves_state_pending() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::Synchronous,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
}

#[test]
fn drop_with_abandon_detached_leaves_state_pending() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::Detached,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    drop(guard);
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
}

#[test]
fn drop_after_terminal_is_no_op() {
    // If the token already won a terminal transition (reply
    // landed), `CancelOnDrop`'s drop-time CAS observes the
    // existing terminal state as a LateNoOp and does not override
    // it.
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied
    );
    drop(guard);
    // Replied stands; Canceled does NOT override it.
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
}

#[test]
fn forget_suppresses_drop_time_cas() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
        None,
    );
    let id = guard.id();
    // forget() returns the raw id and suppresses the drop-time
    // delegate cancel transition.
    let forgotten = guard.forget();
    assert_eq!(forgotten, id);
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
}

// ---------------------------------------------------------------------------
// 7. TokenDropPolicy::from_agent_cancel derivation (DELEGATE-6).
// ---------------------------------------------------------------------------

#[test]
fn token_drop_policy_from_best_effort_is_cancel_on_drop() {
    assert_eq!(
        TokenDropPolicy::from_agent_cancel(AgentCancelPolicy::BestEffort),
        TokenDropPolicy::CancelOnDrop
    );
}

#[test]
fn token_drop_policy_from_synchronous_is_cancel_on_drop() {
    assert_eq!(
        TokenDropPolicy::from_agent_cancel(AgentCancelPolicy::Synchronous),
        TokenDropPolicy::CancelOnDrop
    );
}

#[test]
fn token_drop_policy_from_detached_is_abandon() {
    assert_eq!(
        TokenDropPolicy::from_agent_cancel(AgentCancelPolicy::Detached),
        TokenDropPolicy::Abandon
    );
}

// ---------------------------------------------------------------------------
// 8. delegate endpoint-death transition routes to every matching token (DTOK-2).
// ---------------------------------------------------------------------------

#[test]
fn mark_endpoint_died_walks_all_tokens_with_matching_marker() {
    let registry = DelegateRegistry::new();
    let marker_a = 0xA;
    let marker_b = 0xB;
    let g_a1 = registry.install_request(
        DelegateRequest::Placeholder,
        marker_a,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let g_a2 = registry.install_request(
        DelegateRequest::Placeholder,
        marker_a,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let g_b1 = registry.install_request(
        DelegateRequest::Placeholder,
        marker_b,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );

    let transitioned =
        registry.mark_endpoint_died_with_post(marker_a, direct_delegate_mailbox_post);
    assert_eq!(transitioned, 2);

    assert_eq!(registry.state(g_a1.id()), Some(DelegateState::AgentDied));
    assert_eq!(registry.state(g_a2.id()), Some(DelegateState::AgentDied));
    // marker_b's token is untouched.
    assert_eq!(registry.state(g_b1.id()), Some(DelegateState::Pending));

    let _ = g_a1.forget();
    let _ = g_a2.forget();
    let _ = g_b1.forget();
}

#[test]
fn mark_endpoint_died_skips_already_terminal_tokens() {
    let registry = DelegateRegistry::new();
    let marker = 0xC;
    let g1 = registry.install_request(
        DelegateRequest::Placeholder,
        marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );
    let g2 = registry.install_request(
        DelegateRequest::Placeholder,
        marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        std::sync::Weak::new(),
        None,
    );

    // Pre-terminalize g1 via reply.
    assert_eq!(
        registry.mark_replied_with_post(
            g1.id(),
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied
    );
    // Endpoint death now reaches only g2.
    let transitioned = registry.mark_endpoint_died_with_post(marker, direct_delegate_mailbox_post);
    assert_eq!(transitioned, 1);
    assert_eq!(registry.state(g1.id()), Some(DelegateState::Replied));
    assert_eq!(registry.state(g2.id()), Some(DelegateState::AgentDied));
    let _ = g1.forget();
    let _ = g2.forget();
}

// ---------------------------------------------------------------------------
// 9. UnknownToken reporting on forged ids.
// ---------------------------------------------------------------------------

#[test]
fn mark_methods_report_unknown_token_for_forged_ids() {
    let registry = DelegateRegistry::new();
    let forged = DelegateTokenId::new(99999);
    assert_eq!(registry.state(forged), None);
    assert_eq!(
        registry.mark_replied_with_post(
            forged,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::UnknownToken
    );
    assert_eq!(
        registry.mark_timed_out_with_post(forged, direct_delegate_mailbox_post),
        TransitionOutcome::UnknownToken
    );
    assert_eq!(
        registry.mark_canceled_with_post(forged, direct_delegate_mailbox_post),
        TransitionOutcome::UnknownToken
    );
    assert_eq!(
        registry.mark_agent_died_with_post(forged, direct_delegate_mailbox_post),
        TransitionOutcome::UnknownToken
    );
}
