//! inotify/fanotify syscall scaffolds.
//!
//! The fd/event-storage pieces need VFS fsnotify publication points and, for
//! fanotify permission events, delegate policy. This file only wires the Linux
//! numbers into dispatch with cheap flag validation so missing support is
//! reported as a deliberate `ENOSYS`, not as an unknown syscall.

use super::{SyscallCtx, SyscallResult, ENOSYS_VALUE};

pub(super) fn sys_inotify_add_watch(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_inotify_rm_watch(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_fanotify_mark(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}
