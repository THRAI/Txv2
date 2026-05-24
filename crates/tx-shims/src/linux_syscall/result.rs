//! Syscall dispatch outcome enum.
//!
//! Plan B (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`): the dispatcher
//! reports its outcome to the caller; the userspace-entry shim writes
//! the encoded value into a fresh trap frame's `a0` slot just before
//! `sret`.

use tx_subsystems::execution::Errno;

use super::errno_to_i32;

/// Outcome of a syscall dispatch.
///
/// Plan B (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`): the dispatcher
/// reports its outcome to the caller; the userspace-entry shim writes
/// the encoded `i64`/`-i32` value into a fresh trap frame's `a0` slot
/// just before `sret`. Phase 2a only ever produces these three
/// variants — `Blocked` from a step is awaited internally and never
/// surfaces to the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyscallResult {
    /// Success — encode `value` into `a0` (positive return path).
    Return(i64),
    /// Failure — encode `-errno` into `a0`. `errno` is the positive
    /// magnitude (e.g. 38 for `ENOSYS`); the userspace-entry shim is
    /// responsible for negating before writing.
    Error(i32),
    /// Thread (or process) ended; the future driving this syscall does
    /// not return to userspace. Used by `NR_EXIT` and `NR_EXIT_GROUP`.
    NoReturn,
    /// `execve` succeeded and the process's `AddressSpace` plus the
    /// thread's `saved_user_context` have been replaced. The syscall
    /// arm returned this; the thread future MUST NOT drain
    /// `pending_syscall_return` for this iteration — the next
    /// userspace re-entry runs the new image via the new
    /// `saved_user_context`. The previous trap frame's `a0` is
    /// effectively discarded (the new image's `_start` expects a
    /// fresh stack and zero-initialised gprs).
    ///
    /// Cites: `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`.
    ExecCommitted,
    /// `rt_sigreturn` reached the kernel's platform signal-frame
    /// path. The thread future must decode the on-stack frame with
    /// `SignalFrameIf` before re-entering userspace, so musl-style
    /// handlers that edit the saved ucontext are honored.
    SigreturnRestored,
    /// `rt_sigreturn` already restored a syscall-layer compatibility
    /// signal frame into `saved_user_context`. The thread future MUST
    /// NOT drain `pending_syscall_return` and MUST NOT decode another
    /// platform frame for this iteration.
    ///
    /// N69b wires the minimal signal-frame restore path used by
    /// itimer/SIGALRM delivery; full `SignalFrameIf` integration can
    /// still replace the compat frame later.
    SigreturnContextRestored,
}

impl SyscallResult {
    /// Build an [`Error`](Self::Error) variant from a kernel [`Errno`].
    ///
    /// Folds the [`errno_to_i32`] translation so call sites don't
    /// have to spell it out — every `-errno` return from a syscall
    /// arm collapses from
    /// `SyscallResult::error_from(errno)` to
    /// `SyscallResult::error_from(errno)`.
    pub fn error_from(errno: Errno) -> Self {
        Self::Error(errno_to_i32(errno))
    }
}

impl From<Errno> for SyscallResult {
    fn from(errno: Errno) -> Self {
        Self::error_from(errno)
    }
}

/// Bridge a `Result<T, Errno>` returned by a subsystem-side script
/// or check into a [`SyscallResult`].
///
/// `on_ok` maps the success value to its dispatch outcome (typically
/// a `SyscallResult::Return(_)` or a sum-type match over an
/// outcome enum like `KillScriptOutcome`). The `Err` arm goes
/// through [`SyscallResult::error_from`] uniformly.
///
/// Folds the boilerplate
/// ```ignore
/// match script(...) {
///     Ok(v) => /* per-call mapping */,
///     Err(e) => SyscallResult::error_from(e),
/// }
/// ```
/// into a single call.
pub fn dispatch_errno<T, F>(result: Result<T, Errno>, on_ok: F) -> SyscallResult
where
    F: FnOnce(T) -> SyscallResult,
{
    match result {
        Ok(value) => on_ok(value),
        Err(errno) => SyscallResult::error_from(errno),
    }
}
