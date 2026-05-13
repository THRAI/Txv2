//! Driver-side `await_agent_reply` helper.
//!
//! PR-10 phase 4 lands the consumer of the `OnAgent` mailbox events
//! that PR-7B already publishes. Per D7 §3.4 (gap #2):
//!
//! > "No driver-side `await_agent_reply(token_id, mailbox)` helper
//! > consumes `MailboxEvent::AgentReplied` / `Abort` — PR-7B posts
//! > them but `ActiveWait::matches` ignores agent events."
//!
//! This module is **runtime code, not new substrate surface**. The
//! events are already posted on the registry's `Applied` path; the
//! helper here filters them off the bound mailbox and returns the
//! reply payload (or the abort reason) to the parked `fault_script`
//! frame.
//!
//! ## Semantics
//!
//! - `await_agent_reply(token_id, mailbox)` polls the mailbox until
//!   it sees a `MailboxEvent::AgentReplied` or `MailboxEvent::Abort`
//!   whose `token_id` matches the supplied id.
//! - On `AgentReplied`: the helper calls
//!   `DelegateRegistry::take_reply(token_id)` to drain the installed
//!   reply payload and returns `Ok(reply)`.
//! - On `Abort`: the helper returns `Err(reason)` carrying the
//!   `AbortReason` reported by whichever terminal transition won
//!   (Canceled / AgentDied / TimedOut per DTOK-3).
//! - Other events (`SourceFired`, `SignalDelivered`, or
//!   `AgentReplied`/`Abort` for a *different* token) are spurious for
//!   this wait and are **re-posted to the back of the mailbox queue**
//!   so the rightful owner can consume them on its next poll. The
//!   helper does not consume the active-wait generation, so DTOK-3
//!   carry-through (single-fire `ActiveWait::matches`) is unaffected.
//!
//! ## Why a separate helper
//!
//! The existing `ActiveWait::matches` (per substrate's
//! `wake::mailbox`) is wait-source-shaped — it filters
//! `SourceFired` events keyed by `(generation, source, interests)`.
//! Agent events name a `DelegateTokenId`, not a `WaitSourceId`, so
//! they need a sibling predicate
//! (substrate's `wake::agent_event_matches`) and a sibling
//! consumer (this helper). No substrate redesign required (D7 §3.4
//! "runtime code, not new substrate surface").

use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::adapter::bus_wire::{agent_event_matches, MailboxEvent, TaskMailbox};
use crate::adapter::step_engine::{AbortReason, DelegateRegistry, DelegateReply, DelegateTokenId};

/// Outcome of [`await_agent_reply`]. Mirrors the `Replied` /
/// non-`Replied` terminal split in the registry state machine: on
/// success the reply payload travels back to the parked
/// `fault_script` frame; on abort the reason is what
/// `MailboxEvent::Abort.reason` carried.
///
/// Equivalent to `Result<DelegateReply, AbortReason>` — kept as a
/// named alias inside the module for readability of the helper's
/// signature.
pub type AgentReplyOutcome = Result<DelegateReply, AbortReason>;

/// Await an `OnAgent` reply on the supplied `mailbox` keyed by
/// `token_id`.
///
/// **Flow (5-line sketch).**
///
/// 1. Register a waker on the mailbox so re-polls fire on `post`.
/// 2. Drain queued events; if any matches `token_id` consume it.
///    - `AgentReplied` → `registry.take_reply(token_id)`; return Ok.
///    - `Abort { reason }` → return Err(reason).
///    - Spurious (other source / other token) → re-enqueue and continue.
/// 3. If the queue is exhausted without a match, return `Poll::Pending`.
/// 4. The bound mailbox's waker wakes us on the next `post`; loop.
///
/// **Lifetime.** `mailbox` is borrowed for the duration of the
/// await; the helper does not own it. `registry` is borrowed for the
/// terminal `take_reply` call on the `AgentReplied` arm. Both
/// borrows are short-lived inside `poll`; the future does not hold
/// either across the suspended `Pending` return.
///
/// **Cancellation.** The future is poll-fn-shaped: dropping it before
/// completion drops the registered waker (via [`TaskMailbox::clear_waker`])
/// on next post, and the parked event remains in the queue for a
/// future consumer. Phase 4 callers in `fault_script` couple the
/// drop with the `AgentTokenGuard`'s drop policy
/// (`CancelOnDrop` → registry `mark_canceled` fires the `Abort`).
pub fn await_agent_reply<'a>(
    token_id: DelegateTokenId,
    mailbox: &'a TaskMailbox,
    registry: &'a DelegateRegistry,
) -> AwaitAgentReply<'a> {
    AwaitAgentReply {
        token_id,
        mailbox,
        registry,
    }
}

/// Future returned by [`await_agent_reply`].
///
/// Polls the bound `mailbox` for a matching `AgentReplied` /
/// `Abort` event. Spurious events (other tokens, other shapes) are
/// re-enqueued so the rightful consumer can read them.
pub struct AwaitAgentReply<'a> {
    token_id: DelegateTokenId,
    mailbox: &'a TaskMailbox,
    registry: &'a DelegateRegistry,
}

impl<'a> Future for AwaitAgentReply<'a> {
    type Output = AgentReplyOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Register the waker first so any `post` between our drain
        // and the `Pending` return wakes us. The mailbox replaces the
        // registered waker on every call which is the per-poll
        // contract.
        self.mailbox.register_waker(cx.waker().clone());

        // Drain pending events. Matching event resolves the future;
        // spurious events are re-enqueued so the queue invariant
        // (events for other consumers stay visible) holds.
        let mut spurious: Vec<MailboxEvent> = Vec::new();
        while let Some(event) = self.mailbox.poll() {
            if agent_event_matches(&event, self.token_id) {
                // Re-post any spurious events we drained.
                for evt in spurious.into_iter() {
                    let _ = self.mailbox.post(evt);
                }
                let outcome = match event {
                    MailboxEvent::AgentReplied { .. } => {
                        // Drain the reply payload from the registry.
                        // If `take_reply` returns `None` we treat the
                        // transition as if it had not fired —
                        // shouldn't happen on a freshly-posted
                        // `AgentReplied` per DTOK-1's "reply
                        // installed before mailbox post" ordering,
                        // but guard defensively by surfacing the
                        // canonical "agent died" abort so the
                        // faulting frame at least unwinds rather
                        // than spins.
                        match self.registry.take_reply(self.token_id) {
                            Some(reply) => Ok(reply),
                            None => Err(AbortReason::AgentDied),
                        }
                    }
                    MailboxEvent::Abort { reason, .. } => Err(reason),
                    // `agent_event_matches` filtered these out above,
                    // but the exhaustive match keeps the compiler
                    // honest if a new variant lands later.
                    MailboxEvent::SourceFired { .. } | MailboxEvent::SignalDelivered { .. } => {
                        unreachable!(
                            "agent_event_matches must not return true for non-agent events"
                        )
                    }
                };
                return Poll::Ready(outcome);
            }
            spurious.push(event);
        }
        // No match this round — restore spurious events and park.
        for evt in spurious.into_iter() {
            let _ = self.mailbox.post(evt);
        }
        Poll::Pending
    }
}

// Tests for this helper live in `tests/agent_reply.rs` so they can
// use the `std`/`extern crate alloc` test harness alongside the rest
// of the reactor tests (this crate is `no_std` lib-only — Cargo.toml
// sets `test = false`/`doctest = false`).
