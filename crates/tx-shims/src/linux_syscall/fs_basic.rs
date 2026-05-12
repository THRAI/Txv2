//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, NoProgress, SpinMutex, StepOutcome};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload};
use tx_subsystems::vfs::FsObjectId;

static STAT_META_OVERRIDES: SpinMutex<BTreeMap<FsObjectId, InodeMeta>> =
    SpinMutex::new(BTreeMap::new());

static FCNTL_RECORD_LOCKS: SpinMutex<BTreeMap<FsObjectId, Vec<RecordLock>>> =
    SpinMutex::new(BTreeMap::new());

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

/// `fcntl(fd, cmd, arg)` per the Wave 2 ELF-loader plan §"Part 2 —
/// Per-fd CLOEXEC bitmap + fcntl(F_SETFD) + O_CLOEXEC" plus Slice 7 of
/// the shell-prompt roadmap (fcntl extension).
///
/// Wave 2 surface: `F_GETFD` / `F_SETFD` against the per-process
/// CLOEXEC set.
///
/// Slice 7 surface adds `F_DUPFD` / `F_DUPFD_CLOEXEC` / `F_GETFL`.
/// `F_SETFL` returns `-ENOSYS` (carryover — `OpenFileFlags` is a
/// plain `Copy`-struct field on `OpenFile`, not behind an atomic /
/// mutex, so the "replace flags atomically" semantic is unsafe under
/// the current shape; `TODO(phase-fcntl-setfl)`).
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
            SyscallResult::Return(bits as i64)
        }
        F_SETFL => {
            let arg = args[2] as u64;
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = OpenFileSetFlOp {
                file: &file,
                nonblocking: (arg & O_NONBLOCK as u64) != 0,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(()) => SyscallResult::Return(0),
                Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
            }
        }
        numbers::F_GETLK | numbers::F_OFD_GETLK => fcntl_getlk(ctx, &file, arg),
        numbers::F_SETLK | numbers::F_SETLKW | numbers::F_OFD_SETLK | numbers::F_OFD_SETLKW => {
            fcntl_setlk(ctx, &file, arg)
        }
        numbers::F_SETLEASE => SyscallResult::Error(EAGAIN_VALUE),
        numbers::F_GETLEASE => SyscallResult::Return(F_UNLCK as i64),
        numbers::F_GETPIPE_SZ => {
            if !is_pipe_file(&file) {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            SyscallResult::Return(tx_subsystems::pipe::PIPE_BUF as i64)
        }
        numbers::F_SETPIPE_SZ => {
            if !is_pipe_file(&file) {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            if arg > (1u64 << 31) {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            let pipe_size = tx_subsystems::pipe::PIPE_BUF as u64;
            if arg < pipe_size {
                return SyscallResult::error_from(Errno::EBUSY);
            }
            if arg > pipe_size {
                return SyscallResult::error_from(Errno::EPERM);
            }
            SyscallResult::Return(pipe_size as i64)
        }
        _ => SyscallResult::Error(EINVAL_VALUE),
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

fn is_pipe_file(file: &OpenFile) -> bool {
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return false;
    };
    matches!(
        rnode.backing(),
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe { .. }
        }
    )
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
    {
        let next = ctx.process.allocate_fd();
        let (soft_limit, _) = ctx.process.rlimit_nofile();
        if next >= soft_limit {
            return SyscallResult::Error(EMFILE_VALUE);
        }
    }

    // Bounded inline copy of the user path. Same `EXECVE_PATH_MAX = 4096`
    // budget as the existing `execve` / `fchmodat` arms (and matches
    // Linux's `PATH_MAX`). Empty paths surface as `-ENOENT` from the
    // walker — let it through so the lookup-side error wins.
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };

    // Decode the open flags. Access-mode picks the read/write pair;
    // O_APPEND / O_CLOEXEC thread through to OpenFileFlags. O_NONBLOCK
    // is accepted but ignored (no blocking state on OpenFile yet).
    let (want_read, want_write) = decode_access_mode(flags);
    let want_path_only = flags & 0o10000000 != 0;
    let want_append = flags & O_APPEND != 0;
    let want_cloexec = flags & O_CLOEXEC != 0;
    let want_create = flags & O_CREAT != 0;
    let want_excl = flags & O_EXCL != 0;
    let want_trunc = flags & O_TRUNC != 0;
    // O_NONBLOCK and other unrecognised bits: silently dropped.

    let open_flags = OpenFileFlags {
        read: want_read && !want_path_only,
        write: want_write && !want_path_only,
        append: want_append,
        cloexec: want_cloexec,
        nonblocking: flags & O_NONBLOCK != 0,
    };

    // Resolve the dirfd anchor. AT_FDCWD → process cwd; a real dirfd
    // → the `opendir_dentry` of its OpenFile (an O_DIRECTORY open of
    // that directory). Invalid / non-directory fds surface as EBADF /
    // ENOTDIR.
    let cwd: Cap<DEntry> = if path.starts_with(b"/") {
        match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        }
    } else if dirfd == AT_FDCWD {
        match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        }
    } else if dirfd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    } else {
        let open_file = match ctx.process.fd(dirfd as u32) {
            Some(f) => f,
            None => return SyscallResult::Error(EBADF_VALUE),
        };
        match open_file.opendir_dentry() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOTDIR_VALUE),
        }
    };

    if let Err(err) = ensure_fd_room_under_limit(ctx) {
        return err;
    }

    let walker_cred = ctx.walker_cred();
    // PR async migration: non-O_CREAT, non-O_TRUNC simple open
    // goes through `OpenOp + drive()` — no manual step loop.
    if !want_create && !want_trunc {
        use step_engine::DriveMode;
        use tx_scripts::drive;
        let mut script_ctx = build_subject_script_ctx(ctx);
        let op = OpenOp {
            rooted_at: cwd.clone(),
            path: path.clone(),
            flags: open_flags,
            mode: mode as u16,
            cred: ctx.walker_cred(),
        };
        let mailbox_arc = script_ctx.mailbox().cloned();
        let timer_wheel_arc = script_ctx.timer_wheel().cloned();
        let delegate_registry_arc = script_ctx.delegate_registry().cloned();
        let openfile = match drive(
            op,
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_wheel_arc.as_ref(),
        )
        .await
        {
            Ok(file) => file,
            Err(v3errno) => {
                return SyscallResult::error_from(Errno::from(v3errno));
            }
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

    // Step 1: walk the path to a terminal dentry. Three outcomes:
    //   - success: the file exists. Handle O_EXCL collision; otherwise
    //     fall through to the open + (optional) truncate phase.
    //   - ENOENT + O_CREAT: split into (parent_path, basename), walk
    //     the parent, call `FsOps::create_inode`, then re-walk to
    //     materialise the dentry over the freshly-created inode.
    //   - other errno: forward.
    //
    // We use the dentry (not the OpenFile) as the truncate-anchor so
    // `fs_ops_for_dentry`'s parent-hint ascend can find the in-scope
    // mount payload (freshly-resolved child rnodes don't carry the
    // mount weak; only mount-root rnodes do, per the walker's
    // `current_fs_ops` discipline).
    //
    // Send-future discipline (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`):
    // `Guard` is `!Send + !Sync` so we cannot hold one across this
    // function's `.await`s — the `dispatch` future feeds
    // `Reactor::submit_task` which requires `Send`. Use the
    // `poll_walker_synchronously` helper that the file-mode arms also
    // use; every in-tree walker backend resolves immediately so the
    // noop-waker poll always returns `Ready`.
    use step_engine::{Errno as V3Errno, StepOutcome as V3};
    let walk_first = {
        let guard = step_engine::guard();
        let outcome = step_walk(cwd.clone(), &path, &walker_cred, &guard);
        drop(guard);
        outcome
    };

    let dentry: Cap<DEntry> = match walk_first {
        V3::Done(d) => {
            if want_create && want_excl {
                return SyscallResult::Error(EEXIST_VALUE);
            }
            d
        }
        V3::Continue { .. } | V3::Yield { .. } => {
            return SyscallResult::Error(EIO_VALUE);
        }
        V3::Err(V3Errno::ENOENT) if want_create => {
            let create_mode = (mode as u16) & !ctx.process.umask() & 0o7777;
            match create_then_walk::<P>(&cwd, &path, create_mode, &walker_cred) {
                Ok(d) => d,
                Err(e) => return SyscallResult::Error(e),
            }
        }
        V3::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
    };

    // Step 2: O_TRUNC. Apply *before* materialising the OpenFile so
    // any future `step_read` against the resulting fd observes the
    // truncated state. Directories → -EISDIR; backends without
    // truncate support → -ENOSYS.
    if want_trunc {
        let meta = dentry.rnode().meta();
        if meta.kind() == tx_subsystems::vfs::structure::InodeKind::Directory {
            return SyscallResult::Error(EISDIR_VALUE);
        }
        if meta.size != 0 {
            use StepOutcome as V3Trunc;
            let fs_page_backing = match fs_page_backing_for_dentry(&dentry) {
                Some(b) => b,
                None => return SyscallResult::Error(ENOSYS_VALUE),
            };
            let fs_object_id = dentry.rnode().fs_object_id();
            let guard = step_engine::guard();
            match fs_page_backing.truncate(fs_object_id, 0, &guard) {
                V3Trunc::Done(()) => {}
                V3Trunc::Continue { .. } | V3Trunc::Yield { .. } => {
                    return SyscallResult::Error(EIO_VALUE);
                }
                V3Trunc::Err(errno) => {
                    return SyscallResult::error_from(Errno::from(errno));
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
    // Same Send-future discipline as Step 1: poll_walker_synchronously.
    let openfile: Cap<OpenFile> = {
        let guard = step_engine::guard();
        let outcome = step_open(cwd, &path, open_flags, mode as u16, &walker_cred, &guard);
        drop(guard);
        match outcome {
            V3::Done(file) => file,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    };

    // Step 4: install at the lowest unused fd ≥ 0. Per fd-ops Wave 1
    // the fd table is a sparse `BTreeMap<u32, Cap<OpenFile>>`;
    // `allocate_fd()` scans for the lowest unused key.
    let fd = match allocate_fd_under_limit(ctx) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
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
//
// PR-9 phase 3b: not yet StepOp-driven — pending. `sys_close` does
// not invoke any `*Op::step(ctx)` call site; it operates directly on
// the process fd-table via `Cap<ProcessIdentity>` accessors
// (`fd`, `set_fd`, `set_fd_cloexec`). When fd-table mutation gains a
// StepOp wrap, thread `&mut KernelScriptCtx` here.
/// PR-3 migration: `CloseOp` is a `OneShotStepOp` — dispatched via
/// `drive_oneshot` (no reactor, no yield).
pub(super) fn sys_close<'a>(fd: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    let closing_file = ctx.process.fd(fd);
    if let Some(file) = closing_file.as_deref() {
        maybe_close_socket_file(file);
    }
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = CloseOp {
        process: ctx.process.clone(),
        fd,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(()) => {
            if let Some(file) = closing_file.as_deref() {
                file.flock_release();
                fcntl_release_process_locks_for_file(ctx.process.pid.0, file);
            }
            SyscallResult::Return(0)
        }
        Err(v3errno) => {
            if Errno::from(v3errno) == Errno::EBADF && super::net::close_socket_fd(fd, ctx) {
                SyscallResult::Return(0)
            } else {
                SyscallResult::error_from(Errno::from(v3errno))
            }
        }
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
/// `O_DIRECT` (packet-mode pipes) is recognised but unsupported and
/// returns `-ENOSYS`. Any other bits return `-EINVAL`.
///
/// Userspace writeback: the `pipefd_uaddr` flows through
/// `bootstrap_write_user::<[u32; 2]>` (canonical `aspace.write_user`
/// lane with kernel-pointer fallback for test scaffolding).
pub(super) fn sys_pipe2<'a>(pipefd_uaddr: u64, flags: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    // Validate flags. Recognised: O_CLOEXEC | O_NONBLOCK | O_DIRECT.
    // O_DIRECT is recognised but unsupported (packet-mode pipes are
    // out of scope) → `-ENOSYS`.
    let recognised = O_CLOEXEC | O_NONBLOCK | O_DIRECT;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & O_DIRECT != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }

    let pipe_flags = tx_subsystems::pipe::PipeFlags {
        cloexec: flags & O_CLOEXEC != 0,
        nonblocking: flags & O_NONBLOCK != 0,
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

    // Write the (reader_fd, writer_fd) pair back to userspace
    // through the canonical user-VA lane.
    if let Err(errno) =
        bootstrap_write_user::<[u32; 2]>(&ctx.aspace, pipefd_uaddr, [reader_fd, writer_fd])
    {
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
pub(super) fn sys_ioctl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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

    const BLKGETSIZE64: u32 = 0x8008_1272;
    if request == BLKGETSIZE64 && file.rnode().meta().kind() == InodeKind::BlockDevice {
        let Some(reg) = tx_fs::bdevfs::block_device_for_object_id(file.rnode().fs_object_id())
        else {
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

    if let RNodeBacking::StructBacked {
        payload: StructPayload::CharDevice(binding),
    } = file.rnode().backing()
    {
        if binding.name == "rtc" && request == RTC_RD_TIME {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let rtc_time = RtcTime::fixed_oscomp_time();
            return match bootstrap_write_user::<RtcTime>(&ctx.aspace, argp, rtc_time) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            };
        }
        return SyscallResult::error_from(Errno::ENOTTY);
    }

    // Resolve to a TTY. Non-TTY fds → -ENOTTY for terminal-shape ioctls
    // (Linux semantic — even pipes / regular files return ENOTTY for
    // these requests, per `man ioctl_tty`).
    let tty = match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        _ => return SyscallResult::error_from(Errno::ENOTTY),
    };

    // v3 step_ioctl_* return Done/Err only in practice; helper to
    // collapse the four-variant catalog into a v4 Errno-or-value.
    use StepOutcome as V3Out;
    fn unwrap_v3<T>(v: V3Out<T, NoProgress>) -> Result<T, Errno> {
        match v {
            V3Out::Done(t) => Ok(t),
            V3Out::Err(e) => Err(e.into()),
            V3Out::Continue { .. } | V3Out::Yield { .. } => Err(Errno::EIO),
        }
    }

    match request {
        TCGETS => {
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tcgets(&tty, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(termios) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<Termios>(&ctx.aspace, argp, termios)
                    {
                        return SyscallResult::error_from(errno);
                    }
                    SyscallResult::Return(0)
                }
                Err(errno) => SyscallResult::error_from(errno),
            }
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
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tcsets(&tty, new_termios, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        TIOCGPGRP => {
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tiocgpgrp(&tty, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(pgid) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, argp, pgid) {
                        return SyscallResult::error_from(errno);
                    }
                    SyscallResult::Return(0)
                }
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        TIOCSPGRP => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let new_pgrp: u32 = match bootstrap_read_user::<u32>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            let caller = make_ioctl_caller(ctx);
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tiocspgrp(&tty, caller, new_pgrp, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        TIOCGWINSZ => {
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tiocgwinsz(&tty, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(ws) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<Winsize>(&ctx.aspace, argp, ws) {
                        return SyscallResult::error_from(errno);
                    }
                    SyscallResult::Return(0)
                }
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        TIOCSWINSZ => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let ws: Winsize = match bootstrap_read_user::<Winsize>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tiocswinsz(&tty, ws, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            }
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
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tiocsctty_for_process(&tty, &ctx.process, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            }
        }
        TIOCNOTTY => {
            let caller = make_ioctl_caller(ctx);
            let outcome = {
                let guard = step_engine::guard();
                step_ioctl_tiocnotty(&tty, caller, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::error_from(errno),
            }
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RtcTime {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
}

impl RtcTime {
    const fn fixed_oscomp_time() -> Self {
        Self {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 23,
            tm_mon: 4,
            tm_year: 126,
            tm_wday: 6,
            tm_yday: 142,
            tm_isdst: 0,
        }
    }
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

fn inode_meta_to_statx(meta: &InodeMeta, ino: u64) -> StatxLayout {
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
        stx_rdev_major: 0,
        stx_rdev_minor: 0,
        stx_dev_major: 0,
        stx_dev_minor: 0,
        stx_mnt_id: 0,
        stx_dio_mem_align: 0,
        stx_dio_offset_align: 0,
        __spare3: [0; 12],
    }
}

fn stat_meta_for_open_file(file: &Cap<OpenFile>) -> InodeMeta {
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

    let rnode = file.rnode();
    let fs_object_id = rnode.fs_object_id();
    let meta = stat_meta_for_open_file(&file);
    let ino = fs_object_id.as_u64();
    let stat = inode_meta_to_stat(&meta, ino, 0);

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
    match tx_subsystems::process::step_chdir(&ctx.process, dentry) {
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
    };

    let (mut statx_result, ino) = if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
        if dirfd == AT_FDCWD {
            let cwd = match ctx.process.cwd() {
                Some(d) => d,
                None => return SyscallResult::Error(ENOENT_VALUE),
            };
            (
                StatxResult {
                    meta: cwd.rnode().meta(),
                },
                cwd.rnode().fs_object_id(),
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
            let rnode = file.rnode();
            (
                StatxResult {
                    meta: stat_meta_for_open_file(&file),
                },
                rnode.fs_object_id(),
            )
        }
    } else {
        if path.is_empty() {
            return SyscallResult::Error(ENOENT_VALUE);
        }
        let cwd = match resolve_cwd_for_path(dirfd, &path, ctx) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        };
        let walker_cred = ctx.walker_cred();
        let result = {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = StatxOp {
                rooted_at: &cwd,
                path: &path,
                cred: &walker_cred,
                target: None,
            };
            step_engine::drive_oneshot(&mut op, &mut script_ctx)
        };
        match result {
            Ok((sr, id)) => (sr, id),
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };

    apply_stat_meta_override(ino, &mut statx_result.meta);
    let statx = inode_meta_to_statx(&statx_result.meta, ino.as_u64());
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
/// - `flags & AT_SYMLINK_NOFOLLOW` is accepted but ignored (the
///   walker always follows symlinks today; documented carryover).
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

    // Slice 6 honours: AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW (ignored)
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
    };

    let walker_cred = ctx.walker_cred();

    // AT_EMPTY_PATH + empty path: stat the cwd itself for AT_FDCWD,
    // or mirror fstat(fd) for a real fd. Otherwise use StatOp +
    // drive_oneshot from cwd; directory-fd path walks remain out of
    // scope for this slice.
    let (mut meta, ino) = if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
        if dirfd == AT_FDCWD {
            let cwd = match ctx.process.cwd() {
                Some(d) => d,
                None => return SyscallResult::Error(ENOENT_VALUE),
            };
            (cwd.rnode().meta(), cwd.rnode().fs_object_id())
        } else {
            return sys_fstat([dirfd as u64, statbuf_uaddr, 0, 0, 0, 0], ctx);
        }
    } else {
        if path.is_empty() {
            return SyscallResult::Error(ENOENT_VALUE);
        }
        let cwd = match resolve_cwd_for_path(dirfd, &path, ctx) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        };
        let result = {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = StatOp {
                rooted_at: &cwd,
                path: &path,
                cred: &walker_cred,
                target: None,
            };
            step_engine::drive_oneshot(&mut op, &mut script_ctx)
        };
        match result {
            Ok((m, id)) => (m, id),
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };

    apply_stat_meta_override(ino, &mut meta);
    let stat = inode_meta_to_stat(&meta, ino.as_u64(), 0);

    if let Err(errno) = bootstrap_write_user::<StatLayout>(&ctx.aspace, statbuf_uaddr, stat) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}
/// `getdents64(fd, dirp, count)`. Linux RV64 generic ABI
/// `__NR_getdents64 = 61`.
///
/// Encodes successive `DirEntry` records produced by `FsOps::readdir`
/// into the user buffer as `linux_dirent64` records. The per-fd
/// `OpenFile.readdir_cursor()` holds the cursor across calls so each
/// invocation resumes where the previous one left off. Returns the
/// number of bytes written; `0` at end-of-directory; `-EINVAL` if even
/// the first record won't fit; `-ENOTDIR` for a non-directory fd.
///
/// Synchronous: every in-tree FS backend (`tmpfs`, `devfs`) resolves
/// readdir without `.await`. A future async-aware backend would
/// require shifting to the `wait_source::wait_on_token` pattern; the
/// arm panics defensively on `Blocked` / `AdvancedThenBlocked` per the
/// `Guard` send-future discipline (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
pub(super) async fn sys_getdents64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_uaddr = args[1];
    let buf_len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // Only directory backings produce dirents. Pipes, regular files,
    // TTYs, and chardevs surface `-ENOTDIR` per Linux's
    // `man getdents64`.
    let dir_fs_object_id = match file.rnode().backing() {
        RNodeBacking::Directory => file.rnode().fs_object_id(),
        _ => return SyscallResult::Error(ENOTDIR_VALUE),
    };

    // Resolve the FsOps for this directory's mount. The OpenFile's
    // rnode is the same `Cap<RNode>` that the walker installed via
    // step_open against a mount-published dentry — we can't pull the
    // mount payload directly off the rnode (`materialise_child_rnode`
    // doesn't carry the mount weak), so reuse the dentry-side
    // `fs_ops_for_dentry` shape via a synthetic dentry. In practice
    // every directory rnode this path sees is the mount root or a
    // descendant materialised through step_open, and the rnode
    // itself carries `containing_mount_weak()` only when it *is* the
    // mount root. For descendants we fall through to `None` below
    // and the call surfaces -ENOSYS defensively. tmpfs's directory
    // tree uses a single rnode-per-inode with the mount weak set
    // only at the root, so this is the practical limit today.
    //
    // TODO(phase-readdir-mount): teach `materialise_child_rnode` to
    // forward the mount weak so descendants don't hit the fallback.
    // Until then, every test fixture uses the mount-root directory.
    let fs_ops = match fs_ops_for_rnode(file.rnode()) {
        Some(o) => o,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };

    let mut cursor = file.readdir_cursor();
    let mut written: usize = 0;

    use StepOutcome as V3;
    loop {
        let outcome = {
            let guard = step_engine::guard();
            fs_ops.readdir(dir_fs_object_id, cursor, &guard)
        };
        match outcome {
            V3::Done(Some((entry, next_cursor))) => {
                let name_bytes = entry.name.as_bytes();
                let raw_len = LINUX_DIRENT64_HEADER_BYTES + name_bytes.len() + 1;
                let total_len = align_up_8(raw_len);
                if written + total_len > buf_len {
                    if written == 0 {
                        // Even the first record didn't fit — caller's
                        // buffer is too small. Linux's
                        // `man getdents64` returns EINVAL here.
                        return SyscallResult::Error(EINVAL_VALUE);
                    }
                    // Stop short; the cursor points at this entry so
                    // the next call resumes here.
                    file.set_readdir_cursor(cursor);
                    break;
                }
                // Build the record image in kernel memory, then copy
                // out through the canonical user-VA lane.
                let mut record: alloc::vec::Vec<u8> = alloc::vec![0u8; total_len];
                let header = LinuxDirent64Header {
                    d_ino: entry.fs_object_id.as_u64(),
                    d_off: next_cursor.as_u64() as i64,
                    d_reclen: total_len as u16,
                    d_type: inode_kind_to_dt(entry.kind),
                };
                // Copy header bytes via `as_bytes` proxy. The header
                // is `repr(C)` and Copy; we transmute through a slice.
                {
                    // SAFETY: header is a valid `#[repr(C)] Copy`
                    // struct whose byte image we want to splice into
                    // the staging Vec. Using `from_raw_parts` against
                    // a stack value keeps the read inside our kernel
                    // memory.
                    let header_bytes = unsafe {
                        core::slice::from_raw_parts(
                            &header as *const LinuxDirent64Header as *const u8,
                            LINUX_DIRENT64_HEADER_BYTES,
                        )
                    };
                    record[..LINUX_DIRENT64_HEADER_BYTES].copy_from_slice(header_bytes);
                }
                record[LINUX_DIRENT64_HEADER_BYTES..LINUX_DIRENT64_HEADER_BYTES + name_bytes.len()]
                    .copy_from_slice(name_bytes);
                // NUL terminator after name; remaining padding bytes
                // already zero from `vec![0; total_len]`.
                if let Err(errno) = bootstrap_copy_to_user(
                    &ctx.aspace,
                    buf_uaddr.wrapping_add(written as u64),
                    &record,
                ) {
                    if written > 0 {
                        return SyscallResult::Return(written as i64);
                    }
                    return SyscallResult::error_from(errno);
                }
                written += total_len;
                cursor = next_cursor;
                file.set_readdir_cursor(cursor);
            }
            V3::Done(None) => {
                // End of directory — durable cursor advance is
                // unnecessary (the readdir backend's cursor is
                // self-terminating).
                break;
            }
            V3::Continue { .. } | V3::Yield { .. } => {
                // No in-tree backend produces these. Surface as
                // `-EIO` defensively if the partial-progress shape
                // ever fires.
                if written > 0 {
                    return SyscallResult::Return(written as i64);
                }
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => {
                if written > 0 {
                    return SyscallResult::Return(written as i64);
                }
                return SyscallResult::error_from(Errno::from(errno));
            }
        }
    }

    SyscallResult::Return(written as i64)
}

/// Resolve the `Arc<dyn FsOps>` in scope for a directory rnode.
/// Mirrors `fs_ops_for_dentry`'s shape (in `fs_path.rs`) but
/// operates on the rnode directly — the OpenFile carries `Cap<RNode>`,
/// not `Cap<DEntry>`.
///
/// Returns `None` if the rnode does not carry a `containing_mount`
/// weak (descendant rnodes minted by `materialise_child_rnode` don't
/// — only mount-root rnodes do). The Slice 6 `getdents64` arm
/// surfaces this as `-ENOSYS` defensively (no `FsOps` to dispatch
/// through). In practice every tested directory is the mount root,
/// matching tmpfs's day-1 surface.
///
/// TODO(phase-readdir-mount): forward the mount weak to descendants
/// during `materialise_child_rnode` so this fallback is unnecessary.
pub(super) fn fs_ops_for_rnode(
    rnode: &Cap<tx_subsystems::vfs::structure::RNode>,
) -> Option<Arc<dyn tx_subsystems::vfs::FsOps>> {
    let guard = step_engine::guard();
    let weak = rnode.containing_mount_weak()?;
    let payload = weak.upgrade(&guard)?;
    Some(payload.fs_ops.clone())
}

/// `statfs(path, buf)`. Linux RV64 ABI `__NR_statfs = 43`.
pub(super) async fn sys_statfs<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let buf_uaddr = args[1];
    let statfs = StatfsLayout {
        f_type: 0x0102_1994,
        f_bsize: 4096,
        f_blocks: 1024,
        f_bfree: 768,
        f_bavail: 768,
        f_files: 4096,
        f_ffree: 2048,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: 0,
        f_spare: [0; 4],
    };
    if let Err(e) = bootstrap_write_user::<StatfsLayout>(&ctx.aspace, buf_uaddr, statfs) {
        return SyscallResult::error_from(e);
    }
    SyscallResult::Return(0)
}

/// `fstatfs(fd, buf)`. Linux RV64 ABI `__NR_fstatfs = 44`.
pub(super) async fn sys_fstatfs<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    if ctx.process.fd(fd).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    sys_statfs::<P>(args, ctx).await
}

/// `sync()`. Linux RV64 ABI `__NR_sync = 81`.
pub(super) async fn sys_sync<P: PmapIf>(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    SyscallResult::Return(0)
}

/// `readahead(fd, offset, count)`. Linux RV64 generic ABI
/// `__NR_readahead = 213`.
///
/// The current page cache has no prefetch policy hook, so this syscall is a
/// validation-only no-op for regular PageBacked files.
pub(super) fn sys_readahead(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as i32;
    let offset = args[1] as i64;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if offset < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if !file.flags().read {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    match rnode.backing() {
        RNodeBacking::PageBacked { .. } => SyscallResult::Return(0),
        _ => SyscallResult::Error(EINVAL_VALUE),
    }
}

/// `sync_file_range(fd, offset, nbytes, flags)`. Linux RV64 generic ABI
/// `__NR_sync_file_range = 84`.
///
/// Treats range sync as a no-op after Linux-compatible validation. This
/// unblocks LTP error-path tests without claiming device writeback support.
pub(super) fn sys_sync_file_range(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    const SYNC_FILE_RANGE_WAIT_BEFORE: u64 = 0x1;
    const SYNC_FILE_RANGE_WRITE: u64 = 0x2;
    const SYNC_FILE_RANGE_WAIT_AFTER: u64 = 0x4;
    const SYNC_FILE_RANGE_VALID: u64 =
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER;

    let fd = args[0] as i32;
    let offset = args[1] as i64;
    let nbytes = args[2] as i64;
    let flags = args[3];

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if offset < 0 || nbytes < 0 || (flags & !SYNC_FILE_RANGE_VALID) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return SyscallResult::Error(ESPIPE_VALUE);
    };
    match rnode.backing() {
        RNodeBacking::PageBacked { .. } => SyscallResult::Return(0),
        _ => SyscallResult::Error(ESPIPE_VALUE),
    }
}

/// `syncfs(fd)`. Linux RV64 ABI `__NR_syncfs = 267`.
/// Syncs the filesystem containing the given fd.
pub(super) async fn sys_syncfs<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let guard = step_engine::guard();
    let rnode = open_file.rnode();
    let page_backing = match rnode
        .containing_mount_weak()
        .and_then(|w| w.upgrade(&guard))
    {
        Some(mp) => mp.fs_page_backing().clone(),
        None => return SyscallResult::Error(ENODEV_VALUE),
    };
    // syncfs: flush the entire filesystem. The default impl falls back
    // to `fsync_file(ROOT)`; journaling filesystems can override.
    match page_backing.sync_filesystem(&guard) {
        StepOutcome::Done(()) => SyscallResult::Return(0),
        StepOutcome::Err(e) => SyscallResult::error_from(Errno::from(e)),
        _ => SyscallResult::Error(EIO_VALUE),
    }
}

/// `fsync(fd)`. Linux RV64 ABI `__NR_fsync = 82`.
/// Syncs the specific file referenced by `fd` (data + metadata).
pub(super) async fn sys_fsync<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let rnode = open_file.rnode();
    let fs_object_id = rnode.fs_object_id();
    // The mount-weak upgrade and the page-backing clone are done inside
    // a scoped guard so no guard crosses the subsequent
    // `drive(...).await` (INVARIANTS_v5 EBR-7).
    let page_backing = {
        let guard = step_engine::guard();
        match rnode
            .containing_mount_weak()
            .and_then(|w| w.upgrade(&guard))
        {
            Some(mp) => mp.fs_page_backing().clone(),
            None => return SyscallResult::Error(ENODEV_VALUE),
        }
    };
    // fsync: sync the specific file via FileFsyncOp + drive().
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let op = FileFsyncOp {
        page_backing,
        fs_object_id,
    };
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        None,
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `fdatasync(fd)`. Linux RV64 ABI `__NR_fdatasync = 83`.
/// Syncs file data (not metadata).  v1: delegates to fsync.
pub(super) async fn sys_fdatasync<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    sys_fsync::<P>(args, ctx).await
}

/// `flock(fd, operation)`. Linux RV64 ABI `__NR_flock = 32`.
///
/// v1: exclusive-lock only per open-file-description.  LOCK_SH
/// maps to LOCK_EX.  No deadlock detection.  Per POSIX flock
/// semantics (advisory, not enforced on I/O).
pub(super) async fn sys_flock<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let operation = args[1] as u32;

    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_UN: u32 = 8;
    const LOCK_NB: u32 = 4;

    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if (operation & LOCK_UN) != 0 {
        open_file.flock_release();
        return SyscallResult::Return(0);
    }

    let lock_type = operation & 3; // LOCK_SH=1 or LOCK_EX=2
    if lock_type != LOCK_SH && lock_type != LOCK_EX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let blocking = (operation & LOCK_NB) == 0;

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = FlockOp {
        file: &open_file,
        lock_type,
        blocking,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}
