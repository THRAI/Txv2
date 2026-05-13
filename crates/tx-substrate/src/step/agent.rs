//! Delegate-side runtime for `YieldShape::OnAgent`.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` (txdoc:STEP-V2-YIELD-SHAPE-1), the
//! `YieldShape` catalog includes `OnAgent` (delegate-side waits). The
//! per-field types of `OnAgent` come from `docs/Txv3/05_DELEGATE_v1.md`:
//! `DelegateEndpoint`, `DelegateRequest`, `DelegateToken`, `Deadline`,
//! and `CancelPolicy`.
//!
//! **PR-7 status.** The catalog placeholders (`DelegateEndpoint`,
//! `DelegateToken`, `DelegateRequest`, `DelegateReply`,
//! `AgentCancelPolicy`, `TokenDropPolicy`) remain stable; PR-7 lands
//! the **runtime state machine** that mediates `install_request` →
//! agent reply / timeout / cancel / agent-died races. Per-EndpointKind
//! typed request/reply variants (UfdRequest, FuseRequest, …) are still
//! deferred to PR-10 and beyond.
//!
//! The runtime is the `DelegateRegistry`: it owns per-token state
//! (`DelegateState`), mints `DelegateTokenId`s, and is the single
//! linearization point for the `Pending → ReplyInstalling → Replied`
//! and `Pending → {Canceled, AgentDied, TimedOut}` transitions. See
//! [`DelegateRegistry`] for the state-machine docs and DTOK-1 / DTOK-2
//! / DTOK-3 invariant references.
//!
//! ## Layering and the timer-wheel boundary
//!
//! Per [`docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md`]
//! the `TimerWheel` lives in [`crate::wake::timer`] (same crate). The
//! `tx-reactor` crate re-exports the same types at its root for
//! back-compat, but the canonical home is the substrate side. The
//! integration contract is therefore **a same-crate call**:
//!
//! - The driver installs a `TimerGuard` with role
//!   `TimerGuardRole::DelegateTimeout` against a token's deadline
//!   via [`crate::wake::timer::TimerWheel::install_delegate_timeout`].
//!   That helper tags the timer entry with the `DelegateTokenId`.
//! - On timer-tick the reactor calls
//!   [`crate::wake::timer::TimerWheel::fire_due_delegate_timeouts`]`(now, &registry)`
//!   which walks expired `DelegateTimeout` entries and invokes
//!   [`DelegateRegistry::mark_timed_out`] for each. The CAS races
//!   whichever transition got there first (agent reply, cancel,
//!   agent death). DTOK-3: first writer wins.
//! - When the script-side waiter resumes (any terminal transition),
//!   the driver drops the `TimerGuard`, retiring the timer-wheel
//!   registration regardless of which side won the race.
//!
//! PR-7B layered the wake-side glue on top of this: a per-token
//! `Weak<TaskMailbox>` is stored in the registry slot at
//! `install_request` time; on a `mark_*` transition that returns
//! `TransitionOutcome::Applied` the registry posts the matching
//! `MailboxEvent::AgentReplied` / `MailboxEvent::Abort` to the
//! bound mailbox. Late writers (`LateNoOp`) post nothing — the
//! winner is the single source of the wake event (DTOK-1, DTOK-2,
//! DTOK-3).
//!
//! ## EndpointScope abandonment
//!
//! Per `docs/Txv3/05_DELEGATE_v1.md` §3.1 a `Process`-scoped endpoint
//! dies when its owning process exits; a `Thread`-scoped endpoint
//! dies when its owning thread exits. On endpoint death the runtime
//! walks the in-flight tokens for that endpoint and calls
//! [`DelegateRegistry::mark_agent_died`] on each one. The
//! per-endpoint walk routing is documented but not yet implemented
//! here (placeholder helper [`DelegateRegistry::mark_endpoint_died`]
//! exists; full `EndpointScope` discrimination lands when the
//! endpoint cap-zone does, per `07_BLAST_RADIUS.md` §6 risk row
//! "EndpointScope abandonment routing edge cases").

use alloc::sync::Weak;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::sync::SpinMutex;
use crate::wake::timer::{TimerGuard, TimerWheel};
use crate::wake::{MailboxEvent, TaskMailbox};

/// Closed catalog of agent-side cancellation policies (per
/// `docs/Txv3/05_DELEGATE_v1.md`). Names the protocol the agent
/// sees when a delegate request is cancelled.
///
/// Renamed from `CancelPolicy` per the v3 migration's PR-6: the
/// "Agent" prefix disambiguates from [`TokenDropPolicy`], which
/// governs what happens to the agent token at `ActiveWait` drop
/// time. The two compose orthogonally:
///
/// - `TokenDropPolicy::CancelOnDrop + AgentCancelPolicy::Synchronous`:
///   on drop, cancel the agent's request and wait for ack.
/// - `TokenDropPolicy::Abandon`: drop ⇒ unbind only, no cancel
///   protocol with the agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentCancelPolicy {
    BestEffort,
    Synchronous,
    Detached,
}

/// Closed catalog of token-drop policies (per
/// `docs/Txv3/05_DELEGATE_v1.md` §6.2). Names what happens when an
/// [`ActiveWait`](crate::mailbox::ActiveWait) holding an agent token
/// is dropped without being explicitly resolved (timeout, signal,
/// script frame teardown, …).
///
/// Held internally by [`AgentTokenGuard`]; consumed only at
/// `ActiveWait` drop time. The runtime never reads it during normal
/// resume.
///
/// Composes orthogonally with [`AgentCancelPolicy`]:
/// `AgentCancelPolicy` decides the protocol the agent sees;
/// `TokenDropPolicy` decides whether `ActiveWait` drop initiates
/// that protocol.
///
/// `KeepAlive` is reserved (not in R2); landing it requires a
/// concrete consumer (a token's reply being passed to a different
/// script frame).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenDropPolicy {
    /// Drop ⇒ token.cancel(Abandoned); waiter unbound.
    CancelOnDrop,
    /// Drop ⇒ unbind waiter only; agent reply lands on dead waiter.
    Abandon,
    // Reserved (not in R2):
    // KeepAlive — drop ⇒ unbind only; preserve token for another consumer.
}

impl TokenDropPolicy {
    /// Per DELEGATE-6: derive the drop policy from the agent-cancel
    /// policy at `prepare_active_wait` time.
    ///
    /// - `BestEffort` / `Synchronous` → `CancelOnDrop` (kernel
    ///   should explicitly cancel the agent's request when the
    ///   script-side wait is abandoned).
    /// - `Detached` → `Abandon` (the agent's reply was already
    ///   declared fire-and-forget; drop just unbinds the waiter).
    pub const fn from_agent_cancel(policy: AgentCancelPolicy) -> Self {
        match policy {
            AgentCancelPolicy::BestEffort => Self::CancelOnDrop,
            AgentCancelPolicy::Synchronous => Self::CancelOnDrop,
            AgentCancelPolicy::Detached => Self::Abandon,
        }
    }
}

/// Closed catalog of delegate replies. PR-10 phase 1 (per D7 §3.2)
/// grows this from the original unit placeholder to a closed sum
/// keyed on endpoint kind, with the `Ufd` arm populated for
/// userfaultfd. Future agent kinds (FUSE, fanotify, …) add variants
/// under ARCH-3 review.
///
/// All variants are `Copy` so the surrounding [`ResumeOutcome`]
/// continues to be `Copy` and travels through the resume path
/// without an allocation — see `05_DELEGATE_v1.md` §7 step 5
/// ("driver takes the reply").
///
/// [`Self::placeholder`] remains as a constructor that returns a
/// sensible default (a zero-valued [`UfdReply::ZeroPage`]) so PR-7
/// state-machine unit tests that construct a placeholder reply keep
/// working unchanged. The placeholder is not observed by any wired
/// VM-side consumer in PR-10 phase 1 — that wiring lands in phase
/// 4 / 5 with the real fault-path replies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegateReply {
    /// Userfaultfd reply (PR-10). Carries the per-ioctl payload the
    /// fault-resume path consumes (`UFFDIO_COPY` / `UFFDIO_ZEROPAGE`
    /// / `UFFDIO_CONTINUE`).
    Ufd(UfdReply),
    // future: Fuse(FuseReply), FanotifyPerm(FanotifyPermReply), ...
}

impl DelegateReply {
    /// Construct a placeholder reply. Returns
    /// `DelegateReply::Ufd(UfdReply::ZeroPage { dst_uaddr: 0, len: 0 })`
    /// — a never-matching variant for state-machine unit tests that
    /// don't care about payload (per D7 §3.2 "substrate-test
    /// friendliness, no functional cost").
    pub const fn placeholder() -> Self {
        Self::Ufd(UfdReply::ZeroPage {
            dst_uaddr: 0,
            len: 0,
        })
    }
}

/// Userfaultfd reply payload. Mirrors the three Linux `UFFDIO_*`
/// reply ioctls per `05_DELEGATE_v1.md` §5 and §8.1, populated by
/// the agent thread's `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` /
/// `UFFDIO_CONTINUE` calls in phase 5.
///
/// All fields are raw `u64` addresses / lengths — the substrate
/// does not translate between user-virtual, kernel-virtual, or
/// physical here. The fault-script consumer (phase 4) re-validates
/// the addresses under a fresh epoch guard before installing the
/// page (per A-15).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UfdReply {
    /// `UFFDIO_COPY`: materialise `[dst_uaddr, dst_uaddr+len)` from
    /// kernel-side buffer `src_kernel_addr`. The substrate carries
    /// raw addresses; the fault path resolves them under a fresh
    /// epoch guard.
    Copy {
        src_kernel_addr: u64,
        dst_uaddr: u64,
        len: u64,
    },
    /// `UFFDIO_ZEROPAGE`: zero-fill `[dst_uaddr, dst_uaddr+len)`.
    ZeroPage { dst_uaddr: u64, len: u64 },
    /// `UFFDIO_CONTINUE`: install the existing page-cache contents
    /// for `[dst_uaddr, dst_uaddr+len)` (used by ufd-shm; the
    /// agent does not supply a new buffer).
    Continue { dst_uaddr: u64, len: u64 },
}

/// Timer-id placeholder used by [`ResumeOutcome::TimerExpired`].
/// PR-8 published the [`crate::wake::timer::TimerToken`] public
/// surface (alongside [`crate::wake::timer::TimerWheel`] /
/// [`crate::wake::timer::TimerGuard`]); D6 relocated those types
/// from `tx-reactor::timer` to `tx-substrate::wake::timer`. PR-7
/// will reconcile this `TimerId` with `TimerToken` when the
/// `OnAgent` runtime wires its `DelegateTimeout`-role guards into
/// the resume path. Until then the two ids are the same raw `u64`
/// shape and round-trip via `.raw()` / `::new(raw)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerId(u64);

impl TimerId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Closed catalog of wait-abort reasons consumed by
/// [`ResumeOutcome::Aborted`] (per `docs/Txv3/03_STEP_MODEL_v2.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbortReason {
    /// Wait was interrupted by an interruptible signal.
    Interrupted,
    /// Wait was killed by SIGKILL.
    Killed,
    /// Wait deadline expired before the primary condition was met.
    TimedOut,
    /// `OnBehalfOf<P>` scope abandoned the wait (subject identity
    /// teardown, kthread exit).
    ScopeAbandoned,
    /// The endpoint owning the in-flight delegation died (process
    /// or thread exit, fd close). Surfaced from the
    /// `DelegateState::AgentDied` terminal transition (DTOK-2).
    AgentDied,
    /// Token was dropped under [`TokenDropPolicy::CancelOnDrop`];
    /// surfaced from the `DelegateState::Canceled` terminal
    /// transition.
    Canceled,
}

/// Deadline placeholder. PR-4 replaces with the real reactor deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline(u64);

impl Deadline {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    /// Sentinel meaning "no deadline" (used by ptrace stops, etc.).
    pub const NEVER: Self = Self(u64::MAX);
}

/// Cap-typed delegate endpoint placeholder. PR-10+ replaces with the
/// real `Cap<DelegateEndpoint<K>>` over a zone-allocated endpoint.
///
/// PR-7 keeps this as an opaque placeholder: the runtime state
/// machine reasons about tokens, not endpoints. The
/// `EndpointScope`-driven walk for endpoint-death routing
/// (`05_DELEGATE_v1.md` §3.1) is documented in [`DelegateRegistry`]
/// but waits for the real endpoint cap-zone to land before it can
/// be implemented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateEndpoint {
    _private: (),
}

impl DelegateEndpoint {
    /// Construct a placeholder endpoint. PR-10+ removes this
    /// constructor in favor of zone allocation.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Cap-typed delegate token placeholder used as a value carried in
/// `YieldShape::OnAgent`. Kept `Copy` and equality-equal to all
/// other placeholders so the existing PR-0 / PR-6 catalog tests
/// remain green.
///
/// Runtime token **identity** is carried by [`DelegateTokenId`],
/// which is minted by [`DelegateRegistry::install_request`]. The
/// two layers exist because:
///
/// - The yield shape needs a `Copy` value that travels through
///   `StepOutcome::Yield` without an allocation.
/// - The runtime needs a unique identifier to look up per-token
///   state in the registry.
///
/// PR-10+ collapses these when the real `Cap<DelegateToken>` lands
/// (the cap is itself a `Copy` zone-id and serves both roles).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateToken {
    _private: (),
}

impl DelegateToken {
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Closed catalog of delegate requests. PR-10 phase 4 (per D7 §3.2 +
/// W-T's flag noted in `docs/progress/STATUS.md`) grows this from the
/// original unit placeholder to a closed sum keyed on endpoint kind,
/// symmetric to [`DelegateReply`]. The `Ufd` arm carries the per-fault
/// payload the agent thread will read off the ufd via the phase-5
/// `read(uffd_msg)` arm. Future agent kinds (FUSE, fanotify, …) add
/// variants under ARCH-3 review.
///
/// [`Self::Placeholder`] is retained for substrate-test friendliness
/// (the PR-7 / PR-7B state-machine tests construct a never-matching
/// request to exercise the registry CAS in isolation — they don't care
/// about the payload). No functional cost: the placeholder variant is
/// not observed by any wired consumer in PR-10.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegateRequest {
    /// Userfaultfd request — populated by `fault_script` (PR-10 phase
    /// 4) when a fault hits a ufd-registered VMA. The agent reads this
    /// payload from the ufd via the phase-5 `read(2)` arm and replies
    /// via `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` / `UFFDIO_CONTINUE`.
    Ufd(UfdRequest),
    /// Never-matching placeholder for PR-7 / PR-7B state-machine
    /// tests that exercise the registry CAS without a real payload.
    /// Production fault paths must use a typed variant.
    Placeholder,
    // future: Fuse(FuseRequest), FanotifyPerm(FanotifyPermRequest), ...
}

/// Userfaultfd request payload (PR-10 phase 4). Symmetric to
/// [`UfdReply`]: the substrate installs one of these via
/// [`DelegateRegistry::install_request`] when `fault_script` hits a
/// ufd-registered VMA; the agent reads it off the ufd via the phase-5
/// `read(2)` arm and replies with a [`UfdReply`].
///
/// All fields are raw `u64` so the substrate does not translate
/// between user-virtual / kernel-virtual / physical here; the fault
/// resume re-validates under a fresh epoch guard (A-15).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UfdRequest {
    /// A faulting thread hit `[faulting_addr, faulting_addr+page)`
    /// in a ufd-registered VMA and the page was not yet present.
    /// Mirrors `struct uffd_msg { .pagefault = { .address, .flags, .feat.ptid } }`
    /// per `man userfaultfd(2)`.
    PageFault {
        /// Faulting user-virtual address (page-aligned). The agent
        /// uses this as the `dst_uaddr` for its eventual
        /// `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` reply.
        faulting_addr: u64,
        /// Access classification — `Missing` (the only kind ufd
        /// phase 4 implements per D7 §3.6), `Wp` (write-protect),
        /// `Minor`. PR-10 only emits `Missing`.
        access_kind: UfdAccessKind,
        /// Faulting thread id, if recorded. The substrate uses this
        /// only as an opaque diagnostic; the agent-side resume
        /// already knows which mailbox the reply targets via
        /// `DelegateTokenId`. Zero means "not recorded."
        faulting_tid: u64,
    },
}

/// Closed catalog of userfaultfd access kinds per `man
/// userfaultfd(2)`. PR-10 phase 4 only implements `Missing` (a
/// page-not-present fault in a registered range); `Wp` and `Minor`
/// are reserved for follow-up phases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UfdAccessKind {
    /// Page is missing — the canonical ufd canary case. Phase 4 only
    /// installs this variant.
    Missing,
    /// Write-protect fault (`UFFDIO_REGISTER_MODE_WP`). Reserved.
    Wp,
    /// Minor fault (`UFFDIO_REGISTER_MODE_MINOR`, ufd-shm). Reserved.
    Minor,
}

// =========================================================================
// PR-7 runtime: DelegateState, DelegateTokenId, DelegateRegistry,
// AgentTokenGuard, and the mark_* state transitions.
// =========================================================================

/// Runtime-unique identifier for a delegation registered in a
/// [`DelegateRegistry`]. Monotonic per-registry; never reused — an
/// already-terminal token id stays terminal even after the registry
/// drops its state.
///
/// Distinct from [`DelegateToken`], which is the `Copy` placeholder
/// value the `YieldShape::OnAgent` shape carries. PR-10+ collapses
/// the two when the real `Cap<DelegateToken>` lands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DelegateTokenId(u64);

impl DelegateTokenId {
    /// Construct a `DelegateTokenId` from a raw `u64`. Provided so
    /// tests and future PRs can round-trip an id without going
    /// through [`DelegateRegistry::install_request`].
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    /// The underlying raw id.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Logical lifecycle of a delegation, per `docs/Txv3/05_DELEGATE_v1.md`
/// §4 and DELEGATE-3 / DTOK-1 / DTOK-2 / DTOK-3 in
/// `docs/Txv3/02_INVARIANTS_v5.md`.
///
/// CAS-only transitions, with the first writer winning:
///
/// ```text
///                ┌──────────────────────┐
///                │       Pending        │  (initial state after install_request)
///                └──────────┬───────────┘
///                           │
///        ┌──────────────────┼──────────────────┐
///        │                  │                  │
///        ▼                  ▼                  ▼
///   ReplyInstalling      Canceled           TimedOut
///        │            (or AgentDied)
///        ▼
///     Replied
/// ```
///
/// - `Pending → ReplyInstalling`: agent has begun writing the
///   reply. `ReplyInstalling` is the **single-writer phase** that
///   owns the `reply` slot (DTOK-1).
/// - `ReplyInstalling → Replied`: the reply has been fully
///   installed; the script-side waiter is now eligible to wake.
/// - `Pending → Canceled`: the script-side wait was abandoned and
///   `TokenDropPolicy::CancelOnDrop` was in effect, OR an explicit
///   cancel arrived. Agent's eventual reply is dropped as
///   `LateReply`.
/// - `Pending → AgentDied`: the endpoint owning the in-flight
///   delegation died (`EndpointScope` abandonment, DTOK-2).
/// - `Pending → TimedOut`: the protocol deadline expired before any
///   other terminal transition (DTOK-3, racing the agent's reply
///   CAS).
///
/// **`Replied → TimedOut` is NOT permitted.** Once `Replied`, the
/// token is terminal and locked in for the script-side resume; a
/// late timeout fire is a no-op. Mirrored: a reply against any
/// non-`Pending` state is rejected as `LateReply` (DTOK-1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegateState {
    /// Initial state. The request has been registered and enqueued
    /// for the agent; the kernel is waiting for a reply.
    Pending,
    /// Agent has begun writing the reply. Single-writer phase that
    /// owns the `reply` slot (DTOK-1).
    ReplyInstalling,
    /// Reply has been fully installed. Terminal.
    Replied,
    /// Script-side wait was abandoned (CancelOnDrop or explicit
    /// cancel). Terminal.
    Canceled,
    /// Endpoint died (process / thread exit, fd close). Terminal.
    AgentDied,
    /// Protocol deadline expired before any other transition.
    /// Terminal.
    TimedOut,
}

impl DelegateState {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Pending),
            1 => Some(Self::ReplyInstalling),
            2 => Some(Self::Replied),
            3 => Some(Self::Canceled),
            4 => Some(Self::AgentDied),
            5 => Some(Self::TimedOut),
            _ => None,
        }
    }
    const fn to_raw(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::ReplyInstalling => 1,
            Self::Replied => 2,
            Self::Canceled => 3,
            Self::AgentDied => 4,
            Self::TimedOut => 5,
        }
    }

    /// `true` for `{Replied, Canceled, AgentDied, TimedOut}`. Once a
    /// token reaches a terminal state, no further transitions
    /// observe (DTOK-1).
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Replied | Self::Canceled | Self::AgentDied | Self::TimedOut
        )
    }
}

/// Outcome of a state-transition CAS. Returned by the `mark_*`
/// methods so the caller can distinguish "this transition won the
/// race" from "another terminal transition was already in effect".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionOutcome {
    /// CAS succeeded; the token is now in the requested terminal
    /// state and (for `mark_replied`) the reply is installed.
    Applied,
    /// Token was already terminal; this transition is a late no-op
    /// (DTOK-1 "late writer is dropped"). The current state is
    /// reported so the caller can route the late event (e.g. drop
    /// the agent's reply, log a stale timer fire).
    LateNoOp(DelegateState),
    /// The supplied [`DelegateTokenId`] does not name any token in
    /// the registry. Programmer error / id forged from a raw u64
    /// that was never minted.
    UnknownToken,
}

/// Per-token state owned by the [`DelegateRegistry`].
///
/// The state field is `AtomicU8` so the CAS-only transition
/// machinery (DTOK-3 reply-vs-timeout race) needs no lock. The
/// reply slot is guarded by the `ReplyInstalling` single-writer
/// phase (DTOK-1): only the thread that won the
/// `Pending → ReplyInstalling` CAS may write `reply`, and it must
/// transition to `Replied` after writing.
///
/// `endpoint_marker` is the substrate-side analogue of the
/// `EndpointScope` discriminator from `05_DELEGATE_v1.md` §3.1.
/// PR-7 stores it as a raw `u64` because the real endpoint
/// cap-zone has not landed yet; the registry's
/// `mark_endpoint_died(marker)` walks all tokens with a matching
/// marker. PR-10+ replaces the marker with a real
/// `Cap<DelegateEndpoint<K>>` once available.
struct TokenSlot {
    state: AtomicU8,
    /// Set only by the winner of the `Pending → ReplyInstalling`
    /// CAS, before that thread transitions the state to `Replied`.
    /// Read only when the state is observed as `Replied`. Mutated
    /// under the `SpinMutex<Vec<TokenSlot>>` guard in the registry
    /// to keep the slot allocation contiguous.
    reply: Option<DelegateReply>,
    /// Opaque per-endpoint identifier used by
    /// [`DelegateRegistry::mark_endpoint_died`] to walk every
    /// in-flight token belonging to a dying endpoint. PR-10+
    /// replaces this with a real `Cap<DelegateEndpoint<K>>` when
    /// the endpoint cap-zone lands.
    endpoint_marker: u64,
    /// Mailbox of the task that is (or was) blocking on this
    /// delegation. Held `Weak` so a dead task does not keep its
    /// mailbox alive: a `Weak::upgrade` failure means the script
    /// frame is already gone (abort / drop raced ahead of the
    /// transition) and the wake event is silently dropped — the
    /// correct behaviour per PR-7B (the waiter no longer exists).
    ///
    /// `None` if the caller passed `Weak::new()`; in that case the
    /// CAS still runs but no mailbox is posted. Useful for unit
    /// tests of the state machine in isolation.
    mailbox: Weak<TaskMailbox>,
}

impl TokenSlot {
    fn new(endpoint_marker: u64, mailbox: Weak<TaskMailbox>) -> Self {
        Self {
            state: AtomicU8::new(DelegateState::Pending.to_raw()),
            reply: None,
            endpoint_marker,
            mailbox,
        }
    }
}

/// Runtime registry of delegation state. The substrate-side
/// linearization point for `Pending → ReplyInstalling → Replied`
/// and `Pending → {Canceled, AgentDied, TimedOut}` transitions
/// (DELEGATE-3, DTOK-1, DTOK-2, DTOK-3).
///
/// One registry instance is shared across all `OnAgent` yield sites
/// that participate in a given delegation domain. PR-7B (reactor
/// side) holds the registry as a `&'static` or `Arc`-shared handle;
/// the substrate-side runtime exposes only the state-machine API
/// (no scheduling, no timer-wheel coupling — see the module-level
/// docs for the timer integration contract).
///
/// **Bounded slot model.** PR-7 uses a `Vec<TokenSlot>` keyed by
/// the issued id. Real `RLIMIT_DELEGATE` accounting (DELEGATE-5)
/// lands separately in tx-services; this registry imposes no
/// growth bound today.
pub struct DelegateRegistry {
    next_id: AtomicU64,
    slots: SpinMutex<Vec<(DelegateTokenId, TokenSlot)>>,
}

impl DelegateRegistry {
    /// Construct an empty registry. Token ids are issued starting
    /// at `1`; id `0` is reserved as a never-issued sentinel for
    /// consistency with [`crate::wake::timer::TimerToken`] /
    /// [`crate::wake::SubscriberId`].
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            slots: SpinMutex::new(Vec::new()),
        }
    }

    /// Number of tokens currently tracked (regardless of state).
    /// Includes terminal tokens whose [`AgentTokenGuard`] has not
    /// yet been dropped. Diagnostic helper for tests.
    pub fn tracked_count(&self) -> usize {
        self.slots.lock().len()
    }

    /// Register a delegate request. Returns an [`AgentTokenGuard`]
    /// whose drop semantics are governed by the supplied
    /// `drop_policy`.
    ///
    /// `endpoint_marker` is an opaque per-endpoint identifier used
    /// by [`Self::mark_endpoint_died`]; PR-7 keeps it as a raw
    /// `u64` because the real endpoint cap-zone has not landed yet.
    /// Callers that don't care about endpoint-death routing may
    /// pass `0`.
    ///
    /// `mailbox` is the [`TaskMailbox`] of the task that will block
    /// on this delegation. Held `Weak` so a dead task does not
    /// keep its mailbox alive: if the script frame is torn down
    /// before a transition fires, the eventual `mark_*` posts
    /// nothing (the `Weak::upgrade` returns `None`). Callers that
    /// don't need wake routing (state-machine unit tests, the
    /// `mark_endpoint_died` walk path) may pass `Weak::new()`.
    ///
    /// The request envelope is reserved for the typed
    /// per-EndpointKind shapes that PR-10+ lands (UfdRequest,
    /// FuseRequest, …). Today's signature takes the closed-catalog
    /// [`DelegateRequest`] placeholder and stores it in the slot
    /// for parity with the spec's `OnceCell<DelegateRequest>`
    /// (`05_DELEGATE_v1.md` §4); on the runtime side the request
    /// is currently write-once and is not observed after install
    /// (the agent reads it through a different surface).
    ///
    /// **State.** Token starts in [`DelegateState::Pending`].
    /// CAS-only transitions from here.
    ///
    /// Per D6 §7, the `deadline` parameter pairs a
    /// [`crate::wake::timer::TimerGuard`] with the
    /// [`AgentTokenGuard`] in a single call. When
    /// `deadline = Some((d, wheel))`, the registry installs a
    /// `DelegateTimeout`-role timer on `wheel` against the freshly
    /// minted [`DelegateTokenId`] and stores the resulting
    /// [`TimerGuard`] inside the [`AgentTokenGuard`]; the timer
    /// guard drops before the token-state CAS on
    /// [`AgentTokenGuard::drop`], so any concurrent
    /// [`crate::wake::timer::TimerWheel::fire_due_delegate_timeouts`]
    /// walk is serialized through the registry CAS (DTOK-3 race
    /// determinism carries through unchanged — see
    /// [`AgentTokenGuard`] drop docs).
    ///
    /// When `deadline = None`, the guard carries no timer
    /// registration (`timer: None`); callers that want a separately
    /// managed timer (e.g. an external `TimerWheel` clock domain)
    /// can still install one manually via
    /// [`crate::wake::timer::TimerWheel::install_delegate_timeout`]
    /// after `install_request` returns.
    pub fn install_request(
        &self,
        _request: DelegateRequest,
        endpoint_marker: u64,
        cancel_policy: AgentCancelPolicy,
        drop_policy: TokenDropPolicy,
        mailbox: Weak<TaskMailbox>,
        deadline: Option<(Deadline, &TimerWheel)>,
    ) -> AgentTokenGuard<'_> {
        let id = DelegateTokenId(self.next_id.fetch_add(1, Ordering::Relaxed));
        {
            let mut slots = self.slots.lock();
            slots.push((id, TokenSlot::new(endpoint_marker, mailbox)));
        }
        // Install the paired timer (if requested) AFTER the slot is
        // visible in the registry: a `fire_due_delegate_timeouts`
        // walk that lands between the wheel install and the guard's
        // return must find a `Pending` slot to CAS against. Order
        // here mirrors the drop order in reverse (drop-after-acquire,
        // acquire-before-publish).
        let timer = deadline.map(|(deadline, wheel)| wheel.install_delegate_timeout(deadline, id));
        AgentTokenGuard {
            registry: Some(self),
            id,
            drop_policy,
            cancel_policy,
            timer,
        }
    }

    /// Current logical state of `id`. Returns `None` if `id` is
    /// unknown to this registry.
    pub fn state(&self, id: DelegateTokenId) -> Option<DelegateState> {
        let slots = self.slots.lock();
        slots
            .iter()
            .find(|(slot_id, _)| *slot_id == id)
            .and_then(|(_, slot)| DelegateState::from_raw(slot.state.load(Ordering::Acquire)))
    }

    /// Extract the installed reply, if any. Returns `Some(reply)`
    /// only when the token state is `Replied`; returns `None` for
    /// any other state (including `ReplyInstalling`, where the
    /// reply slot is in single-writer transition).
    ///
    /// The reply is **consumed**: subsequent calls return `None`.
    /// This mirrors the spec's "driver takes the reply" step in
    /// the resume protocol (`05_DELEGATE_v1.md` §7 step 5).
    pub fn take_reply(&self, id: DelegateTokenId) -> Option<DelegateReply> {
        let mut slots = self.slots.lock();
        let entry = slots.iter_mut().find(|(slot_id, _)| *slot_id == id)?;
        let slot = &mut entry.1;
        if slot.state.load(Ordering::Acquire) != DelegateState::Replied.to_raw() {
            return None;
        }
        slot.reply.take()
    }

    /// `Pending → Replied` (via the single-writer `ReplyInstalling`
    /// phase). Per `05_DELEGATE_v1.md` §7:
    ///
    /// 1. CAS state `Pending → ReplyInstalling`. Failure → reply
    ///    rejected as `LateReply`.
    /// 2. Install the reply payload (single-writer phase).
    /// 3. Store state `Replied` (Release).
    /// 4. Post `MailboxEvent::AgentReplied { token_id: id }` to the
    ///    bound waiter's mailbox (`Weak::upgrade` failure → no-op).
    ///
    /// Returns [`TransitionOutcome::Applied`] on success or
    /// [`TransitionOutcome::LateNoOp`] if the token is already
    /// terminal (DTOK-1). The mailbox post happens **only** on
    /// `Applied` — late writers drop the event (DTOK-2 wake-routing
    /// race resolution).
    pub fn mark_replied(&self, id: DelegateTokenId, reply: DelegateReply) -> TransitionOutcome {
        let mailbox = {
            let mut slots = self.slots.lock();
            let entry = match slots.iter_mut().find(|(slot_id, _)| *slot_id == id) {
                Some(e) => e,
                None => return TransitionOutcome::UnknownToken,
            };
            let slot = &mut entry.1;
            // Phase 1: CAS Pending → ReplyInstalling.
            match slot.state.compare_exchange(
                DelegateState::Pending.to_raw(),
                DelegateState::ReplyInstalling.to_raw(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {}
                Err(observed) => {
                    let observed_state =
                        DelegateState::from_raw(observed).unwrap_or(DelegateState::Pending);
                    return TransitionOutcome::LateNoOp(observed_state);
                }
            }
            // Phase 2: install reply (single-writer phase).
            slot.reply = Some(reply);
            // Phase 3: ReplyInstalling → Replied (Release for DTOK-1).
            slot.state
                .store(DelegateState::Replied.to_raw(), Ordering::Release);
            slot.mailbox.clone()
        };
        // Phase 4: post the wake event with the slot lock dropped
        // (mailbox.post takes its own lock; ordering-discipline).
        if let Some(mb) = mailbox.upgrade() {
            let _ = mb.post(MailboxEvent::AgentReplied { token_id: id });
        }
        TransitionOutcome::Applied
    }

    /// `Pending → TimedOut`. Returns [`TransitionOutcome::Applied`]
    /// on first writer; [`TransitionOutcome::LateNoOp`] if any
    /// other terminal transition already won the race (DTOK-3).
    ///
    /// Called by the reactor-side timer-wheel fire path (future
    /// PR-7B). The substrate state machine does not own the timer
    /// wheel — see the module-level "Layering" docs.
    pub fn mark_timed_out(&self, id: DelegateTokenId) -> TransitionOutcome {
        self.cas_terminal(id, DelegateState::TimedOut)
    }

    /// `Pending → Canceled`. Called from the
    /// [`TokenDropPolicy::CancelOnDrop`] path of
    /// [`AgentTokenGuard::drop`] and from explicit cancel paths.
    /// Returns [`TransitionOutcome::Applied`] on first writer;
    /// [`TransitionOutcome::LateNoOp`] if any other terminal
    /// transition already won.
    pub fn mark_canceled(&self, id: DelegateTokenId) -> TransitionOutcome {
        self.cas_terminal(id, DelegateState::Canceled)
    }

    /// `Pending → AgentDied`. Called by the runtime when the
    /// endpoint owning the in-flight delegation dies (fd close,
    /// process / thread exit per `EndpointScope`).
    pub fn mark_agent_died(&self, id: DelegateTokenId) -> TransitionOutcome {
        self.cas_terminal(id, DelegateState::AgentDied)
    }

    /// Walk all in-flight tokens whose endpoint marker matches
    /// `marker` and transition each to `AgentDied`. Used by the
    /// endpoint-death routing for `EndpointScope::Process` /
    /// `EndpointScope::Thread` (`05_DELEGATE_v1.md` §3.1, DTOK-2).
    ///
    /// Returns the number of tokens that transitioned. Tokens that
    /// were already terminal are skipped without affecting the
    /// count.
    ///
    /// **Out-of-scope edge cases.** Per `07_BLAST_RADIUS.md` §6
    /// risk row "EndpointScope abandonment routing edge cases",
    /// full `EndpointScope` discrimination (per-thread vs.
    /// per-process scoping, fd-close vs. exit ordering) lands
    /// when the real endpoint cap-zone does. PR-7 ships the walk
    /// API only.
    pub fn mark_endpoint_died(&self, marker: u64) -> usize {
        let slots = self.slots.lock();
        let ids: Vec<DelegateTokenId> = slots
            .iter()
            .filter_map(|(id, slot)| {
                if slot.endpoint_marker == marker {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        drop(slots);
        let mut transitioned = 0;
        for id in ids {
            if matches!(self.mark_agent_died(id), TransitionOutcome::Applied) {
                transitioned += 1;
            }
        }
        transitioned
    }

    fn cas_terminal(&self, id: DelegateTokenId, target: DelegateState) -> TransitionOutcome {
        debug_assert!(target.is_terminal());
        let (outcome, mailbox) = {
            let slots = self.slots.lock();
            let entry = match slots.iter().find(|(slot_id, _)| *slot_id == id) {
                Some(e) => e,
                None => return TransitionOutcome::UnknownToken,
            };
            match entry.1.state.compare_exchange(
                DelegateState::Pending.to_raw(),
                target.to_raw(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => (TransitionOutcome::Applied, entry.1.mailbox.clone()),
                Err(observed) => {
                    let observed_state =
                        DelegateState::from_raw(observed).unwrap_or(DelegateState::Pending);
                    return TransitionOutcome::LateNoOp(observed_state);
                }
            }
        };
        // DTOK-2 wake-routing: post Abort hint only on the Applied
        // path — late writers drop the event so a single Applied
        // posts to the waiter. Mailbox.post takes its own lock;
        // run with the slots lock dropped.
        if matches!(outcome, TransitionOutcome::Applied) {
            if let Some(reason) = abort_reason_for(target) {
                if let Some(mb) = mailbox.upgrade() {
                    let _ = mb.post(MailboxEvent::Abort {
                        token_id: id,
                        reason,
                    });
                }
            }
        }
        outcome
    }
}

/// Map a terminal [`DelegateState`] to the matching [`AbortReason`]
/// posted to the bound waiter via [`MailboxEvent::Abort`] on a
/// PR-7B `Applied` transition. `Replied` returns `None` — it is
/// surfaced as [`MailboxEvent::AgentReplied`] instead, not as an
/// abort.
const fn abort_reason_for(state: DelegateState) -> Option<AbortReason> {
    match state {
        DelegateState::Canceled => Some(AbortReason::Canceled),
        DelegateState::AgentDied => Some(AbortReason::AgentDied),
        DelegateState::TimedOut => Some(AbortReason::TimedOut),
        DelegateState::Replied | DelegateState::Pending | DelegateState::ReplyInstalling => None,
    }
}

impl Default for DelegateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard returned by [`DelegateRegistry::install_request`].
///
/// Owns the script-side end of the delegation. Drop semantics are
/// governed by the captured [`TokenDropPolicy`]:
///
/// | `TokenDropPolicy` | `AgentCancelPolicy` | Drop behaviour |
/// |---|---|---|
/// | `CancelOnDrop` | `BestEffort` | `mark_canceled(id)`; agent's late reply (if any) is dropped on arrival |
/// | `CancelOnDrop` | `Synchronous` | `mark_canceled(id)`; *caller* is responsible for then waiting on the agent's cancel-ack (synchronous protocol is reactor-side) |
/// | `CancelOnDrop` | `Detached` | `mark_canceled(id)`; agent's eventual reply is dropped on arrival |
/// | `Abandon` | `BestEffort` | no transition; waiter is unbound but token remains `Pending` (or stays in whatever terminal state already won) — agent's reply will land on a dead waiter |
/// | `Abandon` | `Synchronous` | as `Abandon`; the kernel does NOT initiate the sync-cancel protocol |
/// | `Abandon` | `Detached` | as `Abandon` |
///
/// The behaviours collapse: `CancelOnDrop` always calls
/// `mark_canceled` regardless of `AgentCancelPolicy`; `Abandon` is
/// always a pure unbind. `AgentCancelPolicy` shapes only the
/// **agent-facing protocol** the caller drives **after** the
/// `mark_canceled` CAS (see DELEGATE-6 in `02_INVARIANTS_v5.md`).
///
/// To deliberately retain the registration past the guard's
/// lifetime (e.g. transfer of ownership to a state machine that
/// outlives the yield-site frame), call [`Self::forget`]. The
/// caller is then responsible for eventually driving the token to
/// a terminal state.
///
/// ## Pairing with a [`crate::wake::timer::TimerGuard`]
///
/// Per D6 §7, this guard **owns** an optional
/// [`crate::wake::timer::TimerGuard`] internally. Callers pass a
/// `deadline = Some((d, wheel))` to
/// [`DelegateRegistry::install_request`] to mint a `DelegateTimeout`
/// timer registration paired with the token; the resulting
/// [`TimerGuard`] is stored in the `timer` field and dropped
/// **before** the token-state CAS on [`Drop`]. Drop order:
///
/// 1. `Drop::drop` runs `self.timer.take()`, which drops the
///    [`crate::wake::timer::TimerGuard`] and retires the wheel
///    entry.
/// 2. The `TokenDropPolicy::CancelOnDrop` branch runs
///    [`DelegateRegistry::mark_canceled`], CAS-ing the slot.
///
/// A concurrent
/// [`crate::wake::timer::TimerWheel::fire_due_delegate_timeouts`]
/// walk is serialized through the registry CAS exactly as today:
/// either it CASes first (drop's `mark_canceled` returns
/// `LateNoOp(TimedOut)`) or our drop CASes first and the wheel
/// entry has already been retired by step 1 so the fire walk
/// observes no matching entry. DTOK-3 (reply-vs-timeout race
/// determinism) carries through unchanged because the registry CAS
/// remains the single linearization point; the timer-guard drop is
/// pure cleanup of the wheel entry.
///
/// Call sites that want a manually-managed timer (separate clock
/// domain, deferred install) can still pass `deadline = None` and
/// install a [`TimerGuard`] explicitly via
/// [`crate::wake::timer::TimerWheel::install_delegate_timeout`].
#[must_use = "drop the guard to release the delegation; binding to _ may cancel immediately"]
pub struct AgentTokenGuard<'a> {
    /// `None` after [`Self::forget`]; drop becomes a no-op.
    registry: Option<&'a DelegateRegistry>,
    id: DelegateTokenId,
    drop_policy: TokenDropPolicy,
    cancel_policy: AgentCancelPolicy,
    /// `Some` iff [`DelegateRegistry::install_request`] was called
    /// with a `deadline = Some((d, wheel))`. Dropped **before** the
    /// `mark_canceled` CAS in [`Drop::drop`] so a concurrent
    /// [`crate::wake::timer::TimerWheel::fire_due_delegate_timeouts`]
    /// walk is serialized through the registry CAS — see the
    /// type-level "Pairing with a `TimerGuard`" docs and D6 §7.
    timer: Option<TimerGuard>,
}

impl<'a> AgentTokenGuard<'a> {
    /// Runtime identity of the delegation. Stable across the
    /// guard's lifetime; useful for the timer-wheel
    /// `DelegateTimeout` registration (`05_DELEGATE_v1.md` §7
    /// step 4, integration contract in this module's docs).
    pub fn id(&self) -> DelegateTokenId {
        self.id
    }

    /// The drop policy captured at install time.
    pub fn drop_policy(&self) -> TokenDropPolicy {
        self.drop_policy
    }

    /// The agent-cancel policy captured at install time.
    pub fn cancel_policy(&self) -> AgentCancelPolicy {
        self.cancel_policy
    }

    /// Current logical state of the underlying token. Helper that
    /// reads back through the owning registry. `None` only if the
    /// guard was previously [`Self::forget`]ten and then the
    /// registry dropped the slot (which today never happens — the
    /// registry retains slots for the registry's lifetime).
    pub fn state(&self) -> Option<DelegateState> {
        self.registry.and_then(|r| r.state(self.id))
    }

    /// Suppress the drop-time policy action and return the raw
    /// [`DelegateTokenId`]. The caller takes responsibility for
    /// eventually driving the token to a terminal state via one of
    /// the `mark_*` methods.
    ///
    /// Mirrors [`crate::wake::timer::TimerGuard::forget`]: standard
    /// pattern when ownership of a registration is transferred to a
    /// longer-lived state machine.
    pub fn forget(mut self) -> DelegateTokenId {
        self.registry = None;
        self.id
    }
}

impl<'a> Drop for AgentTokenGuard<'a> {
    fn drop(&mut self) {
        // Step 1: retire the paired wheel entry FIRST (if any).
        // Per D6 §7, dropping the TimerGuard cancels the wheel
        // registration so any concurrent
        // `fire_due_delegate_timeouts` walk either (a) already won
        // the CAS — in which case the wheel entry was already
        // retired by the fire path and step 2's `mark_canceled`
        // returns `LateNoOp(TimedOut)` — or (b) lost the race to
        // find the entry post-retire, leaving the registry CAS in
        // step 2 as the unambiguous last writer. The registry CAS
        // remains the linearization point (DTOK-3 race determinism
        // unchanged). Drop the timer **inside** the `drop(...)`
        // call so it runs synchronously here, not at end-of-scope
        // (binding to `let _g = ...` would extend its lifetime past
        // the CAS).
        drop(self.timer.take());
        // Step 2: token-state CAS per drop_policy.
        if let Some(registry) = self.registry.take() {
            match self.drop_policy {
                TokenDropPolicy::CancelOnDrop => {
                    // Idempotent: if a terminal transition already
                    // won (reply / timeout / agent-died), the CAS
                    // returns LateNoOp and we don't override it.
                    let _ = registry.mark_canceled(self.id);
                }
                TokenDropPolicy::Abandon => {
                    // Pure unbind: leave token state alone. The
                    // agent's eventual reply lands on a dead
                    // waiter; the runtime is responsible for
                    // dropping it (reply-routing layer, PR-7B).
                }
            }
        }
    }
}
