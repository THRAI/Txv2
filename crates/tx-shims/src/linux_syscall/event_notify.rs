//! inotify/fanotify syscall scaffolds.
//!
//! The watch/mark/event-production pieces still need VFS fsnotify publication
//! points and, for fanotify permission events, delegate policy. The init
//! syscalls, however, are fd providers: they create typed fsnotify files so
//! descriptor-oriented tests can observe Linux-like fd semantics.

use super::*;
use core::sync::atomic::{AtomicU64, Ordering};
use tx_subsystems::vfs::FsObjectId;

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

const FANOTIFY_INIT_FLAGS: u32 = FAN_CLOEXEC | FAN_NONBLOCK | FAN_CLASS_NOTIF;

const FSNOTIFY_FS_OBJECT_ID_BASE: u64 = 0xFFFB_0000_0000_0000;
static NEXT_FSNOTIFY_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(FSNOTIFY_FS_OBJECT_ID_BASE);

fn allocate_fsnotify_fs_object_id() -> FsObjectId {
    FsObjectId::new(NEXT_FSNOTIFY_FS_OBJECT_ID.fetch_add(1, Ordering::AcqRel))
}

fn install_fsnotify_fd(
    ctx: &SyscallCtx<'_>,
    kind: FsNotifyKind,
    cloexec: bool,
    nonblocking: bool,
) -> SyscallResult {
    let instance = match FsNotifyInstance::new_cap(kind) {
        Ok(instance) => instance,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let rnode = match tx_subsystems::vfs::RNode::new_cap(
        allocate_fsnotify_fs_object_id(),
        InodeMeta::new(InodeKind::Regular, 0o100600),
        RNodeBacking::StructBacked {
            payload: StructPayload::FsNotify { instance },
        },
    ) {
        Ok(rnode) => rnode,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let open_file = match OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: false,
            cloexec,
            nonblocking,
            packet: false,
            ..OpenFileFlags::default()
        },
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let fd = ctx.process.allocate_fd();
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        return SyscallResult::Error(EMFILE_VALUE);
    }
    let _ = ctx.process.install_fd(fd, open_file);
    if cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }

    SyscallResult::Return(fd as i64)
}

pub(super) fn sys_inotify_init1(flags: u32, ctx: &SyscallCtx<'_>) -> SyscallResult {
    if flags & !INOTIFY_INIT1_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    install_fsnotify_fd(
        ctx,
        FsNotifyKind::Inotify,
        flags & O_CLOEXEC != 0,
        flags & O_NONBLOCK != 0,
    )
}

pub(super) fn sys_inotify_add_watch(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_inotify_rm_watch(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_fanotify_init(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let flags = args[0] as u32;
    let event_f_flags = args[1];
    if flags & !FANOTIFY_INIT_FLAGS != 0 || event_f_flags != O_RDONLY as u64 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    install_fsnotify_fd(
        ctx,
        FsNotifyKind::Fanotify,
        flags & FAN_CLOEXEC != 0,
        flags & FAN_NONBLOCK != 0,
    )
}

pub(super) fn sys_fanotify_mark(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}
