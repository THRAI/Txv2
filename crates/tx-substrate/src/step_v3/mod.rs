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

/// v3 errno surface. Mirrors `tx_subsystems::execution::Errno` byte-for-byte
/// (variant names, ordering, doc comments). PR-0 originally pinned only
/// `EAGAIN`; wave-4's first cascade probe (the futex step fns under
/// `tx_subsystems::futex`) added `EINVAL` for uaddr/nargs validation;
/// wave-5 grows the catalog to mirror the full v4 27-variant set in one
/// step so wave-6 fan-out workers (mount, pipe, device, …) do not each
/// add their own variants ad hoc.
///
/// Discipline: this enum stays in lock-step with v4. The
/// `From<execution::Errno> for step_v3::Errno` impl in
/// `tx_subsystems::execution` is an exhaustive no-wildcard match, so
/// adding a new variant on the v4 side fails to compile until the same
/// variant is added here. Removing a variant on either side is
/// similarly load-bearing.
///
/// TBD per the original PR-0 stub note: whether v3's `Errno` ultimately
/// replaces v4's `tx_subsystems::execution::Errno` (relocate upward) or
/// whether both stay forever as a substrate/subsystem split. Wave-5
/// leaves both in place and bridges them with a `From` impl.
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

/// Opaque carrier handle. Replaces `tx_subsystems::execution::WaitToken`'s
/// carrier slot; the underlying integer is the bus-primitive carrier id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WakeCarrier(u64);

impl WakeCarrier {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Bitmask of interest conditions on a carrier. Replaces
/// `tx_subsystems::execution::WaitToken`'s interest slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterestConditions(u64);

impl InterestConditions {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Closed catalog of yield shapes. Extension is ARCH-3.
///
/// PR-0 pinned `OnCarrier`; this PR adds `OnAgent` against placeholder
/// delegate types (see `agent.rs`). PR-4 of the v3 TDD migration plan
/// replaces those placeholders with real cap-typed zone primitives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum YieldShape {
    OnCarrier {
        carrier: WakeCarrier,
        interests: InterestConditions,
    },
    OnAgent {
        endpoint: DelegateEndpoint,
        request: DelegateRequest,
        token: DelegateToken,
        deadline: Deadline,
        cancel: CancelPolicy,
    },
}

impl YieldShape {
    /// Shorthand for `OnCarrier { carrier: WakeCarrier::new(carrier_id),
    /// interests: InterestConditions::new(interest_mask) }`. Wraps the
    /// raw `u64` carrier id and `u64` interest mask the v4 `WaitToken`
    /// exposes; zero-translation conversion. Wave-5 helper added so
    /// fan-out-wave cascade-probe call sites don't have to hand-roll
    /// the struct literal.
    pub const fn on_carrier(carrier_id: u64, interest_mask: u64) -> Self {
        Self::OnCarrier {
            carrier: WakeCarrier::new(carrier_id),
            interests: InterestConditions::new(interest_mask),
        }
    }
}

/// Four-variant step outcome. The v4 five-variant algebra is retired
/// per STEP-1.
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

    /// `StepOutcome::yield_on_carrier(progress, carrier_id, interest_mask)`
    /// — shorthand for `Yield { progress, shape: YieldShape::on_carrier(...) }`.
    /// Constructs a v3 `Yield` over an `OnCarrier` shape from the
    /// underlying `u64` carrier id and `u64` interest mask, the same
    /// pair v4 `WaitToken { carrier, interest }` exposes; zero-translation
    /// conversion. No `yield_on_agent` shorthand this wave: the
    /// `OnAgent` variant has five fields, so a single helper isn't
    /// useful. Builders or shape-specific helpers can come later when
    /// there's a real `OnAgent` client.
    pub const fn yield_on_carrier(
        progress: P,
        carrier_id: u64,
        interest_mask: u64,
    ) -> Self {
        Self::Yield {
            progress,
            shape: YieldShape::on_carrier(carrier_id, interest_mask),
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
pub use subject_context::{
    Credential, ProcessIdentity, RestrictionStackHandle, SubjectAuthority, SubjectContext,
    ThreadIdentity,
};
pub use iovec_progress::IoVecProgress;
pub mod restriction_stack;
pub use restriction_stack::{RestrictionKind, RestrictionStack};
pub use page_progress::PageProgress;
pub mod execution_scope;
pub use execution_scope::{ExecutionScope, OwnedProcessHandle};
pub mod agent;
pub use agent::{CancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken};
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
            (DriveMode::Waiting, YieldShape::OnCarrier { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Waiting, YieldShape::OnAgent { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Selecting, YieldShape::OnCarrier { .. }) => AcceptOutcome::Resolve,
            (DriveMode::Selecting, YieldShape::OnAgent { .. }) => {
                AcceptOutcome::Translate(Translation::UnsupportedShape)
            }
        }
    }
}

/// Per-script execution context handed to each `StepOp::step` call.
///
/// PR-0 placeholder: intentionally empty. Later PRs of the v3 TDD
/// migration thread the SubjectContext, guard, deadline, and
/// per-script scratch state through this struct. The `_private` field
/// keeps the struct nominally non-empty so external crates cannot
/// construct it without `ScriptCtx::new()`, which keeps the door open
/// for future fields without a breaking change.
#[derive(Debug)]
pub struct ScriptCtx {
    _private: (),
}

impl ScriptCtx {
    /// Construct an empty `ScriptCtx`. PR-0 placeholder; later PRs add
    /// real construction parameters (subject, guard, deadline, …).
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ScriptCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Step-able operation.
///
/// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1: every script driver pulls
/// on a `StepOp`, receiving a four-variant `StepOutcome` parameterized
/// by the op's `Output` and a monoid-shaped `Progress` accumulator.
/// PR-0 only requires the trait shape to compile; later PRs land the
/// resolver, the `OnAgent` yield variant, and concrete op impls.
pub trait StepOp {
    /// Final value produced when the op completes (via
    /// `StepOutcome::Done`).
    type Output;
    /// Per-step progress accumulator. Must be a monoid (`StepProgress`)
    /// so partial-progress translation in `DriveMode::classify`
    /// composes across step boundaries.
    type Progress: StepProgress;

    /// Drive the op one step. Returns one of the four `StepOutcome`
    /// variants per STEP-1.
    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress>;
}
