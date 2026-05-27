//! PR-7B mailbox-integration pin tests.
//!
//! Pins the wake routing that PR-7B layers on top of PR-7's
//! `DelegateRegistry`:
//!
//! - On `mark_replied` → `TransitionOutcome::Applied`, the bound
//!   `TaskMailbox` receives
//!   `MailboxEvent::AgentReplied { token_id }`.
//! - On `mark_canceled` / `mark_agent_died` / `mark_timed_out`
//!   → `TransitionOutcome::Applied`, the bound `TaskMailbox`
//!   receives `MailboxEvent::Abort { token_id, reason }` where
//!   `reason` matches the terminal state.
//! - `Weak<TaskMailbox>` is stored per token: if the task has
//!   already been dropped (Weak upgrade fails), the wake event
//!   is silently dropped — correct behaviour because the script
//!   frame is gone.
//! - DTOK-2 wake-routing race: when `mark_replied` and
//!   `mark_timed_out` race, only one `Applied` wins and only one
//!   `MailboxEvent` is posted. The `LateNoOp` writer drops the
//!   event.
//! - `MailboxEvent::AgentReplied` / `MailboxEvent::Abort` are
//!   **not matched** by an `ActiveWait` (which represents a
//!   `WaitSource` registration, not an `OnAgent` token).
//!
//! txdoc cross-refs:
//! - `docs/Txv3/05_DELEGATE_v1.md` §7 step 4 (post WakeHint on
//!   Applied)
//! - `docs/Txv3/02_INVARIANTS_v5.md` DTOK-1, DTOK-2, DTOK-3

use std::sync::Arc;

use tx_substrate::step::{
    AbortReason, AgentCancelPolicy, DelegateRegistry, DelegateReply, DelegateRequest,
    DelegateState, DelegateTokenId, InterestMask, TokenDropPolicy, TransitionOutcome, WaitSourceId,
};
use tx_substrate::wake::{ActiveWait, MailboxEvent, TaskMailbox, WaitGeneration};

// ---------------------------------------------------------------------------
// 1. mark_replied posts AgentReplied on Applied.
// ---------------------------------------------------------------------------

#[test]
fn mark_replied_posts_agent_replied_to_bound_mailbox() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();
    assert!(mailbox.is_empty());

    let outcome = registry.mark_replied(id, DelegateReply::placeholder());
    assert_eq!(outcome, TransitionOutcome::Applied);

    let event = mailbox.poll().expect("mailbox should have one event");
    assert_eq!(event, MailboxEvent::AgentReplied { token_id: id });
    assert!(mailbox.is_empty());
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 2. mark_canceled / mark_agent_died / mark_timed_out post Abort.
// ---------------------------------------------------------------------------

#[test]
fn mark_canceled_posts_abort_with_canceled_reason() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();
    assert_eq!(registry.mark_canceled(id), TransitionOutcome::Applied);
    let event = mailbox.poll().expect("Applied must post");
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::Canceled,
        }
    );
    let _ = guard.forget();
}

#[test]
fn mark_agent_died_posts_abort_with_agent_died_reason() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();
    assert_eq!(registry.mark_agent_died(id), TransitionOutcome::Applied);
    let event = mailbox.poll().expect("Applied must post");
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::AgentDied,
        }
    );
    let _ = guard.forget();
}

#[test]
fn mark_timed_out_posts_abort_with_timed_out_reason() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();
    assert_eq!(registry.mark_timed_out(id), TransitionOutcome::Applied);
    let event = mailbox.poll().expect("Applied must post");
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::TimedOut,
        }
    );
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 3. LateNoOp does NOT post. (DTOK-1 / DTOK-2 wake-routing extension.)
// ---------------------------------------------------------------------------

#[test]
fn late_no_op_does_not_post_a_second_event() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();
    // First writer wins, posts an event.
    assert_eq!(
        registry.mark_replied(id, DelegateReply::placeholder()),
        TransitionOutcome::Applied
    );
    assert_eq!(mailbox.len(), 1);

    // Late timeout fire: state machine returns LateNoOp(Replied);
    // mailbox queue stays at one event.
    let late = registry.mark_timed_out(id);
    assert_eq!(late, TransitionOutcome::LateNoOp(DelegateState::Replied));
    assert_eq!(mailbox.len(), 1);

    // Late cancel: same story.
    let late2 = registry.mark_canceled(id);
    assert_eq!(late2, TransitionOutcome::LateNoOp(DelegateState::Replied));
    assert_eq!(mailbox.len(), 1);
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 4. DTOK-2: reply-vs-timeout wake-routing race.
// ---------------------------------------------------------------------------

#[test]
fn dtok_2_reply_then_timeout_only_posts_one_event() {
    // DTOK-2 wake-routing extension of DTOK-3: only the CAS winner
    // posts a MailboxEvent. We can't drive a true cross-thread
    // race deterministically in a unit test, but we can pin the
    // contract that the second `Applied`-eligible writer observes
    // `LateNoOp` and drops its event.
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();

    let r1 = registry.mark_replied(id, DelegateReply::placeholder());
    let r2 = registry.mark_timed_out(id);

    // First wrote Replied, second saw it as LateNoOp.
    assert_eq!(r1, TransitionOutcome::Applied);
    assert_eq!(r2, TransitionOutcome::LateNoOp(DelegateState::Replied));

    // Mailbox observed exactly one event — the winner's.
    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().unwrap();
    assert_eq!(event, MailboxEvent::AgentReplied { token_id: id });
    let _ = guard.forget();
}

#[test]
fn dtok_2_timeout_then_reply_only_posts_one_event() {
    // Symmetric pin: timeout wins, reply loses → one Abort event.
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();

    let r1 = registry.mark_timed_out(id);
    let r2 = registry.mark_replied(id, DelegateReply::placeholder());

    assert_eq!(r1, TransitionOutcome::Applied);
    assert_eq!(r2, TransitionOutcome::LateNoOp(DelegateState::TimedOut));

    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().unwrap();
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::TimedOut,
        }
    );
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 5. Weak<TaskMailbox> drop: dead-task wake events are silently dropped.
// ---------------------------------------------------------------------------

#[test]
fn weak_upgrade_failure_drops_event_silently() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let weak = Arc::downgrade(&mailbox);
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        weak,
        None,
    );
    let id = guard.id();

    // Task disappears before the agent replies.
    drop(mailbox);

    // The CAS still applies — registry doesn't know about the
    // dead task — but no panic and no observable side effect.
    let outcome = registry.mark_replied(id, DelegateReply::placeholder());
    assert_eq!(outcome, TransitionOutcome::Applied);
    assert_eq!(registry.state(id), Some(DelegateState::Replied));

    let _ = guard.forget();
}

#[test]
fn never_bound_mailbox_install_with_weak_new_silently_drops() {
    // Pin that `Weak::new()` is a valid install_request input —
    // useful for tests of the state machine in isolation that
    // don't care about wake routing.
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
    let outcome = registry.mark_replied(id, DelegateReply::placeholder());
    assert_eq!(outcome, TransitionOutcome::Applied);
    // No assertion needed on a mailbox: there is none.
    let _ = guard.forget();
}

// ---------------------------------------------------------------------------
// 6. ActiveWait::matches: the new variants never match.
// ---------------------------------------------------------------------------

#[test]
fn active_wait_does_not_match_agent_replied() {
    // ActiveWait is a wait-source registration shape; agent
    // events name a delegate token, not a wait-source.
    let aw = ActiveWait::new(
        WaitGeneration::new(1),
        WaitSourceId::new(7),
        InterestMask::new(0b1),
    );
    let agent_reply = MailboxEvent::AgentReplied {
        token_id: DelegateTokenId::new(42),
    };
    assert!(!aw.matches(&agent_reply));
}

#[test]
fn active_wait_does_not_match_abort() {
    let aw = ActiveWait::new(
        WaitGeneration::new(1),
        WaitSourceId::new(7),
        InterestMask::new(0b1),
    );
    let abort = MailboxEvent::Abort {
        token_id: DelegateTokenId::new(42),
        reason: AbortReason::TimedOut,
    };
    assert!(!aw.matches(&abort));
}

// ---------------------------------------------------------------------------
// 7. Multi-token mailbox sharing: one mailbox can receive many events.
// ---------------------------------------------------------------------------

#[test]
fn one_mailbox_receives_events_from_many_tokens_in_order() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let g1 = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let g2 = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id1 = g1.id();
    let id2 = g2.id();

    assert_eq!(
        registry.mark_replied(id1, DelegateReply::placeholder()),
        TransitionOutcome::Applied
    );
    assert_eq!(registry.mark_timed_out(id2), TransitionOutcome::Applied);

    assert_eq!(mailbox.len(), 2);
    let e1 = mailbox.poll().unwrap();
    let e2 = mailbox.poll().unwrap();
    assert_eq!(e1, MailboxEvent::AgentReplied { token_id: id1 });
    assert_eq!(
        e2,
        MailboxEvent::Abort {
            token_id: id2,
            reason: AbortReason::TimedOut,
        }
    );
    let _ = g1.forget();
    let _ = g2.forget();
}

// ---------------------------------------------------------------------------
// 8. mark_endpoint_died routes Abort wake to every transitioned token.
// ---------------------------------------------------------------------------

#[test]
fn mark_endpoint_died_routes_abort_to_each_bound_mailbox() {
    let registry = DelegateRegistry::new();
    let marker = 0xABCD;
    let mb_a = Arc::new(TaskMailbox::new());
    let mb_b = Arc::new(TaskMailbox::new());
    let g_a = registry.install_request(
        DelegateRequest::Placeholder,
        marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_a),
        None,
    );
    let g_b = registry.install_request(
        DelegateRequest::Placeholder,
        marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_b),
        None,
    );

    let n = registry.mark_endpoint_died(marker);
    assert_eq!(n, 2);

    assert_eq!(mb_a.len(), 1);
    assert_eq!(mb_b.len(), 1);
    let e_a = mb_a.poll().unwrap();
    let e_b = mb_b.poll().unwrap();
    match e_a {
        MailboxEvent::Abort { token_id, reason } => {
            assert_eq!(token_id, g_a.id());
            assert_eq!(reason, AbortReason::AgentDied);
        }
        other => panic!("expected Abort, got {:?}", other),
    }
    match e_b {
        MailboxEvent::Abort { token_id, reason } => {
            assert_eq!(token_id, g_b.id());
            assert_eq!(reason, AbortReason::AgentDied);
        }
        other => panic!("expected Abort, got {:?}", other),
    }
    let _ = g_a.forget();
    let _ = g_b.forget();
}

// ---------------------------------------------------------------------------
// 9. AgentTokenGuard drop with CancelOnDrop posts Abort.
// ---------------------------------------------------------------------------

#[test]
fn guard_drop_with_cancel_on_drop_posts_abort_canceled() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());
    let id = {
        let guard = registry.install_request(
            DelegateRequest::Placeholder,
            0,
            AgentCancelPolicy::BestEffort,
            TokenDropPolicy::CancelOnDrop,
            Arc::downgrade(&mailbox),
            None,
        );
        guard.id()
        // guard drops here → mark_canceled CAS → MailboxEvent::Abort
    };

    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
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
