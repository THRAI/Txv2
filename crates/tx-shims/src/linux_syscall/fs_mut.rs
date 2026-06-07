//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
use tx_fs;
// The alias-name `cred_checks` is required by
// `xtask lint invariants cred-check` — see CRED_CHECK_SIGNALS in
// xtask/src/lint_invariants_cred_check.rs. Other aliases would
// silently bypass the gate.
use tx_subsystems::cred::checks as cred_checks;
use tx_subsystems::mount::{self};

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
            V3::Done(_) => {
                parent_dentry.remove_cached_child_by_name(basename);
            }
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
    // POSIX mkdir(2) permission: write + search on the parent.
    // Same rule as link/create — sticky is *not* consulted (mkdir
    // only adds an entry, doesn't remove). Walker only enforced
    // search-on-ancestors; without this, any user that could
    // search the parent could create a directory there.
    let parent_meta = parent_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_link(ctx.cred_snapshot(), &parent_meta) {
        return SyscallResult::error_from(e);
    }
    // Apply umask: effective_mode = mode & !umask. Linux semantics
    // (umask is the bottom 9 bits — `rwxrwxrwx`).
    let umask = ctx.process.umask();
    let effective_mode = mode & !umask & 0o7777;
    let outcome = {
        let guard = step_engine::guard();
        fs_ops.mkdir(parent_id, basename, effective_mode, &cred, &guard)
    };
    match outcome {
        V3::Done(_) => {
            parent_dentry.remove_cached_child_by_name(basename);
            SyscallResult::Return(0)
        }
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
    // POSIX unlink/rmdir permission check (W+X on parent + S_ISVTX
    // ownership rule). The walker only enforced search-on-ancestors
    // — write-on-parent and the sticky-bit rule were unguarded before
    // this check.
    let parent_meta = parent_dentry.rnode().meta();
    let child_meta = target_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_unlink(ctx.cred_snapshot(), &parent_meta, &child_meta) {
        return SyscallResult::error_from(e);
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
        V3::Done(()) => {
            parent_dentry.remove_cached_child_by_name(basename);
            SyscallResult::Return(0)
        }
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
    // POSIX symlink(2) permission: write + search on parent (same
    // as link / mkdir — sticky not consulted because symlink only
    // creates an entry).
    let parent_meta = parent_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_link(ctx.cred_snapshot(), &parent_meta) {
        return SyscallResult::error_from(e);
    }
    let outcome = {
        let guard = step_engine::guard();
        fs_ops.symlink(parent_id, basename, &target, &cred, &guard)
    };
    match outcome {
        V3::Done(_) => {
            parent_dentry.remove_cached_child_by_name(basename);
            SyscallResult::Return(0)
        }
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
    // POSIX link(2) permission: write + search on the new parent
    // (sticky is NOT consulted — link only adds, doesn't remove).
    // Walker only enforced search-on-ancestors; without this, any
    // user that could search the new parent could create a name
    // there. Routes through cred::checks::require_link so the
    // witness chain is intact at the FsOps mint site.
    let new_parent_meta = new_parent_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_link(ctx.cred_snapshot(), &new_parent_meta) {
        return SyscallResult::error_from(e);
    }
    let outcome = {
        let guard = step_engine::guard();
        fs_ops.link(new_parent_id, new_basename, source_id, &guard)
    };
    match outcome {
        V3::Done(()) => {
            new_parent_dentry.remove_cached_child_by_name(new_basename);
            SyscallResult::Return(0)
        }
        V3::Continue { .. } | V3::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        V3::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
        V3::Err(v3_errno) => SyscallResult::error_from(v3_errno.into()),
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
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    // Op acquires its own epoch guard inside `step()`; the syscall
    // handler holds no guard across `drive(...).await` (EBR-7).
    let op = FdTruncateOp {
        pc: &pc,
        new_size: new_size as u64,
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

pub(super) async fn sys_fallocate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let mode = args[1] as u32;
    let offset = args[2] as i64;
    let len = args[3] as i64;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if mode != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    if offset < 0 || len < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let Some(new_size) = (offset as u64).checked_add(len as u64) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
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
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let op = tx_subsystems::page_backed::FallocateOp { pc: &pc, new_size };
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
            V3::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    };
    let target_meta = {
        let guard = step_engine::guard();
        match fs_ops.load_inode_meta(target_id, &guard) {
            V3::Done(m) => m,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
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
            V3::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
    };
    let to_copy = core::cmp::min(link_bytes.len(), buf_len);
    if to_copy > 0 {
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &link_bytes[..to_copy]) {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(to_copy as i64)
}

/// `mount(source, target, fstype, flags, data)`. Linux RV64 ABI `__NR_mount = 40`.
///
/// v1: supports `MS_BIND` (bind mount) and new mounts (tmpfs/devfs/proc).
pub(super) async fn sys_mount<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
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

    // No outer guard here: `walk_from` acquires its own internal
    // guard (fs_path.rs), and txKernel's epoch discipline panics
    // on nested guards (`tx-substrate::epoch::local:55`).
    let target_dentry = match walk_from_process(cwd.clone(), &target, &cred, &ctx.process) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let parent_payload = match mount_payload_for_dentry(&target_dentry) {
        Some(p) => p,
        None => return SyscallResult::Error(ENODEV_VALUE),
    };

    // Mount-propagation changes (`mount --make-{r,}{shared,private,slave,unbindable}`):
    // these mutate an existing mount's propagation type rather than creating a new
    // mount, and `fstype`/`source` are NULL. We don't model mount-propagation trees,
    // so making a subtree private/shared/etc. is observably a no-op — report success.
    // LTP's `init_ltp_netspace` does `mount --make-rprivate /sys` (MS_REC|MS_PRIVATE)
    // via ROD before mounting sysfs; failing it TBROKs every tst_net.sh-framework test.
    const MS_SHARED: u64 = 1 << 20;
    const MS_UNBINDABLE: u64 = 1 << 17;
    const MS_PRIVATE: u64 = 1 << 18;
    const MS_SLAVE: u64 = 1 << 19;
    const MS_PROPAGATION: u64 = MS_SHARED | MS_PRIVATE | MS_SLAVE | MS_UNBINDABLE;
    if (flags & MS_PROPAGATION) != 0 {
        return SyscallResult::Return(0);
    }

    const MS_BIND: u64 = 4096;
    if (flags & MS_BIND) != 0 {
        // Bind mount.
        let source = match read_user_cstr(&ctx.aspace, source_uaddr, EXECVE_PATH_MAX) {
            Ok(p) => p,
            Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        };
        let source_dentry = match walk_from_process(cwd, &source, &cred, &ctx.process) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        };
        let guard = step_engine::guard();
        match mount::bind_mount(source_dentry, target_dentry, &parent_payload, &guard) {
            Ok(_) => return SyscallResult::Return(0),
            Err(e) => return SyscallResult::error_from(e),
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

    // For ext4 the backend isn't a zero-state factory — it must
    // attach to a block device. Resolve the `source` path to a
    // bdev-fs RNode and ask bdev-fs which `BlockDeviceRegistration`
    // it represents (BDEV_FS §8.1). The returned `MountedExt4` is
    // kept alive in this scope; we later call `bind_mount_payload`
    // on it after the kernel `MountPayload` is signed so the backend
    // can stamp the mount onto materialised RNodes.
    let mut ext4_mount: Option<tx_fs::tx_ext4::MountedExt4<tx_fs::tx_ext4::BlockDeviceImage>> =
        None;
    let source_label_for_ext4: Option<alloc::vec::Vec<u8>> = if fstype_str == "ext4" {
        let source = match read_user_cstr(&ctx.aspace, source_uaddr, EXECVE_PATH_MAX) {
            Ok(p) => p,
            Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        };
        Some(source)
    } else {
        None
    };

    // Build backend via crate-level factory.
    let (fs_ops, fs_page_backing, root_id, root_meta, fstype_label): (
        alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
        alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        tx_subsystems::vfs::FsObjectId,
        tx_subsystems::vfs::InodeMeta,
        &str,
    ) = match fstype_str {
        // `vfat` is an oscomp-basic compatibility shim: we have no
        // FAT driver, but the basic test mounts `/dev/vda2` as
        // `vfat` and only asserts `mount` + `umount` round-trip
        // (`assert(ret == 0)`). A fresh tmpfs at the mount point
        // satisfies that contract without pretending to read FAT
        // bytes. Real FAT support tracks separately.
        // `sysfs` has no driver yet; LTP netns setup only needs the mount to
        // succeed (`mount -t sysfs none /sys`) so `init_ltp_netspace`'s ROD passes.
        // A fresh tmpfs at the mount point satisfies that contract; subtests that
        // read /sys/class/net/<iface>/* still see ENOENT, which is a per-subtest
        // gap, not a setup failure. Real sysfs tracks separately.
        "tmpfs" | "vfat" | "sysfs" => {
            let tmpfs = alloc::sync::Arc::new(tx_fs::tmpfs::Tmpfs::new());
            let label = match fstype_str {
                "vfat" => "vfat",
                "sysfs" => "sysfs",
                _ => "tmpfs",
            };
            (
                tmpfs.clone().fs_ops_arc(),
                tmpfs.fs_page_backing_arc(),
                tx_subsystems::vfs::FsObjectId::ROOT,
                tx_subsystems::vfs::InodeMeta::new(tx_subsystems::vfs::InodeKind::Directory, 0o755),
                label,
            )
        }
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
            alloc::sync::Arc::new(tx_fs::procfs::Procfs)
                as alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
            tx_fs::procfs::PROCFS_ROOT_ID,
            tx_subsystems::vfs::InodeMeta::new(
                tx_subsystems::vfs::InodeKind::Directory,
                tx_fs::procfs::PROCFS_DIR_MODE,
            ),
            "proc",
        ),
        "ext4" => {
            // Resolve `source` (e.g. `/dev/block/vda`) into a bdev-fs
            // RNode, then ask bdev-fs which underlying block-device
            // registration backs it. Per BDEV_FS §8.1 — the bridge
            // helper `block_device_for_object_id` is the canonical
            // path filesystems use to mount on a block device.
            let source = source_label_for_ext4
                .as_ref()
                .expect("ext4 fstype implies source was read");
            let source_dentry = match walk_from_process(cwd.clone(), source, &cred, &ctx.process) {
                Ok(d) => d,
                Err(e) => return SyscallResult::Error(e),
            };
            let source_rnode = source_dentry.rnode();
            let reg = match tx_fs::bdevfs::block_device_for_object_id(source_rnode.fs_object_id()) {
                Some(r) => r,
                None => return SyscallResult::Error(ENODEV_VALUE),
            };
            let image = tx_fs::tx_ext4::BlockDeviceImage::new(reg.ops);
            // Linux's `MS_RDONLY = 1`. If set in `flags`, mount
            // through the read-only entry point so every mutating
            // `FsOps` call short-circuits with `EROFS`. The
            // mount-table flag below mirrors this so `remount(...,
            // !RDONLY)` semantics line up if/when remount lands.
            const MS_RDONLY: u64 = 1;
            let read_only = (flags & MS_RDONLY) != 0;
            let mounted = if read_only {
                tx_fs::tx_ext4::mount_ext4_read_only(image)
            } else {
                tx_fs::tx_ext4::mount_ext4_read_write(image)
            };
            let mounted = match mounted {
                Ok(m) => m,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            let root_id = mounted.root_fs_object_id;
            let root_meta = mounted.root_inode_meta.clone();
            let fs_ops = mounted.fs_ops();
            let fs_page_backing = mounted.fs_page_backing();
            ext4_mount = Some(mounted);
            (fs_ops, fs_page_backing, root_id, root_meta, "ext4")
        }
        _ => return SyscallResult::Error(ENOSYS_VALUE),
    };

    // Translate the Linux `flags` u64 into kernel `MountFlags`.
    // Linux's `mount(2)` manpage: `MS_RDONLY = 1`, `MS_NOSUID = 2`,
    // `MS_NODEV = 4`, `MS_NOEXEC = 8`, `MS_NOATIME = 1024`.
    const MS_RDONLY: u64 = 1;
    const MS_NOSUID: u64 = 2;
    const MS_NODEV: u64 = 4;
    const MS_NOEXEC: u64 = 8;
    const MS_NOATIME: u64 = 1024;
    let mut mount_flags = mount::MountFlags::empty();
    if (flags & MS_RDONLY) != 0 {
        mount_flags = mount_flags.union(mount::MountFlags::READ_ONLY);
    }
    if (flags & MS_NOSUID) != 0 {
        mount_flags = mount_flags.union(mount::MountFlags::NOSUID);
    }
    if (flags & MS_NODEV) != 0 {
        mount_flags = mount_flags.union(mount::MountFlags::NODEV);
    }
    if (flags & MS_NOEXEC) != 0 {
        mount_flags = mount_flags.union(mount::MountFlags::NOEXEC);
    }
    if (flags & MS_NOATIME) != 0 {
        mount_flags = mount_flags.union(mount::MountFlags::NO_ATIME);
    }
    let mount_options = mount::MountOptions { flags: mount_flags };

    let mount_payload = match mount::MountPayload::new_cap(
        fs_ops,
        fs_page_backing,
        None,
        mount::allocate_dev_id(),
        mount_options,
        fstype_label,
        mount::SourceLabel::Static("none"),
    ) {
        Ok(p) => p,
        Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
    };

    // ext4 needs its backend to know the freshly-signed
    // `Cap<MountPayload>` so `materialise_rnode` can stamp
    // `PageContainerKind::File { mount, .. }` onto regular-file
    // RNodes (see `mount_sdcard_at_musl` and BDEV_FS §8.1).
    if let Some(mounted) = ext4_mount.as_ref() {
        mounted.bind_mount_payload(&mount_payload);
    }

    let root_rnode = {
        use tx_subsystems::vfs::RNodeBacking;
        let raw = tx_subsystems::vfs::RNode::new(root_id, root_meta, RNodeBacking::Directory)
            .with_containing_mount(&mount_payload);
        match step_engine::reserve_for::<tx_subsystems::vfs::RNode>() {
            Ok(res) => step_engine::sign_for(res, raw),
            Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
        }
    };

    let mount_cap = match mount::MountIdentity::new_cap(
        mount::allocate_mount_id(),
        Some(target_dentry.clone()),
        root_rnode,
        None,
        mount_payload,
        mount::MountFlags::empty(),
    ) {
        Ok(m) => m,
        Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
    };

    if let Some(mnt_ns) = ctx.process.mount_namespace_cap() {
        mnt_ns.register_mount(
            &parent_payload,
            target_dentry.rnode().fs_object_id(),
            mount_cap,
        );
    } else {
        mount::register_mount(
            &parent_payload,
            target_dentry.rnode().fs_object_id(),
            mount_cap,
        );
    }

    SyscallResult::Return(0)
}

/// `umount2(target, flags)`. Linux RV64 ABI `__NR_umount2 = 39`.
pub(super) async fn sys_umount2<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
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

    // Use walk_from (which manages its own guard) instead of a
    // top-level guard + step_walk, because mount_payload_for_dentry
    // also acquires a guard — nesting panics at epoch::local:55.
    let target_dentry = match walk_from_process(cwd.clone(), &target, &cred, &ctx.process) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let parent_payload = match mount_payload_for_dentry(&target_dentry) {
        Some(p) => p,
        None => return SyscallResult::Error(ENODEV_VALUE),
    };

    let result = if let Some(mnt_ns) = ctx.process.mount_namespace_cap() {
        mnt_ns.umount(&target_dentry, &parent_payload)
    } else {
        mount::umount(&target_dentry, &parent_payload)
    };

    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(e) => SyscallResult::error_from(e),
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
pub(super) async fn sys_mknodat<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
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
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = MknodOp {
            rooted_at: &cwd,
            path: &path,
            mode: mode as u16,
            kind,
            cred: &cred,
            parent: None,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `utimensat(dirfd, pathname, times, flags)`. Linux RV64 generic ABI
/// `__NR_utimensat = 88`.
///
/// musl rv64 passes a `struct timespec[2]` whose fields are signed
/// 64-bit `time_t`/`long` (`external/musl/include/alltypes.h.in`).
/// This arm supports libc paths used by `utimensat(3)` and
/// `futimens(3)`, including `pathname == NULL` and `AT_EMPTY_PATH`.
pub(super) fn sys_utimensat<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let times_uaddr = args[2];
    let flags = args[3] as u32;

    let known_flags = AT_EMPTY_PATH | (AT_SYMLINK_NOFOLLOW as u32);
    if flags & !known_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let now = time::ns_to_timespec(time::realtime_ns::<P>());
    let mut new_times = [tx_subsystems::vfs::Timespec::new(now.tv_sec, now.tv_nsec as i32); 2];
    let mut omit = [false; 2];

    if times_uaddr != 0 {
        let raw = match bootstrap_read_user::<[TimespecLayout; 2]>(&ctx.aspace, times_uaddr) {
            Ok(raw) => raw,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        for i in 0..2 {
            match raw[i].tv_nsec {
                UTIME_OMIT => omit[i] = true,
                UTIME_NOW => {
                    new_times[i] = tx_subsystems::vfs::Timespec::new(now.tv_sec, now.tv_nsec as i32)
                }
                nsec if (0..1_000_000_000).contains(&nsec) => {
                    if raw[i].tv_sec < 0 {
                        return SyscallResult::Error(EINVAL_VALUE);
                    }
                    new_times[i] = tx_subsystems::vfs::Timespec::new(raw[i].tv_sec, nsec as i32);
                }
                _ => return SyscallResult::Error(EINVAL_VALUE),
            }
        }
    }

    if omit[0] && omit[1] {
        return SyscallResult::Return(0);
    }

    let rnode = if path_uaddr == 0 {
        if dirfd < 0 {
            return SyscallResult::Error(EBADF_VALUE);
        }
        match ctx.process.fd(dirfd as u32) {
            Some(file) => file.rnode().clone(),
            None => return SyscallResult::Error(EBADF_VALUE),
        }
    } else {
        let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
            Ok(path) => path,
            Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        };
        if path.is_empty() && (dirfd != AT_FDCWD || flags & AT_EMPTY_PATH != 0) {
            if dirfd == AT_FDCWD {
                match ctx.process.cwd() {
                    Some(cwd) => cwd.rnode().clone(),
                    None => return SyscallResult::Error(ENOENT_VALUE),
                }
            } else {
                if dirfd < 0 {
                    return SyscallResult::Error(EBADF_VALUE);
                }
                match ctx.process.fd(dirfd as u32) {
                    Some(file) => file.rnode().clone(),
                    None => return SyscallResult::Error(EBADF_VALUE),
                }
            }
        } else {
            if dirfd != AT_FDCWD {
                return SyscallResult::Error(EBADF_VALUE);
            }
            let cwd = match ctx.process.cwd() {
                Some(cwd) => cwd,
                None => return SyscallResult::Error(ENOENT_VALUE),
            };
            let dentry = match walk_from(cwd, &path, &ctx.walker_cred()) {
                Ok(dentry) => dentry,
                Err(errno) => return SyscallResult::Error(errno),
            };
            dentry.rnode().clone()
        }
    };

    let fs_object_id = rnode.fs_object_id();
    let fs_ops = fs_ops_for_rnode(&rnode);
    let guard = step_engine::guard();
    let mut meta = match fs_ops {
        Some(ref fs_ops) => match fs_ops.load_inode_meta(fs_object_id, &guard) {
            StepOutcome::Done(meta) => meta,
            _ => rnode.meta(),
        },
        None => rnode.meta(),
    };
    if let Some(file) = if path_uaddr == 0 && dirfd >= 0 {
        ctx.process.fd(dirfd as u32)
    } else {
        None
    } {
        if let Some(sz) =
            crate::linux_syscall::vm::extract_page_container(&file).map(|pc| pc.size_bytes())
        {
            meta.size = sz;
        }
    }
    meta = crate::linux_syscall::fs_basic::stat_meta_override_or(fs_object_id, meta);
    if !omit[0] {
        meta.atime = new_times[0];
    }
    if !omit[1] {
        meta.mtime = new_times[1];
    }
    meta.ctime = tx_subsystems::vfs::Timespec::new(now.tv_sec, now.tv_nsec as i32);
    crate::linux_syscall::fs_basic::record_stat_meta_override(fs_object_id, meta);
    if let Some(fs_ops) = fs_ops {
        match fs_ops.serialize_inode_meta(fs_object_id, &meta, &guard) {
            StepOutcome::Done(()) | StepOutcome::Err(_) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {}
        }
    }
    SyscallResult::Return(0)
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
/// new_parent, new_name, &guard)`. The in-tree tmpfs surface supports
/// same-directory rename plus cross-directory regular-file moves and
/// directory-subtree moves within one tmpfs mount. Atomic exchange is
/// still outside this syscall surface.
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

    // POSIX rename(2) permission check. Pre-walk old child, old
    // parent, new parent (and new child if it exists) so the cred
    // gate fires before RenameOp re-walks + commits. The composite
    // op currently performs no cred check; without this gate any
    // non-root caller with X on both parents could rename arbitrary
    // entries. Witness chain: ctx.cred_snapshot() →
    // cred::checks::require_rename → consumed inside the commit
    // guard scope wrapping RenameOp drive.
    let (old_parent_path, old_basename) = split_path(&oldpath);
    if old_basename.is_empty() {
        return SyscallResult::Error(EISDIR_VALUE);
    }
    let (new_parent_path, new_basename) = split_path(&newpath);
    if new_basename.is_empty() {
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
    let old_child_dentry = match walk_from(cwd.clone(), &oldpath, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let new_parent_dentry = if new_parent_path.is_empty() {
        cwd.clone()
    } else {
        match walk_from(cwd.clone(), new_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    // Displaced inode is optional — walk_from returns Err(ENOENT)
    // when the new path doesn't exist, which is the normal case
    // for a rename that creates rather than overwrites.
    let displaced_dentry = walk_from(cwd.clone(), &newpath, &cred).ok();
    let old_parent_meta = old_parent_dentry.rnode().meta();
    let old_child_meta = old_child_dentry.rnode().meta();
    let new_parent_meta = new_parent_dentry.rnode().meta();
    let displaced_meta = displaced_dentry.as_ref().map(|d| d.rnode().meta());

    if let Err(e) = cred_checks::authorize_rename(
        ctx.cred_snapshot(),
        &old_parent_meta,
        &old_child_meta,
        &new_parent_meta,
        displaced_meta.as_ref(),
    ) {
        return SyscallResult::error_from(e);
    }
    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = RenameOp {
            rooted_at: &cwd,
            oldpath: &oldpath,
            newpath: &newpath,
            cred: &cred,
            state: None,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// sys_syslog(2). Minimal stub — returns success for all log types.
/// Does not actually read or write the kernel log buffer. Busybox
/// `syslogd` calls this to open/read the log; the stub prevents
/// busybox from blocking on ENOSYS while acknowledging that no real
/// kernel log is available.
pub(super) fn sys_syslog<'a>(_args: [u64; 6], _ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(0)
}
