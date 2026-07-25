//! W-MM — EndpointScope abandonment routing edge-case pin tests.
//!
//! Closes the BLAST_RADIUS §6 risk register row:
//!
//! > "Risk: EndpointScope abandonment routing edge cases
//! > (Medium / Medium).
//! > Mitigation: Land with explicit test exercising tracee-process
//! > exit during ptrace stop."
//!
//! PR-7 introduced the `DelegateRegistry` delegate endpoint-death transition for `marker`
//! and W-Y's phase-4 fault-script test exercised the "single
//! in-flight token aborts" happy path. This file pins the remaining
//! corner cases the §6 row called out — the cases analogous to:
//!
//! - tracee process exits while ptrace stop is pending,
//! - endpoint fd closed while requests are pending,
//! - endpoint thread dies independently of process exit.
//!
//! All seven scenarios drive against the substrate-side API only
//! (`DelegateRegistry::install_request` / `mark_*` /
//! `delegate endpoint-death transition` / `take_reply`). The faulting-side
//! `await_agent_reply` consumer lives in tx-reactor, which is not a
//! dep of tx-substrate; we pin the wake-routing contract directly
//! by asserting the `MailboxEvent::Abort { reason: AgentDied }`
//! event lands on the bound mailbox — exactly what
//! `await_agent_reply` would observe as `Err(AbortReason::AgentDied)`
//! on its next poll (per `tx-reactor::agent_reply` §"Semantics").
//!
//! ## DTOK-3 carry-through
//!
//! The race-shaped scenarios (test 3, test 5) are deterministic:
//! they fire the two competing `mark_*` calls in a defined order
//! and rely on the CAS first-writer-wins discipline to pick the
//! winner. The state-machine CAS is the single linearization point
//! (DTOK-3) so the actual cross-thread race is structurally
//! covered by serial ordering — adding real concurrency would not
//! exercise any code path the serial test does not already pin.
//!
//! txdoc cross-refs:
//! - `docs/Txv3/05_DELEGATE_v1.md` §3.1 (EndpointScope), §4 (state
//!   machine), §6 (drop policy), §7 (resume protocol)
//! - `docs/Txv3/02_INVARIANTS_v5.md` DTOK-1 / DTOK-2 / DTOK-3,
//!   DELEGATE-3, DELEGATE-6
//! - `docs/Txv3/07_BLAST_RADIUS.md` §6 risk row "EndpointScope
//!   abandonment routing edge cases"
//! - `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//!   §3.6 (ufd's use of delegate endpoint-death transition on fd close)

use std::sync::Arc;

use tx_substrate::step::{
    AbortReason, AgentCancelPolicy, DelegateRegistry, DelegateReply, DelegateRequest,
    DelegateState, TokenDropPolicy, TransitionOutcome,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

fn direct_delegate_mailbox_post(mailbox: std::sync::Weak<TaskMailbox>, event: MailboxEvent) {
    if let Some(mailbox) = mailbox.upgrade() {
        let _ = mailbox.post(event);
    }
}

// =========================================================================
// 1. Process-exit during pending request: the canonical case the
//    BLAST_RADIUS row calls out. Endpoint dies while a single token
//    is in flight; the token must transition to AgentDied and the
//    bound mailbox must observe `Abort { reason: AgentDied }` —
//    the same event the faulting-side `await_agent_reply` consumes
//    to surface `AbortReason::AgentDied`.
// =========================================================================

#[test]
fn endpoint_process_exit_aborts_single_in_flight_token_with_agent_died() {
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0xA1B2_C3D4_u64;
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();
    assert_eq!(registry.state(id), Some(DelegateState::Pending));
    assert!(mailbox.is_empty());

    // Endpoint process exits. The reactor-side ufd-close arm /
    // ptrace-exit arm calls into this:
    let transitioned =
        registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(transitioned, 1, "the in-flight token must abort");
    assert_eq!(registry.state(id), Some(DelegateState::AgentDied));

    // The bound mailbox now carries the Abort event. This is what
    // `await_agent_reply` on the faulting side observes; the helper
    // returns `Err(AbortReason::AgentDied)` to the parked frame.
    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().expect("Abort event was posted");
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::AgentDied,
        },
        "the agent-died abort reason is the faulting-side signal",
    );
    let _ = guard.forget();
}

// =========================================================================
// 2. Multiple in-flight tokens for the same endpoint: the walk
//    must transition all N. No orphans, no partial transitions.
//    This is the "endpoint fd closed while multiple requests are
//    pending" shape — a single fd close aborts every fault that was
//    parked on it.
// =========================================================================

#[test]
fn endpoint_death_walks_all_in_flight_tokens_for_that_endpoint() {
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0xDEAD_BEEF_u64;
    let mb_1 = Arc::new(TaskMailbox::new());
    let mb_2 = Arc::new(TaskMailbox::new());
    let mb_3 = Arc::new(TaskMailbox::new());
    let mb_4 = Arc::new(TaskMailbox::new());

    let g1 = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_1),
        None,
    );
    let g2 = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_2),
        None,
    );
    let g3 = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_3),
        None,
    );
    let g4 = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_4),
        None,
    );

    let transitioned =
        registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(transitioned, 4, "every in-flight token must transition");

    for (guard, mb) in [(&g1, &mb_1), (&g2, &mb_2), (&g3, &mb_3), (&g4, &mb_4)] {
        assert_eq!(
            registry.state(guard.id()),
            Some(DelegateState::AgentDied),
            "every token reaches the AgentDied terminal",
        );
        assert_eq!(mb.len(), 1, "every bound mailbox receives one Abort");
        let event = mb.poll().expect("Abort event posted");
        assert_eq!(
            event,
            MailboxEvent::Abort {
                token_id: guard.id(),
                reason: AbortReason::AgentDied,
            },
        );
    }

    // No orphan tokens: tracked_count is unchanged (the registry
    // retains slot allocation for its lifetime), but every slot is
    // now terminal.
    assert_eq!(registry.tracked_count(), 4);
    let _ = g1.forget();
    let _ = g2.forget();
    let _ = g3.forget();
    let _ = g4.forget();
}

// =========================================================================
// 3. Endpoint death races a concurrent `delegate reply transition`: exactly one
//    wins (DTOK-3, first-writer-wins). The other reports LateNoOp.
//    Test both orderings.
// =========================================================================

#[test]
fn mark_replied_wins_then_endpoint_death_is_late_no_op() {
    // The agent finished writing the reply before the endpoint
    // died. Reply CAS lands first; the endpoint-death walk sees
    // the slot already terminal and skips it (no double-transition,
    // no extra mailbox event).
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0x4242_u64;
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();

    // Agent gets there first.
    assert_eq!(
        registry.mark_replied_with_post(
            id,
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied,
    );
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    assert_eq!(mailbox.len(), 1, "AgentReplied event posted");

    // Endpoint dies — the walk's per-token delegate agent-death transition observes
    // a Replied slot and returns LateNoOp. The walk's count is the
    // number of *Applied* transitions, so it must be 0 here.
    let transitioned =
        registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(
        transitioned, 0,
        "walk skips the already-terminal slot — no double-transition",
    );
    // State is unchanged.
    assert_eq!(registry.state(id), Some(DelegateState::Replied));
    // Mailbox unchanged: no second event posted.
    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().unwrap();
    assert_eq!(event, MailboxEvent::AgentReplied { token_id: id });
    let _ = guard.forget();
}

#[test]
fn endpoint_death_wins_then_late_mark_replied_is_late_no_op() {
    // Mirror direction (DTOK-1 / DTOK-3): endpoint death CASes the
    // slot to AgentDied; the agent's in-flight delegate reply transition loses
    // the race and is rejected as LateNoOp(AgentDied). The reply
    // payload is never installed, and the mailbox observes exactly
    // one event — the AgentDied abort.
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0x4243_u64;
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();

    let transitioned =
        registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(transitioned, 1);
    assert_eq!(registry.state(id), Some(DelegateState::AgentDied));
    assert_eq!(mailbox.len(), 1);

    // The agent's stale delegate reply transition — submitted before the agent
    // observed its own death — loses to the AgentDied CAS.
    let late = registry.mark_replied_with_post(
        id,
        DelegateReply::placeholder(),
        direct_delegate_mailbox_post,
    );
    assert_eq!(late, TransitionOutcome::LateNoOp(DelegateState::AgentDied));
    // The reply payload was NOT installed; take_reply returns None.
    assert_eq!(registry.take_reply(id), None);
    // Mailbox still holds only the Abort.
    assert_eq!(mailbox.len(), 1);
    let event = mailbox.poll().unwrap();
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::AgentDied,
        },
    );
    let _ = guard.forget();
}

// =========================================================================
// 4. Endpoint death after some tokens already terminal: the
//    walk's per-token CAS observes the existing terminal state
//    as LateNoOp and does not override it. The count of newly
//    transitioned tokens excludes the already-terminal ones.
// =========================================================================

#[test]
fn endpoint_death_after_some_tokens_already_terminal_skips_them() {
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0x00C0_FFEE_u64;

    // Three tokens on the same endpoint.
    let mb_replied = Arc::new(TaskMailbox::new());
    let mb_canceled = Arc::new(TaskMailbox::new());
    let mb_live = Arc::new(TaskMailbox::new());

    let g_replied = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_replied),
        None,
    );
    let g_canceled = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_canceled),
        None,
    );
    let g_live = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_live),
        None,
    );

    // Pre-terminate two tokens.
    assert_eq!(
        registry.mark_replied_with_post(
            g_replied.id(),
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied,
    );
    assert_eq!(
        registry.mark_canceled_with_post(g_canceled.id(), direct_delegate_mailbox_post),
        TransitionOutcome::Applied,
    );

    // Each already-terminal mailbox saw one event.
    assert_eq!(mb_replied.len(), 1);
    assert_eq!(mb_canceled.len(), 1);
    assert_eq!(mb_live.len(), 0);

    // Endpoint dies.
    let transitioned =
        registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(
        transitioned, 1,
        "only the live token transitions; the two terminals are skipped",
    );

    // States: Replied / Canceled stand; live one moved to AgentDied.
    assert_eq!(registry.state(g_replied.id()), Some(DelegateState::Replied));
    assert_eq!(
        registry.state(g_canceled.id()),
        Some(DelegateState::Canceled)
    );
    assert_eq!(registry.state(g_live.id()), Some(DelegateState::AgentDied));

    // Mailbox accounting: no second event posted to the already-terminal
    // mailboxes (DTOK-1 wake-routing extension — only Applied posts).
    assert_eq!(mb_replied.len(), 1, "no duplicate event for Replied token");
    assert_eq!(
        mb_canceled.len(),
        1,
        "no duplicate event for Canceled token",
    );
    assert_eq!(mb_live.len(), 1, "live token's Abort posted");

    // The live mailbox carries the AgentDied abort.
    let live_event = mb_live.poll().unwrap();
    assert_eq!(
        live_event,
        MailboxEvent::Abort {
            token_id: g_live.id(),
            reason: AbortReason::AgentDied,
        },
    );

    let _ = g_replied.forget();
    let _ = g_canceled.forget();
    let _ = g_live.forget();
}

// =========================================================================
// 5. Late delegate reply transition for an endpoint that already died: this is
//    the "agent thread was mid-reply when its endpoint died"
//    case — the subtle one called out in the §6 row. The agent
//    can't observe its own death synchronously; it submits the
//    reply, sees LateNoOp(AgentDied), and drops the reply payload.
// =========================================================================

#[test]
fn late_mark_replied_for_dead_endpoint_is_late_no_op_and_no_state_change() {
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0xCAFE_u64;
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();

    // Endpoint dies first (e.g. process exit handler ran before
    // the agent thread's syscall returned).
    let n = registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(n, 1);
    let snapshot_state = registry.state(id);
    let snapshot_mailbox_len = mailbox.len();
    assert_eq!(snapshot_state, Some(DelegateState::AgentDied));
    assert_eq!(snapshot_mailbox_len, 1);

    // The agent — unaware its endpoint is gone — calls delegate reply transition.
    // It MUST return LateNoOp(AgentDied) and MUST NOT change the
    // observable state of the registry or the mailbox.
    let outcome = registry.mark_replied_with_post(
        id,
        DelegateReply::placeholder(),
        direct_delegate_mailbox_post,
    );
    assert_eq!(
        outcome,
        TransitionOutcome::LateNoOp(DelegateState::AgentDied)
    );

    // State unchanged.
    assert_eq!(registry.state(id), snapshot_state);
    // No reply payload visible (the LateNoOp branch never enters
    // the single-writer ReplyInstalling phase, so the slot's
    // reply field stays None).
    assert_eq!(registry.take_reply(id), None);
    // Mailbox unchanged (no AgentReplied event posted on LateNoOp).
    assert_eq!(mailbox.len(), snapshot_mailbox_len);
    let event = mailbox.poll().unwrap();
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::AgentDied,
        },
    );
    let _ = guard.forget();
}

// =========================================================================
// 6. AgentTokenGuard drop after endpoint died: the CancelOnDrop
//    policy's drop-time delegate cancel transition CAS observes the existing
//    AgentDied terminal and is a LateNoOp. No double-transition,
//    no second mailbox event.
// =========================================================================

#[test]
fn agent_token_guard_drop_after_endpoint_died_is_late_no_op() {
    let registry = DelegateRegistry::new();
    let endpoint_marker = 0xBEEF_u64;
    let mailbox = Arc::new(TaskMailbox::new());

    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        endpoint_marker,
        // CancelOnDrop is the policy that would CAS the slot to
        // Canceled at drop time — we want to verify it observes
        // the existing AgentDied as a LateNoOp.
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        Arc::downgrade(&mailbox),
        None,
    );
    let id = guard.id();

    // Endpoint dies first.
    let n = registry.mark_endpoint_died_with_post(endpoint_marker, direct_delegate_mailbox_post);
    assert_eq!(n, 1);
    assert_eq!(registry.state(id), Some(DelegateState::AgentDied));
    assert_eq!(mailbox.len(), 1);

    // Now drop the guard. The CancelOnDrop branch's delegate cancel transition
    // CAS hits an already-terminal slot and is a LateNoOp.
    drop(guard);

    // State remains AgentDied — the AgentDied terminal is NOT
    // overridden by a late Canceled.
    assert_eq!(registry.state(id), Some(DelegateState::AgentDied));

    // Mailbox accounting: still exactly one event (the AgentDied
    // abort posted by delegate endpoint-death transition), no second `Canceled`
    // abort posted by the drop.
    assert_eq!(
        mailbox.len(),
        1,
        "CancelOnDrop's LateNoOp posts no second event",
    );
    let event = mailbox.poll().unwrap();
    assert_eq!(
        event,
        MailboxEvent::Abort {
            token_id: id,
            reason: AbortReason::AgentDied,
        },
    );
}

// =========================================================================
// 7. DelegateRegistry survives independently: delegate endpoint-death transition
//    only walks tokens bound to that endpoint_marker. Other
//    endpoints' tokens are untouched — both the state and the
//    bound mailboxes.
//
//    Models the "endpoint thread dies independently of process
//    exit" case the §6 row called out: a per-thread endpoint
//    dies, but other endpoints (other threads in the same
//    process, other processes entirely) keep their in-flight
//    requests alive.
// =========================================================================

#[test]
fn endpoint_death_is_scoped_to_matching_endpoint_marker_only() {
    let registry = DelegateRegistry::new();

    // Three distinct endpoints: the dying one, and two siblings
    // that must NOT be affected.
    let marker_dying = 0xDEAD_u64;
    let marker_sibling_a = 0x1234_u64;
    let marker_sibling_b = 0x5678_u64;

    let mb_dying = Arc::new(TaskMailbox::new());
    let mb_sib_a = Arc::new(TaskMailbox::new());
    let mb_sib_b = Arc::new(TaskMailbox::new());

    let g_dying = registry.install_request(
        DelegateRequest::Placeholder,
        marker_dying,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_dying),
        None,
    );
    let g_sib_a = registry.install_request(
        DelegateRequest::Placeholder,
        marker_sibling_a,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_sib_a),
        None,
    );
    // A second sibling-A token to verify routing handles multiple
    // matching markers on the unaffected side too.
    let mb_sib_a2 = Arc::new(TaskMailbox::new());
    let g_sib_a2 = registry.install_request(
        DelegateRequest::Placeholder,
        marker_sibling_a,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_sib_a2),
        None,
    );
    let g_sib_b = registry.install_request(
        DelegateRequest::Placeholder,
        marker_sibling_b,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(&mb_sib_b),
        None,
    );

    // The dying endpoint dies.
    let transitioned =
        registry.mark_endpoint_died_with_post(marker_dying, direct_delegate_mailbox_post);
    assert_eq!(
        transitioned, 1,
        "exactly one token (the dying endpoint's) transitions",
    );

    // Dying endpoint's token is AgentDied; its mailbox has the
    // Abort.
    assert_eq!(registry.state(g_dying.id()), Some(DelegateState::AgentDied));
    assert_eq!(mb_dying.len(), 1);
    let abort = mb_dying.poll().unwrap();
    assert_eq!(
        abort,
        MailboxEvent::Abort {
            token_id: g_dying.id(),
            reason: AbortReason::AgentDied,
        },
    );

    // Sibling endpoints are entirely untouched.
    assert_eq!(
        registry.state(g_sib_a.id()),
        Some(DelegateState::Pending),
        "sibling-a's token must remain Pending",
    );
    assert_eq!(
        registry.state(g_sib_a2.id()),
        Some(DelegateState::Pending),
        "sibling-a's second token must remain Pending",
    );
    assert_eq!(
        registry.state(g_sib_b.id()),
        Some(DelegateState::Pending),
        "sibling-b's token must remain Pending",
    );
    assert!(mb_sib_a.is_empty(), "no spurious wake to sibling-a");
    assert!(
        mb_sib_a2.is_empty(),
        "no spurious wake to sibling-a (second)"
    );
    assert!(mb_sib_b.is_empty(), "no spurious wake to sibling-b");

    // And the surviving endpoints can still complete normally.
    // Reply to sibling-a's first token; verify the AgentReplied
    // lands and the state machine is fully functional.
    assert_eq!(
        registry.mark_replied_with_post(
            g_sib_a.id(),
            DelegateReply::placeholder(),
            direct_delegate_mailbox_post
        ),
        TransitionOutcome::Applied,
    );
    assert_eq!(registry.state(g_sib_a.id()), Some(DelegateState::Replied));
    assert_eq!(mb_sib_a.len(), 1);

    let _ = g_dying.forget();
    let _ = g_sib_a.forget();
    let _ = g_sib_a2.forget();
    let _ = g_sib_b.forget();
}
