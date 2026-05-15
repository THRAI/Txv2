//! v3 step algebra stub.
//!
//! Closed-catalog skeleton for `docs/Txv3/03_STEP_MODEL_v2.md`. PR-0
//! introduces the type shapes and the minimum set of `StepProgress`
//! impls required to pin the monoid laws and the `DriveMode::classify`
//! matrix in `crates/tx-substrate/tests/v3_algebra.rs`. The `StepOp`
//! trait (per §2.1) is now defined alongside a placeholder `ScriptCtx`
//! struct so consumers can implement step-able operations; the
//! `ScriptCtx` is intentionally empty for PR-0 and gets fleshed out
//! (SubjectContext, guard, …) in later PRs. Later PRs also flesh out
//! the remaining `StepProgress` impls (PageProgress, EntryProgress,
//! IoVecProgress), the `OnAgent` `YieldShape` variant (PR-4), and
//! migrate consumers off `tx_subsystems::execution::StepOutcome` per
//! `docs/progress/plans/2026-05-09-v3-tdd-migration.md`.
//!
//! Doc tags pinned by the integration tests:
//! - `txdoc:TXV3-STEP-MODEL-V2`
//! - `txdoc:STEP-V2-OUTCOME-ALGEBRA-1` (closed four-variant outcome)
//! - `txdoc:STEP-V2-PROGRESS-TYPED-1` (StepProgress is a monoid)
//! - `txdoc:STEP-V2-YIELD-SHAPE-1` (YieldShape is a closed catalog)
//! - `txdoc:STEP-V2-STEP-OP-1` (StepOp trait shape)
//! - `txdoc:STEP-V2-DRIVER-MODE-1` (DriveMode classify matrix)

use crate::zone::Cap;

/// Errno surface. Mirrors `tx_subsystems::execution::Errno` byte-for-byte
/// (variant names, ordering, doc comments).
///
/// Discipline: this enum stays in lock-step with `execution::Errno`. The
/// `From<execution::Errno> for step_v3::Errno` impl in
/// `tx_subsystems::execution` is an exhaustive no-wildcard match, so
/// adding a new variant on one side fails to compile until the same
/// variant is added here. Removing a variant on either side is
/// similarly load-bearing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Errno {
    EACCES,
    /// Resource temporarily unavailable. Surfaced by `O_NONBLOCK` I/O
    /// paths (e.g. fd-ops Wave 3 `pipe::step_read` / `step_write` with
    /// `nonblocking = true` and no progress yet).
    EAGAIN,
    /// Bad file descriptor. Today only surfaced by fd-ops Wave 3
    /// pipe dispatch when a wrong-side `step_read` / `step_write`
    /// reaches the dispatcher despite the OpenFileFlags read/write
    /// guard (defence in depth — the flag check at the top of
    /// `OpenFile::step_read/step_write` returns `EINVAL` first for
    /// the common case). Linux semantic: `read(2)` on a writer-end
    /// fd is `-EBADF`, not `-EPIPE`.
    EBADF,
    EBUSY,
    EDQUOT,
    EEXIST,
    EFAULT,
    EINVAL,
    EIO,
    EISDIR,
    ELOOP,
    ENAMETOOLONG,
    ENODEV,
    ENOEXEC,
    ENOMEM,
    ENOENT,
    ENOSYS,
    ENOTDIR,
    ENOTEMPTY,
    /// Inappropriate ioctl for device. Surfaced by Slice 5 of the
    /// shell-prompt roadmap (`ioctl(2)` arm) when the target fd is not
    /// a TTY (terminal-shape ioctl on a pipe / regular file / dir / etc.)
    /// or the request code is not one of the eight TTY ioctls v1
    /// implements. Linux value: 25.
    ENOTTY,
    EPERM,
    /// Broken pipe: write to a pipe with all readers closed. The
    /// caller is responsible for delivering SIGPIPE before returning
    /// `-EPIPE` to userspace (fd-ops Wave 3, Q2 DECIDED 2026-05-07).
    EPIPE,
    /// Numerical result out of range. Surfaced by Slice 6's
    /// `getcwd(2)` arm when the user buffer is smaller than the
    /// rendered path (NUL terminator inclusive). Linux value: 34.
    ERANGE,
    EROFS,
    /// Illegal seek. Surfaced by `lseek(2)` when called against a
    /// non-seekable file (pipe / TTY / chardev / socket). fd-ops
    /// Wave 4. Linux value: 29.
    ESPIPE,
    ESRCH,
    ESTALE,
}

/// Opaque wait-source handle. Replaces `tx_subsystems::execution::WaitToken`'s
/// carrier slot; the underlying integer is the bus-primitive source id.
/// Renamed from `WakeCarrier` per docs/Txv3/07_BLAST_RADIUS.md §3.1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitSourceId(u64);

impl WaitSourceId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    /// Sentinel zero value — used by `YieldResolved::PLACEHOLDER` and
    /// by non-`OnWaitSource` yield shapes that carry no source id.
    pub const ZERO: Self = Self(0);
}

/// Bitmask of interest conditions on a wait source. Replaces
/// `tx_subsystems::execution::WaitToken`'s interest slot. Renamed from
/// `InterestConditions` per docs/Txv3/07_BLAST_RADIUS.md §3.1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterestMask(u64);

impl InterestMask {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Closed catalog of yield shapes. Extension is ARCH-3.
///
/// PR-0 pinned `OnWaitSource` (formerly `OnCarrier`); the OnAgent variant
/// lands against placeholder delegate types (see `agent.rs`). PR-4 of the
/// v3 TDD migration plan replaces those placeholders with real cap-typed
/// zone primitives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum YieldShape {
    OnWaitSource {
        source: WaitSourceId,
        interests: InterestMask,
    },
    OnAgent {
        endpoint: DelegateEndpoint,
        request: DelegateRequest,
        token: DelegateToken,
        deadline: Deadline,
        cancel: AgentCancelPolicy,
    },
    /// Primary timer wait: a deadline fires and the driver resumes
    /// the op via [`ResumeOutcome::TimerExpired`]. Distinct from a
    /// secondary `Deadline` carried by `OnAgent` (which times out
    /// the agent's reply); `OnTimer` is the timer itself being the
    /// wait subject.
    ///
    /// Per `docs/Txv3/03_STEP_MODEL_v2.md` and PR-8 of the v3
    /// migration. The [`crate::wake::timer::TimerToken`]
    /// corresponding to this `token` is held by a
    /// [`crate::wake::timer::TimerGuard`] (future PR-8 follow-up)
    /// that the driver retires on resume.
    OnTimer { token: TimerId, deadline: Deadline },
    /// Edge-triggered epoll subscription. Resolved identically to
    /// [`OnWaitSource`] at the protocol layer (park on source,
    /// return [`ResumeOutcome::Retry`]); the edge-vs-level
    /// distinction is handled by the epoll subsystem's readiness
    /// tracking, not by the yield-resolve path.
    OnEdge {
        source: WaitSourceId,
        interests: InterestMask,
    },
}

impl YieldShape {
    /// Shorthand for `OnWaitSource { source: WaitSourceId::new(source_id),
    /// interests: InterestMask::new(interest_mask) }`. Wraps the
    /// raw `u64` source id and `u64` interest mask; zero-translation
    /// conversion so call sites don't have to hand-roll the struct
    /// literal.
    pub const fn on_wait_source(source_id: u64, interest_mask: u64) -> Self {
        Self::OnWaitSource {
            source: WaitSourceId::new(source_id),
            interests: InterestMask::new(interest_mask),
        }
    }
}

/// Four-variant step outcome per STEP-1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepOutcome<T, P> {
    Continue { progress: P },
    Yield { progress: P, shape: YieldShape },
    Done(T),
    Err(Errno),
}

impl<T, P: StepProgress> StepOutcome<T, P> {
    /// `StepOutcome::done(t)` — terminal value. Wave-5 helper.
    pub const fn done(value: T) -> Self {
        Self::Done(value)
    }

    /// `StepOutcome::err(errno)` — terminal error. Wave-5 helper.
    pub const fn err(errno: Errno) -> Self {
        Self::Err(errno)
    }

    /// `StepOutcome::continue_with(progress)` — made progress, may
    /// continue without waiting. Named `continue_with` (not `continue`)
    /// because `continue` is a Rust keyword.
    pub const fn continue_with(progress: P) -> Self {
        Self::Continue { progress }
    }

    /// `StepOutcome::yield_on_wait_source(progress, source_id, interest_mask)`
    /// — shorthand for `Yield { progress, shape: YieldShape::on_wait_source(...) }`.
    /// Constructs a `Yield` over an `OnWaitSource` shape from the
    /// underlying `u64` source id and `u64` interest mask;
    /// zero-translation conversion. No `yield_on_agent` shorthand: the
    /// `OnAgent` variant has five fields, so a single helper isn't
    /// useful. Builders or shape-specific helpers can come later when
    /// there's a real `OnAgent` client.
    pub const fn yield_on_wait_source(progress: P, source_id: u64, interest_mask: u64) -> Self {
        Self::Yield {
            progress,
            shape: YieldShape::on_wait_source(source_id, interest_mask),
        }
    }
}

/// Per STEP-3: `(Self, EMPTY, extend)` is monoid-shaped — associative,
/// with `EMPTY` as identity. The integration tests pin both laws.
pub trait StepProgress: Sized {
    /// The concrete value this progress can be converted into via
    /// [`into_output`](Self::into_output). For progress types that
    /// track meaningful partial-result quantities (e.g.,
    /// [`ByteProgress`] tracks bytes read), this is the same as
    /// `StepOp::Output`; for marker progress types ([`NoProgress`]),
    /// this is `()` and `into_output` always returns `None`.
    type Output;

    const EMPTY: Self;
    fn is_empty(&self) -> bool;
    fn extend(&mut self, other: Self);

    /// Convert accumulated progress into a concrete output value.
    ///
    /// Returns `Some(val)` for progress types that carry partial-result
    /// semantics (e.g. `ByteProgress → usize`). Returns `None` for
    /// marker progress (`NoProgress`, `EntryProgress`) that track
    /// intermediate state but don't represent a partial `StepOp::Output`.
    ///
    /// Used by [`drive()`] when `DriveMode::Nonblocking` produces a
    /// `PartialReturn` translation — the driver has accumulated
    /// progress across one or more `Continue` steps and surfaces
    /// that partial work instead of returning `EAGAIN`.
    fn into_output(self) -> Option<Self::Output>;
}

/// One-shot ops: open, mkdir, fork, dup, close, mmap-reservation, …
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoProgress;

impl StepProgress for NoProgress {
    type Output = ();
    const EMPTY: Self = NoProgress;
    fn is_empty(&self) -> bool {
        true
    }
    fn extend(&mut self, _other: Self) {}
    fn into_output(self) -> Option<()> {
        None
    }
}

/// Byte-moving ops: read, write, splice, sendfile, copy_file_range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteProgress {
    bytes: usize,
}

impl ByteProgress {
    pub const fn new(bytes: usize) -> Self {
        Self { bytes }
    }
    pub const fn bytes(self) -> usize {
        self.bytes
    }
    /// Inherent shorthand for `<ByteProgress as StepProgress>::EMPTY`.
    /// Avoids requiring `use StepProgress;` at byte-moving call sites
    /// (e.g. `step_v3::StepOutcome::yield_on_wait_source(ByteProgress::EMPTY,
    /// source_id, interest_mask)`).
    pub const EMPTY: Self = Self { bytes: 0 };
}

impl StepProgress for ByteProgress {
    type Output = usize;
    const EMPTY: Self = ByteProgress { bytes: 0 };
    fn is_empty(&self) -> bool {
        self.bytes == 0
    }
    fn extend(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
    fn into_output(self) -> Option<usize> {
        Some(self.bytes)
    }
}

// Closed-catalog StepProgress impls (txdoc:STEP-V2-PROGRESS-TYPED-1).
// Each lives in its own file so the impl-side conventions for one
// progress shape (e.g. the iovec cursor-reset rule) stay readable in
// isolation.
pub mod entry_progress;
pub mod iovec_progress;
pub mod page_progress;
pub use entry_progress::{DirCursor, EntryProgress};
pub mod subject_context;
pub use iovec_progress::IoVecProgress;
pub use subject_context::{
    Credential, CredentialView, ProcessIdentity, RestrictionStackHandle, RestrictionStackView,
    SubjectAuthority, SubjectContext, SubjectIdentity, ThreadIdentity,
};
pub mod restriction_stack;
pub use page_progress::PageProgress;
pub use restriction_stack::{RestrictionKind, RestrictionStack};
pub mod execution_scope;
pub use execution_scope::ExecutionScope;
pub mod on_behalf_of;
pub use on_behalf_of::{
    with_on_behalf_of, AbortSignal, CancelReason, OnBehalfOfAbort, OnBehalfOfBorrow,
};
pub mod agent;
pub use agent::{
    AbortReason, AgentCancelPolicy, AgentTokenGuard, Deadline, DelegateEndpoint, DelegateRegistry,
    DelegateReply, DelegateRequest, DelegateState, DelegateToken, DelegateTokenId, TimerId,
    TokenDropPolicy, TransitionOutcome, UfdAccessKind, UfdReply, UfdRequest,
};
pub mod endpoint_kind;
pub use endpoint_kind::EndpointKind;
pub mod wait_protocol;
pub use wait_protocol::{WaitOutcome, WaitProtocol};
pub mod binding_obligations;
pub use binding_obligations::BindingObligation;

// ── YieldOutcome / YieldResolved — OBS-3b wake-context return types ──────────
//
// These types extend the `yield_resolve` closure signature from
// `Option<Errno>` to `YieldOutcome`, enabling the `drive` convergence
// point to emit L3 YieldBegin + Resume records with full metadata.
//
// See `docs/progress/decisions/2026-05-13-d17-obs3b-resume-emission.md` §5.

use crate::wake::mailbox::WaitGeneration;

/// Discriminant encoding why a yield resolved.
///
/// Wire encoding (same values as `PayloadResume::resume_kind`):
/// 0=Retry, 1=WithReply, 2=TimerExpired, 3=Aborted.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ResumeKind {
    /// Source fired; retry the step. Used by `OnWaitSource`.
    Retry = 0,
    /// Agent replied with payload. Used by `OnAgent`.
    WithReply = 1,
    /// Primary timer arm fired. Used by `OnTimer`.
    TimerExpired = 2,
    /// Wait aborted (signal, cancellation, kill, …).
    Aborted = 3,
}

/// Discriminant encoding why an `Aborted` wake occurred.
///
/// Wire encoding (same values as `PayloadResume::abort_reason`):
/// 0=None, 1=Signal, 2=Cancelled, 3=Killed.
/// This enum is separate from [`AbortReason`] (which is the delegate-side
/// runtime catalog) — it collapses all abort varieties into a compact wire
/// discriminant suitable for `PayloadResume`.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum WireAbortReason {
    /// Not aborted (valid only when `resume_kind != Aborted`).
    None = 0,
    /// Wait interrupted by a signal (EINTR path).
    Signal = 1,
    /// Wait cancelled (delegate token drop, scope teardown).
    Cancelled = 2,
    /// Wait killed (SIGKILL, irrecoverable).
    Killed = 3,
}

/// Wake-context metadata returned by a `yield_resolve` closure.
///
/// Carries the generation, source, and classification fields that
/// the L3 Resume convergence point in `drive` needs to emit a
/// `PayloadResume` record. Fields match the `PayloadResume` wire layout
/// (see `08_OBSERVATION_SERIALIZATION_v0.md §8.4`).
///
/// **Production call sites** populate all fields from the active wait
/// state captured in the closure (`ActiveWait::generation`, `source`).
/// **Stub / test / kernel-actor call sites** use `PLACEHOLDER` to
/// avoid scope-bloating boilerplate — the daemon emits a degenerate
/// record but will not fail.
#[derive(Copy, Clone, Debug)]
pub struct YieldResolved {
    /// Generation of the wait that fired (from `ActiveWait::generation`).
    pub wait_generation: WaitGeneration,
    /// Source that delivered the wake; `WaitSourceId::ZERO` for non-`OnWaitSource` shapes.
    pub source_id: WaitSourceId,
    /// Why the wait ended.
    pub resume_kind: ResumeKind,
    /// Abort discriminant; valid iff `resume_kind == ResumeKind::Aborted`.
    pub abort_reason: WireAbortReason,
}

impl YieldResolved {
    /// Placeholder for closures that do not have wake-context
    /// (test stubs, kernel actors that never actually park, etc.).
    ///
    /// The daemon emits a degenerate flow record but will not panic.
    /// Every production path where a task genuinely parks and wakes
    /// should populate the real values.
    ///
    /// Call sites using this should be annotated with:
    /// `// TODO(α-followup): real metadata once <X> is in scope`
    pub const PLACEHOLDER: Self = Self {
        wait_generation: WaitGeneration::ZERO,
        source_id: WaitSourceId::ZERO,
        resume_kind: ResumeKind::Retry,
        abort_reason: WireAbortReason::None,
    };
}

/// Return type of a `yield_resolve` closure.
///
/// Replaces the previous `Option<Errno>` return:
/// - `None` (resolved, continue loop) → `Resolved(YieldResolved { .. })`
/// - `Some(errno)` (abort) → `Aborted { resolved: YieldResolved { .. }, errno }`
///
/// The `resolved` field in the `Aborted` arm carries whatever
/// wake-context the closure observed at the time of the abort; the
/// L3 Resume emit in `drive` uses it before breaking out.
pub enum YieldOutcome {
    /// Wait resolved; continue the step loop.
    Resolved(YieldResolved),
    /// Wait aborted; break the step loop with `errno`.
    Aborted {
        resolved: YieldResolved,
        errno: Errno,
    },
}

/// Closed catalog of driver dispatch modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriveMode {
    Nonblocking,
    Waiting,
    Selecting,
}

/// Result of `DriveMode::classify`: either resolve the yield (block
/// or register) or translate the yield into a syscall-side answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptOutcome {
    Resolve,
    Translate(Translation),
}

/// Translation kinds emitted by classify when the mode rejects the
/// yield shape (or the progress accumulator).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Translation {
    /// Caller asked for nonblocking and no progress has been made.
    Eagain,
    /// Caller asked for nonblocking but progress was made; return the
    /// partial result.
    PartialReturn,
    /// Mode does not support this yield shape (e.g. `Selecting` over
    /// `OnAgent`).
    UnsupportedShape,
}

impl DriveMode {
    /// Per `docs/Txv3/03_STEP_MODEL_v2.md` §5.1.
    pub const fn classify(&self, shape: &YieldShape, progress_empty: bool) -> AcceptOutcome {
        match (self, shape) {
            (DriveMode::Nonblocking, _) if progress_empty => {
                AcceptOutcome::Translate(Translation::Eagain)
            }
            (DriveMode::Nonblocking, _) => AcceptOutcome::Translate(Translation::PartialReturn),
            (DriveMode::Waiting, YieldShape::OnWaitSource { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Waiting, YieldShape::OnEdge { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Waiting, YieldShape::OnAgent { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Waiting, YieldShape::OnTimer { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Selecting, YieldShape::OnWaitSource { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Selecting, YieldShape::OnEdge { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Selecting, YieldShape::OnAgent { .. }) => {
                AcceptOutcome::Translate(Translation::UnsupportedShape)
            }
            // `OnTimer` under `Selecting` is rejected: select(2) expresses
            // its own timeout via the `timeout` argument; a step-level
            // primary timer wait does not compose with select-style
            // multiplexing in a single dispatch surface.
            (DriveMode::Selecting, YieldShape::OnTimer { .. }) => {
                AcceptOutcome::Translate(Translation::UnsupportedShape)
            }
        }
    }
}

/// Per-script execution context handed to each `StepOp::step` call.
///
/// Script-scoped state bag threaded through every `StepOp::step`
/// invocation.
///
/// Generic over `I: SubjectIdentity` per
/// [D1](../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md).
/// Default `I = ProcessIdentity` (placeholder) keeps existing PR-2
/// wraps compiling unchanged. Production uses
/// `KernelScriptCtx = ScriptCtx<tx_subsystems::process::ProcessIdentity>`.
///
/// Fields are `Option<T>` so `ScriptCtx::new()` is zero-arg for any
/// `I`. Production callers populate via builder methods (`with_subject`,
/// `with_deadline`). PR-2 wraps that don't read these fields work
/// unchanged whether they're populated or not.
///
/// **The `epoch::Guard` is NOT a field here** — guards are step-local
/// (acquired inside `StepOp::step` body, never carried across
/// `.await`/yield). See D1 §"guard is step-local, not ScriptCtx-held".
///
/// PR-9 phase 3 populates `subject` / `deadline` from the syscall
/// trampoline; subsequent waves populate `mailbox` and `trace`.
pub struct ScriptCtx<I: SubjectIdentity = ProcessIdentity> {
    subject: Option<SubjectContext<I>>,
    deadline: Option<Deadline>,
}

impl<I: SubjectIdentity> ScriptCtx<I> {
    /// Construct an empty `ScriptCtx<I>`. All fields default to `None`;
    /// production callers populate via the builder methods.
    pub const fn new() -> Self {
        Self {
            subject: None,
            deadline: None,
        }
    }

    /// Populate the subject context (PR-9 phase 3).
    pub fn with_subject(mut self, subject: SubjectContext<I>) -> Self {
        self.subject = Some(subject);
        self
    }

    /// Populate the script-level deadline (PR-8B / later).
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Subject context if populated; `None` for placeholder/test
    /// contexts.
    pub fn subject(&self) -> Option<&SubjectContext<I>> {
        self.subject.as_ref()
    }

    /// Script-level deadline if populated.
    pub fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// Low 32 bits of the subject's task trace identity.
    ///
    /// Delegates to `SubjectIdentity::task_id_low` on the subject's
    /// process identity if a `SubjectContext` is populated; returns
    /// `0` for empty (`None`) contexts (test / kernel-internal actors
    /// with no subject wired in).
    ///
    /// Used by `drive` to populate `PayloadDriveBegin::task_id_low`
    /// (OBS-4 / γ-fix) so the daemon's `compute_flow_id` hashes the
    /// right identity.
    #[inline]
    pub fn task_id_low(&self) -> u32 {
        self.subject
            .as_ref()
            .map(|s| s.process().task_id_low())
            .unwrap_or(0)
    }
}

impl<I: SubjectIdentity> Default for ScriptCtx<I> {
    fn default() -> Self {
        Self::new()
    }
}

/// Closed catalog of resume outcomes (per `docs/Txv3/03_STEP_MODEL_v2.md`).
///
/// The driver consumes a `StepOutcome::Yield`, waits on the named
/// `YieldShape`, and produces a `ResumeOutcome` it hands back to the
/// op via [`StepOp::apply_resume`]. The op stashes any payload in
/// `&mut self` and the driver then re-invokes [`StepOp::step`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeOutcome {
    /// Rerun step. Used by `OnWaitSource` — the source fired with a
    /// matching interest; the step re-checks its semantic predicate.
    Retry,
    /// Delegate reply available. Used by `OnAgent` — the agent has
    /// written a reply payload that the op should consume.
    WithReply(DelegateReply),
    /// Primary timer wait expired. Used by `OnTimer`.
    TimerExpired(TimerId),
    /// Wait aborted (signal, scope teardown, kill, timeout, …).
    Aborted(AbortReason),
}

/// Step-able operation.
///
/// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1: every script driver pulls
/// on a `StepOp`, receiving a four-variant `StepOutcome` parameterized
/// by the op's `Output` and a monoid-shaped `Progress` accumulator.
///
/// Generic over `I: SubjectIdentity` per
/// [D1](../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md).
/// Default `I = ProcessIdentity` (the step_v3 placeholder) keeps the
/// 80 PR-2 wraps compiling unchanged — their `impl StepOp for FooOp`
/// resolves to `impl StepOp<ProcessIdentity> for FooOp` via default.
///
/// Production wraps that need to consume a real
/// `ScriptCtx<tx_subsystems::process::ProcessIdentity>` impl
/// `StepOp<tx_subsystems::process::ProcessIdentity>` explicitly. Wraps
/// that don't access identity-specific fields can be polymorphic
/// (`impl<I: SubjectIdentity> StepOp<I> for FooOp`).
pub trait StepOp<I: SubjectIdentity = ProcessIdentity> {
    /// Final value produced when the op completes (via
    /// `StepOutcome::Done`).
    type Output;
    /// Per-step progress accumulator. Must be a monoid (`StepProgress`)
    /// so partial-progress translation in `DriveMode::classify`
    /// composes across step boundaries.
    type Progress: StepProgress;

    /// Drive the op one step. Returns one of the four `StepOutcome`
    /// variants per STEP-1.
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress>;

    /// Apply a resume payload between two `step()` invocations.
    ///
    /// Called by the driver after a wait (named in the previous
    /// `step()`'s `Yield`) completes. The op should stash any
    /// payload in `&mut self`; the driver then re-invokes `step()`.
    ///
    /// The default impl accepts only [`ResumeOutcome::Retry`] and
    /// rejects other variants with [`Errno::EINVAL`]. **This is
    /// deliberate**: any StepOp that yields `OnAgent` (and therefore
    /// can receive `WithReply`) **must** explicitly opt in to
    /// handling reply payloads by overriding this method. Silent
    /// acceptance of unhandled resumes is a bug class the framework
    /// forecloses. Same for `OnTimer` ⇒ `TimerExpired`.
    ///
    /// `Aborted(_)` is also rejected by the default impl — abort
    /// handling is op-specific (cleanup of partial state, etc.) and
    /// must be opted into.
    fn apply_resume(&mut self, resume: ResumeOutcome) -> Result<(), Errno> {
        match resume {
            ResumeOutcome::Retry => Ok(()),
            _ => Err(Errno::EINVAL),
        }
    }
}

// ---------------------------------------------------------------------------
// OneShotStepOp — a StepOp that terminates on first invocation
// ---------------------------------------------------------------------------

/// A `StepOp` whose first `step()` returns `Done` or `Err`, never
/// `Continue` or `Yield`.
///
/// Contract (STEP-11, `docs/Txv3/02_INVARIANTS_v5.md`):
/// - `step()` must return `Done(T)` or `Err(Errno)` on first call.
/// - Returning `Continue` or `Yield` is an invariant violation.
/// - `Progress` must be `NoProgress` (one-shot ops don't accumulate).
///
/// This is stronger than `Nonblocking` driver mode.
pub trait OneShotStepOp<I: SubjectIdentity = ProcessIdentity>:
    StepOp<I, Progress = NoProgress>
{
}

/// Synchronous drive for one-shot ops. Does not allocate an
/// `ActiveWait`, does not enter the reactor, does not register
/// on a `WaitSource`.
pub fn drive_oneshot<I: SubjectIdentity, O: OneShotStepOp<I>>(
    op: &mut O,
    ctx: &mut ScriptCtx<I>,
) -> Result<O::Output, Errno> {
    match op.step(ctx) {
        StepOutcome::Done(v) => Ok(v),
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Continue { .. } => {
            panic!("OneShotStepOp violated contract: unexpected Continue")
        }
        StepOutcome::Yield { .. } => {
            panic!("OneShotStepOp violated contract: unexpected Yield")
        }
    }
}

// ---------------------------------------------------------------------------
// ImmediateCtx — narrow context for pure ABI queries (ImmediateSyscall lane)
// ---------------------------------------------------------------------------

/// Narrower than `ScriptCtx`: carries only the subject/process/thread
/// handles needed for pure ABI queries. No VFS, VM, reactor, timer,
/// or mailbox access.
pub struct ImmediateCtx<'a, I: SubjectIdentity = ProcessIdentity> {
    pub process: &'a Cap<I>,
    pub thread: &'a Cap<I::ThreadIdentity>,
}
