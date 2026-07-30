//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, NoProgress, SpinMutex, StepOutcome};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use tx_services::time::{
    ClockRead, DeadlineRegistrarHandle, TimekeeperClock, TimekeeperIf, timekeeper,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};
use tx_subsystems::device::{RtcAlarmEmulation, RtcTime};
use tx_subsystems::tty::execution::{
    IoctlTcgetsOp, IoctlTcsetsOp, IoctlTiocgpgrpOp, IoctlTiocgwinszOp, IoctlTiocnottyOp,
    IoctlTiocscttyForProcessOp, IoctlTiocspgrpForProcessOp, IoctlTiocswinszOp,
};
use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload};
use tx_subsystems::vfs::{
    CreateInParentOp, FsObjectId, FsOps, OpenInMountNamespaceOp, OpenNoFollowInMountNamespaceOp,
    PathWalkOp, ResolveOpenTargetInMountNamespaceOp, ResolveOpenTargetOp, TruncateFsObjectOp,
    UnlinkFromParentOp,
};

mod dir_sync;
pub(super) use dir_sync::{
    fs_ops_for_rnode, sys_fdatasync, sys_flock, sys_fstatfs, sys_fsync, sys_getdents64, sys_statfs,
    sys_sync, sys_syncfs,
};

static STAT_META_OVERRIDES: SpinMutex<BTreeMap<FsObjectId, InodeMeta>> =
    SpinMutex::new(BTreeMap::new());
static FCNTL_RECORD_LOCKS: SpinMutex<BTreeMap<FsObjectId, Vec<RecordLock>>> =
    SpinMutex::new(BTreeMap::new());

static OPENAT_TMPFILE_COUNTER: AtomicU64 = AtomicU64::new(1);
static PROCFS_PROJECTED_MOUNT: SpinMutex<Option<Cap<tx_subsystems::mount::MountPayload>>> =
    SpinMutex::new(None);

const MEMFD_NAME_MAX: usize = 249;
const MEMFD_CAPACITY_BYTES: u64 = 16 * 1024 * 1024;
const MEMFD_FS_OBJECT_ID_BASE: u64 = 0xFFFC_0000_0000_0000;
const MEMFD_SECRET_FS_OBJECT_ID_BASE: u64 = 0xFFFA_0000_0000_0000;
static NEXT_MEMFD_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(MEMFD_FS_OBJECT_ID_BASE);
static NEXT_MEMFD_SECRET_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(MEMFD_SECRET_FS_OBJECT_ID_BASE);
const CALLER_DEV_TTY_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7674);

pub(super) fn record_stat_meta_override(fs_object_id: FsObjectId, meta: InodeMeta) {
    STAT_META_OVERRIDES.lock().insert(fs_object_id, meta);
}

pub(super) fn stat_meta_override_or(fs_object_id: FsObjectId, fallback: InodeMeta) -> InodeMeta {
    STAT_META_OVERRIDES
        .lock()
        .get(&fs_object_id)
        .copied()
        .unwrap_or(fallback)
}

/// Drain all stale entries from the global `STAT_META_OVERRIDES` map.
/// Called from test setup to prevent cross-test pollution (a
/// `sys_utimensat` test writing an override for a `FsObjectId` that
/// a later `newfstatat` test also uses).
#[cfg(test)]
pub(crate) fn clear_stat_meta_overrides() {
    STAT_META_OVERRIDES.lock().clear();
}

fn apply_stat_meta_override(fs_object_id: FsObjectId, meta: &mut InodeMeta) {
    *meta = stat_meta_override_or(fs_object_id, *meta);
}

fn allocate_fd_under_limit<'a>(ctx: &SyscallCtx<'a>) -> Result<u32, SyscallResult> {
    let fd = ctx.process.allocate_fd();
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        Err(SyscallResult::Error(EMFILE_VALUE))
    } else {
        Ok(fd)
    }
}

fn ensure_fd_room_under_limit<'a>(ctx: &SyscallCtx<'a>) -> Result<(), SyscallResult> {
    let fd = ctx.process.next_fd_above(0);
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        Err(SyscallResult::Error(EMFILE_VALUE))
    } else {
        Ok(())
    }
}

fn proc_self_userns_file_id(path: &[u8], pid: Pid) -> Option<FsObjectId> {
    match path {
        b"/proc/self/uid_map" => Some(tx_fs::procfs::pid_uid_map_id(pid)),
        b"/proc/self/gid_map" => Some(tx_fs::procfs::pid_gid_map_id(pid)),
        b"/proc/self/setgroups" => Some(tx_fs::procfs::pid_setgroups_id(pid)),
        _ => None,
    }
}

fn procfs_root_relative_netns_pid(path: &[u8], cwd: &Cap<DEntry>) -> Option<Pid> {
    if cwd.rnode().fs_object_id() != tx_fs::procfs::PROCFS_ROOT_ID {
        return None;
    }
    let rest = path.strip_prefix(b"/")?;
    let rest = rest.strip_suffix(b"/ns/net")?;
    if rest.is_empty() || rest.contains(&b'/') {
        return None;
    }
    let pid = core::str::from_utf8(rest).ok()?.parse::<u32>().ok()?;
    Some(Pid(pid))
}

pub(super) fn is_devfs_root_dentry(dentry: &Cap<DEntry>) -> bool {
    dentry.rnode().fs_object_id() == tx_fs::devfs::DEVFS_ROOT_OBJECT_ID
}

pub(super) fn is_caller_dev_tty_path(path: &[u8], cwd: &Cap<DEntry>) -> bool {
    path == b"/dev/tty" || (path == b"tty" && is_devfs_root_dentry(cwd))
}

fn proc_self_fd_number(path: &[u8]) -> Option<u32> {
    let rest = path.strip_prefix(b"/proc/self/fd/")?;
    if rest.is_empty() || rest.contains(&b'/') {
        return None;
    }
    core::str::from_utf8(rest).ok()?.parse::<u32>().ok()
}

pub(super) fn caller_dev_tty_stat_info<'a>(
    ctx: &SyscallCtx<'a>,
) -> Result<(InodeMeta, FsObjectId, u32, u32), SyscallResult> {
    if ctx
        .process
        .pgrp_cap()
        .session_cap()
        .controlling_tty_cap()
        .is_none()
    {
        return Err(SyscallResult::Error(ENXIO_VALUE));
    }
    Ok((
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        CALLER_DEV_TTY_OBJECT_ID,
        5,
        0,
    ))
}

fn maybe_acquire_controlling_tty_on_open<'a>(
    file: &Cap<OpenFile>,
    raw_flags: u32,
    want_path_only: bool,
    ctx: &SyscallCtx<'a>,
) {
    if want_path_only || raw_flags & O_NOCTTY != 0 {
        return;
    }

    let tty = match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        _ => return,
    };

    let guard = step_engine::guard();
    let _ = step_ioctl_tiocsctty_for_process(&tty, &ctx.process, &guard);
}

fn open_caller_dev_tty<'a>(
    open_flags: OpenFileFlags,
    ctx: &SyscallCtx<'a>,
) -> Result<Cap<OpenFile>, SyscallResult> {
    let tty = match ctx.process.pgrp_cap().session_cap().controlling_tty_cap() {
        Some(tty) => tty,
        None => return Err(SyscallResult::Error(ENXIO_VALUE)),
    };
    let guard = step_engine::guard();
    match tx_subsystems::tty::project::open_file_for_tty_with_flags(tty, open_flags, &guard) {
        StepOutcome::Done(file) => Ok(file),
        StepOutcome::Err(errno) => Err(SyscallResult::error_from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            Err(SyscallResult::Error(EIO_VALUE))
        }
    }
}

fn open_procfs_projected_file(
    fs_object_id: FsObjectId,
    flags: OpenFileFlags,
) -> Result<Cap<OpenFile>, SyscallResult> {
    let procfs = tx_fs::procfs::Procfs::new();
    let mount = procfs_projected_mount()?;

    let guard = step_engine::guard();
    let meta = match procfs.load_inode_meta(fs_object_id, &guard) {
        StepOutcome::Done(meta) => meta,
        StepOutcome::Err(errno) => return Err(SyscallResult::error_from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            return Err(SyscallResult::Error(EIO_VALUE));
        }
    };
    let rnode = match procfs.materialise_rnode(fs_object_id, meta, &mount, &guard) {
        StepOutcome::Done(rnode) => rnode,
        StepOutcome::Err(errno) => return Err(SyscallResult::error_from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            return Err(SyscallResult::Error(EIO_VALUE));
        }
    };
    OpenFile::new_cap(rnode, flags).map_err(|_| SyscallResult::Error(ENOMEM_VALUE))
}

fn procfs_projected_mount() -> Result<Cap<tx_subsystems::mount::MountPayload>, SyscallResult> {
    let mut cached = PROCFS_PROJECTED_MOUNT.lock();
    if let Some(mount) = cached.as_ref() {
        return Ok(mount.clone());
    }

    let mount = tx_subsystems::mount::MountPayload::new_cap(
        tx_fs::procfs::Procfs::fs_ops_arc(),
        alloc::sync::Arc::new(tx_fs::procfs::Procfs::new())
            as alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        tx_subsystems::mount::allocate_dev_id(),
        tx_subsystems::mount::MountOptions::default(),
        "proc",
        tx_subsystems::mount::SourceLabel::Static("proc"),
    )
    .map_err(|_| SyscallResult::Error(ENOMEM_VALUE))?;
    *cached = Some(mount.clone());
    Ok(mount)
}

fn allocate_fd_at_least_under_limit<'a>(
    ctx: &SyscallCtx<'a>,
    min: u32,
) -> Result<u32, SyscallResult> {
    let fd = ctx.process.allocate_fd_at_least(min);
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        Err(SyscallResult::Error(EMFILE_VALUE))
    } else {
        Ok(fd)
    }
}

fn openat_tmpfile_name(seq: u64) -> Vec<u8> {
    let mut name = Vec::from(&b".tx-tmpfile-"[..]);
    let mut digits = [0u8; 20];
    let mut n = seq;
    let mut idx = digits.len();
    loop {
        idx -= 1;
        digits[idx] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    name.extend_from_slice(&digits[idx..]);
    name
}

fn allocate_memfd_fs_object_id() -> FsObjectId {
    FsObjectId::new(NEXT_MEMFD_FS_OBJECT_ID.fetch_add(1, Ordering::AcqRel))
}

fn allocate_memfd_secret_fs_object_id() -> FsObjectId {
    FsObjectId::new(NEXT_MEMFD_SECRET_FS_OBJECT_ID.fetch_add(1, Ordering::AcqRel))
}

/// `memfd_create(name, flags)`. Linux generic ABI `__NR_memfd_create = 279`.
///
/// Models memfd as an anonymous PageBacked regular file with no path
/// namespace presence. Sealing and hugetlb are rejected until their owners
/// exist.
pub(super) fn sys_memfd_create<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let name_uaddr = args[0];
    let flags = args[1] as u32;

    if name_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let recognised = MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_HUGETLB;
    if flags & !recognised != 0 || flags & (MFD_ALLOW_SEALING | MFD_HUGETLB) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    match bootstrap_read_user_cstr(&ctx.aspace, name_uaddr, MEMFD_NAME_MAX + 1) {
        Ok(name) if name.len() <= MEMFD_NAME_MAX => {}
        Ok(_) | Err(Errno::ENAMETOOLONG) => return SyscallResult::Error(EINVAL_VALUE),
        Err(errno) => return SyscallResult::error_from(errno),
    }

    let page_count = MEMFD_CAPACITY_BYTES / USER_PAGE_SIZE as u64;
    let pc = match PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    ) {
        Ok(pc) => pc,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    pc.set_size_bytes(0);

    let rnode = match tx_subsystems::vfs::RNode::new_cap(
        allocate_memfd_fs_object_id(),
        InodeMeta::new(InodeKind::Regular, 0o100666),
        RNodeBacking::PageBacked { pc },
    ) {
        Ok(rnode) => rnode,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let open_file = match OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            cloexec: flags & MFD_CLOEXEC != 0,
            packet: false,
            ..OpenFileFlags::default()
        },
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let fd = match allocate_fd_under_limit(ctx) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let _ = ctx.process.install_fd(fd, open_file);
    if flags & MFD_CLOEXEC != 0 {
        ctx.process.set_fd_cloexec(fd, true);
    }

    SyscallResult::Return(fd as i64)
}

/// `memfd_secret(flags)`. Linux generic ABI `__NR_memfd_secret = 447`.
pub(super) fn sys_memfd_secret<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let flags = args[0];
    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let page_count = MEMFD_CAPACITY_BYTES / USER_PAGE_SIZE as u64;
    let pc = match PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    ) {
        Ok(pc) => pc,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    pc.set_size_bytes(0);

    let rnode = match tx_subsystems::vfs::RNode::new_cap(
        allocate_memfd_secret_fs_object_id(),
        InodeMeta::new(InodeKind::Regular, 0o100600),
        RNodeBacking::PageBacked { pc },
    ) {
        Ok(rnode) => rnode,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let open_file = match OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            packet: false,
            ..OpenFileFlags::default()
        },
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let fd = match allocate_fd_under_limit(ctx) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let _ = ctx.process.install_fd(fd, open_file);

    SyscallResult::Return(fd as i64)
}

/// `fcntl(fd, cmd, arg)` per the Wave 2 ELF-loader plan §"Part 2 —
/// Per-fd CLOEXEC bitmap + fcntl(F_SETFD) + O_CLOEXEC" plus Slice 7 of
/// the shell-prompt roadmap (fcntl extension).
///
/// Wave 2 surface: `F_GETFD` / `F_SETFD` against the per-process
/// CLOEXEC set.
///
/// Slice 7 surface adds `F_DUPFD` / `F_DUPFD_CLOEXEC` / `F_GETFL`.
/// `F_SETFL` updates the mutable nonblocking and packet-mode status
/// bits exposed through `OpenFile::flags()`.
///
/// Validation (fd-ops Wave 1: `EBADF` is now driven by "is this fd
/// open?" rather than the retired `FD_TABLE_SIZE = 8` ceiling — Linux
/// returns `-EBADF` for `F_GETFD`/`F_SETFD` against a closed fd):
/// - fd not currently open → `-EBADF`.
/// - Unknown `cmd` → `-ENOSYS`.
pub(super) fn sys_fcntl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as u32;
    let cmd = args[1] as i32;
    let arg = args[2];

    // EBADF if the fd is not open. fd-ops Wave 1 retires the static
    // `FD_TABLE_SIZE` ceiling — the fd table is now a sparse
    // `BTreeMap<u32, Cap<OpenFile>>`, so any `u32` could be a key;
    // openness is the only meaningful EBADF discriminant.
    let file = match ctx.process.fd(fd) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // PR-3: F_GETFD/F_SETFD go through FcntlFdOp + drive_oneshot.
    if cmd == F_GETFD || cmd == F_SETFD {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let set_on = if cmd == F_SETFD {
            Some((arg & FD_CLOEXEC as u64) != 0)
        } else {
            None
        };
        let mut op = FcntlFdOp {
            process: ctx.process.clone(),
            fd,
            set_on,
        };
        return match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(Some(cloexec)) => SyscallResult::Return(if cloexec { FD_CLOEXEC as i64 } else { 0 }),
            Ok(None) => SyscallResult::Return(0),
            Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
        };
    }

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let min = match u32::try_from(arg) {
                Ok(min) => min,
                Err(_) => return SyscallResult::Error(EINVAL_VALUE),
            };
            let (soft_limit, _) = ctx.process.rlimit_nofile();
            if min >= soft_limit {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            if let Err(err) = allocate_fd_at_least_under_limit(ctx, min) {
                return err;
            }
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = FcntlDupFdOp {
                process: ctx.process.clone(),
                fd,
                min,
                cloexec: cmd == F_DUPFD_CLOEXEC,
            };
            return match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(new_fd) => SyscallResult::Return(new_fd as i64),
                Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
            };
        }
        F_GETFL => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = OpenFileGetFlOp { file: &file };
            let f = match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(flags) => flags,
                Err(v3errno) => {
                    return SyscallResult::error_from(Errno::from(v3errno));
                }
            };
            let mut bits: u64 = match (f.read, f.write) {
                (true, false) => O_RDONLY as u64,
                (false, true) => O_WRONLY as u64,
                (true, true) => O_RDWR as u64,
                (false, false) => O_RDONLY as u64,
            };
            if f.append {
                bits |= O_APPEND as u64;
            }
            if f.nonblocking {
                bits |= O_NONBLOCK as u64;
            }
            if f.packet {
                bits |= O_DIRECT as u64;
            }
            SyscallResult::Return(bits as i64)
        }
        F_SETFL => {
            let arg = args[2] as u64;
            let packet = (arg & O_DIRECT as u64) != 0;
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = OpenFileSetFlOp {
                file: &file,
                nonblocking: (arg & O_NONBLOCK as u64) != 0,
                packet,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(()) => {
                    if (arg & O_NONBLOCK as u64) != 0 {
                        if let Some(ops) = file.file_ops() {
                            let mut post = |mailbox: &TaskMailbox, event: MailboxEvent| {
                                ctx.post_mailbox_ref_event(mailbox, event)
                            };
                            ops.on_set_fl_nonblock(&mut post);
                        }
                    }
                    SyscallResult::Return(0)
                }
                Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
            }
        }
        F_GETLK | F_OFD_GETLK => fcntl_getlk(ctx, &file, arg),
        F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => fcntl_setlk(ctx, &file, arg),
        F_SETLEASE => SyscallResult::Error(EAGAIN_VALUE),
        F_GETLEASE => SyscallResult::Return(F_UNLCK as i64),
        F_GETPIPE_SZ | F_SETPIPE_SZ => {
            let Some(payload) = pipe_payload_for_fcntl(&file) else {
                return SyscallResult::Error(EINVAL_VALUE);
            };
            if cmd == F_GETPIPE_SZ {
                return SyscallResult::Return(payload.pipe_size_bytes() as i64);
            }
            let requested = match usize::try_from(arg) {
                Ok(size) => size,
                Err(_) => return SyscallResult::Error(EINVAL_VALUE),
            };
            match payload.set_pipe_size_bytes(requested) {
                Ok(size) => SyscallResult::Return(size as i64),
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct FlockLayout {
    l_type: i16,
    l_whence: i16,
    _pad0: i32,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
    _pad1: i32,
}

#[derive(Clone, Copy)]
struct RecordLock {
    owner: u32,
    lock_type: i16,
    start: i64,
    len: i64,
}

impl RecordLock {
    fn conflicts_with(&self, other: &RecordLock) -> bool {
        self.owner != other.owner
            && (self.lock_type == F_WRLCK || other.lock_type == F_WRLCK)
            && lock_ranges_overlap(self.start, self.len, other.start, other.len)
    }
}

fn lock_range_end(start: i64, len: i64) -> (i64, Option<i64>) {
    if len == 0 {
        (start, None)
    } else if len > 0 {
        (start, Some(start.saturating_add(len)))
    } else {
        (start.saturating_add(len), Some(start))
    }
}

fn lock_ranges_overlap(a_start: i64, a_len: i64, b_start: i64, b_len: i64) -> bool {
    let (a0, a1) = lock_range_end(a_start, a_len);
    let (b0, b1) = lock_range_end(b_start, b_len);
    let a_before_b = a1.is_some_and(|end| end <= b0);
    let b_before_a = b1.is_some_and(|end| end <= a0);
    !a_before_b && !b_before_a
}

fn fcntl_valid_whence(whence: i16) -> bool {
    matches!(whence, 0..=2)
}

fn fcntl_valid_lock_type(lock_type: i16) -> bool {
    matches!(lock_type, F_RDLCK | F_WRLCK | F_UNLCK)
}

fn fcntl_file_id(file: &OpenFile) -> Option<FsObjectId> {
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => Some(rnode.fs_object_id()),
        _ => None,
    }
}

fn fcntl_release_process_locks_for_file(owner: u32, file: &OpenFile) {
    let Some(file_id) = fcntl_file_id(file) else {
        return;
    };
    let mut locks = FCNTL_RECORD_LOCKS.lock();
    if let Some(list) = locks.get_mut(&file_id) {
        list.retain(|lock| lock.owner != owner);
        if list.is_empty() {
            locks.remove(&file_id);
        }
    }
}

fn queue_file_close_writeback(file: &Cap<OpenFile>) {
    if let Some(pc) = crate::linux_syscall::vm::extract_page_container(file) {
        // The async close-writeback admission only makes progress when the
        // mount has a backend planner driving the L4 pipeline. The bootstrap
        // sdcard ext4 deliberately has none (block-completion IRQs cannot be
        // serviced while the bootstrap exec is the only thing running), so
        // fall back to the synchronous flush — the same planner/no-planner
        // split `FsyncOp` makes. Without this, every file written on that
        // mount closed with its bytes still in the page cache: `git init`
        // left `.git/HEAD` and `.git/config` existing but EMPTY, and git
        // then reported "not a git repository".
        let planner_backed = match pc.kind() {
            tx_subsystems::page_backed::PageContainerKind::File { mount, .. } => {
                mount.payload().backend_planner().is_some()
            }
            _ => true,
        };
        if planner_backed {
            let _ = pc.queue_dirty_file_writeback();
        } else {
            let guard = step_engine::guard();
            let _ = tx_subsystems::page_backed::step_fsync(&pc, &guard);
        }
    }
}

fn fcntl_getlk(ctx: &SyscallCtx<'_>, file: &OpenFile, flock_uaddr: u64) -> SyscallResult {
    let mut flock = match bootstrap_read_user::<FlockLayout>(&ctx.aspace, flock_uaddr) {
        Ok(flock) => flock,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    if !fcntl_valid_whence(flock.l_whence) || !fcntl_valid_lock_type(flock.l_type) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let query = RecordLock {
        owner: ctx.process.pid.0,
        lock_type: flock.l_type,
        start: flock.l_start,
        len: flock.l_len,
    };
    if let Some(file_id) = fcntl_file_id(file) {
        if let Some(conflict) = FCNTL_RECORD_LOCKS.lock().get(&file_id).and_then(|locks| {
            locks
                .iter()
                .copied()
                .find(|lock| lock.conflicts_with(&query))
        }) {
            flock.l_type = conflict.lock_type;
            flock.l_whence = 0;
            flock.l_start = conflict.start;
            flock.l_len = conflict.len;
            flock.l_pid = conflict.owner as i32;
            return match bootstrap_write_user::<FlockLayout>(&ctx.aspace, flock_uaddr, flock) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            };
        }
    }
    flock.l_type = F_UNLCK;
    match bootstrap_write_user::<FlockLayout>(&ctx.aspace, flock_uaddr, flock) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

fn fcntl_setlk(ctx: &SyscallCtx<'_>, file: &OpenFile, flock_uaddr: u64) -> SyscallResult {
    let flock = match bootstrap_read_user::<FlockLayout>(&ctx.aspace, flock_uaddr) {
        Ok(flock) => flock,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    if !fcntl_valid_whence(flock.l_whence) || !fcntl_valid_lock_type(flock.l_type) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let Some(file_id) = fcntl_file_id(file) else {
        return SyscallResult::Return(0);
    };
    let owner = ctx.process.pid.0;
    let request = RecordLock {
        owner,
        lock_type: flock.l_type,
        start: flock.l_start,
        len: flock.l_len,
    };
    let mut locks = FCNTL_RECORD_LOCKS.lock();
    let list = locks.entry(file_id).or_default();
    if flock.l_type == F_UNLCK {
        list.retain(|lock| {
            lock.owner != owner
                || !lock_ranges_overlap(lock.start, lock.len, request.start, request.len)
        });
        if list.is_empty() {
            locks.remove(&file_id);
        }
        return SyscallResult::Return(0);
    }
    if list.iter().any(|lock| lock.conflicts_with(&request)) {
        return SyscallResult::Error(EAGAIN_VALUE);
    }
    list.retain(|lock| {
        lock.owner != owner
            || !lock_ranges_overlap(lock.start, lock.len, request.start, request.len)
    });
    list.push(request);
    SyscallResult::Return(0)
}

fn pipe_payload_for_fcntl(file: &Cap<OpenFile>) -> Option<Cap<tx_subsystems::pipe::PipePayload>> {
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return None;
    };
    let RNodeBacking::StructBacked {
        payload: StructPayload::Pipe { payload, .. },
    } = rnode.backing()
    else {
        return None;
    };
    Some(payload.clone())
}

/// `close_range(first, last, flags)`.
///
/// Walks sparse fd/cloexec keys rather than scanning the numeric
/// interval. `CLOSE_RANGE_UNSHARE` is recognised but intentionally
/// deferred because txKernel has no fd-table sharing model yet.
pub(super) fn sys_close_range<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let first = args[0] as u32;
    let last = args[1] as u32;
    let flags = args[2] as u32;

    const KNOWN_FLAGS: u32 = CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC;
    if flags & !KNOWN_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & CLOSE_RANGE_UNSHARE != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    if first > last {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    if flags & CLOSE_RANGE_CLOEXEC != 0 {
        let open_keys = ctx.process.open_fd_numbers();
        for fd in open_keys.range(first..=last).copied() {
            ctx.process.set_fd_cloexec(fd, true);
        }
        return SyscallResult::Return(0);
    }

    for file in ctx.process.take_fds_for_close_range(first, last) {
        queue_file_close_writeback(&file);
        file.flock_release();
        fcntl_release_process_locks_for_file(ctx.process.pid.0, &file);
        maybe_close_socket_file_after_fd_remove(&file);
    }
    SyscallResult::Return(0)
}

/// `openat(dirfd, path, flags, mode)`. Linux RV64 generic ABI
/// `__NR_openat = 56`.
///
/// Wave 2's surface: `dirfd == AT_FDCWD` only (non-cwd dirfds return
/// `-EBADF` because the slice's fd table doesn't carry directory-fd
/// semantics yet — `TODO(phase-dirfd)` matches Wave 4 Part 4's
/// `resolve_path_at`). Path resolution goes through `vfs::step_open`
/// using the caller's `walker_cred()` (effective ids per POSIX DAC
/// rule).
///
/// Flag decoding mirrors Linux's `man 2 open` (`O_RDONLY`/`O_WRONLY`/
/// `O_RDWR` access mode + `O_CREAT`/`O_EXCL`/`O_TRUNC`/`O_APPEND`/
/// `O_CLOEXEC`/`O_NONBLOCK`). Unrecognised bits are accept-and-ignore
/// (matches Linux's lenient open-flag policy). `O_NONBLOCK` itself is
/// accepted but ignored — `OpenFile` doesn't carry a non-blocking
/// state today (`TODO(phase-nonblock)`).
///
/// `O_CREAT` semantic: `step_open` is resolve-only by contract (the
/// slice plan §"Cross-cutting risks #6"), so the syscall arm
/// implements create-on-missing at this layer. On the first
/// `step_open` returning `Errno::ENOENT` with `O_CREAT` set, the arm
/// walks to the parent directory via `step_walk`, calls
/// `FsOps::create_inode(parent, basename, mode, &cred, &guard)`, then
/// re-runs `step_open` against the now-existing inode. `O_CREAT |
/// O_EXCL` against an existing file fast-fails with `-EEXIST`.
///
/// `O_TRUNC` semantic: applied **after** the file has been resolved
/// or created. Targets the in-scope `MountPayload.fs_page_backing`'s
/// `truncate(fs_object_id, 0, &guard)` hook. Backends without
/// page-backed regular files surface `Errno::ENOSYS`. Truncate on a
/// directory returns `-EISDIR` matching Linux. The DAC permission
/// check on the truncate-implies-write Linux policy is **deferred**
/// per the DAC + setuid plan Open Q #6 — the `O_TRUNC` request is
/// honoured iff the open mode itself was permitted (which already
/// went through `check_open_perm` inside `step_open`).
//
// PR-9 phase 3b: not yet StepOp-driven — pending. `sys_openat`
// orchestrates resolution via the free fns `step_walk` and
// `step_open` (no `*Op` wrap exists for either today); creation
// goes through `FsOps::create_inode`, also free-fn. When walker /
// open / create gain StepOp wraps, thread `&mut KernelScriptCtx`
// here and replace the synchronous-poll dance with the wrap form.
pub(super) async fn sys_openat<'a, P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    flags: u32,
    mode: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    // Early FD-limit check: Linux returns EMFILE before doing any
    // significant work (path resolution, inode lookup). The per-
    // process soft limit is the gate — if the lowest free fd is ≥
    // soft_limit, there is no room for a new descriptor. This avoids
    // wasted I/O when the table is already full.
    if let Err(err) = ensure_fd_room_under_limit(ctx) {
        return err;
    }

    // Bounded inline copy of the user path. Same `EXECVE_PATH_MAX = 4096`
    // budget as the existing `execve` / `fchmodat` arms (and matches
    // Linux's `PATH_MAX`). Empty paths surface as `-ENOENT` from the
    // walker — let it through so the lookup-side error wins.
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };

    // Decode the open flags. Access-mode picks the read/write pair;
    // O_APPEND / O_CLOEXEC thread through to OpenFileFlags. O_NONBLOCK
    // is accepted but ignored (no blocking state on OpenFile yet).
    let want_path_only = flags & O_PATH != 0;
    let (want_read, want_write) = if want_path_only {
        (false, false)
    } else {
        decode_access_mode(flags)
    };
    let want_append = flags & O_APPEND != 0;
    let want_cloexec = flags & O_CLOEXEC != 0;
    let want_create = !want_path_only && flags & O_CREAT != 0;
    let want_excl = !want_path_only && flags & O_EXCL != 0;
    let want_trunc = !want_path_only && flags & O_TRUNC != 0;
    let want_directory = flags & O_DIRECTORY != 0;
    let want_tmpfile = flags & __O_TMPFILE != 0;
    let want_nofollow = flags & numbers::O_NOFOLLOW != 0;
    // O_DIRECT remains an OpenFile status bit until its PageBacked owner
    // selects the direct-I/O submit path. Other unrecognised bits are dropped.

    if flags & __O_TMPFILE != 0 && flags & O_TMPFILE != O_TMPFILE {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if want_create && want_directory {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let open_flags = OpenFileFlags {
        read: want_read && !want_path_only,
        write: want_write && !want_path_only,
        append: want_append,
        cloexec: want_cloexec,
        nonblocking: flags & O_NONBLOCK != 0,
        packet: flags & O_DIRECT != 0,
    };

    if dirfd != AT_FDCWD {
        if dirfd < 0 || ctx.process.fd(dirfd as u32).is_none() {
            return SyscallResult::Error(EBADF_VALUE);
        }
    }

    if let Some(fs_object_id) = proc_self_userns_file_id(path.as_slice(), ctx.process.pid) {
        if want_directory {
            return SyscallResult::Error(ENOTDIR_VALUE);
        }
        if want_create && want_excl {
            return SyscallResult::Error(EEXIST_VALUE);
        }
        let openfile = match open_procfs_projected_file(fs_object_id, open_flags) {
            Ok(file) => file,
            Err(result) => return result,
        };
        let fd = match allocate_fd_under_limit(ctx) {
            Ok(fd) => fd,
            Err(err) => return err,
        };
        let _ = ctx.process.set_fd(fd, Some(openfile));
        if want_cloexec {
            ctx.process.set_fd_cloexec(fd, true);
        }
        return SyscallResult::Return(fd as i64);
    }

    if path.as_slice() == b"/proc/self/ns/net" && !want_create && !want_trunc {
        if want_directory {
            return SyscallResult::Error(ENOTDIR_VALUE);
        }
        if want_write {
            return SyscallResult::Error(EACCES_VALUE);
        }
        let Some(payload) = ctx.process.net_namespace() else {
            return SyscallResult::Error(EIO_VALUE);
        };
        let file = match tx_subsystems::net::net_namespace_open_file_from_payload(payload) {
            Ok(file) => file,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let fd = match allocate_fd_under_limit(ctx) {
            Ok(fd) => fd,
            Err(err) => return err,
        };
        let _ = ctx.process.set_fd(fd, Some(file));
        if want_cloexec {
            ctx.process.set_fd_cloexec(fd, true);
        }
        return SyscallResult::Return(fd as i64);
    }

    if !want_create && !want_trunc {
        if let Some(cwd) = ctx.process.cwd() {
            if let Some(pid) = procfs_root_relative_netns_pid(path.as_slice(), &cwd) {
                if want_directory {
                    return SyscallResult::Error(ENOTDIR_VALUE);
                }
                if want_write {
                    return SyscallResult::Error(EACCES_VALUE);
                }
                let Some(process) = process_by_pid(Pid(pid.0)) else {
                    return SyscallResult::Error(ENOENT_VALUE);
                };
                let Some(payload) = process.net_namespace() else {
                    return SyscallResult::Error(EIO_VALUE);
                };
                let file = match tx_subsystems::net::net_namespace_open_file_from_payload(payload) {
                    Ok(file) => file,
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                };
                let fd = match allocate_fd_under_limit(ctx) {
                    Ok(fd) => fd,
                    Err(err) => return err,
                };
                let _ = ctx.process.set_fd(fd, Some(file));
                if want_cloexec {
                    ctx.process.set_fd_cloexec(fd, true);
                }
                return SyscallResult::Return(fd as i64);
            }
        }
    }

    // Resolve the dirfd anchor. AT_FDCWD → process cwd; a real dirfd
    // → the `opendir_dentry` of its OpenFile (an O_DIRECTORY open of
    // that directory). Invalid / non-directory fds surface as EBADF /
    // ENOTDIR.
    let cwd: Cap<DEntry> = match dirfd_anchor_for_path(dirfd, &path, ctx) {
        Ok(cwd) => cwd,
        Err(result) => return result,
    };
    let mounted_cwd = if dirfd == AT_FDCWD {
        match (ctx.process.cwd_binding(), ctx.process.mount_namespace_cap()) {
            (Some(binding), Some(namespace)) => Some((binding, namespace)),
            _ => None,
        }
    } else {
        None
    };

    let walker_cred = ctx.walker_cred();

    if want_tmpfile {
        if want_path_only || !want_write || want_create || want_trunc {
            return SyscallResult::Error(EINVAL_VALUE);
        }

        use step_engine::DriveMode;
        use tx_scripts::drive;
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mailbox_arc = script_ctx.mailbox().cloned();
        let delegate_registry_arc = script_ctx.delegate_registry().cloned();
        let timer_registrar_handle = script_ctx.timer_registrar().cloned();
        let dir_dentry = match drive(
            PathWalkOp {
                rooted_at: cwd.clone(),
                path: path.clone(),
                cred: walker_cred.clone(),
            },
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(dentry) => dentry,
            Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        };
        if dir_dentry.rnode().meta().kind() != InodeKind::Directory {
            return SyscallResult::Error(ENOTDIR_VALUE);
        }

        let fs_ops = match fs_ops_for_dentry(&dir_dentry) {
            Some(ops) => ops,
            None => return SyscallResult::Error(EOPNOTSUPP_VALUE),
        };
        let parent_id = dir_dentry.rnode().fs_object_id();
        let create_mode = (mode as u16) & !ctx.process.umask() & 0o7777;

        let mut opened = None;
        for _ in 0..16 {
            let seq = OPENAT_TMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let tmp_name = openat_tmpfile_name(seq);
            let create_result = drive(
                CreateInParentOp {
                    fs_ops: &fs_ops,
                    parent: parent_id,
                    name: &tmp_name,
                    mode: create_mode,
                    cred: &walker_cred,
                },
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await;
            match create_result {
                Ok(_) => {
                    dir_dentry.remove_cached_child_by_name(&tmp_name);
                }
                Err(step_engine::Errno::EEXIST) => continue,
                Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
            }

            let openfile = match drive(
                OpenOp {
                    rooted_at: dir_dentry.clone(),
                    path: tmp_name.clone(),
                    flags: open_flags,
                    mode: mode as u16,
                    cred: walker_cred.clone(),
                },
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await
            {
                Ok(file) => file,
                Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
            };

            let target_id = openfile.rnode().fs_object_id();
            let unlink_result = {
                let mut op = UnlinkFromParentOp {
                    fs_ops: &fs_ops,
                    parent: parent_id,
                    name: &tmp_name,
                    target: target_id,
                    remove_dir: false,
                };
                step_engine::drive_oneshot(&mut op, &mut script_ctx)
            };
            match unlink_result {
                Ok(()) => {
                    dir_dentry.remove_cached_child_by_name(&tmp_name);
                    let guard = step_engine::guard();
                    let _ = fs_ops.destroy_inode(target_id, &guard);
                    opened = Some(openfile);
                    break;
                }
                Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
            }
        }

        let Some(openfile) = opened else {
            return SyscallResult::Error(EEXIST_VALUE);
        };
        let fd = match allocate_fd_under_limit(ctx) {
            Ok(fd) => fd,
            Err(err) => return err,
        };
        let _ = ctx.process.set_fd(fd, Some(openfile));
        if want_cloexec {
            ctx.process.set_fd_cloexec(fd, true);
        }
        return SyscallResult::Return(fd as i64);
    }

    if is_caller_dev_tty_path(&path, &cwd) {
        if want_directory {
            return SyscallResult::Error(ENOTDIR_VALUE);
        }
        let openfile = match open_caller_dev_tty(open_flags, ctx) {
            Ok(file) => file,
            Err(result) => return result,
        };
        let fd = match allocate_fd_under_limit(ctx) {
            Ok(fd) => fd,
            Err(err) => return err,
        };
        let _ = ctx.process.set_fd(fd, Some(openfile));
        if want_cloexec {
            ctx.process.set_fd_cloexec(fd, true);
        }
        return SyscallResult::Return(fd as i64);
    }

    // PR async migration: non-O_CREAT, non-O_TRUNC simple open
    // goes through `OpenOp + drive()` — no manual step loop.
    if !want_create && !want_trunc {
        use step_engine::DriveMode;
        let fd = match allocate_fd_under_limit(ctx) {
            Ok(fd) => fd,
            Err(err) => return err,
        };
        use tx_scripts::drive;
        let mut script_ctx = build_subject_script_ctx(ctx);
        let walker_cred = ctx.walker_cred();
        let mailbox_arc = script_ctx.mailbox().cloned();
        let timer_registrar_handle = script_ctx.timer_registrar().cloned();
        let delegate_registry_arc = script_ctx.delegate_registry().cloned();
        let openfile = if let Some((binding, mount_namespace)) = mounted_cwd.clone() {
            if want_nofollow {
                let op = OpenNoFollowInMountNamespaceOp {
                    rooted_at: binding.dentry,
                    origin_mount: binding.mount,
                    mount_namespace,
                    path: path.clone(),
                    flags: open_flags,
                    mode: mode as u16,
                    cred: walker_cred,
                };
                match drive(
                    op,
                    &mut script_ctx,
                    DriveMode::Waiting,
                    mailbox_arc.as_ref(),
                    delegate_registry_arc.as_deref(),
                    timer_registrar_handle.as_ref(),
                )
                .await
                {
                    Ok(opened) => opened.open_file,
                    Err(v3errno) => {
                        return SyscallResult::error_from(Errno::from(v3errno));
                    }
                }
            } else {
                let op = OpenInMountNamespaceOp {
                    rooted_at: binding.dentry,
                    origin_mount: binding.mount,
                    mount_namespace,
                    path: path.clone(),
                    flags: open_flags,
                    mode: mode as u16,
                    cred: walker_cred,
                };
                match drive(
                    op,
                    &mut script_ctx,
                    DriveMode::Waiting,
                    mailbox_arc.as_ref(),
                    delegate_registry_arc.as_deref(),
                    timer_registrar_handle.as_ref(),
                )
                .await
                {
                    Ok(opened) => opened.open_file,
                    Err(v3errno) => {
                        return SyscallResult::error_from(Errno::from(v3errno));
                    }
                }
            }
        } else if want_nofollow {
            let op = OpenNoFollowOp {
                rooted_at: cwd.clone(),
                path: path.clone(),
                flags: open_flags,
                mode: mode as u16,
                cred: walker_cred,
            };
            match drive(
                op,
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await
            {
                Ok(file) => file,
                Err(v3errno) => {
                    return SyscallResult::error_from(Errno::from(v3errno));
                }
            }
        } else {
            let op = OpenOp {
                rooted_at: cwd.clone(),
                path: path.clone(),
                flags: open_flags,
                mode: mode as u16,
                cred: walker_cred,
            };
            match drive(
                op,
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await
            {
                Ok(file) => file,
                Err(v3errno) => {
                    return SyscallResult::error_from(Errno::from(v3errno));
                }
            }
        };
        if want_directory && openfile.rnode().meta().kind() != InodeKind::Directory {
            return SyscallResult::Error(ENOTDIR_VALUE);
        }
        if want_write && openfile.rnode().meta().kind() == InodeKind::Directory {
            return SyscallResult::Error(EISDIR_VALUE);
        }
        maybe_acquire_controlling_tty_on_open(&openfile, flags, want_path_only, ctx);
        let _ = ctx.process.set_fd(fd, Some(openfile));
        if want_cloexec {
            ctx.process.set_fd_cloexec(fd, true);
        }
        return SyscallResult::Return(fd as i64);
    }

    // Resolve or create the target under the full VFS driver. We use the
    // resulting dentry (not the OpenFile) as the truncate-anchor so
    // `fs_ops_for_dentry`'s parent-hint ascend can find the in-scope
    // mount payload (freshly-resolved child rnodes don't carry the
    // mount weak; only mount-root rnodes do, per the walker's
    // `current_fs_ops` discipline).
    use step_engine::DriveMode;
    use tx_scripts::drive;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let create_mode = want_create.then(|| (mode as u16) & !ctx.process.umask() & 0o7777);
    let dentry: Cap<DEntry> = if let Some((binding, mount_namespace)) = mounted_cwd.clone() {
        match drive(
            ResolveOpenTargetInMountNamespaceOp::new(
                binding.dentry,
                binding.mount,
                mount_namespace,
                path.clone(),
                walker_cred.clone(),
                create_mode,
                want_excl,
            ),
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(resolved) => resolved.dentry,
            Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    } else {
        match drive(
            ResolveOpenTargetOp::new(
                cwd.clone(),
                path.clone(),
                walker_cred.clone(),
                create_mode,
                want_excl,
            ),
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(dentry) => dentry,
            Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    };
    let dentry_meta = dentry.rnode().meta();
    if want_directory && dentry_meta.kind() != InodeKind::Directory {
        return SyscallResult::Error(ENOTDIR_VALUE);
    }
    if want_create && dentry_meta.kind() == InodeKind::Directory {
        return SyscallResult::Error(EISDIR_VALUE);
    }
    if want_write && dentry_meta.kind() == InodeKind::Directory {
        return SyscallResult::Error(EISDIR_VALUE);
    }

    // Authorize the terminal object before O_TRUNC mutates it. `step_open`
    // repeats this check when materialising the OpenFile, but the truncate
    // phase intentionally precedes that materialisation.
    {
        let guard = step_engine::guard();
        if let Err(errno) = tx_subsystems::cred::checks::require_open_with_walker_cred(
            &walker_cred,
            &dentry_meta,
            open_flags,
            &guard,
        ) {
            return SyscallResult::error_from(Errno::from(errno));
        }
    }

    // Step 2: O_TRUNC. Apply *before* materialising the OpenFile so
    // any future `step_read` against the resulting fd observes the
    // truncated state. Directories → -EISDIR; backends without
    // truncate support → -ENOSYS.
    if want_trunc {
        if dentry_meta.kind() == InodeKind::Directory {
            return SyscallResult::Error(EISDIR_VALUE);
        }
        if dentry_meta.kind() == InodeKind::CharDevice {
            // Linux treats O_TRUNC on character devices as a no-op.
        } else {
            use tx_subsystems::page_backed::adapter::step_engine::StepOutcome as V3Trunc;
            // Page-backed rnodes must truncate the LIVE PageContainer together
            // with the FS inode (`step_truncate`, the same both-sides path
            // ftruncate takes). Truncating only the FS side leaves a cached
            // `pc` at its stale pre-open size: the write lands at offset 0
            // without shrinking `pc.size_bytes()`, and the close-time
            // `step_fsync` then persists that stale size straight back over
            // the truncate — `echo new > tracked-file` kept the old st_size,
            // so stat-cache-based change detection (git) never saw shell-
            // redirect edits, while readers got the old length padded from
            // the fresh zero page.
            //
            // Only File-kind containers route through `step_truncate`: an
            // Anon-kind pc (tmpfs) gets no `FsPageBacking::truncate` callback
            // from it, which would skip tmpfs's own payload-size update —
            // tmpfs's `truncate` impl already does the pc-level shrink
            // itself, so it stays on the `fs_page_backing` arm below.
            let live_pc = match dentry.rnode().backing() {
                tx_subsystems::vfs::structure::RNodeBacking::PageBacked { pc }
                    if matches!(
                        pc.kind(),
                        tx_subsystems::page_backed::PageContainerKind::File { .. }
                    ) =>
                {
                    Some(pc.clone())
                }
                _ => None,
            };
            if let Some(pc) = live_pc {
                let guard = step_engine::guard();
                match tx_subsystems::page_backed::step_truncate(&pc, 0, &guard) {
                    V3Trunc::Done(()) => {}
                    V3Trunc::Continue { .. } | V3Trunc::Yield { .. } => {
                        return SyscallResult::Error(EIO_VALUE);
                    }
                    V3Trunc::Err(errno) => {
                        return SyscallResult::error_from(Errno::from(errno));
                    }
                }
            } else {
                let fs_page_backing = match fs_page_backing_for_dentry(&dentry) {
                    Some(b) => b,
                    None => return SyscallResult::Error(ENOSYS_VALUE),
                };
                let fs_object_id = dentry.rnode().fs_object_id();
                match drive(
                    TruncateFsObjectOp {
                        page_backing: fs_page_backing,
                        fs_object_id,
                        new_size: 0,
                    },
                    &mut script_ctx,
                    DriveMode::Waiting,
                    mailbox_arc.as_ref(),
                    delegate_registry_arc.as_deref(),
                    timer_registrar_handle.as_ref(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
                }
            }
        }
    }

    // Step 3: materialise the OpenFile cap by re-running step_open
    // against the path. step_open consumes step_walk internally + adds
    // the terminal-component R/W permission check (cite:
    // `txdoc:VFS-CHECKS-PERMISSIONS-1`). Errors here surface the DAC
    // EACCES the walker guards against; we cannot bypass it because
    // the walker-side check_open_perm runs against the inode's mode
    // bits, and tmpfs's create_inode honoured those bits at create
    // time, so the perms apply equally to the just-created file.
    //
    let openfile: Cap<OpenFile> = if let Some((binding, mount_namespace)) = mounted_cwd {
        if want_nofollow {
            match drive(
                OpenNoFollowInMountNamespaceOp {
                    rooted_at: binding.dentry,
                    origin_mount: binding.mount,
                    mount_namespace,
                    path: path.clone(),
                    flags: open_flags,
                    mode: mode as u16,
                    cred: walker_cred.clone(),
                },
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await
            {
                Ok(opened) => opened.open_file,
                Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
            }
        } else {
            match drive(
                OpenInMountNamespaceOp {
                    rooted_at: binding.dentry,
                    origin_mount: binding.mount,
                    mount_namespace,
                    path: path.clone(),
                    flags: open_flags,
                    mode: mode as u16,
                    cred: walker_cred.clone(),
                },
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await
            {
                Ok(opened) => opened.open_file,
                Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
            }
        }
    } else if want_nofollow {
        match drive(
            OpenNoFollowOp {
                rooted_at: cwd.clone(),
                path: path.clone(),
                flags: open_flags,
                mode: mode as u16,
                cred: walker_cred.clone(),
            },
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(file) => file,
            Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    } else {
        match drive(
            OpenOp {
                rooted_at: cwd.clone(),
                path: path.clone(),
                flags: open_flags,
                mode: mode as u16,
                cred: walker_cred.clone(),
            },
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(file) => file,
            Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    };

    // Step 4: install at the lowest unused fd ≥ 0. Per fd-ops Wave 1
    // the fd table is a sparse `BTreeMap<u32, Cap<OpenFile>>`;
    // `allocate_fd()` scans for the lowest unused key.
    let fd = match allocate_fd_under_limit(ctx) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    maybe_acquire_controlling_tty_on_open(&openfile, flags, want_path_only, ctx);
    let _ = ctx.process.set_fd(fd, Some(openfile));
    if want_cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }

    SyscallResult::Return(fd as i64)
}

/// `close(fd)`. Linux RV64 generic ABI `__NR_close = 57`.
///
/// Removes the `Cap<OpenFile>` from the fd table; EBR-deferred
/// reclamation drops the cap (the `OpenFile`'s `Drop` body — TTY
/// reference releases, page-backing teardown — fires there).
/// Also clears the cloexec bit so future `fcntl(F_GETFD)` against the
/// same fd number reports a clean state if it gets reused.
///
/// Returns `0` on success, `-EBADF` if the fd was already closed.
/// PR-3 migration: `CloseOp` is a `OneShotStepOp` — dispatched via
/// `drive_oneshot` (no reactor, no yield).
pub(super) fn sys_close<'a>(fd: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = CloseOp {
        process: ctx.process.clone(),
        fd,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(file) => {
            // Close is not a durability fence. It only admits the file just
            // removed from the fd table to L4; fsync/fdatasync own the ordered
            // journal commit.
            queue_file_close_writeback(&file);
            file.flock_release();
            fcntl_release_process_locks_for_file(ctx.process.pid.0, &file);
            maybe_close_socket_file_after_fd_remove(&file);
            SyscallResult::Return(0)
        }
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `dup(oldfd)`. Linux RV64 generic ABI `__NR_dup = 23`.
///
/// Returns the lowest unused fd ≥ 0 referring to the same `OpenFile`
/// as `oldfd`. The new fd's cloexec bit is **clear** per POSIX —
/// `dup` never inherits the cloexec disposition; only
/// `dup3(.., O_CLOEXEC)` sets it. The underlying `OpenFile` is shared
/// (we clone the `Cap<OpenFile>`); both fds reference the same
/// epoch-managed identity.
pub(super) fn sys_dup<'a>(oldfd: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    if ctx.process.fd(oldfd).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if let Err(err) = allocate_fd_under_limit(ctx) {
        return err;
    }
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = DupOp {
        process: ctx.process.clone(),
        oldfd,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(newfd) => SyscallResult::Return(newfd as i64),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `dup3(oldfd, newfd, flags)`. Linux RV64 generic ABI
/// `__NR_dup3 = 24`.
///
/// The atomic-replace form: any existing `newfd` is silently closed
/// and `newfd` is bound to the same `OpenFile` as `oldfd`. Returns
/// `newfd` on success.
///
/// - `oldfd == newfd` → `-EINVAL` per Linux (musl's `dup2` shim emits
///   `dup3(oldfd, newfd, 0)`; the legacy `dup2` no-op-on-same-fd
///   shape does not apply here).
/// - `flags & ~O_CLOEXEC != 0` → `-EINVAL` (only `O_CLOEXEC` is
///   defined for this argument).
/// - `oldfd` not open → `-EBADF`.
/// - Otherwise: `install_fd(newfd, file.clone())` — the previous
///   occupant cap is dropped immediately (its EBR-deferred `Drop`
///   fires once the next epoch reclamation runs).
pub(super) fn sys_dup3<'a>(
    oldfd: u32,
    newfd: u32,
    flags: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if newfd >= soft_limit {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = Dup3Op {
        process: ctx.process.clone(),
        oldfd,
        newfd,
        flags,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(fd) => SyscallResult::Return(fd as i64),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `pipe2(int pipefd[2], int flags)`. Linux RV64 generic ABI
/// `__NR_pipe2 = 59`.
///
/// fd-ops Wave 3. Builds a (reader, writer) pair via
/// `tx_subsystems::pipe::step_pipe2`, allocates two fds via
/// `process.allocate_fd()`, installs both, and writes the pair back
/// to userspace at `pipefd_uaddr` as `[u32; 2]` little-endian.
///
/// Recognised flag bits: `O_CLOEXEC` (sets cloexec on both fds) and
/// `O_NONBLOCK` (sets the per-OpenFile nonblocking flag, threaded
/// through `OpenFileFlags.nonblocking` so reader/writer-side
/// `step_read`/`step_write` short-circuit `Blocked` to `EAGAIN`).
/// `O_DIRECT` creates a packet-mode pipe. Any other bits return
/// `-EINVAL`.
///
/// Userspace writeback: the `pipefd_uaddr` flows through
/// `bootstrap_write_user::<[u32; 2]>` (canonical `aspace.write_user`
/// lane with kernel-pointer fallback for test scaffolding).
pub(super) fn sys_pipe2<'a>(pipefd_uaddr: u64, flags: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    // Validate flags. Recognised: O_CLOEXEC | O_NONBLOCK | O_DIRECT.
    let recognised = O_CLOEXEC | O_NONBLOCK | O_DIRECT;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let pipe_flags = tx_subsystems::pipe::PipeFlags {
        cloexec: flags & O_CLOEXEC != 0,
        nonblocking: flags & O_NONBLOCK != 0,
        packet: flags & O_DIRECT != 0,
    };
    // PR-9 phase 3b: drive `step_pipe2` via the `Pipe2Op` StepOp
    // wrap, threading a `&mut KernelScriptCtx`.
    //
    // PR-9 phase 5 (D5 Path A): populate `SubjectContext` from
    // `SyscallCtx`. SUBJ-1 hygiene — even arms whose step body does
    // not (yet) read authority receive the same context shape so
    // future authority-bearing arms compose. Restrictions cap is a
    // fresh placeholder until PR-K (D5 §7).
    use tx_subsystems::pipe::Pipe2Op;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let (reader_cap, writer_cap) = {
        let mut op = Pipe2Op { flags: pipe_flags };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(pair) => pair,
            Err(v3errno) => {
                return SyscallResult::error_from(Errno::from(v3errno));
            }
        }
    };

    // Install at the lowest two unused fds. `allocate_fd()` returns
    // the lowest unused slot; install_fd() commits. Allocate the
    // reader first so on a fresh process it lands at 0 and the
    // writer at 1, matching Linux's user-visible (3, 4) pattern
    // post-stdin/out/err.
    let reader_fd = match allocate_fd_under_limit(ctx) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let _ = ctx.process.install_fd(reader_fd, reader_cap);
    let writer_fd = match allocate_fd_under_limit(ctx) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = ctx.process.set_fd(reader_fd, None);
            return err;
        }
    };
    let _ = ctx.process.install_fd(writer_fd, writer_cap);

    if pipe_flags.cloexec {
        ctx.process.set_fd_cloexec(reader_fd, true);
        ctx.process.set_fd_cloexec(writer_fd, true);
    }

    // Write the (reader_fd, writer_fd) pair back to userspace as the
    // Linux ABI's two adjacent little-endian `int` slots. Keep this
    // byte-explicit instead of relying on a typed `[u32; 2]` write so
    // fd publication is independent of Rust aggregate layout details.
    let mut pipefd_bytes = [0u8; 8];
    pipefd_bytes[0..4].copy_from_slice(&reader_fd.to_le_bytes());
    pipefd_bytes[4..8].copy_from_slice(&writer_fd.to_le_bytes());
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, pipefd_uaddr, &pipefd_bytes) {
        return SyscallResult::error_from(errno);
    }

    SyscallResult::Return(0)
}

/// `lseek(fd, offset, whence)`. Linux RV64 generic ABI
/// `__NR_lseek = 62`.
///
/// fd-ops Wave 4. Resolves `fd` against the per-process fd-table
/// `BTreeMap<u32, Cap<OpenFile>>` (fd-ops Wave 1) and dispatches
/// through `OpenFile::step_lseek`, which handles the
/// SEEK_SET/SEEK_CUR/SEEK_END whence cases plus the non-seekable-
/// backing → `-ESPIPE` short-circuit.
///
/// `lseek` is a non-blocking step — `Blocked`/`AdvancedThenBlocked`
/// outcomes are unreachable from `OpenFile::step_lseek`, but the
/// match below maps them to `-EIO` for symmetry with the other fd
/// arms (the alternative would be a panic which makes the syscall
/// surface fragile).
pub(super) fn sys_lseek<'a>(
    fd: u32,
    offset: i64,
    whence: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let file = match resolve_fd(&ctx.process, fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = tx_subsystems::vfs::OpenFileLseekOp {
        file: &file,
        offset,
        whence,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(new_offset) => SyscallResult::Return(new_offset as i64),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

// =====================================================================
// Slice 5 of the shell-prompt roadmap — `ioctl(2)` + TTY routing.
//
// Eight TTY ioctl arms are implemented:
//
// - TCGETS / TCSETS / TCSETSW / TCSETSF — termios get/set. The W/F
//   variants currently alias to `step_ioctl_tcsets` (drain semantics
//   are not yet implemented).
// - TIOCGPGRP / TIOCSPGRP — foreground process-group id get/set.
// - TIOCGWINSZ / TIOCSWINSZ — window size get/set.
// - TIOCSCTTY / TIOCNOTTY — controlling-terminal acquire/release.
//
// Non-TTY fds (pipes, regular files, dirs, etc.) and unknown request
// codes return `-ENOTTY` per Linux's `man ioctl_tty` (some systems use
// `ENOSYS` for unknown ioctls; ENOTTY is the standard for the
// terminal-shape ioctls — POSIX `tcgetattr(3)` documents ENOTTY as the
// "fd is not a terminal" return).
//
// User-VA discipline: each ioctl arm routes its `argp` read or write
// through `bootstrap_read_user::<T>` / `bootstrap_write_user::<T>`,
// which delegate to the canonical `aspace.read_user` /
// `aspace.write_user` lane (with a kernel-pointer fallback for test
// scaffolding). A null `argp` short-circuits to `-EFAULT`.
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 5.
// =====================================================================

/// Build an [`IoctlCaller`] from `ctx.process` for a session-control
/// ioctl (TIOCSCTTY / TIOCNOTTY / TIOCSPGRP). Pulls the caller's
/// session id and process-group id from the process's `pgrp_cap()` and
/// derives the session-leader bit from `pid == sid` (per
/// `PROCESS_v1` §2.4 the session leader's pid equals the session id).
///
/// `has_controlling_tty` is set from the session's
/// `has_controlling_tty()` accessor; this is consumed by
/// `require_session_leader` for TIOCSCTTY (the check rejects callers
/// who already have a controlling TTY).
pub(super) fn make_ioctl_caller(ctx: &SyscallCtx<'_>) -> IoctlCaller {
    let pgrp = ctx.process.pgrp_cap();
    let pgid = pgrp.pgid.0;
    let session = pgrp.session_cap();
    let sid = session.sid.0;
    let pid = ctx.process.pid.0;
    let mut caller = IoctlCaller::new(sid, pgid);
    caller = caller.with_pgrp_weak(pgrp.downgrade());
    if pid == sid {
        caller = caller.as_session_leader();
    }
    if session.has_controlling_tty() {
        caller = caller.with_controlling_tty();
    }
    caller
}

/// `ioctl(fd, request, argp)`. Linux RV64 generic syscall #29.
///
/// Decodes `request` against the eight TTY ioctls v1 supports and
/// dispatches to the matching `tty::execution::step_ioctl_*` helper.
/// Non-TTY fds and unknown request codes return `-ENOTTY`.
///
/// All eight TTY step functions are non-blocking (they operate on
/// `AtomicSlot` / `SpinMutex` state inside the TTY identity), so the
/// arm itself is non-async; `Blocked` / `AdvancedThenBlocked` outcomes
/// are unreachable in practice and surface as `-EIO` for symmetry with
/// the other fd arms.
pub(super) fn sys_ioctl<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let fd = args[0] as i32;
    let request = args[1] as u32;
    let argp = args[2];

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // PR-10 phase 2: route userfaultfd-shape ioctls before the
    // VFS/TTY discriminator. The `OpenFile::rnode()` accessor panics
    // for `OpenFileBacking::Ufd`, so any ufd-shape ioctl must be
    // handled (or short-circuited with `-ENOTTY`/`-EINVAL`) before
    // we reach the TTY-shaped match below.
    if file.ufd().is_some() {
        // All `UFFDIO_*` numbers fit in u32 per the
        // `_IOWR(0xAA, _, _)` encoding; dispatch on the request word.
        return match request {
            super::numbers::UFFDIO_API => super::userfaultfd::step_uffdio_api(&file, argp, ctx),
            super::numbers::UFFDIO_REGISTER => {
                super::userfaultfd::step_uffdio_register(&file, argp, ctx)
            }
            super::numbers::UFFDIO_COPY => super::userfaultfd::step_uffdio_copy(&file, argp, ctx),
            super::numbers::UFFDIO_ZEROPAGE => {
                super::userfaultfd::step_uffdio_zeropage(&file, argp, ctx)
            }
            super::numbers::UFFDIO_CONTINUE => {
                super::userfaultfd::step_uffdio_continue(&file, argp, ctx)
            }
            _ => SyscallResult::error_from(Errno::EINVAL),
        };
    }

    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return SyscallResult::error_from(Errno::ENOTTY);
    };

    const BLKGETSIZE64: u32 = 0x8008_1272;
    if request == BLKGETSIZE64 && rnode.meta().kind() == InodeKind::BlockDevice {
        let Some(reg) = tx_fs::bdevfs::block_device_for_object_id(rnode.fs_object_id()) else {
            return SyscallResult::error_from(Errno::ENOTTY);
        };
        let bytes = reg
            .ops
            .total_blocks()
            .saturating_mul(reg.ops.block_size() as u64);
        return match bootstrap_write_user::<u64>(&ctx.aspace, argp, bytes) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::error_from(errno),
        };
    }

    if let RNodeBacking::StructBacked { payload } = rnode.backing() {
        match payload {
            StructPayload::Socket { .. } => {
                return sys_socket_ioctl(request, argp, ctx);
            }
            StructPayload::CharDevice(binding) => {
                if request == RTC_ALM_READ {
                    let Some(rtc_ops) = binding.ops.rtc_ops() else {
                        return SyscallResult::error_from(Errno::ENOTTY);
                    };
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    let guard = step_engine::guard();
                    let alarm = match rtc_ops.read_alarm(&guard) {
                        Ok(alarm) => alarm,
                        Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
                    };
                    return match bootstrap_write_user::<RtcTime>(&ctx.aspace, argp, alarm.time) {
                        Ok(()) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::error_from(errno),
                    };
                }
                if request == RTC_ALM_SET {
                    let Some(rtc_ops) = binding.ops.rtc_ops() else {
                        return SyscallResult::error_from(Errno::ENOTTY);
                    };
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    let rtc_time = match bootstrap_read_user::<RtcTime>(&ctx.aspace, argp) {
                        Ok(time) => time,
                        Err(errno) => return SyscallResult::error_from(errno),
                    };
                    let guard = step_engine::guard();
                    let alarm = tx_subsystems::device::RtcAlarm {
                        time: rtc_time,
                        enabled: true,
                        pending: false,
                    };
                    let emulation_registrar = ctx.timer_registrar.as_ref().cloned();
                    let emulation = match (emulation_registrar.as_ref(), rtc_time.to_unix_ns()) {
                        (Some(registrar), Ok(realtime_ns)) => Some(RtcAlarmEmulation::new(
                            registrar,
                            timekeeper().monotonic_deadline_from_realtime_ns(realtime_ns),
                        )),
                        _ => None,
                    };
                    return match rtc_ops.set_alarm_with_emulation(alarm, &guard, emulation) {
                        Ok(()) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::error_from(Errno::from(errno)),
                    };
                }
                if request == RTC_RD_TIME {
                    let Some(rtc_ops) = binding.ops.rtc_ops() else {
                        return SyscallResult::error_from(Errno::ENOTTY);
                    };
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    let guard = step_engine::guard();
                    let rtc_time = match rtc_ops.read_time(&guard) {
                        Ok(time) => time,
                        Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
                    };
                    return match bootstrap_write_user::<RtcTime>(&ctx.aspace, argp, rtc_time) {
                        Ok(()) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::error_from(errno),
                    };
                }
                if request == RTC_SET_TIME {
                    let Some(rtc_ops) = binding.ops.rtc_ops() else {
                        return SyscallResult::error_from(Errno::ENOTTY);
                    };
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    let rtc_time = match bootstrap_read_user::<RtcTime>(&ctx.aspace, argp) {
                        Ok(time) => time,
                        Err(errno) => return SyscallResult::error_from(errno),
                    };
                    let guard = step_engine::guard();
                    return match rtc_ops.set_time(rtc_time, &guard) {
                        Ok(()) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::error_from(Errno::from(errno)),
                    };
                }
                return SyscallResult::error_from(Errno::ENOTTY);
            }
            _ => {}
        }
    }

    // Resolve to a TTY. Non-TTY fds → -ENOTTY for terminal-shape ioctls
    // (Linux semantic — even pipes / regular files return ENOTTY for
    // these requests, per `man ioctl_tty`).
    let tty = match rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        _ => return SyscallResult::error_from(Errno::ENOTTY),
    };

    let mut script_ctx = build_subject_script_ctx(ctx);
    macro_rules! drive_tty_oneshot {
        ($op:expr) => {{
            let mut op = $op;
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(value) => value,
                Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
            }
        }};
    }

    match request {
        TCGETS => {
            let termios = drive_tty_oneshot!(IoctlTcgetsOp { tty: &tty });
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            if let Err(errno) = bootstrap_write_user::<Termios>(&ctx.aspace, argp, termios) {
                return SyscallResult::error_from(errno);
            }
            SyscallResult::Return(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            // TCSETSW (drain output queue) and TCSETSF (drain output +
            // flush input) currently alias to TCSETS — the drain/flush
            // semantics aren't implemented yet. Treating all three as
            // immediate-install matches Linux's behaviour for an empty
            // output queue.
            let new_termios: Termios = match bootstrap_read_user::<Termios>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            let _ = drive_tty_oneshot!(IoctlTcsetsOp {
                tty: &tty,
                new_termios,
            });
            SyscallResult::Return(0)
        }
        TIOCGPGRP => {
            let pgid = drive_tty_oneshot!(IoctlTiocgpgrpOp { tty: &tty });
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, argp, pgid) {
                return SyscallResult::error_from(errno);
            }
            SyscallResult::Return(0)
        }
        TIOCSPGRP => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let new_pgrp: u32 = match bootstrap_read_user::<u32>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            let Some(new_pgrp_cap) = tx_subsystems::process::process_group_by_pgid(Pgid(new_pgrp))
            else {
                return SyscallResult::Error(EINVAL_VALUE);
            };
            let _ = drive_tty_oneshot!(IoctlTiocspgrpForProcessOp {
                tty: &tty,
                caller: &ctx.process,
                new_pgrp: &new_pgrp_cap,
            });
            SyscallResult::Return(0)
        }
        TIOCGWINSZ => {
            let winsize = drive_tty_oneshot!(IoctlTiocgwinszOp { tty: &tty });
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            if let Err(errno) = bootstrap_write_user::<Winsize>(&ctx.aspace, argp, winsize) {
                return SyscallResult::error_from(errno);
            }
            SyscallResult::Return(0)
        }
        TIOCSWINSZ => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let ws: Winsize = match bootstrap_read_user::<Winsize>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            let _ = drive_tty_oneshot!(IoctlTiocswinszOp {
                tty: &tty,
                winsize: ws
            });
            SyscallResult::Return(0)
        }
        TIOCSCTTY => {
            // The `argp` for TIOCSCTTY is a "force" bit (0 or 1) on
            // Linux, used to steal the TTY from another session when
            // the caller is root. v1 ignores it — the underlying step
            // rejects already-bound TTYs with -EBUSY regardless.
            // Use the process-aware variant so session.controlling_tty
            // is updated; the legacy step_ioctl_tiocsctty only binds
            // the session_pgrp field and leaves has_controlling_tty()
            // false, which breaks subsequent TIOCGPGRP calls.
            let _ = drive_tty_oneshot!(IoctlTiocscttyForProcessOp {
                tty: &tty,
                caller: &ctx.process,
            });
            SyscallResult::Return(0)
        }
        TIOCNOTTY => {
            let caller = make_ioctl_caller(ctx);
            let _ = drive_tty_oneshot!(IoctlTiocnottyOp { tty: &tty, caller });
            SyscallResult::Return(0)
        }
        // Unknown ioctl request → -ENOTTY (the POSIX `man ioctl_tty`
        // semantic). musl's `isatty(3)` resolves to TCGETS so it never
        // hits this arm, but other libc paths (or buggy userspace)
        // observing -ENOTTY here is the canonical Linux signal that
        // the request is not a terminal ioctl on this fd.
        _ => SyscallResult::error_from(Errno::ENOTTY),
    }
}

const RTC_RD_TIME: u32 = 0x8024_7009;
const RTC_SET_TIME: u32 = 0x4024_700a;
const RTC_ALM_READ: u32 = 0x8024_7008;
const RTC_ALM_SET: u32 = 0x4024_7007;

fn sys_socket_ioctl<'a>(request: u32, argp: u64, ctx: &SyscallCtx<'a>) -> SyscallResult {
    super::socket::sys_socket_ioctl(request, argp, ctx)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct StatLayout {
    pub(super) st_dev: u64,
    pub(super) st_ino: u64,
    pub(super) st_mode: u32,
    pub(super) st_nlink: u32,
    pub(super) st_uid: u32,
    pub(super) st_gid: u32,
    pub(super) st_rdev: u64,
    __pad1: u64,
    pub(super) st_size: i64,
    pub(super) st_blksize: i32,
    __pad2: i32,
    pub(super) st_blocks: i64,
    pub(super) st_atime_sec: i64,
    pub(super) st_atime_nsec: u64,
    pub(super) st_mtime_sec: i64,
    pub(super) st_mtime_nsec: u64,
    pub(super) st_ctime_sec: i64,
    pub(super) st_ctime_nsec: u64,
    __unused: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    __reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StatxLayout {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    __spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTimestamp,
    stx_btime: StatxTimestamp,
    stx_ctime: StatxTimestamp,
    stx_mtime: StatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    stx_dio_mem_align: u32,
    stx_dio_offset_align: u32,
    __spare3: [u64; 12],
}

const _: () = assert!(core::mem::size_of::<StatxTimestamp>() == 16);
const _: () = assert!(core::mem::size_of::<StatxLayout>() == 256);

#[repr(C)]
#[derive(Clone, Copy)]
struct StatfsLayout {
    f_type: u64,
    f_bsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [i32; 2],
    f_namelen: u64,
    f_frsize: u64,
    f_flags: u64,
    f_spare: [u64; 4],
}

const _: () = assert!(core::mem::size_of::<StatfsLayout>() == 120);

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxDirent64Header {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
}

pub(super) mod layout_descriptors {
    use core::mem::{align_of, offset_of, size_of};

    pub(super) use super::{
        LinuxDirent64Header, StatLayout, StatfsLayout, StatxLayout, StatxTimestamp,
    };
    use crate::linux_syscall::{KernelToUserLayout, KernelUserField, KernelUserLayout};

    impl KernelToUserLayout for StatLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "StatLayout",
            musl_header: "sys/stat.h",
            musl_type: "struct stat",
            size: size_of::<StatLayout>(),
            align: align_of::<StatLayout>(),
            fields: &[
                KernelUserField {
                    rust: "st_dev",
                    musl: "st_dev",
                    offset: offset_of!(StatLayout, st_dev),
                },
                KernelUserField {
                    rust: "st_ino",
                    musl: "st_ino",
                    offset: offset_of!(StatLayout, st_ino),
                },
                KernelUserField {
                    rust: "st_mode",
                    musl: "st_mode",
                    offset: offset_of!(StatLayout, st_mode),
                },
                KernelUserField {
                    rust: "st_nlink",
                    musl: "st_nlink",
                    offset: offset_of!(StatLayout, st_nlink),
                },
                KernelUserField {
                    rust: "st_uid",
                    musl: "st_uid",
                    offset: offset_of!(StatLayout, st_uid),
                },
                KernelUserField {
                    rust: "st_gid",
                    musl: "st_gid",
                    offset: offset_of!(StatLayout, st_gid),
                },
                KernelUserField {
                    rust: "st_rdev",
                    musl: "st_rdev",
                    offset: offset_of!(StatLayout, st_rdev),
                },
                KernelUserField {
                    rust: "st_size",
                    musl: "st_size",
                    offset: offset_of!(StatLayout, st_size),
                },
                KernelUserField {
                    rust: "st_blksize",
                    musl: "st_blksize",
                    offset: offset_of!(StatLayout, st_blksize),
                },
                KernelUserField {
                    rust: "st_blocks",
                    musl: "st_blocks",
                    offset: offset_of!(StatLayout, st_blocks),
                },
                KernelUserField {
                    rust: "st_atime_sec",
                    musl: "st_atim.tv_sec",
                    offset: offset_of!(StatLayout, st_atime_sec),
                },
                KernelUserField {
                    rust: "st_atime_nsec",
                    musl: "st_atim.tv_nsec",
                    offset: offset_of!(StatLayout, st_atime_nsec),
                },
                KernelUserField {
                    rust: "st_mtime_sec",
                    musl: "st_mtim.tv_sec",
                    offset: offset_of!(StatLayout, st_mtime_sec),
                },
                KernelUserField {
                    rust: "st_mtime_nsec",
                    musl: "st_mtim.tv_nsec",
                    offset: offset_of!(StatLayout, st_mtime_nsec),
                },
                KernelUserField {
                    rust: "st_ctime_sec",
                    musl: "st_ctim.tv_sec",
                    offset: offset_of!(StatLayout, st_ctime_sec),
                },
                KernelUserField {
                    rust: "st_ctime_nsec",
                    musl: "st_ctim.tv_nsec",
                    offset: offset_of!(StatLayout, st_ctime_nsec),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const STAT_LAYOUT: KernelUserLayout =
        <StatLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for StatxTimestamp {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "StatxTimestamp",
            musl_header: "sys/stat.h",
            musl_type: "struct statx_timestamp",
            size: size_of::<StatxTimestamp>(),
            align: align_of::<StatxTimestamp>(),
            fields: &[
                KernelUserField {
                    rust: "tv_sec",
                    musl: "tv_sec",
                    offset: offset_of!(StatxTimestamp, tv_sec),
                },
                KernelUserField {
                    rust: "tv_nsec",
                    musl: "tv_nsec",
                    offset: offset_of!(StatxTimestamp, tv_nsec),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const STATX_TIMESTAMP_LAYOUT: KernelUserLayout =
        <StatxTimestamp as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for StatxLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "StatxLayout",
            musl_header: "sys/stat.h",
            musl_type: "struct statx",
            size: size_of::<StatxLayout>(),
            align: align_of::<StatxLayout>(),
            fields: &[
                KernelUserField {
                    rust: "stx_mask",
                    musl: "stx_mask",
                    offset: offset_of!(StatxLayout, stx_mask),
                },
                KernelUserField {
                    rust: "stx_blksize",
                    musl: "stx_blksize",
                    offset: offset_of!(StatxLayout, stx_blksize),
                },
                KernelUserField {
                    rust: "stx_attributes",
                    musl: "stx_attributes",
                    offset: offset_of!(StatxLayout, stx_attributes),
                },
                KernelUserField {
                    rust: "stx_nlink",
                    musl: "stx_nlink",
                    offset: offset_of!(StatxLayout, stx_nlink),
                },
                KernelUserField {
                    rust: "stx_uid",
                    musl: "stx_uid",
                    offset: offset_of!(StatxLayout, stx_uid),
                },
                KernelUserField {
                    rust: "stx_gid",
                    musl: "stx_gid",
                    offset: offset_of!(StatxLayout, stx_gid),
                },
                KernelUserField {
                    rust: "stx_mode",
                    musl: "stx_mode",
                    offset: offset_of!(StatxLayout, stx_mode),
                },
                KernelUserField {
                    rust: "stx_ino",
                    musl: "stx_ino",
                    offset: offset_of!(StatxLayout, stx_ino),
                },
                KernelUserField {
                    rust: "stx_size",
                    musl: "stx_size",
                    offset: offset_of!(StatxLayout, stx_size),
                },
                KernelUserField {
                    rust: "stx_blocks",
                    musl: "stx_blocks",
                    offset: offset_of!(StatxLayout, stx_blocks),
                },
                KernelUserField {
                    rust: "stx_attributes_mask",
                    musl: "stx_attributes_mask",
                    offset: offset_of!(StatxLayout, stx_attributes_mask),
                },
                KernelUserField {
                    rust: "stx_atime",
                    musl: "stx_atime",
                    offset: offset_of!(StatxLayout, stx_atime),
                },
                KernelUserField {
                    rust: "stx_btime",
                    musl: "stx_btime",
                    offset: offset_of!(StatxLayout, stx_btime),
                },
                KernelUserField {
                    rust: "stx_ctime",
                    musl: "stx_ctime",
                    offset: offset_of!(StatxLayout, stx_ctime),
                },
                KernelUserField {
                    rust: "stx_mtime",
                    musl: "stx_mtime",
                    offset: offset_of!(StatxLayout, stx_mtime),
                },
                KernelUserField {
                    rust: "stx_rdev_major",
                    musl: "stx_rdev_major",
                    offset: offset_of!(StatxLayout, stx_rdev_major),
                },
                KernelUserField {
                    rust: "stx_rdev_minor",
                    musl: "stx_rdev_minor",
                    offset: offset_of!(StatxLayout, stx_rdev_minor),
                },
                KernelUserField {
                    rust: "stx_dev_major",
                    musl: "stx_dev_major",
                    offset: offset_of!(StatxLayout, stx_dev_major),
                },
                KernelUserField {
                    rust: "stx_dev_minor",
                    musl: "stx_dev_minor",
                    offset: offset_of!(StatxLayout, stx_dev_minor),
                },
                KernelUserField {
                    rust: "stx_mnt_id",
                    musl: "stx_mnt_id",
                    offset: offset_of!(StatxLayout, stx_mnt_id),
                },
                KernelUserField {
                    rust: "stx_dio_mem_align",
                    musl: "stx_dio_mem_align",
                    offset: offset_of!(StatxLayout, stx_dio_mem_align),
                },
                KernelUserField {
                    rust: "stx_dio_offset_align",
                    musl: "stx_dio_offset_align",
                    offset: offset_of!(StatxLayout, stx_dio_offset_align),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const STATX_LAYOUT: KernelUserLayout =
        <StatxLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for StatfsLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "StatfsLayout",
            musl_header: "sys/statfs.h",
            musl_type: "struct statfs",
            size: size_of::<StatfsLayout>(),
            align: align_of::<StatfsLayout>(),
            fields: &[
                KernelUserField {
                    rust: "f_type",
                    musl: "f_type",
                    offset: offset_of!(StatfsLayout, f_type),
                },
                KernelUserField {
                    rust: "f_bsize",
                    musl: "f_bsize",
                    offset: offset_of!(StatfsLayout, f_bsize),
                },
                KernelUserField {
                    rust: "f_blocks",
                    musl: "f_blocks",
                    offset: offset_of!(StatfsLayout, f_blocks),
                },
                KernelUserField {
                    rust: "f_bfree",
                    musl: "f_bfree",
                    offset: offset_of!(StatfsLayout, f_bfree),
                },
                KernelUserField {
                    rust: "f_bavail",
                    musl: "f_bavail",
                    offset: offset_of!(StatfsLayout, f_bavail),
                },
                KernelUserField {
                    rust: "f_files",
                    musl: "f_files",
                    offset: offset_of!(StatfsLayout, f_files),
                },
                KernelUserField {
                    rust: "f_ffree",
                    musl: "f_ffree",
                    offset: offset_of!(StatfsLayout, f_ffree),
                },
                KernelUserField {
                    rust: "f_fsid",
                    musl: "f_fsid",
                    offset: offset_of!(StatfsLayout, f_fsid),
                },
                KernelUserField {
                    rust: "f_namelen",
                    musl: "f_namelen",
                    offset: offset_of!(StatfsLayout, f_namelen),
                },
                KernelUserField {
                    rust: "f_frsize",
                    musl: "f_frsize",
                    offset: offset_of!(StatfsLayout, f_frsize),
                },
                KernelUserField {
                    rust: "f_flags",
                    musl: "f_flags",
                    offset: offset_of!(StatfsLayout, f_flags),
                },
                KernelUserField {
                    rust: "f_spare",
                    musl: "f_spare",
                    offset: offset_of!(StatfsLayout, f_spare),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const STATFS_LAYOUT: KernelUserLayout =
        <StatfsLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for LinuxDirent64Header {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "LinuxDirent64Header",
            musl_header: "dirent.h",
            musl_type: "struct dirent",
            size: size_of::<LinuxDirent64Header>(),
            align: align_of::<LinuxDirent64Header>(),
            fields: &[
                KernelUserField {
                    rust: "d_ino",
                    musl: "d_ino",
                    offset: offset_of!(LinuxDirent64Header, d_ino),
                },
                KernelUserField {
                    rust: "d_off",
                    musl: "d_off",
                    offset: offset_of!(LinuxDirent64Header, d_off),
                },
                KernelUserField {
                    rust: "d_reclen",
                    musl: "d_reclen",
                    offset: offset_of!(LinuxDirent64Header, d_reclen),
                },
                KernelUserField {
                    rust: "d_type",
                    musl: "d_type",
                    offset: offset_of!(LinuxDirent64Header, d_type),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const LINUX_DIRENT64_HEADER_LAYOUT: KernelUserLayout =
        <LinuxDirent64Header as KernelToUserLayout>::LAYOUT;
}

/// Map an `InodeMeta` + (`fs_object_id`, `rdev`) pair onto the Linux
/// `struct stat` byte image. Single-device kernel today
/// (`st_dev = 0`); `st_blksize = 4096` is the universal page size on
/// the platforms txKernel supports. `rdev` is `0` for non-device
/// inodes; future device-fs work can plumb the major/minor encoding
/// through this argument.
pub(super) fn inode_meta_to_stat(meta: &InodeMeta, ino: u64, rdev: u64) -> StatLayout {
    StatLayout {
        st_dev: 0,
        st_ino: ino,
        st_mode: meta.mode as u32,
        st_nlink: meta.nlinks,
        st_uid: meta.uid,
        st_gid: meta.gid,
        st_rdev: rdev,
        __pad1: 0,
        st_size: meta.size as i64,
        st_blksize: STAT_BLKSIZE,
        __pad2: 0,
        st_blocks: meta.blocks as i64,
        st_atime_sec: meta.atime.sec,
        st_atime_nsec: meta.atime.nsec as u64,
        st_mtime_sec: meta.mtime.sec,
        st_mtime_nsec: meta.mtime.nsec as u64,
        st_ctime_sec: meta.ctime.sec,
        st_ctime_nsec: meta.ctime.nsec as u64,
        __unused: [0, 0],
    }
}

fn linux_encode_dev_t(major: u32, minor: u32) -> u64 {
    ((minor & 0xff) as u64)
        | (((major & 0xfff) as u64) << 8)
        | (((minor & !0xff) as u64) << 12)
        | (((major & !0xfff) as u64) << 32)
}

/// Raw (major, minor) of an open file's device backing, or (0, 0) for
/// non-device files. The `statx` ABI carries split major/minor fields, so we
/// expose the pair directly rather than only the packed `dev_t`.
fn rdev_major_minor_for_open_file(file: &Cap<OpenFile>) -> (u32, u32) {
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::CharDevice(binding),
            } => (binding.devt.major(), binding.devt.minor()),
            RNodeBacking::StructBacked {
                payload: StructPayload::BlockDevice(reg),
            } => (reg.devt.major(), reg.devt.minor()),
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            } => tx_subsystems::tty::project::devt_major_minor_for_tty(tty),
            _ => (0, 0),
        },
        _ => (0, 0),
    }
}

fn rdev_major_minor_for_fs_object_id(id: FsObjectId) -> (u32, u32) {
    tx_fs::devfs::devt_for_object_id(id)
        .map(|devt| (devt.major(), devt.minor()))
        .unwrap_or((0, 0))
}

fn stat_info_for_open_file(
    file: &Cap<OpenFile>,
    fallback_ino: u64,
) -> (InodeMeta, FsObjectId, u32, u32) {
    let (rmaj, rmin) = rdev_major_minor_for_open_file(file);
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => (
            stat_meta_for_open_file(file),
            rnode.fs_object_id(),
            rmaj,
            rmin,
        ),
        _ => (
            stat_meta_for_open_file(file),
            FsObjectId::new(fallback_ino),
            rmaj,
            rmin,
        ),
    }
}

fn proc_self_fd_stat_info<'a>(
    ctx: &SyscallCtx<'a>,
    path: &[u8],
) -> Result<Option<(InodeMeta, FsObjectId, u32, u32)>, SyscallResult> {
    let Some(fd_num) = proc_self_fd_number(path) else {
        return Ok(None);
    };
    let file = resolve_fd(&ctx.process, fd_num).ok_or(SyscallResult::Error(ENOENT_VALUE))?;
    Ok(Some(stat_info_for_open_file(&file, fd_num as u64)))
}

fn stat_rdev_for_open_file(file: &Cap<OpenFile>) -> u64 {
    let (major, minor) = rdev_major_minor_for_open_file(file);
    linux_encode_dev_t(major, minor)
}

fn inode_meta_to_statx(
    meta: &InodeMeta,
    ino: u64,
    rdev_major: u32,
    rdev_minor: u32,
) -> StatxLayout {
    let ts = |sec, nsec| StatxTimestamp {
        tv_sec: sec,
        tv_nsec: nsec as u32,
        __reserved: 0,
    };

    StatxLayout {
        stx_mask: numbers::STATX_BASIC_STATS,
        stx_blksize: STAT_BLKSIZE as u32,
        stx_attributes: 0,
        stx_nlink: meta.nlinks,
        stx_uid: meta.uid,
        stx_gid: meta.gid,
        stx_mode: meta.mode,
        __spare0: 0,
        stx_ino: ino,
        stx_size: meta.size,
        stx_blocks: meta.blocks,
        stx_attributes_mask: 0,
        stx_atime: ts(meta.atime.sec, meta.atime.nsec),
        stx_btime: ts(0, 0),
        stx_ctime: ts(meta.ctime.sec, meta.ctime.nsec),
        stx_mtime: ts(meta.mtime.sec, meta.mtime.nsec),
        stx_rdev_major: rdev_major,
        stx_rdev_minor: rdev_minor,
        stx_dev_major: 0,
        stx_dev_minor: 0,
        stx_mnt_id: 0,
        stx_dio_mem_align: 0,
        stx_dio_offset_align: 0,
        __spare3: [0; 12],
    }
}

fn stat_meta_for_open_file(file: &Cap<OpenFile>) -> InodeMeta {
    if let Some(meta) = stat_meta_for_non_vfs_open_file(file) {
        return meta;
    }

    let rnode = file.rnode();
    let fs_object_id = rnode.fs_object_id();
    // Live size resolution: cached `rnode.meta()` is the snapshot at
    // materialisation time and doesn't see in-place writes. For a
    // page-backed regular file the in-memory `PageContainer.size_bytes`
    // is the authoritative live size (`step_write_from_*` calls
    // `pc.grow_size_to` on every write). Fall back to
    // `fs_ops.load_inode_meta` for other rnode kinds, then to the
    // cached meta. Without this, oscomp basic test_mmap/test_munmap
    // print `file len: 0` and crash on the 0-length mmap because
    // tmpfs/ext4's on-disk inode metadata is never refreshed after
    // the page-cache write.
    let mut meta = match fs_ops_for_rnode(rnode) {
        Some(fs_ops) => {
            let guard = step_engine::guard();
            match fs_ops.load_inode_meta(fs_object_id, &guard) {
                StepOutcome::Done(m) => m,
                _ => rnode.meta(),
            }
        }
        None => rnode.meta(),
    };
    if let Some(sz) =
        crate::linux_syscall::vm::extract_page_container(file).map(|pc| pc.size_bytes())
    {
        meta.size = sz;
    }
    apply_stat_meta_override(fs_object_id, &mut meta);
    meta
}

fn stat_meta_for_non_vfs_open_file(file: &OpenFile) -> Option<InodeMeta> {
    match file.backing() {
        OpenFileBacking::Rnode { .. } => None,
        OpenFileBacking::Eventfd { .. }
        | OpenFileBacking::Timerfd { .. }
        | OpenFileBacking::Epoll { .. }
        | OpenFileBacking::SignalFd { .. }
        | OpenFileBacking::Ufd { .. }
        | OpenFileBacking::AioContext { .. }
        | OpenFileBacking::IoUring { .. }
        | OpenFileBacking::PosixMq { .. }
        | OpenFileBacking::Pidfd { .. }
        | OpenFileBacking::KernelObject { .. }
        | OpenFileBacking::MountApi { .. }
        | OpenFileBacking::SocketPair { .. } => Some(InodeMeta {
            mode: 0o600,
            uid: 0,
            gid: 0,
            size: 0,
            atime: tx_subsystems::vfs::structure::Timespec::EPOCH,
            mtime: tx_subsystems::vfs::structure::Timespec::EPOCH,
            ctime: tx_subsystems::vfs::structure::Timespec::EPOCH,
            nlinks: 1,
            blocks: 0,
            flags: 0,
        }),
    }
}

/// `fstat(fd, statbuf)`. Linux RV64 generic ABI `__NR_fstat = 80`.
///
/// Reads `OpenFile.rnode().meta()` for the fd and writes the Linux
/// `struct stat` layout into the user buffer. Synchronous (no walker
/// path; the inode meta is already cached in the rnode).
///
/// - `fd < 0` → `-EBADF`.
/// - Unknown / closed fd → `-EBADF`.
/// - `statbuf == 0` (NULL) → `-EFAULT`.
/// - All other paths return `0` after writing the buffer.
pub(super) fn sys_fstat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let statbuf_uaddr = args[1];

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if statbuf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    let (meta, ino) = match file.backing() {
        OpenFileBacking::Rnode { rnode } => {
            let meta = stat_meta_for_open_file(&file);
            (meta, rnode.fs_object_id().as_u64())
        }
        _ => (stat_meta_for_open_file(&file), fd as u64),
    };
    let stat = inode_meta_to_stat(&meta, ino, stat_rdev_for_open_file(&file));

    if let Err(errno) = bootstrap_write_user::<StatLayout>(&ctx.aspace, statbuf_uaddr, stat) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

/// `fchdir(fd)`. Linux RV64 ABI `__NR_fchdir = 50`.
pub(super) async fn sys_fchdir<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let dentry = match open_file.opendir_dentry() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOTDIR_VALUE),
    };
    let mount = ctx
        .process
        .cwd_binding()
        .filter(|cwd| cwd.dentry.key() == dentry.key())
        .map(|cwd| cwd.mount)
        .or_else(|| {
            ctx.process
                .mount_namespace_cap()
                .and_then(|namespace| namespace.mount_containing_dentry(&dentry))
        });
    let Some(mount) = mount else {
        return SyscallResult::Error(ENOENT_VALUE);
    };
    match tx_subsystems::process::step_chdir_with_mount(&ctx.process, dentry, mount) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => SyscallResult::Return(0),
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => SyscallResult::Error(EACCES_VALUE),
    }
}

/// `statx/// `statx(dirfd, path, flags, mask, statxbuf)`. Linux generic ABI
/// `__NR_statx = 291`.
///
/// This is the metadata probe LA64 musl/busybox uses before `ls`
/// opens a directory and, on LA64 musl, for some `fstat(fd)` wrappers
/// via `statx(fd, "", AT_EMPTY_PATH, ...)`. Txv2 reports the same
/// inode metadata already used by `newfstatat`; unsupported sync
/// policy bits are accepted because there is no cache coherency
/// distinction in the current VFS layer.
pub(super) async fn sys_statx<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let flags = args[2] as u32;
    let mask = args[3] as u32;
    let statxbuf_uaddr = args[4];

    if path_uaddr == 0 || statxbuf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let known_flags = AT_EMPTY_PATH
        | AT_NO_AUTOMOUNT
        | numbers::AT_STATX_SYNC_TYPE
        | (AT_SYMLINK_NOFOLLOW as u32);
    if flags & !known_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let known_mask = numbers::STATX_BASIC_STATS | numbers::STATX_BTIME | numbers::STATX_MNT_ID;
    if mask & !known_mask != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };

    let cwd = if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
        match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        }
    } else {
        match dirfd_anchor_for_path(dirfd, &path, ctx) {
            Ok(cwd) => cwd,
            Err(result) => return result,
        }
    };
    // Device nodes carry a (major, minor) rdev that glibc reads back from
    // statx — notably `daemon()` fstat()s /dev/null and rejects it with
    // ENODEV unless st_rdev == makedev(1, 3). LA64 glibc routes fstat()
    // through statx(fd, "", AT_EMPTY_PATH), so the fd branch must surface the
    // real rdev (RV64 glibc uses newfstatat → sys_fstat, which already does).
    let (mut statx_result, ino, rdev_major, rdev_minor) =
        if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
            if dirfd == AT_FDCWD {
                (
                    StatxResult {
                        meta: cwd.rnode().meta(),
                    },
                    cwd.rnode().fs_object_id(),
                    0,
                    0,
                )
            } else {
                let fd = dirfd;
                if fd < 0 {
                    return SyscallResult::Error(EBADF_VALUE);
                }
                let file = match resolve_fd(&ctx.process, fd as u32) {
                    Some(f) => f,
                    None => return SyscallResult::Error(EBADF_VALUE),
                };
                let (rmaj, rmin) = rdev_major_minor_for_open_file(&file);
                match file.backing() {
                    OpenFileBacking::Rnode { rnode } => (
                        StatxResult {
                            meta: stat_meta_for_open_file(&file),
                        },
                        rnode.fs_object_id(),
                        rmaj,
                        rmin,
                    ),
                    _ => (
                        StatxResult {
                            meta: stat_meta_for_open_file(&file),
                        },
                        FsObjectId::new(fd as u64),
                        rmaj,
                        rmin,
                    ),
                }
            }
        } else {
            if let Some((meta, ino, rmaj, rmin)) = match proc_self_fd_stat_info(ctx, &path) {
                Ok(info) => info,
                Err(result) => return result,
            } {
                (StatxResult { meta }, ino, rmaj, rmin)
            } else if is_caller_dev_tty_path(&path, &cwd) {
                let (meta, ino, rmaj, rmin) = match caller_dev_tty_stat_info(ctx) {
                    Ok(info) => info,
                    Err(result) => return result,
                };
                (StatxResult { meta }, ino, rmaj, rmin)
            } else {
                let walker_cred = ctx.walker_cred();
                let result = {
                    let mut script_ctx = build_subject_script_ctx(ctx);
                    if flags & AT_SYMLINK_NOFOLLOW as u32 != 0 {
                        let mut op = LstatxOp {
                            rooted_at: &cwd,
                            path: &path,
                            cred: &walker_cred,
                            target: None,
                        };
                        step_engine::drive_oneshot(&mut op, &mut script_ctx)
                    } else {
                        let mut op = StatxOp {
                            rooted_at: &cwd,
                            path: &path,
                            cred: &walker_cred,
                            target: None,
                        };
                        step_engine::drive_oneshot(&mut op, &mut script_ctx)
                    }
                };
                match result {
                    Ok((sr, id)) => {
                        let (rmaj, rmin) = rdev_major_minor_for_fs_object_id(id);
                        (sr, id, rmaj, rmin)
                    }
                    Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
                }
            }
        };

    apply_stat_meta_override(ino, &mut statx_result.meta);
    let statx = inode_meta_to_statx(&statx_result.meta, ino.as_u64(), rdev_major, rdev_minor);
    if let Err(errno) = bootstrap_write_user::<StatxLayout>(&ctx.aspace, statxbuf_uaddr, statx) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

/// `newfstatat(dirfd, path, statbuf, flags)`. Linux RV64 generic ABI
/// `__NR_newfstatat = 79`.
///
/// Slice 6 surface:
/// - `dirfd == AT_FDCWD` for path walks; non-cwd dirfds → `-EBADF`.
/// - `flags & AT_EMPTY_PATH` paired with empty path stats either the
///   cwd (`AT_FDCWD`) or the supplied fd. LA64 musl uses this fd form
///   to implement `fstat(fd)`.
/// - `flags & AT_SYMLINK_NOFOLLOW` stats the terminal symlink
///   itself instead of following it.
/// - `flags & AT_NO_AUTOMOUNT` is accepted but ignored (no
///   automount machinery — matches Linux's lenience).
/// - Other flag bits → `-EINVAL`.
///
/// Path resolution mirrors `resolve_path_at`'s shape (using
/// `step_walk` from cwd with `walker_cred`).
pub(super) async fn sys_newfstatat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let statbuf_uaddr = args[2];
    let flags = args[3] as u32;

    if statbuf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    // Slice 6 honours: AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW
    // | AT_NO_AUTOMOUNT (ignored). Other bits are rejected so a
    // future caller passing an unrecognised flag (`AT_STATX_*`,
    // `AT_RECURSIVE`, etc.) sees `-EINVAL` rather than silent
    // misbehaviour. Note: AT_SYMLINK_NOFOLLOW is `i32` in numbers.rs
    // (file-mode arms convention); cast to u32 for the bit-OR.
    let known_mask = AT_EMPTY_PATH | AT_NO_AUTOMOUNT | (AT_SYMLINK_NOFOLLOW as u32);
    if flags & !known_mask != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
    };

    // AT_EMPTY_PATH + empty path: stat the cwd itself for AT_FDCWD,
    // or mirror fstat(fd) for a real fd. Otherwise use StatOp +
    // drive_oneshot from cwd; directory-fd path walks remain out of
    // scope for this slice.
    let cwd = if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
        match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        }
    } else {
        match dirfd_anchor_for_path(dirfd, &path, ctx) {
            Ok(cwd) => cwd,
            Err(result) => return result,
        }
    };
    let (mut meta, ino, rdev_major, rdev_minor) = if path.is_empty() && (flags & AT_EMPTY_PATH != 0)
    {
        if dirfd == AT_FDCWD {
            (cwd.rnode().meta(), cwd.rnode().fs_object_id(), 0, 0)
        } else {
            return sys_fstat([dirfd as u64, statbuf_uaddr, 0, 0, 0, 0], ctx);
        }
    } else {
        if let Some(info) = match proc_self_fd_stat_info(ctx, &path) {
            Ok(info) => info,
            Err(result) => return result,
        } {
            info
        } else if is_caller_dev_tty_path(&path, &cwd) {
            match caller_dev_tty_stat_info(ctx) {
                Ok(info) => info,
                Err(result) => return result,
            }
        } else {
            let walker_cred = ctx.walker_cred();
            let result = {
                let mut script_ctx = build_subject_script_ctx(ctx);
                if flags & AT_SYMLINK_NOFOLLOW as u32 != 0 {
                    let mut op = LstatOp {
                        rooted_at: &cwd,
                        path: &path,
                        cred: &walker_cred,
                        target: None,
                    };
                    step_engine::drive_oneshot(&mut op, &mut script_ctx)
                } else {
                    let mut op = StatOp {
                        rooted_at: &cwd,
                        path: &path,
                        cred: &walker_cred,
                        target: None,
                    };
                    step_engine::drive_oneshot(&mut op, &mut script_ctx)
                }
            };
            match result {
                Ok((m, id)) => {
                    let (rmaj, rmin) = rdev_major_minor_for_fs_object_id(id);
                    (m, id, rmaj, rmin)
                }
                Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
            }
        }
    };

    apply_stat_meta_override(ino, &mut meta);
    let stat = inode_meta_to_stat(
        &meta,
        ino.as_u64(),
        linux_encode_dev_t(rdev_major, rdev_minor),
    );

    if let Err(errno) = bootstrap_write_user::<StatLayout>(&ctx.aspace, statbuf_uaddr, stat) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}
