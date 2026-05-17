//! Syscall dispatch outcome enum.
//!
//! Plan B (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`): the dispatcher
//! reports its outcome to the caller; the userspace-entry shim writes
//! the encoded value into a fresh trap frame's `a0` slot just before
//! `sret`.

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
    /// `rt_sigreturn` restored the saved signal frame into the
    /// thread's `saved_user_context`.  The thread future MUST NOT
    /// drain `pending_syscall_return` for this iteration — the next
    /// userspace re-entry uses the restored context (which carries
    /// the original registers saved before the signal handler was
    /// invoked).  Same fall-through semantics as `ExecCommitted`:
    /// AST drain + `prepare_userspace_entry_payload` +
    /// `enter_userspace_with_context`.
    ///
    /// Phase B (first pass): the variant is declared and the thread
    /// future handles it, but the actual `SignalFrameIf` restore
    /// (reading `SavedSignalFrame` from user stack) lands in
    /// Phase D with full handler delivery.
    SigreturnRestored,
}
