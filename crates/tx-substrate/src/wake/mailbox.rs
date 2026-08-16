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
//! enum (`Normal`/`WakeHandoff`/`LifecycleWake`/`SignalDelivery`/
//! `PriorityBoost`/`None`) that
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

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use core::task::Waker;

use alloc::collections::VecDeque;

use crate::step::{AbortReason, DelegateTokenId, InterestMask, WaitSourceId};
use crate::wake::deadline::TimerToken;
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
    /// Sentinel zero value — used by `YieldResolved::PLACEHOLDER` and
    /// other callers that have no wait generation in scope (e.g. kernel
    /// actors that never actually park).
    pub const ZERO: Self = Self(0);
}

/// Classification of a [`MailboxEvent::SignalDelivered`] post per
/// `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
/// §4 Option A. The variant tells the eventual driver/siginfo
/// formatter where the signal came from so it can later attribute
/// `si_code` (`SI_USER` for thread-directed, `SI_KERNEL` /
/// `SI_QUEUE` for process-directed posts produced by `kill(pid,sig)`,
/// etc.).
///
/// Day-1 callers post `ProcessDirected` from process-directed kill /
/// Gewalt routing (process-wide / group-wide fanout) and
/// `ThreadDirected { tid }` from `tgkill`-shaped paths
/// (only Gewalt SIGKILL bypass and the catchable-signal
/// thread post today; the `tid` is the targeted thread's TID for
/// future siginfo attribution).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalRouting {
    /// `kill(pid, sig)` / Gewalt routing / group-fanout post: the
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
/// Wake-event variants today:
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
/// - [`SignalTimerFired`](Self::SignalTimerFired) — a signal-producing timer
///   deadline expired. It is a wake hint asking the entry path to re-check the
///   POSIX/itimer tables and perform canonical signal delivery; it does not
///   itself mean a signal is already pending.
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
    /// Posted by the `DelegateRegistry` delegate reply transition on the
    /// `TransitionOutcome::Applied` path. Late writers (any
    /// `LateNoOp`) do **not** post — DTOK-1.
    AgentReplied { token_id: DelegateTokenId },
    /// A `DelegateRegistry` transitioned the named token to a
    /// non-`Replied` terminal state. `reason` names the abort
    /// (`Canceled` / `AgentDied` / `TimedOut`) so the driver can
    /// translate to the right errno (`EINTR` / `EOWNERDEAD` /
    /// `ETIMEDOUT` per `05_DELEGATE_v1.md` §7).
    ///
    /// Posted by the `DelegateRegistry` delegate cancel transition /
    /// `delegate agent-death transition` / `delegate timeout transition` on the
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
    /// - `thread_runtime::execution::post_signal_with_post` (catchable
    ///   per-thread post; `ProcessDirected` when invoked through
    ///   process-directed kill, `ThreadDirected { tid }` when invoked
    ///   directly from a tgkill-shaped path).
    /// - `signal::route_gewalt_with_post` (SIGSTOP/SIGCONT per-thread loop).
    /// - `thread_runtime::execution::set_thread_zombie`
    ///   (terminal-state notification so a parked future observes
    ///   `summary.termination` and resolves to `Killed`/`Interrupted`).
    SignalDelivered { signum: u32, routing: SignalRouting },
    /// A reactor deadline entry has expired. The driver matches
    /// this against the in-flight `OnTimer` wait's token to resolve
    /// the yield via `ResumeOutcome::TimerExpired`.
    ///
    /// Posted by the reactor deadline domain when its clock tick advances
    /// past the entry's deadline.
    TimerFired { token: TimerToken },
    /// A signal-producing reactor deadline entry expired.
    ///
    /// This is intentionally distinct from [`SignalDelivered`](Self::SignalDelivered):
    /// the deadline domain does not own POSIX signal semantics. Consumers should
    /// use this event to re-run the timer table due scan, which queues the real
    /// signal through the normal signal subsystem and then posts
    /// `SignalDelivered` if delivery state changed.
    SignalTimerFired { token: TimerToken },
}

/// Action returned by [`TaskMailbox::poll_select`] for each queued event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxPollAction {
    /// Leave this event queued and continue scanning later events.
    Keep,
    /// Remove this event as stale or no longer relevant, then keep scanning.
    Drop,
    /// Remove and return this event to the caller.
    Take,
}

/// Scheduler-facing priority hint latched by [`TaskMailbox::post`].
///
/// The mailbox owns the semantic event type, while the reactor owns concrete
/// queue placement. Default semantic posts use [`Normal`](Self::Normal)
/// except for delivered signals; producers that know a stronger scheduling
/// reason use [`TaskMailbox::post_with_scheduler_hint`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxSchedulerHint {
    Normal,
    WakeHandoff,
    LifecycleWake,
    PriorityBoost,
    SignalDelivery,
}

impl MailboxSchedulerHint {
    const fn code(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::WakeHandoff => 1,
            Self::LifecycleWake => 2,
            Self::PriorityBoost => 3,
            Self::SignalDelivery => 4,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            4 => Self::SignalDelivery,
            3 => Self::PriorityBoost,
            2 => Self::LifecycleWake,
            1 => Self::WakeHandoff,
            _ => Self::Normal,
        }
    }
}

/// Bounded MPSC queue capacity for a single mailbox.
///
/// Tuned for "many small wakes per scheduling slice." If full,
/// [`TaskMailbox::post`] sets [`TaskMailbox::overflow`] and drops the
/// event. The driver treats an overflow flag as a wake hint and
/// re-observes the underlying source on next poll — semantically a
/// safe fallback because masks are hints, not truth.
pub const MAILBOX_QUEUE_BOUND: usize = 64;

/// Reactor scheduler owner bound to a task mailbox.
///
/// This is intentionally substrate-neutral: the substrate records raw slot
/// identity and generation, while `tx-reactor` interprets them as `TaskId` and
/// `TaskGeneration`. Trace fields such as `task_id_low` are not scheduler
/// authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskMailboxSchedulerOwner {
    task_index: usize,
    task_generation: u64,
}

impl TaskMailboxSchedulerOwner {
    pub const fn new(task_index: usize, task_generation: u64) -> Self {
        Self {
            task_index,
            task_generation,
        }
    }

    pub const fn task_index(self) -> usize {
        self.task_index
    }

    pub const fn task_generation(self) -> u64 {
        self.task_generation
    }
}

/// Per-reactor-task wake mailbox.
///
/// Owned by a reactor task (in practice via a `Cap<TaskMailbox>`
/// once zone allocation is wired in). Holds the generation counter,
/// a bounded MPSC of [`MailboxEvent`], and an overflow flag.
///
/// **Not tied to `ProcessIdentity`.** A process may have many
/// reactor tasks (user threads, kthreads, OnBehalfOf-borrowed scope
/// workers); each owns its own mailbox.
///
/// `task_id_low` carries the low 32 bits of the task's trace
/// identity (thread TID, or 0 for kernel actors). Set at
/// construction via [`Self::with_task_id`]; read by
/// [`WaitSource::notify_emit`] when emitting
/// `PayloadWaitSourceNotify` records.
pub struct TaskMailbox {
    generation: AtomicU64,
    /// Monotonic edge counter advanced for every post attempt, including
    /// coalesced and overflowed events.  Poll wrappers use this to close the
    /// register-vs-park race without treating an old queued event as a reason
    /// to self-wake forever.
    post_sequence: AtomicU64,
    queue: SpinMutex<VecDeque<MailboxEvent>>,
    overflow: AtomicBool,
    /// Optional `core::task::Waker` registered by the current poll
    /// context. Set via [`Self::register_waker`]; called by
    /// [`Self::post`] when an event arrives so the parked future
    /// gets a re-poll signal. PR-3D step 1.
    waker: SpinMutex<Option<Waker>>,
    /// Low 32 bits of the task's trace identity (thread TID for
    /// user threads, 0 for kernel actors). Populated at construction
    /// via [`Self::with_task_id`]; read by `notify_emit` and other
    /// substrate convergence-point emitters (OBS-4 / γ-fix).
    task_id_low: u32,
    /// Low 32 bits of the owning process trace identity (PID for
    /// user threads; 0 for kernel-internal actors and tests). Set via
    /// [`Self::with_process_id`]; read by `emit_sched_*` so the daemon
    /// can render per-process ProcessDescriptor tracks parenting
    /// per-thread tracks (OBS-V1 §15.6 sched_switch view).
    process_id_low: u32,
    scheduler_owner_present: AtomicBool,
    scheduler_owner_task: AtomicUsize,
    scheduler_owner_generation: AtomicU64,
    scheduler_hint: AtomicU64,
}

impl TaskMailbox {
    /// Construct a mailbox with no task-id attached (kernel actors,
    /// tests that do not exercise the task-id field). The trace
    /// identity will be `task_id_low = 0`, which is the documented
    /// value for kernel-internal actors.
    pub fn new() -> Self {
        Self {
            // Generations are 1-based; `WaitGeneration::new(0)` is a
            // never-issued sentinel callers can use to mean "no active
            // wait."
            generation: AtomicU64::new(1),
            post_sequence: AtomicU64::new(0),
            queue: SpinMutex::new(VecDeque::new()),
            overflow: AtomicBool::new(false),
            waker: SpinMutex::new(None),
            task_id_low: 0,
            process_id_low: 0,
            scheduler_owner_present: AtomicBool::new(false),
            scheduler_owner_task: AtomicUsize::new(0),
            scheduler_owner_generation: AtomicU64::new(0),
            scheduler_hint: AtomicU64::new(0),
        }
    }

    /// Builder: attach a task trace identity to this mailbox.
    ///
    /// Pass the low 32 bits of the thread/task's canonical trace id
    /// (typically `Tid.0` for user threads). Kernel actors that have
    /// no stable identity should use `new()` (implicitly `task_id_low
    /// = 0`).
    ///
    /// ```ignore
    /// let mailbox = Arc::new(TaskMailbox::new().with_task_id(tid.0));
    /// ```
    #[must_use]
    pub fn with_task_id(mut self, task_id_low: u32) -> Self {
        self.task_id_low = task_id_low;
        self
    }

    /// Builder: attach a process trace identity to this mailbox.
    ///
    /// Pass the low 32 bits of the owning process's canonical
    /// `Pid.0`. Kernel actors with no stable identity may leave the
    /// default of `0`.
    #[must_use]
    pub fn with_process_id(mut self, process_id_low: u32) -> Self {
        self.process_id_low = process_id_low;
        self
    }

    /// Builder: attach the reactor scheduler owner for this mailbox.
    ///
    /// The owner is separate from trace ids. It is the identity the reactor
    /// uses to turn a mailbox event into runnable scheduler placement.
    #[must_use]
    pub fn with_scheduler_owner(self, task_index: usize, task_generation: u64) -> Self {
        self.scheduler_owner_task
            .store(task_index, Ordering::Relaxed);
        self.scheduler_owner_generation
            .store(task_generation, Ordering::Relaxed);
        self.scheduler_owner_present.store(true, Ordering::Release);
        self
    }

    /// Return the scheduler owner associated with this mailbox, if any.
    #[inline]
    pub fn scheduler_owner(&self) -> Option<TaskMailboxSchedulerOwner> {
        if !self.scheduler_owner_present.load(Ordering::Acquire) {
            return None;
        }
        Some(TaskMailboxSchedulerOwner::new(
            self.scheduler_owner_task.load(Ordering::Acquire),
            self.scheduler_owner_generation.load(Ordering::Acquire),
        ))
    }

    /// Low 32 bits of the owning process's trace identity.
    ///
    /// Returns the value set via [`Self::with_process_id`], or `0`
    /// for kernel-internal actors.
    #[inline]
    pub fn process_id_low(&self) -> u32 {
        self.process_id_low
    }

    /// Low 32 bits of the task's trace identity.
    ///
    /// Reads the value set at construction via [`Self::with_task_id`].
    /// Returns `0` for mailboxes constructed with [`Self::new()`]
    /// that have not called `with_task_id`. A return value of `0`
    /// is the documented sentinel for kernel-internal actors.
    #[inline]
    pub fn task_id_low(&self) -> u32 {
        self.task_id_low
    }

    /// Register a `core::task::Waker` to be woken when an event is
    /// posted to this mailbox. Replaces any previously-registered
    /// waker (futures call this on every `poll` per the async
    /// contract, so the most recent waker is the right one to wake).
    pub fn register_waker(&self, waker: Waker) {
        *self.waker.lock() = Some(waker);
    }

    /// Drop any registered waker without waking.
    ///
    /// This is only valid when the caller owns the whole mailbox (for example
    /// a private deadline mailbox). A reactor task mailbox is shared by the
    /// task wrapper, signals, and nested wait protocols; a nested wait must
    /// not clear that task-level wake route when only its own wait completes.
    pub fn clear_waker(&self) {
        *self.waker.lock() = None;
    }

    /// Whether a future currently has a wake route installed.
    ///
    /// Diagnostic only: semantic readiness remains owned by the source.
    pub fn has_waker(&self) -> bool {
        self.waker.lock().is_some()
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

    /// Return the current mailbox-post edge sequence.
    ///
    /// Unlike queue length, this distinguishes an event that arrived during
    /// the caller's poll from unrelated state that was already queued.  Every
    /// post attempt advances the sequence, even when it coalesces with an
    /// existing event or only latches overflow.
    #[inline]
    pub fn post_sequence(&self) -> u64 {
        self.post_sequence.load(Ordering::Acquire)
    }

    /// Post an event to the mailbox. Returns `true` if enqueued,
    /// `false` if the queue overflowed. On overflow the
    /// [`Self::overflow`] flag is latched until the driver consumes
    /// it via [`Self::take_overflow`].
    pub fn post(&self, event: MailboxEvent) -> bool {
        self.post_with_scheduler_hint(event, Self::default_scheduler_hint(event))
    }

    /// Post an event with an explicit scheduler hint.
    ///
    /// Use this only at producer convergence points that know the reason for
    /// the wake. Generic readiness notifications should continue to use
    /// [`Self::post`], which maps `SourceFired` to `Normal`.
    pub fn post_with_scheduler_hint(
        &self,
        event: MailboxEvent,
        hint: MailboxSchedulerHint,
    ) -> bool {
        self.publish_scheduler_hint(hint);
        let enqueued = {
            let mut q = self.queue.lock();
            // Coalesce consecutive `SourceFired` deliveries for the
            // same `(generation, source)` so repeated `fire()` calls
            // between observations only wake once. Drivers re-observe
            // the underlying source on poll, so widening the OR'd
            // interest mask is safe.
            if let MailboxEvent::SourceFired {
                generation,
                source,
                interests,
            } = event
            {
                let mut coalesced = false;
                for existing in q.iter_mut() {
                    if let MailboxEvent::SourceFired {
                        generation: g,
                        source: s,
                        interests: i,
                    } = existing
                    {
                        if *g == generation && *s == source {
                            *i = crate::step::InterestMask::new(i.raw() | interests.raw());
                            coalesced = true;
                            break;
                        }
                    }
                }
                if coalesced {
                    false
                } else if q.len() >= MAILBOX_QUEUE_BOUND {
                    self.overflow.store(true, Ordering::Release);
                    false
                } else {
                    q.push_back(event);
                    true
                }
            } else if q.len() >= MAILBOX_QUEUE_BOUND {
                self.overflow.store(true, Ordering::Release);
                false
            } else {
                q.push_back(event);
                true
            }
        };
        // Publish the post edge after the queue/overflow update is visible and
        // before firing the registered waker.  A poll wrapper can therefore
        // register, sample, poll an inner future that temporarily replaces the
        // mailbox waker, then re-register and compare the sequence to close
        // the complete register-vs-park window.
        self.post_sequence.fetch_add(1, Ordering::AcqRel);
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

    const fn default_scheduler_hint(event: MailboxEvent) -> MailboxSchedulerHint {
        match event {
            MailboxEvent::SignalDelivered { .. } | MailboxEvent::SignalTimerFired { .. } => {
                MailboxSchedulerHint::SignalDelivery
            }
            MailboxEvent::SourceFired { .. }
            | MailboxEvent::AgentReplied { .. }
            | MailboxEvent::Abort { .. }
            | MailboxEvent::TimerFired { .. } => MailboxSchedulerHint::Normal,
        }
    }

    fn publish_scheduler_hint(&self, hint: MailboxSchedulerHint) {
        let code = u64::from(hint.code());
        let mut current = self.scheduler_hint.load(Ordering::Acquire);
        while code > current {
            match self.scheduler_hint.compare_exchange_weak(
                current,
                code,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    /// Consume the scheduler hint associated with the current wake batch.
    pub fn take_scheduler_hint(&self) -> MailboxSchedulerHint {
        MailboxSchedulerHint::from_code(self.scheduler_hint.swap(0, Ordering::AcqRel) as u8)
    }

    /// Drain the next event. Returns `None` if empty.
    pub fn poll(&self) -> Option<MailboxEvent> {
        self.queue.lock().pop_front()
    }

    /// Drain the first event selected by `classify` while preserving unrelated
    /// events in queue order.
    ///
    /// This supports task-owned mailboxes shared by timer, signal, delegate,
    /// and wait-source drivers: each driver can take only its own matching
    /// event and drop its own stale generations without consuming other wake
    /// payloads.
    pub fn poll_select<F>(&self, mut classify: F) -> Option<MailboxEvent>
    where
        F: FnMut(&MailboxEvent) -> MailboxPollAction,
    {
        let mut q = self.queue.lock();
        let mut index = 0;
        while index < q.len() {
            match classify(&q[index]) {
                MailboxPollAction::Keep => index += 1,
                MailboxPollAction::Drop => {
                    let _ = q.remove(index);
                }
                MailboxPollAction::Take => return q.remove(index),
            }
        }
        None
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
            | MailboxEvent::SignalDelivered { .. }
            | MailboxEvent::TimerFired { .. }
            | MailboxEvent::SignalTimerFired { .. } => false,
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
        MailboxEvent::SourceFired { .. }
        | MailboxEvent::SignalDelivered { .. }
        | MailboxEvent::TimerFired { .. }
        | MailboxEvent::SignalTimerFired { .. } => false,
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
    fn post_sequence_tracks_enqueued_coalesced_and_overflow_posts() {
        let mb = TaskMailbox::new();
        assert_eq!(mb.post_sequence(), 0);

        let first = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        };
        assert!(mb.post(first));
        assert_eq!(mb.post_sequence(), 1);

        assert!(!mb.post(MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b10),
        }));
        assert_eq!(mb.post_sequence(), 2, "coalesced post is still an edge");

        for i in 1..MAILBOX_QUEUE_BOUND {
            assert!(mb.post(MailboxEvent::SourceFired {
                generation: WaitGeneration::new(i as u64 + 1),
                source: WaitSourceId::new(1),
                interests: InterestMask::new(0b1),
            }));
        }
        let before_overflow = mb.post_sequence();
        assert!(!mb.post(MailboxEvent::TimerFired {
            token: TimerToken::new(99),
        }));
        assert!(mb.overflow());
        assert_eq!(mb.post_sequence(), before_overflow + 1);
    }

    #[test]
    fn poll_select_takes_matching_event_and_preserves_unrelated_events() {
        let mb = TaskMailbox::new();
        let signal = MailboxEvent::SignalDelivered {
            signum: 10,
            routing: SignalRouting::ProcessDirected,
        };
        let stale = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(11),
            interests: InterestMask::new(0b1),
        };
        let ready = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(2),
            source: WaitSourceId::new(11),
            interests: InterestMask::new(0b1),
        };
        let timer = MailboxEvent::TimerFired {
            token: TimerToken::new(9),
        };

        assert!(mb.post(signal));
        assert!(mb.post(stale));
        assert!(mb.post(timer));
        assert!(mb.post(ready));

        assert_eq!(
            mb.poll_select(|event| match event {
                MailboxEvent::SourceFired {
                    source, generation, ..
                } if *source == WaitSourceId::new(11) && *generation == WaitGeneration::new(1) => {
                    MailboxPollAction::Drop
                }
                MailboxEvent::SourceFired {
                    source, generation, ..
                } if *source == WaitSourceId::new(11) && *generation == WaitGeneration::new(2) => {
                    MailboxPollAction::Take
                }
                _ => MailboxPollAction::Keep,
            }),
            Some(ready)
        );
        assert_eq!(mb.poll(), Some(signal));
        assert_eq!(mb.poll(), Some(timer));
        assert_eq!(mb.poll(), None);
    }

    #[test]
    fn source_fired_latches_normal_scheduler_hint_by_default() {
        let mb = TaskMailbox::new();
        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(11),
            interests: InterestMask::new(0b1),
        };
        assert_eq!(mb.take_scheduler_hint(), MailboxSchedulerHint::Normal);
        assert!(mb.post(evt));
        assert_eq!(mb.take_scheduler_hint(), MailboxSchedulerHint::Normal);
        assert_eq!(mb.take_scheduler_hint(), MailboxSchedulerHint::Normal);
    }

    #[test]
    fn explicit_source_fired_hint_latches_strongest_scheduler_hint() {
        let mb = TaskMailbox::new();
        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(11),
            interests: InterestMask::new(0b1),
        };
        assert!(mb.post_with_scheduler_hint(evt, MailboxSchedulerHint::WakeHandoff));
        assert_eq!(mb.take_scheduler_hint(), MailboxSchedulerHint::WakeHandoff);
        assert_eq!(mb.poll(), Some(evt));

        let evt2 = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(2),
            source: WaitSourceId::new(11),
            interests: InterestMask::new(0b1),
        };
        assert!(mb.post_with_scheduler_hint(evt2, MailboxSchedulerHint::LifecycleWake));
        assert!(mb.post_with_scheduler_hint(
            MailboxEvent::TimerFired {
                token: TimerToken::new(1)
            },
            MailboxSchedulerHint::PriorityBoost
        ));
        assert!(mb.post_with_scheduler_hint(
            MailboxEvent::TimerFired {
                token: TimerToken::new(2)
            },
            MailboxSchedulerHint::WakeHandoff
        ));
        assert!(mb.post(MailboxEvent::SignalDelivered {
            signum: 15,
            routing: SignalRouting::ProcessDirected,
        }));
        assert_eq!(
            mb.take_scheduler_hint(),
            MailboxSchedulerHint::SignalDelivery
        );
    }

    #[test]
    fn overflow_latches_on_full_queue_and_drops_event() {
        let mb = TaskMailbox::new();
        for i in 0..MAILBOX_QUEUE_BOUND {
            assert!(mb.post(MailboxEvent::SourceFired {
                generation: WaitGeneration::new(i as u64 + 1),
                source: WaitSourceId::new(1),
                interests: InterestMask::new(0b1),
            }));
        }
        assert!(!mb.overflow());
        // Next post overflows.
        assert!(!mb.post(MailboxEvent::SourceFired {
            generation: WaitGeneration::new(MAILBOX_QUEUE_BOUND as u64 + 1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        }));
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
    fn coalesced_source_fired_still_wakes_registered_waker() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let mb = TaskMailbox::new();
        let evt = MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        };
        assert!(mb.post(evt));

        let flag = Arc::new(AtomicBool::new(false));
        mb.register_waker(make_test_waker(Arc::clone(&flag)));

        assert!(
            !mb.post(MailboxEvent::SourceFired {
                generation: WaitGeneration::new(1),
                source: WaitSourceId::new(1),
                interests: InterestMask::new(0b10),
            }),
            "coalescing does not enqueue a second event"
        );
        assert!(
            flag.load(Ordering::Acquire),
            "coalescing an existing event must still wake its consumer"
        );
        assert_eq!(mb.len(), 1);
        assert_eq!(
            mb.poll(),
            Some(MailboxEvent::SourceFired {
                generation: WaitGeneration::new(1),
                source: WaitSourceId::new(1),
                interests: InterestMask::new(0b11),
            })
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
        for i in 0..MAILBOX_QUEUE_BOUND {
            assert!(mb.post(MailboxEvent::SourceFired {
                generation: WaitGeneration::new(i as u64 + 1),
                source: WaitSourceId::new(1),
                interests: InterestMask::new(0b1),
            }));
        }

        let flag = Arc::new(AtomicBool::new(false));
        let waker = make_test_waker(Arc::clone(&flag));
        mb.register_waker(waker);

        // Now overflow.
        assert!(!mb.post(MailboxEvent::SourceFired {
            generation: WaitGeneration::new(MAILBOX_QUEUE_BOUND as u64 + 1),
            source: WaitSourceId::new(1),
            interests: InterestMask::new(0b1),
        }));
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
