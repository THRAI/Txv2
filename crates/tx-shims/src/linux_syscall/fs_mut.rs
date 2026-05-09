//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;

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
        let guard = tx_substrate::epoch::guard();
        use tx_substrate::step_v3::StepOutcome as V3;
        let outcome =
            poll_walker_synchronously(step_walk(cwd.clone(), parent_path, cred, &guard));
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
        use tx_substrate::step_v3::StepOutcome as V3;
        let guard = tx_substrate::epoch::guard();
        let outcome =
            fs_ops.create_inode(parent_fs_object_id, basename, new_mode, cred, &guard);
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
    let guard = tx_substrate::epoch::guard();
    use tx_substrate::step_v3::StepOutcome as V3;
    let outcome = poll_walker_synchronously(step_walk(cwd.clone(), path, cred, &guard));
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
    let guard = tx_substrate::epoch::guard();
    let mut cursor: Cap<DEntry> = dentry.clone();
    loop {
        if let Some(weak) = cursor.rnode().containing_mount_weak() {
            if let Some(payload) = weak.upgrade(&guard) {
                return Some(payload.fs_page_backing.clone());
            }
        }
        let next = cursor.parent_hint().and_then(|w| w.upgrade(&guard))?;
        cursor = next;
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
    use tx_substrate::step_v3::StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    // Apply umask: effective_mode = mode & !umask. Linux semantics
    // (umask is the bottom 9 bits — `rwxrwxrwx`).
    let umask = ctx.process.umask();
    let effective_mode = mode & !umask & 0o7777;
    let outcome = {
        let guard = tx_substrate::epoch::guard();
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
    use tx_substrate::step_v3::StepOutcome as V3;
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
        let guard = tx_substrate::epoch::guard();
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
    use tx_substrate::step_v3::StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = tx_substrate::epoch::guard();
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
    use tx_substrate::step_v3::StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&new_parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = tx_substrate::epoch::guard();
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
        let guard = tx_substrate::epoch::guard();
        tx_subsystems::page_backed::step_truncate(&pc, new_size, &guard)
    };
    use tx_substrate::step_v3::StepOutcome as V3;
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
pub(super) fn sys_ftruncate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        tx_subsystems::page_backed::step_truncate(&pc, new_size, &guard)
    };
    use tx_substrate::step_v3::StepOutcome as V3;
    match outcome {
        V3::Done(()) | V3::Continue { .. } => SyscallResult::Return(0),
        V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(v3_errno) => SyscallResult::Error(errno_to_i32(v3_errno.into())),
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
    use tx_substrate::step_v3::StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };
    // Resolve the basename in the parent directly via `FsOps::lookup`
    // — bypasses the walker's symlink-chase loop so the symlink's
    // own inode (not its target's) is what we read.
    let target_id = {
        let guard = tx_substrate::epoch::guard();
        match fs_ops.lookup(parent_id, basename, &guard) {
            V3::Done(id) => id,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };
    let target_meta = {
        let guard = tx_substrate::epoch::guard();
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
        let guard = tx_substrate::epoch::guard();
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
    let cred = ctx.walker_cred();
    let (old_parent_path, old_basename) = split_path(&oldpath);
    let (new_parent_path, new_basename) = split_path(&newpath);
    if old_basename.is_empty() || new_basename.is_empty() {
        return SyscallResult::Error(EISDIR_VALUE);
    }
    let old_parent_dentry = if old_parent_path.is_empty() {
        cwd.clone()
    } else {
        match walk_from(cwd.clone(), old_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let new_parent_dentry = if new_parent_path.is_empty() {
        cwd.clone()
    } else {
        match walk_from(cwd.clone(), new_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    // RENAME_NOREPLACE pre-check: walk the full new path; if it
    // resolves, the rename must fail with -EEXIST (Linux semantic).
    if (flags & RENAME_NOREPLACE) != 0 && walk_from(cwd, &newpath, &cred).is_ok() {
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let old_parent_id = old_parent_dentry.rnode().fs_object_id();
    let new_parent_id = new_parent_dentry.rnode().fs_object_id();
    use tx_substrate::step_v3::StepOutcome as V3;
    let fs_ops = match fs_ops_for_dentry(&old_parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        fs_ops.rename(
            old_parent_id,
            old_basename,
            new_parent_id,
            new_basename,
            &guard,
        )
    };
    match outcome {
        V3::Done(()) => SyscallResult::Return(0),
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::Error(errno_to_i32(Errno::from(errno))),
    }
}
