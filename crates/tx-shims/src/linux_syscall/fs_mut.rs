//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
use tx_subsystems::mount::{self, MountPayload};
use tx_fs;

/// Split a path into `(parent, basename)` for the `O_CREAT`-on-missing
/// re-walk. `path` is a slash-separated sequence; trailing slashes
/// before the basename are dropped. Returns `(b"", path)` for a
/// single-component name (no slash) — the caller treats `parent ==
/// b""` as "current directory" and walks the cwd.
///
/// Examples:
/// - `b"foo"` → `(b"", b"foo")`
/// - `b"a/b"` → `(b"a", b"b")`
/// - `b"/x/y/z"` → `(b"/x/y", b"z")`
/// - `b"/"` → `(b"/", b"")` (degenerate; the create call would fail
///   with `EINVAL` anyway because `InlineName::new(b"")` rejects
///   the empty basename)
pub(super) fn split_path(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|b| *b == b'/') {
        None => (&[], path),
        Some(idx) => {
            // The slash itself stays with the parent so an absolute
            // path like `/x/y/z` keeps its leading `/` on `parent`
            // (the walker treats a path starting with `/` as
            // "restart from mount root").
            let parent = &path[..=idx];
            let basename = &path[idx + 1..];
            (parent, basename)
        }
    }
}

/// Helper for the `O_CREAT`-on-missing path inside `sys_openat`.
/// Walks to the parent of `path`, calls `FsOps::create_inode` for the
/// basename, then re-walks the full path to return a dentry over the
/// freshly-installed RNode. The dentry (not the OpenFile) is the
/// return shape because the syscall arm needs to apply `O_TRUNC`
/// against `FsPageBacking` *before* materialising the OpenFile, and
/// the dentry's parent-hint chain is what `fs_ops_for_dentry` /
/// `fs_page_backing_for_dentry` consume to find the in-scope mount.
///
/// Synchronous — uses `poll_walker_synchronously` for the same
/// Send-future reason `resolve_path_at` does (the `Guard` argument is
/// `!Send + !Sync`, so cross-`.await` holds break the dispatch
/// future's `Send` bound). Returns the positive-magnitude `-errno`
/// on failure.
pub(crate) fn create_then_walk<P: PmapIf>(
    cwd: &Cap<DEntry>,
    path: &[u8],
    mode: u16,
    cred: &Credential,
) -> Result<Cap<DEntry>, i32> {
    let _ = core::marker::PhantomData::<P>;
    let (parent_path, basename) = split_path(path);
    if basename.is_empty() {
        // A path like `/` or `foo/` has an empty basename — can't
        // create. Surface as -EISDIR (matches Linux's behaviour for
        // `open("/", O_CREAT, ...)`).
        return Err(EISDIR_VALUE);
    }

    // Walk to the parent. Empty `parent_path` means "use the cwd as
    // the parent" (the basename was a single component with no
    // slash). step_walk handles a leading `/` as "restart from mount
    // root" and `b""` as "stay at rooted_at".
    let parent_dentry: Cap<DEntry> = if parent_path.is_empty() {
        cwd.clone()
    } else {
        let guard = step_engine::guard();
        use StepOutcome as V3;
        let outcome = step_walk(cwd.clone(), parent_path, cred, &guard);
        drop(guard);
        match outcome {
            V3::Done(d) => d,
            V3::Continue { .. } | V3::Yield { .. } => {
                return Err(EIO_VALUE);
            }
            V3::Err(errno) => return Err(errno_to_i32(Errno::from(errno))),
        }
    };

    // Resolve the FsOps in scope at `parent_dentry`. Mirrors the
    // existing `fs_ops_for_dentry` shape used by the file-mode arms.
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(ops) => ops,
        None => return Err(EROFS_VALUE),
    };

    // Mint the new inode under the parent. The mode arrives from
    // userspace as the bottom 12 bits (`rwxrwxrwx | S_ISUID/S_ISGID/
    // S_ISVTX`); umask plumbing is deferred to a future slice
    // (TODO(phase-umask)).
    let parent_fs_object_id = parent_dentry.rnode().fs_object_id();
    let new_mode = mode & 0o7777;
    {
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let outcome = fs_ops.create_inode(parent_fs_object_id, basename, new_mode, cred, &guard);
        match outcome {
            V3::Done(_) => {}
            V3::Continue { .. } | V3::Yield { .. } => {
                return Err(EIO_VALUE);
            }
            V3::Err(errno) => return Err(errno_to_i32(Errno::from(errno))),
        }
    }

    // Re-walk the full path. Lookup now resolves the freshly-created
    // inode; the resulting dentry carries the proper parent-hint
    // chain back to the mount root.
    let guard = step_engine::guard();
    use StepOutcome as V3;
    let outcome = step_walk(cwd.clone(), path, cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => Ok(d),
        V3::Continue { .. } | V3::Yield { .. } => Err(EIO_VALUE),
        V3::Err(errno) => Err(errno_to_i32(Errno::from(errno))),
    }
}

/// Resolve the in-scope `Arc<dyn FsPageBacking>` for the given dentry.
/// Mirrors `fs_ops_for_dentry`'s parent-hint ascent shape but reads
/// `payload.fs_page_backing` so callers can dispatch against the
/// `FsPageBacking` trait surface.
pub(super) fn fs_page_backing_for_dentry(
    dentry: &Cap<DEntry>,
) -> Option<Arc<dyn tx_subsystems::page_backed::FsPageBacking>> {
    let guard = step_engine::guard();
    let mut cursor: Cap<DEntry> = dentry.clone();
    loop {
        if let Some(weak) = cursor.rnode().containing_mount_weak() {
            if let Some(payload) = weak.upgrade(&guard) {
                return Some(payload.fs_page_backing.clone());
            }
        }
        cursor = cursor.parent_hint()?;
    }
}

/// `mkdirat(dirfd, pathname, mode)`. Linux RV64 generic ABI
/// `__NR_mkdirat = 34`.
///
/// Slice 8: `dirfd == AT_FDCWD` only. Walks the parent directory of
/// `pathname` (split via [`split_path`]), then calls
/// `FsOps::mkdir(parent, basename, mode & !umask, &cred, &guard)`.
/// Empty paths and basenames surface as `-ENOENT` (let the FsOps
/// surface canonicalise the error).
pub(super) async fn sys_mkdirat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let mode = args[2] as u16;
    if dirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&path);
    if basename.is_empty() {
        // Trailing-slash-only basename, e.g. `mkdir("/")` — the FsOps
        // layer rejects an empty `InlineName`. Linux's behaviour for
        // `mkdir("/")` is `-EEXIST`; we surface the more conservative
        // `-EINVAL` (matches `InlineName::new(b"")`'s rejection).
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        cwd
    } else {
        match walk_from(cwd, parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let parent_id = parent_dentry.rnode().fs_object_id();
    use StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    // Apply umask: effective_mode = mode & !umask. Linux semantics
    // (umask is the bottom 9 bits — `rwxrwxrwx`).
    let umask = ctx.process.umask();
    let effective_mode = mode & !umask & 0o7777;
    let outcome = {
        let guard = step_engine::guard();
        fs_ops.mkdir(parent_id, basename, effective_mode, &cred, &guard)
    };
    match outcome {
        V3::Done(_) => SyscallResult::Return(0),
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::Error(errno_to_i32(Errno::from(errno))),
    }
}

/// `unlinkat(dirfd, pathname, flags)`. Linux RV64 generic ABI
/// `__NR_unlinkat = 35`.
///
/// Without `AT_REMOVEDIR` the arm dispatches through `FsOps::unlink`
/// (rejects directory targets with `-EISDIR`); with `AT_REMOVEDIR` it
/// dispatches through `FsOps::rmdir` (rejects non-directory targets
/// with `-ENOTDIR`).
pub(super) async fn sys_unlinkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let flags = args[2] as u32;
    if dirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&path);
    if basename.is_empty() {
        return SyscallResult::Error(EISDIR_VALUE);
    }
    // Walk parent first so the parent FsOps is in scope. The full
    // walk gives us the target's `FsObjectId` and inode kind so the
    // arm can pick `unlink` vs `rmdir` correctly.
    let parent_dentry = if parent_path.is_empty() {
        cwd.clone()
    } else {
        match walk_from(cwd.clone(), parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let target_dentry = match walk_from(cwd, &path, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let parent_id = parent_dentry.rnode().fs_object_id();
    let target_id = target_dentry.rnode().fs_object_id();
    let target_kind = target_dentry.rnode().meta().kind();
    use StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let want_rmdir = (flags & AT_REMOVEDIR) != 0;
    if want_rmdir && target_kind != InodeKind::Directory {
        return SyscallResult::Error(ENOTDIR_VALUE);
    }
    if !want_rmdir && target_kind == InodeKind::Directory {
        return SyscallResult::Error(EISDIR_VALUE);
    }
    let outcome = {
        let guard = step_engine::guard();
        if want_rmdir {
            fs_ops.rmdir(parent_id, basename, target_id, &guard)
        } else {
            fs_ops.unlink(parent_id, basename, target_id, &guard)
        }
    };
    match outcome {
        V3::Done(()) => SyscallResult::Return(0),
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::Error(errno_to_i32(Errno::from(errno))),
    }
}

/// `symlinkat(target, newdirfd, linkpath)`. Linux RV64 generic ABI
/// `__NR_symlinkat = 36`.
///
/// `target` is the symlink's textual content (no path resolution).
/// `linkpath` is split into `(parent_path, basename)`; the parent is
/// walked, then `FsOps::symlink(parent, basename, target, &cred,
/// &guard)` is dispatched.
pub(super) async fn sys_symlinkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let target_uaddr = args[0];
    let newdirfd = args[1] as i32;
    let linkpath_uaddr = args[2];
    if newdirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let target = match read_user_cstr(&ctx.aspace, target_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if target.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let linkpath = match read_user_cstr(&ctx.aspace, linkpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if linkpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&linkpath);
    if basename.is_empty() {
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        cwd
    } else {
        match walk_from(cwd, parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let parent_id = parent_dentry.rnode().fs_object_id();
    use StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = step_engine::guard();
        fs_ops.symlink(parent_id, basename, &target, &cred, &guard)
    };
    match outcome {
        V3::Done(_) => SyscallResult::Return(0),
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::Error(errno_to_i32(Errno::from(errno))),
    }
}

/// `linkat(olddirfd, oldpath, newdirfd, newpath, flags)`. Linux RV64
/// generic ABI `__NR_linkat = 37`.
///
/// Slice 8: same-filesystem hard link only — the existing tmpfs
/// `FsOps::link` returns `-ENOSYS` for now (Phase 3b carryover), so
/// this arm forwards whatever the FsOps surface produces. The
/// `AT_SYMLINK_FOLLOW` flag bit is silently accepted; default
/// (no-flag) Linux behaviour is "do not follow symlinks", but
/// `step_walk` follows symlinks unconditionally — Slice 8 carries
/// that limitation forward (deferred under `TODO(phase-linkat-nofollow)`).
pub(super) async fn sys_linkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let olddirfd = args[0] as i32;
    let oldpath_uaddr = args[1];
    let newdirfd = args[2] as i32;
    let newpath_uaddr = args[3];
    let _flags = args[4] as u32;
    if olddirfd != AT_FDCWD || newdirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let oldpath = match read_user_cstr(&ctx.aspace, oldpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if oldpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let newpath = match read_user_cstr(&ctx.aspace, newpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if newpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();
    // Walk source → target FsObjectId. Linux rejects directories
    // here as `-EPERM` (no hard-linking directories).
    let source_dentry = match walk_from(cwd.clone(), &oldpath, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    if source_dentry.rnode().meta().kind() == InodeKind::Directory {
        return SyscallResult::Error(EPERM_VALUE);
    }
    let source_id = source_dentry.rnode().fs_object_id();
    // Walk new path's parent directory.
    let (new_parent_path, new_basename) = split_path(&newpath);
    if new_basename.is_empty() {
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let new_parent_dentry = if new_parent_path.is_empty() {
        cwd
    } else {
        match walk_from(cwd.clone(), new_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let new_parent_id = new_parent_dentry.rnode().fs_object_id();
    use StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&new_parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = step_engine::guard();
        fs_ops.link(new_parent_id, new_basename, source_id, &guard)
    };
    match outcome {
        V3::Done(()) => SyscallResult::Return(0),
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::Error(errno_to_i32(Errno::from(errno))),
    }
}

/// `truncate(path, length)`. Linux RV64 generic ABI
/// `__NR_truncate = 45`.
///
/// Walks the path to the regular file, validates the backing is
/// `RNodeBacking::PageBacked`, then dispatches to
/// [`tx_subsystems::page_backed::step_truncate`]. Non-page-backed
/// rnodes (directories, TTYs, char devices, pipes, projected) surface
/// as `-EINVAL` per the step body's own contract.
pub(super) async fn sys_truncate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let path_uaddr = args[0];
    let new_size = args[1];
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();
    let dentry = match walk_from(cwd, &path, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let pc = match dentry.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        RNodeBacking::Directory => return SyscallResult::Error(EISDIR_VALUE),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let outcome = {
        let guard = step_engine::guard();
        tx_subsystems::page_backed::step_truncate(&pc, new_size, &guard)
    };
    use StepOutcome as V3;
    match outcome {
        V3::Done(()) | V3::Continue { .. } => SyscallResult::Return(0),
        V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(v3_errno) => SyscallResult::Error(errno_to_i32(v3_errno.into())),
    }
}

/// `ftruncate(fd, length)`. Linux RV64 generic ABI
/// `__NR_ftruncate = 46`.
///
/// Resolves `fd` against the per-process fd table and dispatches to
/// [`tx_subsystems::page_backed::step_truncate`] against
/// the OpenFile's PageBacked container. Non-page-backed fds surface as
/// `-EINVAL` (matches Linux for char devices, sockets, pipes); a fd
/// pointing at a directory backing returns `-EISDIR`.
pub(super) async fn sys_ftruncate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let new_size = args[1];
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let pc = match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        RNodeBacking::Directory => return SyscallResult::Error(EISDIR_VALUE),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let guard = step_engine::guard();
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let mut op = FdTruncateOp {
        pc: &pc,
        new_size: new_size as u64,
        guard: &guard,
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
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `readlinkat(dirfd, pathname, buf, bufsiz)`. Linux RV64 generic ABI
/// `__NR_readlinkat = 78`.
///
/// `step_walk` follows symlinks unconditionally, so the arm cannot
/// reuse the standard walker for the terminal component. Instead it
/// walks `pathname`'s **parent** directory and consults
/// `FsOps::lookup` against the basename to obtain the symlink's
/// `FsObjectId` without materialising it through the walker's
/// symlink-chase loop. The bytes returned by `FsOps::read_link` are
/// then copied into the user buffer (capped at `bufsiz`); the
/// non-terminator byte count is returned.
///
/// `bufsiz == 0` returns `-EINVAL` (Linux semantic). A null `buf`
/// surfaces as `-EFAULT`. A non-symlink target returns `-EINVAL`
/// (POSIX `readlink(2)` shape).
pub(super) async fn sys_readlinkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let buf_uaddr = args[2];
    let buf_len = args[3] as usize;
    if dirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if buf_len == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&path);
    if basename.is_empty() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        cwd
    } else {
        match walk_from(cwd, parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let parent_id = parent_dentry.rnode().fs_object_id();
    use StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };
    // Resolve the basename in the parent directly via `FsOps::lookup`
    // — bypasses the walker's symlink-chase loop so the symlink's
    // own inode (not its target's) is what we read.
    let target_id = {
        let guard = step_engine::guard();
        match fs_ops.lookup(parent_id, basename, &guard) {
            V3::Done(id) => id,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };
    let target_meta = {
        let guard = step_engine::guard();
        match fs_ops.load_inode_meta(target_id, &guard) {
            V3::Done(m) => m,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };
    if target_meta.kind() != InodeKind::Symlink {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let link_bytes = {
        let guard = step_engine::guard();
        match fs_ops.read_link(target_id, &guard) {
            V3::Done(b) => b,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };
    let to_copy = core::cmp::min(link_bytes.len(), buf_len);
    if to_copy > 0 {
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &link_bytes[..to_copy]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    SyscallResult::Return(to_copy as i64)
}

/// `mount(source, target, fstype, flags, data)`. Linux RV64 ABI `__NR_mount = 40`.
///
/// v1: supports `MS_BIND` (bind mount) and new mounts (tmpfs/devfs/proc).
pub(super) async fn sys_mount<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let source_uaddr = args[0];
    let target_uaddr = args[1];
    let fstype_uaddr = args[2];
    let flags = args[3] as u64;

    let target = match read_user_cstr(&ctx.aspace, target_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };

    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();

    let guard = step_engine::guard();
    use StepOutcome as V3;
    let target_dentry = match walk_from(cwd.clone(), &target, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let parent_payload = match mount_payload_for_dentry(&target_dentry) {
        Some(p) => p,
        None => return SyscallResult::Error(ENODEV_VALUE),
    };

    const MS_BIND: u64 = 4096;
    if (flags & MS_BIND) != 0 {
        // Bind mount.
        let source = match read_user_cstr(&ctx.aspace, source_uaddr, EXECVE_PATH_MAX) {
            Ok(p) => p,
            Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        };
        let source_dentry = match walk_from(cwd, &source, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        };
        match mount::bind_mount(source_dentry, target_dentry, &parent_payload, &guard) {
            Ok(_) => return SyscallResult::Return(0),
            Err(e) => return SyscallResult::Error(errno_to_i32(e)),
        }
    }

    // New filesystem mount.
    let fstype = match read_user_cstr(&ctx.aspace, fstype_uaddr, 64) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    let fstype_str = match core::str::from_utf8(&fstype) {
        Ok(s) => s,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Build backend via crate-level factory.
    let (fs_ops, fs_page_backing, root_id, root_meta, fstype_label): (
        alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
        alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        tx_subsystems::vfs::FsObjectId,
        tx_subsystems::vfs::InodeMeta,
        &str,
    ) = match fstype_str {
        "tmpfs" => (
            tx_fs::tmpfs::Tmpfs::fs_ops_arc(),
            tx_fs::tmpfs::Tmpfs::fs_page_backing_arc(),
            tx_subsystems::vfs::FsObjectId::ROOT,
            tx_subsystems::vfs::InodeMeta::new(
                tx_subsystems::vfs::InodeKind::Directory, 0o755,
            ),
            "tmpfs",
        ),
        "devfs" => (
            tx_fs::devfs::Devfs::fs_ops_arc(),
            tx_fs::devfs::Devfs::fs_page_backing_arc(),
            tx_fs::devfs::DEVFS_ROOT_OBJECT_ID,
            tx_subsystems::vfs::InodeMeta::new(
                tx_subsystems::vfs::InodeKind::Directory,
                tx_fs::devfs::DEVFS_ROOT_MODE,
            ),
            "devfs",
        ),
        "proc" => (
            tx_fs::procfs::Procfs::fs_ops_arc(),
            alloc::sync::Arc::new(tx_fs::procfs::Procfs) as alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
            tx_fs::procfs::PROCFS_ROOT_ID,
            tx_subsystems::vfs::InodeMeta::new(
                tx_subsystems::vfs::InodeKind::Directory,
                tx_fs::procfs::PROCFS_DIR_MODE,
            ),
            "proc",
        ),
        _ => return SyscallResult::Error(ENOSYS_VALUE),
    };

    let mount_payload = match mount::MountPayload::new_cap(
        fs_ops,
        fs_page_backing,
        None,
        mount::allocate_dev_id(),
        mount::MountOptions::default(),
        fstype_label,
        mount::SourceLabel::Static("none"),
    ) {
        Ok(p) => p,
        Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
    };

    let root_rnode = {
        use tx_subsystems::vfs::RNodeBacking;
        let raw = tx_subsystems::vfs::RNode::new(root_id, root_meta, RNodeBacking::Directory)
            .with_containing_mount(&mount_payload);
        match step_engine::reserve_for::<tx_subsystems::vfs::RNode>() {
            Ok(res) => match step_engine::sign_for(res, raw) {
                Ok(cap) => cap,
                Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
            },
            Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
        }
    };

    let mount_cap = match mount::MountIdentity::new_cap(
        mount::allocate_mount_id(),
        Some(target_dentry),
        root_rnode,
        None,
        mount_payload,
        mount::MountFlags::empty(),
    ) {
        Ok(m) => m,
        Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
    };

    mount::register_mount(
        &parent_payload,
        target_dentry.rnode().fs_object_id(),
        mount_cap,
    );

    SyscallResult::Return(0)
}

/// `umount2(target, flags)`. Linux RV64 ABI `__NR_umount2 = 39`.
pub(super) async fn sys_umount2<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let target_uaddr = args[0];
    let flags = args[1] as u64;

    if flags != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }

    let target = match read_user_cstr(&ctx.aspace, target_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };

    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let cred = ctx.walker_cred();

    let guard = step_engine::guard();
    use StepOutcome as V3;
    let target_dentry = match step_walk(cwd.clone(), &target, &cred, &guard) {
        V3::Done(d) => d,
        V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        _ => return SyscallResult::Error(EIO_VALUE),
    };
    let parent_payload = match mount_payload_for_dentry(&target_dentry) {
        Some(p) => p,
        None => return SyscallResult::Error(ENODEV_VALUE),
    };

    match mount::umount(&target_dentry, &parent_payload) {
        Ok(()) => SyscallResult::Return(0),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

/// `mknodat(dirfd, path, mode, dev)`. Linux RV64 ABI `__NR_mknodat = 33`.
///
/// v1: ENOSYS — needs FsOps::mknod + InodeMeta.rdev.
/// `mknodat(dirfd, path, mode, dev)`. Linux RV64 ABI `__NR_mknodat = 33`.
///
/// Creates a device node at the given path.  `mode` encodes the
/// file type (S_IFCHR, S_IFBLK, S_IFIFO, S_IFREG).  `dev` encodes
/// major/minor (major = (dev >> 8) & 0xfff, minor = dev & 0xff
/// | (dev >> 12) & 0xfff00).
pub(super) async fn sys_mknodat<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let _dirfd = args[0] as u32;
    let path_uaddr = args[1];
    let mode = args[2] as u32;
    let _dev = args[3] as u64;

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };

    let S_IFREG: u32 = 0o100000;
    let S_IFCHR: u32 = 0o020000;
    let S_IFBLK: u32 = 0o060000;
    let S_IFIFO: u32 = 0o010000;

    let cred = ctx.walker_cred();
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let kind = match mode & 0o170000u32 {
        t if t == S_IFREG || t == 0 => tx_subsystems::vfs::InodeKind::Regular,
        t if t == S_IFCHR => tx_subsystems::vfs::InodeKind::CharDevice,
        t if t == S_IFBLK => tx_subsystems::vfs::InodeKind::BlockDevice,
        t if t == S_IFIFO => tx_subsystems::vfs::InodeKind::Fifo,
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let result = {
        let guard = step_engine::guard();
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = MknodOp {
            rooted_at: &cwd,
            path: &path,
            mode: mode as u16,
            kind,
            cred: &cred,
            guard: &guard,
            parent: None,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `utimensat(dirfd, pathname, times, flags)`. Linux RV64 generic ABI
/// `__NR_utimensat = 88`.
///
/// Slice 8: returns `-ENOSYS`. The `FsOps` surface does not yet expose
/// a `set_times` hook (`InodeMeta` carries `atime`/`mtime`/`ctime`
/// fields, but the backend trait has no method to mutate them).
/// Most shells ignore `utimensat` failures — the deferred
/// implementation is documented under
/// `TODO(phase-vfs-utimens)` in the slice plan.
pub(super) fn sys_utimensat<'a>(_args: [u64; 6], _ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)`. Linux RV64
/// generic ABI `__NR_renameat2 = 276`.
///
/// Slice 8 surface:
/// - `dirfd != AT_FDCWD` → `-EBADF`.
/// - `RENAME_EXCHANGE` → `-ENOSYS` (no atomic-swap surface yet).
/// - `RENAME_WHITEOUT` → `-EINVAL` (recognised but unsupported).
/// - Unknown flag bits → `-EINVAL`.
/// - `RENAME_NOREPLACE` honoured via a pre-walk: if `newpath`
///   resolves successfully, the arm short-circuits with `-EEXIST`.
///
/// The dispatch routes through `FsOps::rename(old_parent, old_name,
/// new_parent, new_name, &guard)`. The in-tree tmpfs surface only
/// supports same-directory rename today; cross-directory rename
/// surfaces as `-ENOSYS` from the backend.
pub(super) async fn sys_renameat2<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let olddirfd = args[0] as i32;
    let oldpath_uaddr = args[1];
    let newdirfd = args[2] as i32;
    let newpath_uaddr = args[3];
    let flags = args[4] as u32;
    if olddirfd != AT_FDCWD || newdirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
    // Validate flags. RENAME_EXCHANGE → ENOSYS (atomic swap unsupported).
    // RENAME_WHITEOUT and any unrecognised bits → EINVAL.
    if (flags & RENAME_EXCHANGE) != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    let recognised = RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT;
    if (flags & !recognised) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (flags & RENAME_WHITEOUT) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let oldpath = match read_user_cstr(&ctx.aspace, oldpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if oldpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let newpath = match read_user_cstr(&ctx.aspace, newpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    if newpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let cwd = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    // RENAME_NOREPLACE: the composite RenameOp handles path resolution
    // internally; the flag acts as a post-resolution collision check
    // inside the op. RENAME_EXCHANGE was rejected above.
    let cred = ctx.walker_cred();
    let result = {
        let guard = step_engine::guard();
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = RenameOp {
            rooted_at: &cwd,
            oldpath: &oldpath,
            newpath: &newpath,
            cred: &cred,
            guard: &guard,
            state: None,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}
