//! inotify/fanotify syscall scaffolds.
//!
//! The fd/event-storage pieces need VFS fsnotify publication points and, for
//! fanotify permission events, delegate policy. This file only wires the Linux
//! numbers into dispatch with cheap flag validation so missing support is
//! reported as a deliberate `ENOSYS`, not as an unknown syscall.

use super::{SyscallCtx, SyscallResult, EINVAL_VALUE, ENOSYS_VALUE, O_CLOEXEC, O_NONBLOCK};

const INOTIFY_INIT1_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
const FAN_UNLIMITED_QUEUE: u32 = 0x0000_0010;
const FAN_UNLIMITED_MARKS: u32 = 0x0000_0020;
const FAN_ENABLE_AUDIT: u32 = 0x0000_0040;
const FAN_REPORT_PIDFD: u32 = 0x0000_0080;
const FAN_REPORT_TID: u32 = 0x0000_0100;
const FAN_REPORT_FID: u32 = 0x0000_0200;
const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
const FAN_REPORT_NAME: u32 = 0x0000_0800;
const FAN_REPORT_TARGET_FID: u32 = 0x0000_1000;
const FAN_REPORT_FD_ERROR: u32 = 0x0000_2000;
const FAN_REPORT_MNT: u32 = 0x0000_4000;
const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_DIR_FID | FAN_REPORT_NAME;
const FAN_REPORT_DFID_NAME_TARGET: u32 =
    FAN_REPORT_DFID_NAME | FAN_REPORT_FID | FAN_REPORT_TARGET_FID;

const FANOTIFY_INIT_FLAGS: u32 = FAN_CLOEXEC
    | FAN_NONBLOCK
    | FAN_CLASS_NOTIF
    | FAN_CLASS_CONTENT
    | FAN_CLASS_PRE_CONTENT
    | FAN_UNLIMITED_QUEUE
    | FAN_UNLIMITED_MARKS
    | FAN_ENABLE_AUDIT
    | FAN_REPORT_PIDFD
    | FAN_REPORT_TID
    | FAN_REPORT_FID
    | FAN_REPORT_DIR_FID
    | FAN_REPORT_NAME
    | FAN_REPORT_TARGET_FID
    | FAN_REPORT_FD_ERROR
    | FAN_REPORT_MNT
    | FAN_REPORT_DFID_NAME
    | FAN_REPORT_DFID_NAME_TARGET;

pub(super) fn sys_inotify_init1(flags: u32, _ctx: &SyscallCtx<'_>) -> SyscallResult {
    if flags & !INOTIFY_INIT1_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_inotify_add_watch(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_inotify_rm_watch(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_fanotify_init(args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    let flags = args[0] as u32;
    if flags & !FANOTIFY_INIT_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_fanotify_mark(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}
