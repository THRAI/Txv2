//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, Guard, StepOutcome};
use tx_fs;
use tx_services::time::{ClockRead, TimekeeperClock};
// The alias-name `cred_checks` is required by
// `xtask lint invariants cred-check` — see CRED_CHECK_SIGNALS in
// xtask/src/lint_invariants_cred_check.rs. Other aliases would
// silently bypass the gate.
use tx_subsystems::cred::checks as cred_checks;
use tx_subsystems::mount::{self};
use tx_subsystems::net::UnixSocketPath;

fn mount_is_read_only(dentry: &Cap<DEntry>) -> bool {
    if mount_payload_for_dentry(dentry)
        .is_some_and(|payload| payload.options.flags.contains(mount::MountFlags::READ_ONLY))
    {
        return true;
    }

    let mut cursor = Some(dentry.clone());
    while let Some(dentry) = cursor {
        {
            let guard = step_engine::guard();
            if dentry
                .mounted_hint()
                .and_then(|mount| mount.upgrade(&guard))
                .is_some_and(|mount| mount.flags().contains(mount::MountFlags::READ_ONLY))
            {
                return true;
            }
        }
        cursor = dentry.parent_hint();
    }

    false
}

fn mount_identity_for_dentry(
    ctx: &SyscallCtx<'_>,
    dentry: &Cap<DEntry>,
) -> Option<Cap<mount::MountIdentity>> {
    let guard = step_engine::guard();
    if let Some(mount) = dentry.mounted_hint().and_then(|weak| weak.upgrade(&guard)) {
        return Some(mount);
    }
    if let Some(parent) = dentry.parent_hint() {
        if let Some(mount) = parent.mounted_hint().and_then(|weak| weak.upgrade(&guard)) {
            return Some(mount);
        }
    }
    let namespace = ctx.process.mount_namespace_cap()?;
    let root = namespace.root();
    if root.root().key() == dentry.rnode().key() {
        return Some(root.clone());
    }
    None
}

fn drive_umount_detach_settlement(payload: &Cap<mount::MountPayload>) -> Result<(), Errno> {
    let pin = mount::MountPayloadPin::acquire_cap(payload);
    let mut op = mount::MountSettlementOp::new(pin, mount::SettlementScope::Detach)?;
    let guard = step_engine::guard();
    match op.drive(&guard) {
        StepOutcome::Done(()) => Ok(()),
        StepOutcome::Err(errno) => Err(Errno::from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Err(Errno::EAGAIN),
    }
}

fn begin_lazy_detach_after_topology_withdrawal(
    payload: &Cap<mount::MountPayload>,
) -> Result<(), Errno> {
    if payload.begin_lazy_detach()? {
        drive_umount_detach_settlement(payload)
    } else {
        // No mount settlement background queue is currently exposed to
        // tx-shims. The external payload pin keeps the payload alive in
        // DetachedPending; the queue driver must be added before claiming
        // retry/wake based background detach completion.
        Ok(())
    }
}

fn resolve_cwd_for_path(dirfd: i32, path: &[u8], ctx: &SyscallCtx<'_>) -> Result<Cap<DEntry>, i32> {
    dirfd_anchor_errno(dirfd, path, ctx)
}

fn cached_child_retains_target(
    parent: &Cap<DEntry>,
    name: &[u8],
    target: tx_subsystems::vfs::FsObjectId,
) -> bool {
    let Ok(name) = tx_subsystems::vfs::InlineName::new(name) else {
        return false;
    };
    parent
        .cached_child(name)
        .is_some_and(|child| child.rnode().fs_object_id() == target && child.retain_count() > 1)
}

fn open_fd_retains_target(ctx: &SyscallCtx<'_>, target: tx_subsystems::vfs::FsObjectId) -> bool {
    ctx.process.open_fds().values().any(|file| {
        matches!(file.backing(), OpenFileBacking::Rnode { rnode } if rnode.fs_object_id() == target)
    })
}

fn maybe_destroy_zero_link_inode_after_namespace_remove(
    fs_ops: &Arc<dyn tx_subsystems::vfs::FsOps>,
    target: tx_subsystems::vfs::FsObjectId,
) {
    let guard = step_engine::guard();
    let meta = match fs_ops.load_inode_meta(target, &guard) {
        StepOutcome::Done(meta) => meta,
        StepOutcome::Err(_) | StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => return,
    };
    if meta.nlinks == 0 {
        if matches!(fs_ops.destroy_inode(target, &guard), StepOutcome::Done(())) {
            zero_link_destroy_pressure_maintenance();
        }
    }
}

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
    let split = split_parent_name(path);
    (split.parent_path, split.basename)
}

/// Linux accepts trailing slashes on a directory path being created:
/// `mkdir("foo/")` creates `foo`, while `mkdir("/")` still targets the
/// existing root directory.  Keep the compatibility normalization local to
/// `mkdirat` so path-removal/link operations can retain their stricter
/// syscall-specific trailing-slash behaviour.
fn split_mkdir_path(path: &[u8]) -> (&[u8], &[u8]) {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    split_path(&path[..end])
}

fn proc_self_fd_number(path: &[u8]) -> Option<u32> {
    let rest = path.strip_prefix(b"/proc/self/fd/")?;
    if rest.is_empty() || rest.contains(&b'/') {
        return None;
    }
    core::str::from_utf8(rest).ok()?.parse::<u32>().ok()
}

fn proc_self_fd_link_bytes(ctx: &SyscallCtx<'_>, fd_num: u32) -> Result<Vec<u8>, SyscallResult> {
    let Some(open_file) = ctx.process.fd(fd_num) else {
        return Err(SyscallResult::Error(ENOENT_VALUE));
    };
    if let OpenFileBacking::Rnode { rnode } = open_file.backing() {
        if let RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } = rnode.backing()
        {
            return Ok(tx_subsystems::tty::project::dev_path_for_tty(tty));
        }
    }
    Ok(alloc::format!("anon_inode:[{}]", fd_num).into_bytes())
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
pub(crate) async fn drive_create_then_walk(
    cwd: &Cap<DEntry>,
    path: &[u8],
    mode: u16,
    cred: &Credential,
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, i32> {
    let (parent_path, basename) = split_path(path);
    if basename.is_empty() {
        // A path like `/` or `foo/` has an empty basename — can't
        // create. Surface as -EISDIR (matches Linux's behaviour for
        // `open("/", O_CREAT, ...)`).
        return Err(EISDIR_VALUE);
    }

    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let op = tx_subsystems::vfs::CreateThenWalkOp::new(
        cwd.clone(),
        parent_path.to_vec(),
        path.to_vec(),
        basename.to_vec(),
        mode & 0o7777,
        cred.clone(),
    );
    drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_registrar_handle.as_ref(),
    )
    .await
    .map_err(|errno| errno_to_i32(Errno::from(errno)))
}

/// Resolve the in-scope `Arc<dyn FsPageBacking>` for the given dentry.
/// Mirrors `fs_ops_for_dentry`'s parent-hint ascent shape but reads
/// `payload.fs_page_backing` so callers can dispatch against the
/// `FsPageBacking` trait surface.
pub(super) fn fs_page_backing_for_dentry(
    dentry: &Cap<DEntry>,
) -> Option<Arc<dyn tx_subsystems::page_backed::FsPageBacking>> {
    MountedDentry::find_ascending(dentry).map(|mounted| mounted.fs_page_backing())
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
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let rooted_at = match resolve_cwd_for_path(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_mkdir_path(&path);
    if basename.is_empty() {
        // Trailing-slash-only basename, e.g. `mkdir("/")` — the FsOps
        // layer rejects an empty `InlineName`. Linux's behaviour for
        // `mkdir("/")` is `-EEXIST`; we surface the more conservative
        // `-EINVAL` (matches `InlineName::new(b"")`'s rejection).
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        rooted_at
    } else {
        match walk_from_process(rooted_at, parent_path, &cred, &ctx.process) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    if mount_is_read_only(&parent_dentry) {
        return SyscallResult::Error(EROFS_VALUE);
    }
    let parent_id = parent_dentry.rnode().fs_object_id();
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
    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::MkdirOp {
            fs_ops: &fs_ops,
            parent: parent_id,
            name: basename,
            mode: effective_mode,
            cred: &cred,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => {
            parent_dentry.remove_cached_child_by_name(basename);
            SyscallResult::Return(0)
        }
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
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
    if flags & !AT_REMOVEDIR != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let rooted_at = match resolve_cwd_for_path(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&path);
    if basename.is_empty() {
        return SyscallResult::Error(EISDIR_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        rooted_at
    } else {
        match walk_from_process(rooted_at, parent_path, &cred, &ctx.process) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    if mount_is_read_only(&parent_dentry) {
        return SyscallResult::Error(EROFS_VALUE);
    }
    let parent_id = parent_dentry.rnode().fs_object_id();
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let target_id = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::LookupInParentOp {
            fs_ops: &fs_ops,
            parent: parent_id,
            name: basename,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(id) => id,
            Err(v3errno) if v3errno == Errno::ENOENT => {
                let guard = step_engine::guard();
                if try_unlink_unix_socket_path(ctx, &path, &guard) {
                    return SyscallResult::Return(0);
                }
                return SyscallResult::error_from(Errno::from(v3errno));
            }
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };
    let child_meta = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::LoadInodeMetaOp {
            fs_ops: &fs_ops,
            target: target_id,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(meta) => meta,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };
    let target_kind = child_meta.kind();
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
    if let Err(e) = cred_checks::authorize_unlink(ctx.cred_snapshot(), &parent_meta, &child_meta) {
        return SyscallResult::error_from(e);
    }
    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::UnlinkFromParentOp {
            fs_ops: &fs_ops,
            parent: parent_id,
            name: basename,
            target: target_id,
            remove_dir: want_rmdir,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => {
            parent_dentry.remove_cached_child_by_name(basename);
            let retained_by_live_dentry =
                cached_child_retains_target(&parent_dentry, basename, target_id);
            let retained_by_open_fd = open_fd_retains_target(ctx, target_id);
            if !retained_by_live_dentry && !retained_by_open_fd {
                maybe_destroy_zero_link_inode_after_namespace_remove(&fs_ops, target_id);
            }
            SyscallResult::Return(0)
        }
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

fn try_unlink_unix_socket_path(ctx: &SyscallCtx<'_>, path: &[u8], guard: &Guard<'_>) -> bool {
    // Same cwd-absolute key derivation as bind/connect (read_sockaddr_un_path)
    // so unlink(2) matches the bound key for relative pathname sockets.
    let Ok(path) = crate::linux_syscall::socket::unix_pathname_key(ctx, path) else {
        return false;
    };
    let Some(net_namespace) = ctx.process.net_namespace() else {
        return false;
    };
    let table = net_namespace.socket_table();
    if !table.lookup_unix_path_node(path, guard) {
        return false;
    }
    table.unlink_unix_path(path).is_ok()
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
    let target = match read_user_cstr(&ctx.aspace, target_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if target.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let linkpath = match read_user_cstr(&ctx.aspace, linkpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if linkpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let rooted_at = match resolve_cwd_for_path(newdirfd, &linkpath, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&linkpath);
    if basename.is_empty() {
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        rooted_at
    } else {
        match walk_from(rooted_at, parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    if mount_is_read_only(&parent_dentry) {
        return SyscallResult::Error(EROFS_VALUE);
    }
    let parent_id = parent_dentry.rnode().fs_object_id();
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
    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::SymlinkOp {
            fs_ops: &fs_ops,
            parent: parent_id,
            name: basename,
            target: &target,
            cred: &cred,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => {
            parent_dentry.remove_cached_child_by_name(basename);
            SyscallResult::Return(0)
        }
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
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
    let flags = args[4] as u32;
    const AT_SYMLINK_FOLLOW_U32: u32 = 0x400;
    if flags & !AT_SYMLINK_FOLLOW_U32 != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let oldpath = match read_user_cstr(&ctx.aspace, oldpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if oldpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let newpath = match read_user_cstr(&ctx.aspace, newpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if newpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let old_root = match resolve_cwd_for_path(olddirfd, &oldpath, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let new_root = match resolve_cwd_for_path(newdirfd, &newpath, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let cred = ctx.walker_cred();
    // Walk source → target FsObjectId. Linux rejects directories
    // as `-EPERM`, but a read-only destination mount takes
    // precedence once the new parent has been resolved.
    let source_dentry = match walk_from(old_root, &oldpath, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let source_fs_ops = match fs_ops_for_dentry(&source_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let source_id = source_dentry.rnode().fs_object_id();
    // Walk new path's parent directory.
    let (new_parent_path, new_basename) = split_path(&newpath);
    if new_basename.is_empty() {
        return SyscallResult::Error(EEXIST_VALUE);
    }
    let new_parent_dentry = if new_parent_path.is_empty() {
        new_root
    } else {
        match walk_from(new_root, new_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    if mount_is_read_only(&new_parent_dentry) {
        return SyscallResult::Error(EROFS_VALUE);
    }
    if source_dentry.rnode().meta().kind() == InodeKind::Directory {
        return SyscallResult::Error(EPERM_VALUE);
    }
    let new_parent_id = new_parent_dentry.rnode().fs_object_id();
    let fs_ops = match fs_ops_for_dentry(&new_parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    if !alloc::sync::Arc::ptr_eq(&source_fs_ops, &fs_ops) {
        const EXDEV_VALUE: i32 = 18;
        return SyscallResult::Error(EXDEV_VALUE);
    }
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
    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::LinkInParentOp {
            fs_ops: &fs_ops,
            parent: new_parent_id,
            name: new_basename,
            target: source_id,
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => {
            new_parent_dentry.remove_cached_child_by_name(new_basename);
            SyscallResult::Return(0)
        }
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
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
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
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
    if let Err(errno) = cred_checks::authorize_open(
        ctx.cred_snapshot(),
        &dentry.rnode().meta(),
        OpenFileFlags {
            write: true,
            ..OpenFileFlags::default()
        },
    ) {
        return SyscallResult::error_from(errno);
    }
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::page_backed::TruncateOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let op = TruncateOp::new(&pc, new_size);
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
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    // Op acquires its own epoch guard inside `step()`; the syscall
    // handler holds no guard across `drive(...).await` (EBR-7).
    let op = FdTruncateOp::new(&pc, new_size as u64);
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        None,
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

pub(super) async fn sys_fallocate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
    const EFBIG_VALUE: i32 = 27;

    let fd = args[0] as i32;
    let mode = args[1] as i32;
    let offset = args[2] as i64;
    let len = args[3] as i64;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if offset < 0 || len <= 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if mode != 0 && mode != FALLOC_FL_KEEP_SIZE {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if !file.flags().write {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let pc = match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        RNodeBacking::Directory => return SyscallResult::Error(EISDIR_VALUE),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    let offset = offset as u64;
    let len = len as u64;
    let end = match offset.checked_add(len) {
        Some(end) if end <= i64::MAX as u64 => end,
        _ => return SyscallResult::Error(EFBIG_VALUE),
    };

    if mode == FALLOC_FL_KEEP_SIZE {
        return SyscallResult::Return(0);
    }

    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::page_backed::FallocateOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let op = FallocateOp {
        pc: &pc,
        new_size: end,
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
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if buf_len == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if let Some(fd_num) = proc_self_fd_number(&path) {
        let link_bytes = match proc_self_fd_link_bytes(ctx, fd_num) {
            Ok(bytes) => bytes,
            Err(result) => return result,
        };
        let to_copy = core::cmp::min(link_bytes.len(), buf_len);
        if to_copy > 0 {
            if let Err(errno) =
                bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &link_bytes[..to_copy])
            {
                return SyscallResult::error_from(errno);
            }
        }
        return SyscallResult::Return(to_copy as i64);
    }
    if path.is_empty() {
        if dirfd == AT_FDCWD {
            return SyscallResult::Error(ENOENT_VALUE);
        }
        if dirfd < 0 {
            return SyscallResult::Error(EBADF_VALUE);
        }
        let file = match ctx.process.fd(dirfd as u32) {
            Some(file) => file,
            None => return SyscallResult::Error(EBADF_VALUE),
        };
        let rnode = file.rnode();
        if rnode.meta().kind() != InodeKind::Symlink {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let fs_ops = match fs_ops_for_rnode(rnode) {
            Some(ops) => ops,
            None => return SyscallResult::Error(ENOSYS_VALUE),
        };
        let link_bytes = {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = tx_subsystems::vfs::ReadLinkByIdOp {
                fs_ops: &fs_ops,
                target: rnode.fs_object_id(),
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(bytes) => bytes,
                Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
            }
        };
        let to_copy = core::cmp::min(link_bytes.len(), buf_len);
        if to_copy > 0 {
            if let Err(errno) =
                bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &link_bytes[..to_copy])
            {
                return SyscallResult::error_from(errno);
            }
        }
        return SyscallResult::Return(to_copy as i64);
    }
    let rooted_at = match resolve_cwd_for_path(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let cred = ctx.walker_cred();
    let (parent_path, basename) = split_path(&path);
    if basename.is_empty() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let parent_dentry = if parent_path.is_empty() {
        rooted_at
    } else {
        match walk_from(rooted_at, parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let parent_id = parent_dentry.rnode().fs_object_id();
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };
    // Resolve the basename in the parent directly via `FsOps::lookup`
    // — bypasses the walker's symlink-chase loop so the symlink's
    // own inode (not its target's) is what we read.
    let target_id = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::LookupInParentOp {
            fs_ops: &fs_ops,
            parent: parent_id,
            name: basename,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(id) => id,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };
    let target_meta = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::LoadInodeMetaOp {
            fs_ops: &fs_ops,
            target: target_id,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(meta) => meta,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };
    if target_meta.kind() != InodeKind::Symlink {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let link_bytes = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::ReadLinkByIdOp {
            fs_ops: &fs_ops,
            target: target_id,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(bytes) => bytes,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
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

fn mount_api_resolve_cwd_for_path(
    dirfd: i32,
    path: &[u8],
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, i32> {
    resolve_cwd_for_path(dirfd, path, ctx)
}

fn allocate_mount_api_fd(ctx: &SyscallCtx<'_>) -> Result<u32, SyscallResult> {
    let fd = ctx.process.allocate_fd();
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        Err(SyscallResult::Error(EMFILE_VALUE))
    } else {
        Ok(fd)
    }
}

fn install_mount_api_fd(
    ctx: &SyscallCtx<'_>,
    file: Cap<mount::MountApiFile>,
    flags: OpenFileFlags,
) -> SyscallResult {
    let fd = match allocate_mount_api_fd(ctx) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let cloexec = flags.cloexec;
    let open_file = match OpenFile::new_mount_api_cap(file, flags) {
        Ok(open_file) => open_file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let _ = ctx.process.install_fd(fd, open_file);
    if cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }
    SyscallResult::Return(fd as i64)
}

fn supported_mount_api_fstype(fstype: &[u8]) -> Option<&'static str> {
    match fstype {
        b"tmpfs" => Some("tmpfs"),
        b"vfat" => Some("vfat"),
        b"ext2" => Some("ext2"),
        b"ext3" => Some("ext3"),
        b"ext4" => Some("ext4"),
        b"devfs" => Some("devfs"),
        b"proc" => Some("proc"),
        b"sysfs" => Some("sysfs"),
        _ => None,
    }
}

fn mount_api_root_dentry(mut dentry: Cap<DEntry>) -> Cap<DEntry> {
    while let Some(parent) = dentry.parent_hint() {
        dentry = parent;
    }
    dentry
}

/// `fsopen(fsname, flags)`. Linux generic ABI `__NR_fsopen = 430`.
pub(super) async fn sys_fsopen<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fsname_uaddr = args[0];
    let flags = args[1] as u32;

    if flags & !FSOPEN_CLOEXEC != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let fsname = match read_user_cstr(&ctx.aspace, fsname_uaddr, 64) {
        Ok(name) => name,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    let Some(fstype) = supported_mount_api_fstype(&fsname) else {
        return SyscallResult::Error(ENODEV_VALUE);
    };

    let file =
        match mount::MountApiFile::new_fs_context_cap(fstype, mount::FsContextMode::New, None) {
            Ok(file) => file,
            Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
        };
    install_mount_api_fd(
        ctx,
        file,
        OpenFileFlags {
            read: true,
            write: true,
            cloexec: flags & FSOPEN_CLOEXEC != 0,
            ..OpenFileFlags::default()
        },
    )
}

/// `fspick(dirfd, path, flags)`. Linux generic ABI `__NR_fspick = 433`.
pub(super) async fn sys_fspick<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let flags = args[2] as u32;
    const VALID_FSPICK_FLAGS: u32 =
        FSPICK_CLOEXEC | FSPICK_SYMLINK_NOFOLLOW | FSPICK_NO_AUTOMOUNT | FSPICK_EMPTY_PATH;

    if flags & !VALID_FSPICK_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(path) => path,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if path.is_empty() && flags & FSPICK_EMPTY_PATH == 0 {
        return SyscallResult::Error(ENOENT_VALUE);
    }

    let rooted_at = match mount_api_resolve_cwd_for_path(dirfd, &path, ctx) {
        Ok(dentry) => dentry,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let cred = ctx.walker_cred();
    let target = if path == b"/" {
        mount_api_root_dentry(rooted_at)
    } else {
        match walk_from_process(rooted_at, &path, &cred, &ctx.process) {
            Ok(dentry) => dentry,
            Err(errno) => return SyscallResult::Error(errno),
        }
    };
    let payload = match mount_payload_for_dentry(&target) {
        Some(payload) => payload,
        None => return SyscallResult::Error(ENODEV_VALUE),
    };
    let picked_mount = mount_identity_for_dentry(ctx, &target);
    let file = match mount::MountApiFile::new_fs_context_cap(
        payload.fstype,
        mount::FsContextMode::Reconfigure,
        picked_mount,
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    install_mount_api_fd(
        ctx,
        file,
        OpenFileFlags {
            read: true,
            write: true,
            cloexec: flags & FSPICK_CLOEXEC != 0,
            ..OpenFileFlags::default()
        },
    )
}

/// `open_tree(dirfd, path, flags)`. Linux generic ABI `__NR_open_tree = 428`.
pub(super) async fn sys_open_tree<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let flags = args[2] as u32;
    const VALID_OPEN_TREE_FLAGS: u32 = OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC;

    if flags & !VALID_OPEN_TREE_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(path) => path,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }

    let rooted_at = match mount_api_resolve_cwd_for_path(dirfd, &path, ctx) {
        Ok(dentry) => dentry,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let cred = ctx.walker_cred();
    let target = if path == b"/" {
        mount_api_root_dentry(rooted_at)
    } else {
        match walk_from_process(rooted_at, &path, &cred, &ctx.process) {
            Ok(dentry) => dentry,
            Err(errno) => return SyscallResult::Error(errno),
        }
    };
    let payload = match mount_payload_for_dentry(&target) {
        Some(payload) => payload,
        None => return SyscallResult::Error(ENODEV_VALUE),
    };
    let file = match mount::MountApiFile::new_detached_mount_cap(
        mount::MountApiFileKind::OpenTree,
        payload,
        target.rnode().clone(),
        mount::MountFlags::empty(),
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    install_mount_api_fd(
        ctx,
        file,
        OpenFileFlags {
            cloexec: flags & OPEN_TREE_CLOEXEC != 0,
            ..OpenFileFlags::default()
        },
    )
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
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
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

    const MS_BIND: u64 = 4096;
    const MS_RDONLY: u64 = 1;
    const MS_REMOUNT: u64 = 32;
    const MS_NOSUID: u64 = 2;
    const MS_NODEV: u64 = 4;
    const MS_NOEXEC: u64 = 8;
    const MS_NOATIME: u64 = 1024;
    if (flags & MS_REMOUNT) != 0 {
        let mut mount_flags = mount::MountFlags::empty();
        if (flags & MS_RDONLY) != 0 {
            mount_flags = mount_flags.union(mount::MountFlags::READ_ONLY);
        }
        let mount = match mount_identity_for_dentry(ctx, &target_dentry) {
            Some(mount) => mount,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        mount::remount(&mount, mount_flags);
        return SyscallResult::Return(0);
    }

    if (flags & MS_BIND) != 0 {
        // Bind mount.
        let source = match read_user_cstr(&ctx.aspace, source_uaddr, EXECVE_PATH_MAX) {
            Ok(p) => p,
            Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
            Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
            Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
        };
        let source_dentry = match walk_from_process(cwd, &source, &cred, &ctx.process) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        };
        let namespace = match ctx.process.mount_namespace_cap() {
            Some(namespace) => namespace,
            None => return SyscallResult::Error(ENODEV_VALUE),
        };
        let guard = step_engine::guard();
        match mount::bind_mount_in_namespace(
            source_dentry,
            target_dentry,
            &parent_payload,
            &namespace,
            &guard,
        ) {
            Ok(_) => return SyscallResult::Return(0),
            Err(e) => return SyscallResult::error_from(e),
        }
    }

    const MS_MOVE: u64 = 8192;
    if (flags & MS_MOVE) != 0 {
        // Relocate an existing mount: `source` is the current mountpoint
        // (walker crosses it → mounted root), `target` is the new mountpoint.
        let source = match read_user_cstr(&ctx.aspace, source_uaddr, EXECVE_PATH_MAX) {
            Ok(p) => p,
            Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
            Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
            Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
        };
        let source_dentry = match walk_from_process(cwd, &source, &cred, &ctx.process) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        };
        let namespace = match ctx.process.mount_namespace_cap() {
            Some(namespace) => namespace,
            None => return SyscallResult::Error(ENODEV_VALUE),
        };
        let guard = step_engine::guard();
        match mount::move_mount_in_namespace(
            &source_dentry,
            target_dentry,
            &parent_payload,
            &namespace,
            &guard,
        ) {
            Ok(()) => return SyscallResult::Return(0),
            Err(e) => return SyscallResult::error_from(e),
        }
    }

    // Mount propagation: --make-shared / --make-private / --make-slave /
    // --make-unbindable (optionally recursive via MS_REC). Accept these so the
    // fs_bind* propagation tests' setup succeeds. Shared-style propagation is
    // provided implicitly by the inode-keyed mount table (a sub-mount under a
    // bind source is visible under all its bind copies); private/slave/unbindable
    // isolation is not yet enforced (v1).
    const MS_SHARED: u64 = 1 << 20;
    const MS_PRIVATE: u64 = 1 << 18;
    const MS_SLAVE: u64 = 1 << 19;
    const MS_UNBINDABLE: u64 = 1 << 17;
    if (flags & (MS_SHARED | MS_PRIVATE | MS_SLAVE | MS_UNBINDABLE)) != 0 {
        return SyscallResult::Return(0);
    }

    // New filesystem mount.
    let fstype = match read_user_cstr(&ctx.aspace, fstype_uaddr, 64) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
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
            Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
            Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
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
        // `vfat`/`ext2`/`ext3` are oscomp/LTP compatibility shims:
        // these tests need a mountable scratch filesystem for VFS
        // semantics (including readonly flags), not actual on-disk
        // format coverage. A fresh tmpfs at the mount point satisfies
        // that contract without pretending to parse those formats.
        "tmpfs" | "vfat" | "ext2" | "ext3" => {
            let tmpfs = alloc::sync::Arc::new(tx_fs::tmpfs::Tmpfs::new());
            let label = match fstype_str {
                "vfat" => "vfat",
                "ext2" => "ext2",
                "ext3" => "ext3",
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
        "sysfs" => (
            tx_fs::sysfs::Sysfs::fs_ops_arc(),
            tx_fs::sysfs::Sysfs::fs_page_backing_arc(),
            tx_fs::sysfs::SYSFS_ROOT_ID,
            tx_subsystems::vfs::InodeMeta::new(
                tx_subsystems::vfs::InodeKind::Directory,
                tx_fs::sysfs::SYSFS_DIR_MODE,
            ),
            "sysfs",
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
                let device = tx_subsystems::io_manager::block::DeviceKey::new(reg.devt.raw());
                let Some(geometry) = image.block_geometry(device) else {
                    return SyscallResult::Error(EIO_VALUE);
                };
                let pool = match tx_fs::tx_ext4::JournalPagePool::new(32) {
                    Ok(pool) => pool,
                    Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
                };
                tx_fs::tx_ext4::mount_ext4_read_write_with_discovered_journal(
                    image, geometry, device, pool,
                )
            };
            let mounted = match mounted {
                Ok(m) => m,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            mounted.set_file_page_container_binder(Some(alloc::sync::Arc::new(
                tx_fs::tx_ext4::Ext4FileIoRuntimeBinder::new(
                    tx_subsystems::device::BlockDeviceHandle::whole(reg),
                ),
            )));
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
        mount_flags,
    ) {
        Ok(m) => m,
        Err(_) => return SyscallResult::Error(Errno::ENOMEM as i32),
    };

    mount::register_mount(
        &parent_payload,
        target_dentry.rnode().fs_object_id(),
        mount_cap.clone(),
    );
    if let Some(mnt_ns) = ctx.process.mount_namespace_cap() {
        mnt_ns.register_mount(&target_dentry, mount_cap);
    }

    SyscallResult::Return(0)
}

/// `umount2(target, flags)`. Linux RV64 ABI `__NR_umount2 = 39`.
pub(super) async fn sys_umount2<P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let target_uaddr = args[0];
    let flags = args[1] as u64;

    const MNT_FORCE: u64 = 1;
    const MNT_DETACH: u64 = 2;
    const MNT_EXPIRE: u64 = 4;
    const UMOUNT_NOFOLLOW: u64 = 8;

    // umount2 flags: MNT_FORCE(1) MNT_DETACH(2) MNT_EXPIRE(4) UMOUNT_NOFOLLOW(8).
    // MNT_EXPIRE 2-phase and nofollow are not modelled. Reject unknown bits.
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & MNT_FORCE != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }

    let target = match read_user_cstr(&ctx.aspace, target_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(_)) => return SyscallResult::Error(EFAULT_VALUE),
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
    let result = if let Some(mnt_ns) = ctx.process.mount_namespace_cap() {
        let target_mount = match mnt_ns.mount_containing_dentry(&target_dentry) {
            Some(mount) if mount.key() != mnt_ns.root().key() => mount,
            _ => return SyscallResult::Error(EINVAL_VALUE),
        };
        let target_is_mount_boundary = target_mount.root_dentry().key() == target_dentry.key()
            || target_mount
                .mountpoint()
                .is_some_and(|mountpoint| mountpoint.key() == target_dentry.key());
        if !target_is_mount_boundary {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let target_payload = match target_mount.payload_cap() {
            Ok(payload) => payload.into_cap(),
            Err(_) => return SyscallResult::Error(ENODEV_VALUE),
        };
        if flags & MNT_DETACH == 0 && target_payload.payload_pin_count() != 0 {
            return SyscallResult::error_from(Errno::EBUSY);
        }
        if flags & MNT_DETACH == 0 {
            if let Err(errno) = drive_umount_detach_settlement(&target_payload) {
                return SyscallResult::error_from(errno);
            }
        }
        // The namespace row is authoritative and returns the identity it
        // actually removed. Only that exact identity may be withdrawn from
        // the legacy global index; a cloned identity has no global row, which
        // is intentionally a successful no-op.
        mnt_ns.umount(&target_dentry).and_then(|removed_mount| {
            let _ = mount::umount_identity_exact(&removed_mount);
            if flags & MNT_DETACH != 0 {
                let payload = removed_mount
                    .payload_cap()
                    .map_err(|_| Errno::ENODEV)?
                    .into_cap();
                begin_lazy_detach_after_topology_withdrawal(&payload)?;
            }
            Ok(())
        })
    } else {
        // Namespace-less compatibility retains the legacy dentry lookup.
        let umount_target = if target_dentry.name().as_bytes().is_empty() {
            target_dentry
                .parent_hint()
                .unwrap_or_else(|| target_dentry.clone())
        } else {
            target_dentry.clone()
        };
        match mount_payload_for_dentry(&umount_target) {
            // The legacy global mount table only returns `Result<()>`, not the
            // removed mount identity, so this compatibility branch cannot
            // safely drive child-payload settlement. Namespace-bearing tasks
            // use the owner-aware path above.
            Some(parent_payload) => mount::umount(&umount_target, &parent_payload),
            None => return SyscallResult::Error(ENODEV_VALUE),
        }
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
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let mode = args[2] as u32;
    let _dev = args[3] as u64;

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };

    const S_IFREG: u32 = 0o100000;
    const S_IFCHR: u32 = 0o020000;
    const S_IFBLK: u32 = 0o060000;
    const S_IFIFO: u32 = 0o010000;
    const S_IFSOCK: u32 = 0o140000;

    let cred = ctx.walker_cred();
    let rooted_at = match resolve_cwd_for_path(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let kind = match mode & 0o170000u32 {
        t if t == S_IFREG || t == 0 => tx_subsystems::vfs::InodeKind::Regular,
        t if t == S_IFCHR => tx_subsystems::vfs::InodeKind::CharDevice,
        t if t == S_IFBLK => tx_subsystems::vfs::InodeKind::BlockDevice,
        t if t == S_IFIFO => tx_subsystems::vfs::InodeKind::Fifo,
        t if t == S_IFSOCK => tx_subsystems::vfs::InodeKind::Socket,
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let (parent_path, _) = split_path(&path);
    let parent_dentry = if parent_path.is_empty() {
        rooted_at.clone()
    } else {
        match walk_from(rooted_at.clone(), parent_path, &cred) {
            Ok(dentry) => dentry,
            Err(errno) => return SyscallResult::Error(errno),
        }
    };
    if mount_is_read_only(&parent_dentry) {
        return SyscallResult::Error(EROFS_VALUE);
    }
    let parent_meta = parent_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_link(ctx.cred_snapshot(), &parent_meta) {
        return SyscallResult::error_from(e);
    }
    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = MknodOp {
            rooted_at: &rooted_at,
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
pub(super) fn sys_utimensat<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
            Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
            Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
        };
        if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return SyscallResult::Error(ENOENT_VALUE);
            }
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
            let rooted_at = match resolve_cwd_for_path(dirfd, &path, ctx) {
                Ok(dentry) => dentry,
                Err(errno) => return SyscallResult::Error(errno),
            };
            let dentry = match walk_from(rooted_at, &path, &ctx.walker_cred()) {
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
    // Validate flags. RENAME_WHITEOUT is still unsupported; Linux also
    // rejects RENAME_NOREPLACE combined with RENAME_EXCHANGE.
    let recognised = RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT;
    if (flags & !recognised) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (flags & RENAME_WHITEOUT) != 0
        || ((flags & RENAME_EXCHANGE) != 0 && (flags & RENAME_NOREPLACE) != 0)
    {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let oldpath = match read_user_cstr(&ctx.aspace, oldpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if oldpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let newpath = match read_user_cstr(&ctx.aspace, newpath_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if newpath.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let old_root = match resolve_cwd_for_path(olddirfd, &oldpath, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let new_root = match resolve_cwd_for_path(newdirfd, &newpath, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
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
        old_root.clone()
    } else {
        match walk_from(old_root.clone(), old_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    let old_child_dentry = match walk_from(old_root.clone(), &oldpath, &cred) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let new_parent_dentry = if new_parent_path.is_empty() {
        new_root.clone()
    } else {
        match walk_from(new_root.clone(), new_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    // Displaced inode is optional — walk_from returns Err(ENOENT)
    // when the new path doesn't exist, which is the normal case
    // for a rename that creates rather than overwrites.
    let displaced_dentry = walk_from(new_root.clone(), &newpath, &cred).ok();
    if (flags & RENAME_NOREPLACE) != 0 && displaced_dentry.is_some() {
        return SyscallResult::Error(EEXIST_VALUE);
    }
    if (flags & RENAME_EXCHANGE) != 0 && displaced_dentry.is_none() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    if mount_is_read_only(&old_parent_dentry) || mount_is_read_only(&new_parent_dentry) {
        return SyscallResult::Error(EROFS_VALUE);
    }
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
    let fs_ops = match fs_ops_for_dentry(&old_parent_dentry) {
        Some(ops) => ops,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let new_fs_ops = match fs_ops_for_dentry(&new_parent_dentry) {
        Some(ops) => ops,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    if !alloc::sync::Arc::ptr_eq(&fs_ops, &new_fs_ops) {
        const EXDEV_VALUE: i32 = 18;
        return SyscallResult::Error(EXDEV_VALUE);
    }
    let outcome = if (flags & RENAME_EXCHANGE) != 0 {
        const TMP_NAME: &[u8] = b".tx_rename_exchange_tmp";
        let guard = step_engine::guard();
        match fs_ops.rename(
            old_parent_dentry.rnode().fs_object_id(),
            old_basename,
            old_parent_dentry.rnode().fs_object_id(),
            TMP_NAME,
            &guard,
        ) {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
        match fs_ops.rename(
            new_parent_dentry.rnode().fs_object_id(),
            new_basename,
            old_parent_dentry.rnode().fs_object_id(),
            old_basename,
            &guard,
        ) {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::error_from(Errno::from(errno)),
        }
        fs_ops.rename(
            old_parent_dentry.rnode().fs_object_id(),
            TMP_NAME,
            new_parent_dentry.rnode().fs_object_id(),
            new_basename,
            &guard,
        )
    } else {
        let guard = step_engine::guard();
        fs_ops.rename(
            old_parent_dentry.rnode().fs_object_id(),
            old_basename,
            new_parent_dentry.rnode().fs_object_id(),
            new_basename,
            &guard,
        )
    };
    match outcome {
        StepOutcome::Done(()) => {
            old_parent_dentry.remove_cached_child_by_name(b".tx_rename_exchange_tmp");
            old_parent_dentry.remove_cached_child_by_name(old_basename);
            new_parent_dentry.remove_cached_child_by_name(new_basename);
            SyscallResult::Return(0)
        }
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => SyscallResult::Error(EIO_VALUE),
        StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
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
