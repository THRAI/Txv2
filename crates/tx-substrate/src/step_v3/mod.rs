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
    OnTimer {
        token: TimerId,
        deadline: Deadline,
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
    const EMPTY: Self;
    fn is_empty(&self) -> bool;
    fn extend(&mut self, other: Self);
}

/// One-shot ops: open, mkdir, fork, dup, close, mmap-reservation, …
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoProgress;

impl StepProgress for NoProgress {
    const EMPTY: Self = NoProgress;
    fn is_empty(&self) -> bool {
        true
    }
    fn extend(&mut self, _other: Self) {}
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
    const EMPTY: Self = ByteProgress { bytes: 0 };
    fn is_empty(&self) -> bool {
        self.bytes == 0
    }
    fn extend(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
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
            (DriveMode::Waiting, YieldShape::OnAgent { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Waiting, YieldShape::OnTimer { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Selecting, YieldShape::OnWaitSource { .. }) => AcceptOutcome::Resolve,
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
