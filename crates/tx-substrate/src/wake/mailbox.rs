//! PR-3A wake-substrate foundation: `TaskMailbox` + `WaitGeneration` +
//! `MailboxEvent` + `ActiveWait`.
//!
//! Per [`docs/progress/decisions/2026-05-11-pr-3-wake-substrate-shape.md`]:
//! **task-owned wake delivery + object-owned wait publication.** This
//! module introduces the task-side primitives. The object-side
//! `WaitSource` wrapper lands in PR-3B; prepared-registration migration
//! lands in PR-3C; retirement of the 92 direct `Waker` sites lands in
//! PR-3D.
//!
//! ## Naming note
//!
//! The PR-3 ADR uses the term "WakeHint" for the event posted to a
//! mailbox. The `tx-reactor` crate already has a `scheduler::WakeHint`
//! enum (`Normal`/`SignalDelivery`/`PriorityBoost`/`None`) that
//! classifies scheduler-input metadata, not wake-event content. To
//! avoid collision the ADR's `WakeHint` is spelled [`MailboxEvent`]
//! here. Both serve different concerns.
//!
//! ## Why this exists
//!
//! A semantic object (pipe ring, futex bucket, exit channel, …) may
//! have many tasks waiting on it. If wait state lived on the object,
//! a single source-side counter would have to mean different things
//! for different waiters. Wrong ownership. Generation identifies
//! **this task's** currently active wait, and the mailbox is the
//! delivery point.
//!
//! Each registration captures `generation` from
//! [`TaskMailbox::next_generation`]; a later [`MailboxEvent::SourceFired`]
//! carrying that same generation is fresh, anything else is stale and
//! the driver drops it.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::Waker;

use alloc::collections::VecDeque;

use crate::step_v3::{AbortReason, DelegateTokenId, InterestMask, WaitSourceId};
use crate::SpinMutex;

/// Generation counter for a [`TaskMailbox`]'s currently-active wait.
///
/// Monotonically advances each time the task installs a new wait
/// registration. The driver stores the generation in its
/// [`ActiveWait`]; incoming events carry the generation they were
/// posted with. Mismatched generation → stale → drop.
///
/// Wrap-around at `u64::MAX` is not protected against. With a 1-GHz
/// wait-installation rate the wrap horizon is ~584 years, so the
/// counter is effectively monotonic for the lifetime of the system.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WaitGeneration(u64);

impl WaitGeneration {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Classification of a [`MailboxEvent::SignalDelivered`] post per
/// `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
/// §4 Option A. The variant tells the eventual driver/siginfo
/// formatter where the signal came from so it can later attribute
/// `si_code` (`SI_USER` for thread-directed, `SI_KERNEL` /
/// `SI_QUEUE` for process-directed posts produced by `kill(pid,sig)`,
/// etc.).
///
/// Day-1 callers post `ProcessDirected` from
/// `step_kill_process`/`route_gewalt` (process-wide / group-wide
/// fanout) and `ThreadDirected { tid }` from `tgkill`-shaped paths
/// (only `route_gewalt`'s SIGKILL bypass and `post_signal`'s direct
/// thread post today; the `tid` is the targeted thread's TID for
/// future siginfo attribution).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalRouting {
    /// `kill(pid, sig)` / `route_gewalt` / group-fanout post: the
    /// signal targets the process as a whole and any eligible thread
    /// may serve.
    ProcessDirected,
    /// `tgkill(pid, tid, sig)` / per-thread synchronous fault: the
    /// signal targets a specific thread by TID. The `tid` field is
    /// the raw TID value (matches `Tid::0`'s raw representation).
    ThreadDirected { tid: u64 },
}

/// Event posted to a [`TaskMailbox`] describing a wake-relevant fact.
///
/// Four variants today:
///
/// - [`SourceFired`](Self::SourceFired) — a `WaitSource` fired
///   (PR-3A/3B). Driver compares the carried generation against
///   the active wait to filter stale events.
/// - [`AgentReplied`](Self::AgentReplied) — a `DelegateRegistry`
///   transitioned an `OnAgent` token to `Replied` (PR-7B). The
///   bound waiter resumes via `ResumeOutcome::WithReply` after
///   calling `DelegateRegistry::take_reply(token_id)`.
/// - [`Abort`](Self::Abort) — a `DelegateRegistry` transitioned an
///   `OnAgent` token to a non-`Replied` terminal state (Canceled
///   / AgentDied / TimedOut). The bound waiter resumes via
///   `ResumeOutcome::Aborted(reason)`.
/// - [`SignalDelivered`](Self::SignalDelivered) — a catchable or
///   Gewalt signal was posted to the owning thread (D9-A). Forces
///   the parked future to re-poll so it observes
///   `InterruptSummary::deliverable_signal` /
///   `InterruptSummary::termination` / `stop_requested`. The event
///   is **not** matched by [`ActiveWait::matches`] — it is a
///   wake-hint, not a wait-source-fire. See D9 §4 Option A.
///
/// Per `docs/Txv3/05_DELEGATE_v1.md` §7 the spec calls the
/// `AgentReplied` / `Abort` variants `WakeHint::AgentReplied` /
/// `WakeHint::Abort`. They are spelled here as `MailboxEvent::*`
/// to share the queue with `SourceFired` and to avoid colliding
/// with the reactor's `scheduler::WakeHint` (which classifies
/// scheduler-input metadata, not wake-event content). See the
/// module-level naming note.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxEvent {
    /// A `Channel`-equivalent `WaitSource` fired. Carries the
    /// **generation captured at registration time** plus the firing
    /// source's id and the mask delta that triggered the fire. The
    /// driver compares `generation` against its
    /// [`ActiveWait::generation`]; stale events are dropped.
    SourceFired {
        generation: WaitGeneration,
        source: WaitSourceId,
        interests: InterestMask,
    },
    /// A `DelegateRegistry` reply was installed for the named token
    /// (PR-7B). The driver pairs the event with its in-flight
    /// `OnAgent` wait and resumes the op with
    /// `ResumeOutcome::WithReply` once the reply payload has been
    /// drained via `DelegateRegistry::take_reply(token_id)`.
    ///
    /// Posted by `DelegateRegistry::mark_replied` on the
    /// `TransitionOutcome::Applied` path. Late writers (any
    /// `LateNoOp`) do **not** post — DTOK-1.
    AgentReplied { token_id: DelegateTokenId },
    /// A `DelegateRegistry` transitioned the named token to a
    /// non-`Replied` terminal state. `reason` names the abort
    /// (`Canceled` / `AgentDied` / `TimedOut`) so the driver can
    /// translate to the right errno (`EINTR` / `EOWNERDEAD` /
    /// `ETIMEDOUT` per `05_DELEGATE_v1.md` §7).
    ///
    /// Posted by `DelegateRegistry::mark_canceled` /
    /// `mark_agent_died` / `mark_timed_out` on the
    /// `TransitionOutcome::Applied` path. Late writers (any
    /// `LateNoOp`) do **not** post — DTOK-3.
    Abort {
        token_id: DelegateTokenId,
        reason: AbortReason,
    },
    /// A signal was delivered to the owning thread's pending-signal
    /// state (or, for Gewalt SIGSTOP/SIGCONT, the per-thread
    /// `signal_summary` was updated) per D9-A. The event carries the
    /// raw signum and a [`SignalRouting`] tag classifying the post
    /// site (process-directed / thread-directed) so future siginfo
    /// formatting can attribute `si_code` correctly.
    ///
    /// **Wake-hint, not wait-fire.** The matching truth lives in
    /// `InterruptSummary` (`deliverable_signal`, `termination`,
    /// `stop_requested`). This event's role is to force a parked
    /// future to re-poll *now* so the summary is observed instead of
    /// the thread sitting idle until some unrelated wake fires. As
    /// such, [`ActiveWait::matches`] returns `false` for this
    /// variant — same drop-on-the-floor shape `AgentReplied`/`Abort`
    /// use.
    ///
    /// Posted by:
    /// - `thread_runtime::execution::post_signal` (catchable
    ///   per-thread post; `ProcessDirected` when invoked via
    ///   `step_kill_process`, `ThreadDirected { tid }` when invoked
    ///   directly from a tgkill-shaped path).
    /// - `signal::route_gewalt` (SIGSTOP/SIGCONT per-thread loop).
    /// - `thread_runtime::execution::set_thread_zombie`
    ///   (terminal-state notification so a parked future observes
    ///   `summary.termination` and resolves to `Killed`/`Interrupted`).
    SignalDelivered { signum: u32, routing: SignalRouting },
}

/// Bounded MPSC queue capacity for a single mailbox.
///
/// Tuned for "many small wakes per scheduling slice." If full,
/// [`TaskMailbox::post`] sets [`TaskMailbox::overflow`] and drops the
/// event. The driver treats an overflow flag as a wake hint and
/// re-observes the underlying source on next poll — semantically a
/// safe fallback because masks are hints, not truth.
pub const MAILBOX_QUEUE_BOUND: usize = 64;

/// Per-reactor-task wake mailbox.
///
/// Owned by a reactor task (in practice via a `Cap<TaskMailbox>`
/// once zone allocation is wired in). Holds the generation counter,
/// a bounded MPSC of [`MailboxEvent`], and an overflow flag.
///
/// **Not tied to `ProcessIdentity`.** A process may have many
/// reactor tasks (user threads, kthreads, OnBehalfOf-borrowed scope
/// workers); each owns its own mailbox.
pub struct TaskMailbox {
    generation: AtomicU64,
    queue: SpinMutex<VecDeque<MailboxEvent>>,
    overflow: AtomicBool,
    /// Optional `core::task::Waker` registered by the current poll
    /// context. Set via [`Self::register_waker`]; called by
    /// [`Self::post`] when an event arrives so the parked future
    /// gets a re-poll signal. PR-3D step 1.
    waker: SpinMutex<Option<Waker>>,
}

impl TaskMailbox {
    pub fn new() -> Self {
        Self {
            // Generations are 1-based; `WaitGeneration::new(0)` is a
            // never-issued sentinel callers can use to mean "no active
            // wait."
            generation: AtomicU64::new(1),
            queue: SpinMutex::new(VecDeque::new()),
            overflow: AtomicBool::new(false),
            waker: SpinMutex::new(None),
        }
    }

    /// Register a `core::task::Waker` to be woken when an event is
    /// posted to this mailbox. Replaces any previously-registered
    /// waker (futures call this on every `poll` per the async
    /// contract, so the most recent waker is the right one to wake).
    pub fn register_waker(&self, waker: Waker) {
        *self.waker.lock() = Some(waker);
    }

    /// Drop any registered waker without waking. Used by drivers
    /// that have observed completion and want to detach.
    pub fn clear_waker(&self) {
        *self.waker.lock() = None;
    }

    /// Claim the next generation for a new active wait. Monotonic.
    pub fn next_generation(&self) -> WaitGeneration {
        let raw = self.generation.fetch_add(1, Ordering::AcqRel);
        WaitGeneration(raw)
    }

    /// Read the most recent generation without claiming a new one.
    pub fn current_generation(&self) -> WaitGeneration {
        let raw = self.generation.load(Ordering::Acquire).saturating_sub(1);
        WaitGeneration(raw)
    }

    /// Post an event to the mailbox. Returns `true` if enqueued,
    /// `false` if the queue overflowed. On overflow the
    /// [`Self::overflow`] flag is latched until the driver consumes
    /// it via [`Self::take_overflow`].
    pub fn post(&self, event: MailboxEvent) -> bool {
        let enqueued = {
            let mut q = self.queue.lock();
            if q.len() >= MAILBOX_QUEUE_BOUND {
                self.overflow.store(true, Ordering::Release);
                false
            } else {
                q.push_back(event);
                true
            }
        };
        // Wake the parked future (if any) so it re-polls and drains
        // the queue. Both enqueue and overflow paths wake: an
        // overflow is still wake-relevant (driver re-observes via
        // `take_overflow`).
        //
        // Drop the queue lock before grabbing the waker lock to
        // avoid lock-ordering hazards if the waker callback itself
        // ever touches a queue (it shouldn't, but defence in depth).
        let waker = self.waker.lock().clone();
        if let Some(w) = waker {
            w.wake_by_ref();
        }
        enqueued
    }

    /// Drain the next event. Returns `None` if empty.
    pub fn poll(&self) -> Option<MailboxEvent> {
        self.queue.lock().pop_front()
    }

    /// Number of queued events (for diagnostics; tests).
    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    /// Whether the queue is empty (for diagnostics; tests).
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// Whether the overflow flag is currently set.
    pub fn overflow(&self) -> bool {
        self.overflow.load(Ordering::Acquire)
    }

    /// Atomically read-and-clear the overflow flag.
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::AcqRel)
    }
}

impl Default for TaskMailbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Driver-local active suspension state.
///
/// Captured at wait registration; matched against incoming
/// [`MailboxEvent`]s to filter stale hints. **Driver-local**: not
/// stored as a long-lived `ThreadPayload` wait frame (per ADR).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveWait {
    pub generation: WaitGeneration,
    pub source: WaitSourceId,
    pub interests: InterestMask,
}

impl ActiveWait {
    pub const fn new(
        generation: WaitGeneration,
        source: WaitSourceId,
        interests: InterestMask,
    ) -> Self {
        Self {
            generation,
            source,
            interests,
        }
    }

    /// Match an incoming [`MailboxEvent`] against this active wait.
    ///
    /// Returns `true` iff the event is a `SourceFired` event that is
    /// **fresh** (generation matches), **for this source**, and the
    /// firing mask **overlaps** the active interests. Mask overlap
    /// is bitwise AND non-zero.
    ///
    /// `ActiveWait` represents a *wait-source* registration only;
    /// `AgentReplied` / `Abort` events name an `OnAgent` token, not
    /// a `WaitSourceId`, so they never match an `ActiveWait`. The
    /// driver routes those events through the `OnAgent`-side
    /// bookkeeping (the `DelegateTokenId` carried by the
    /// in-flight wait frame).
    ///
    /// `SignalDelivered` events (D9-A) are wake-hints whose truth
    /// lives in `InterruptSummary`; the future re-polls and
    /// `WaitProtocol::classify_interrupt` reads the summary. The
    /// event never matches an `ActiveWait` — same drop-on-the-floor
    /// shape as `AgentReplied`/`Abort`.
    pub fn matches(&self, event: &MailboxEvent) -> bool {
        match event {
            MailboxEvent::SourceFired {
                generation,
                source,
                interests,
            } => {
                *generation == self.generation
                    && *source == self.source
                    && (interests.raw() & self.interests.raw()) != 0
            }
            MailboxEvent::AgentReplied { .. }
            | MailboxEvent::Abort { .. }
            | MailboxEvent::SignalDelivered { .. } => false,
        }
    }
}

/// Match an incoming [`MailboxEvent`] against an active `OnAgent`
/// wait keyed by a [`DelegateTokenId`].
///
/// Returns `true` iff the event is an `AgentReplied` or `Abort` whose
/// `token_id` matches `expected`. `SourceFired` / `SignalDelivered`
/// events never match an agent wait — they belong to other dispatch
/// paths and the driver must route them through [`ActiveWait::matches`]
/// or `WaitProtocol::classify_interrupt` instead.
///
/// This is the sibling routing predicate PR-10 phase 4 introduces per
/// D7 §3.4 (gap #2): "no driver-side `await_agent_reply` helper
/// consumes `MailboxEvent::AgentReplied` / `Abort`". The helper
/// [`crate::wake::agent_reply::await_agent_reply`] uses this predicate
/// to filter spurious wakes against the bound mailbox. The
/// `ActiveWait::matches` single-fire semantics (DTOK-3 carry-through)
/// are untouched: `agent_event_matches` is a pure predicate, no
/// consumption.
#[inline]
pub fn agent_event_matches(event: &MailboxEvent, expected: DelegateTokenId) -> bool {
    match event {
        MailboxEvent::AgentReplied { token_id } => *token_id == expected,
        MailboxEvent::Abort { token_id, .. } => *token_id == expected,
        MailboxEvent::SourceFired { .. } | MailboxEvent::SignalDelivered { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_generation_starts_at_1_and_is_monotonic() {
        let mb = TaskMailbox::new();
        let a = mb.next_generation();
        let b = mb.next_generation();
        let c = mb.next_generation();
        assert_eq!(a.raw(), 1);
        assert_eq!(b.raw(), 2);
        assert_eq!(c.raw(), 3);
        assert!(a < b && b < c);
    }

    #[test]
    fn current_generation_lags_next_by_one() {
        let mb = TaskMailbox::new();
        let _ = mb.next_generation(); // claims 1
        let _ = mb.next_generation(); // claims 2
        assert_eq!(mb.current_generation().raw(), 2);
    }

    #[test]
    fn post_and_poll_fifo() {
        let mb = TaskMailbox::new();
        let e1 = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(11),
            interests: InterestMask::new(0b1),
        };
        let e2 = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(2),
            source: WaitSourceId::new(22),
            interests: InterestMask::new(0b10),
        };
        assert!(mb.post(e1));
        assert!(mb.post(e2));
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.poll(), Some(e1));
        assert_eq!(mb.poll(), Some(e2));
        assert_eq!(mb.poll(), None);
    }

    #[test]
    fn overflow_latches_on_full_queue_and_drops_event() {
        let mb = TaskMailbox::new();
        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        };
        for _ in 0..MAILBOX_QUEUE_BOUND {
            assert!(mb.post(evt));
        }
        assert!(!mb.overflow());
        // Next post overflows.
        assert!(!mb.post(evt));
        assert!(mb.overflow());
        // Latched until consumed.
        assert!(mb.overflow());
        assert!(mb.take_overflow());
        assert!(!mb.overflow());
    }

    #[test]
    fn active_wait_matches_fresh_event_with_overlapping_interests() {
        let aw = ActiveWait::new(
            WaitGeneration::new(7),
            WaitSourceId::new(42),
            InterestMask::new(0b1010),
        );
        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(7),
            source: WaitSourceId::new(42),
            interests: InterestMask::new(0b0010),
        };
        assert!(aw.matches(&evt));
    }

    #[test]
    fn active_wait_rejects_stale_generation() {
        let aw = ActiveWait::new(
            WaitGeneration::new(7),
            WaitSourceId::new(42),
            InterestMask::new(0b1010),
        );
        let stale = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(6),
            source: WaitSourceId::new(42),
            interests: InterestMask::new(0b0010),
        };
        assert!(!aw.matches(&stale));
    }

    #[test]
    fn active_wait_rejects_wrong_source() {
        let aw = ActiveWait::new(
            WaitGeneration::new(7),
            WaitSourceId::new(42),
            InterestMask::new(0b1010),
        );
        let wrong = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(7),
            source: WaitSourceId::new(99),
            interests: InterestMask::new(0b0010),
        };
        assert!(!aw.matches(&wrong));
    }

    fn make_test_waker(flag: alloc::sync::Arc<core::sync::atomic::AtomicBool>) -> Waker {
        use alloc::sync::Arc;
        use core::task::{RawWaker, RawWakerVTable};

        unsafe fn clone(p: *const ()) -> RawWaker {
            let arc = unsafe { Arc::from_raw(p as *const core::sync::atomic::AtomicBool) };
            let cloned = Arc::clone(&arc);
            let _ = Arc::into_raw(arc);
            RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
        }
        unsafe fn wake(p: *const ()) {
            let arc = unsafe { Arc::from_raw(p as *const core::sync::atomic::AtomicBool) };
            arc.store(true, Ordering::Release);
        }
        unsafe fn wake_by_ref(p: *const ()) {
            let arc = unsafe { Arc::from_raw(p as *const core::sync::atomic::AtomicBool) };
            arc.store(true, Ordering::Release);
            let _ = Arc::into_raw(arc);
        }
        unsafe fn drop_fn(p: *const ()) {
            drop(unsafe { Arc::from_raw(p as *const core::sync::atomic::AtomicBool) });
        }
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_fn);

        let raw = RawWaker::new(Arc::into_raw(flag) as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    #[test]
    fn post_wakes_registered_waker() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let mb = TaskMailbox::new();
        let flag = Arc::new(AtomicBool::new(false));
        let waker = make_test_waker(Arc::clone(&flag));
        mb.register_waker(waker);

        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        };
        assert!(mb.post(evt));
        assert!(
            flag.load(Ordering::Acquire),
            "registered waker should fire on post"
        );
    }

    #[test]
    fn clear_waker_suppresses_future_wakes() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let mb = TaskMailbox::new();
        let flag = Arc::new(AtomicBool::new(false));
        let waker = make_test_waker(Arc::clone(&flag));
        mb.register_waker(waker);
        mb.clear_waker();

        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        };
        assert!(mb.post(evt));
        assert!(
            !flag.load(Ordering::Acquire),
            "cleared waker should not fire"
        );
    }

    #[test]
    fn post_wakes_even_on_overflow_path() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let mb = TaskMailbox::new();
        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        };
        for _ in 0..MAILBOX_QUEUE_BOUND {
            assert!(mb.post(evt));
        }

        let flag = Arc::new(AtomicBool::new(false));
        let waker = make_test_waker(Arc::clone(&flag));
        mb.register_waker(waker);

        // Now overflow.
        assert!(!mb.post(evt));
        assert!(mb.overflow());
        assert!(
            flag.load(Ordering::Acquire),
            "overflow path should still wake — the driver re-observes"
        );
    }

    #[test]
    fn active_wait_rejects_disjoint_interest_mask() {
        let aw = ActiveWait::new(
            WaitGeneration::new(7),
            WaitSourceId::new(42),
            InterestMask::new(0b1010),
        );
        let disjoint = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(7),
            source: WaitSourceId::new(42),
            interests: InterestMask::new(0b0101),
        };
        assert!(!aw.matches(&disjoint));
    }

    #[test]
    fn active_wait_rejects_signal_delivered() {
        // D9-A: `SignalDelivered` is a wake-hint, not a wait-source
        // fire. `ActiveWait::matches` returns `false` so the driver
        // does not resolve the active wait on the event; the future
        // re-polls and reads `InterruptSummary` instead.
        let aw = ActiveWait::new(
            WaitGeneration::new(7),
            WaitSourceId::new(42),
            InterestMask::new(0b1010),
        );
        let evt = MailboxEvent::SignalDelivered {
            signum: 15, // SIGTERM
            routing: SignalRouting::ProcessDirected,
        };
        assert!(!aw.matches(&evt));

        let evt_thread = MailboxEvent::SignalDelivered {
            signum: 9, // SIGKILL
            routing: SignalRouting::ThreadDirected { tid: 42 },
        };
        assert!(!aw.matches(&evt_thread));
    }

    #[test]
    fn signal_delivered_post_wakes_registered_waker() {
        // D9-A: even though `SignalDelivered` does not match an
        // `ActiveWait`, posting it still wakes the registered waker
        // so the future re-polls and observes `InterruptSummary`.
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let mb = TaskMailbox::new();
        let flag = Arc::new(AtomicBool::new(false));
        let waker = make_test_waker(Arc::clone(&flag));
        mb.register_waker(waker);

        let evt = MailboxEvent::SignalDelivered {
            signum: 15,
            routing: SignalRouting::ProcessDirected,
        };
        assert!(mb.post(evt));
        assert!(
            flag.load(Ordering::Acquire),
            "SignalDelivered post should still wake registered waker",
        );
        assert_eq!(mb.poll(), Some(evt));
    }
}
