//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
// The alias-name `cred_checks` is required by
// `xtask lint invariants cred-check` — see CRED_CHECK_SIGNALS in
// xtask/src/lint_invariants_cred_check.rs.
use tx_subsystems::cred::checks as cred_checks;
use tx_subsystems::mount::MountPayload;
use tx_subsystems::vfs::structure::OpenFileBacking;

// =====================================================================
// Wave 4 Part 4 of the DAC + setuid slice — file-mode syscall arms.
//
// Each arm decodes a `dirfd` arg (only `AT_FDCWD` is supported in the
// slice — non-CWD dirfds return `-EBADF` because the trio's day-1 fd
// table doesn't carry directory-fd semantics yet) and a path string
// from the user pointer (bounded inline at `EXECVE_PATH_MAX = 4096`,
// mirroring the existing trio bootstrap exemption), resolves the path
// through the walker, and dispatches to the FsOps method (Wave 3 Part
// 2) or the inline `access(2)` predicate over the inode meta.
//
// `chmod` / `chown` use `walker_cred()` (POSIX path-resolution rule:
// effective ids); `access(2)` defaults to **real** ids per POSIX
// (`AT_EACCESS` flag in `faccessat2` switches to effective).
//
// Cites: `txdoc:VFS-CHECKS-PERMISSIONS-1` and the DAC + setuid plan
// §"Part 4 — File-mode syscall arms".
// =====================================================================

/// Resolve `dirfd + path` to the final `Cap<DEntry>` via the walker.
///
/// Slice surface: only `AT_FDCWD` is supported. Real dirfd-relative
/// resolution requires directory file descriptors — the trio's
/// fd-table doesn't carry them yet. Non-AT_FDCWD dirfds produce
/// `-EBADF`. A missing cwd (init pre-rootfs / zombie) also returns
/// `-EBADF` defensively (the alive caller of these arms has a cwd
/// installed by `step_chdir`).
///
/// Walker errors are translated through `errno_to_i32`. Walker
/// `Blocked` shapes don't fire under tmpfs/devfs (the FS surface is
/// synchronous in this slice), so they're flattened to `-EIO`
/// defensively rather than awaited — none of the file-mode arms park
/// today.
///
/// We return the terminal `Cap<DEntry>` (not the bare `Cap<RNode>`)
/// so callers can locate the in-scope `FsOps` via the
/// parent-hint chain. Per the walker's `materialise_child_rnode`
/// shape, freshly-resolved child rnodes do **not** carry a
/// `with_containing_mount` weak — the walker tracks `current_fs_ops`
/// internally via the parent dentry chain. Callers that need to
/// dispatch through `FsOps::chmod_inode` etc. must ascend the dentry
/// chain to find an rnode with the mount weak set (the mount root
/// rnode, which `MountIdentity::new_cap` wires up).
///
/// ## Send-future discipline
///
/// `Guard` is `!Send + !Sync` by design — holding one across an
/// `.await` makes the future `!Send` and breaks the kernel's
/// `Reactor::submit_task` `Send` bound (cite:
/// `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` and
/// `tx_scripts::process::exec::poll_walker_synchronously`). We poll
/// the walker future once with a noop waker rather than awaiting it.
/// Every in-tree walker backend resolves synchronously today; if a
/// future async-aware backend lands, `poll_walker_synchronously`'s
/// `panic!` arm fires and this site must shift to the canonical
/// "fresh guard inside `await_*`" shape.
// `P` is unused in the body but kept on the signature so the dispatch
// arms keep their parameter passthrough shape; clippy's
// extra-unused-type-parameters gate is silenced via cfg_attr so the
// arch-lint substring check (`#[allow(`) does not also fire.
#[cfg_attr(not(test), allow(clippy::extra_unused_type_parameters))]
#[cfg_attr(test, allow(clippy::extra_unused_type_parameters))]
fn resolve_path_at<P: PmapIf>(
    dirfd: i32,
    path: &[u8],
    cred: &Credential,
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, i32> {
    ResolvedPath::at(dirfd, path, cred, ctx).map(ResolvedPath::into_dentry)
}

/// Poll a walker future synchronously, panicking if it returns
/// `Pending`. Mirrors `tx_scripts::process::exec::poll_walker_synchronously`
/// (which is private to that module). The walker module's docs
/// guarantee that every in-tree backend resolves immediately
/// (`crates/tx-subsystems/src/vfs/walker.rs:23` — "every `.await` is
/// a no-op today"); polling once with a noop waker therefore returns
/// `Ready` for every shipping FS backend. The borrowed `&Guard` only
/// lives across the synchronous poll body, not across an `.await`,
/// so the resulting parent future stays `Send`.
pub(crate) fn poll_walker_synchronously<F: core::future::Future>(future: F) -> F::Output {
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: the vtable above never dereferences the data pointer.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = pin!(future);
    match pinned.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!(
            "poll_walker_synchronously: walker returned Pending; in-tree walker \
             backends never await today (cite: vfs::walker module docs)."
        ),
    }
}

/// Translate an `Errno` from `chmod_inode` / `chown_inode` to the
/// dispatched `-errno` magnitude. Use the central Linux ABI table so
/// filesystem mutation failures stay visible to the syscall boundary
/// instead of being collapsed to `-EINVAL`.
pub(super) fn fs_change_errno_magnitude(errno: Errno) -> i32 {
    errno.linux_i32()
}

/// Resolve the in-scope `Arc<dyn FsOps>` for the given dentry.
/// Mirrors the walker's private `fs_ops_for` helper, but ascends the
/// `parent_hint` chain to find an rnode that carries
/// `with_containing_mount`. Freshly-materialised child rnodes don't
/// — the walker tracks `current_fs_ops` independently — so the only
/// rnodes carrying the mount weak today are mount roots
/// (`MountIdentity::new_cap` wires the link via
/// `with_containing_mount` on the root rnode). For an in-mount file,
/// the chain ascends to the mount root and returns its FS surface.
///
/// Returns `None` for orphan dentries (no parent-hint chain reaches
/// a rnode with a mount weak); the file-mode arms surface that as
/// `-EROFS` defensively (no FS to act through).
pub(super) fn fs_ops_for_dentry(
    dentry: &Cap<DEntry>,
) -> Option<Arc<dyn tx_subsystems::vfs::FsOps>> {
    MountedDentry::find_ascending(dentry).map(|mounted| mounted.fs_ops())
}

/// `fchmodat(dirfd, path, mode, flags)`. Linux RV64 generic ABI.
///
/// Wraps `FsOps::chmod_inode` (Wave 3 Part 2). Permission failures
/// surface as `-EPERM`; read-only filesystems (e.g. devfs) return
/// `-EROFS`. The `flags` argument (`AT_SYMLINK_NOFOLLOW`) is accepted
/// silently — chmod doesn't follow symlinks at this layer in the
/// slice anyway. Mode is masked to the bottom 12 bits (preserving
/// `S_ISUID`, `S_ISGID`, `S_ISVTX` plus `rwxrwxrwx`).
pub(super) fn sys_fchmodat<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    mode: u32,
    _flags: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }
    let rooted_at = match dirfd_anchor_errno(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let walker_cred = ctx.walker_cred();
    let requested_mode = (mode & 0o7777) as u16;

    // Cred check at the syscall arm using ctx.cred_snapshot().
    // Per-FS chmod_inode impls (tmpfs / devfs / bdevfs / procfs)
    // also enforce the rule internally; this is defense-in-depth at
    // the canonical cred::checks::* seam per cred_service_v_1 §"Cred
    // owns credential semantics". When the per-FS check is removed
    // in a follow-up, this remains the single enforcement site.
    let target_dentry =
        match ResolvedPath::from_process_root(rooted_at.clone(), &path, &walker_cred, &ctx.process)
        {
            Ok(resolved) => resolved.into_dentry(),
            Err(e) => return SyscallResult::Error(e),
        };
    let target_meta = target_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_chmod(ctx.cred_snapshot(), &target_meta, requested_mode)
    {
        return SyscallResult::error_from(e);
    }
    let new_mode = chmod_mode_after_linux_fsetid_clear(requested_mode, &target_meta, ctx);

    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = ChmodOp {
            rooted_at: &rooted_at,
            path: &path,
            mode: new_mode,
            cred: &walker_cred,
            target: Some(target_dentry),
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::Error(fs_change_errno_magnitude(Errno::from(v3errno))),
    }
}

/// `fchmod(fd, mode)`. Linux RV64 generic ABI.
///
/// Resolves the fd's rnode and applies the same authorization and
/// `FsOps::chmod_inode` mutation path as `fchmodat`, without adding
/// any path or symlink policy.
pub(super) fn sys_fchmod(fd: u32, mode: u32, ctx: &SyscallCtx<'_>) -> SyscallResult {
    let open_file = match ctx.process.fd(fd) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let rnode = match open_file.backing() {
        OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return SyscallResult::Error(EBADF_VALUE),
    };
    let requested_mode = (mode & 0o7777) as u16;
    let target_meta = rnode.meta();
    if let Err(e) = cred_checks::authorize_chmod(ctx.cred_snapshot(), &target_meta, requested_mode)
    {
        return SyscallResult::error_from(e);
    }
    let new_mode = chmod_mode_after_linux_fsetid_clear(requested_mode, &target_meta, ctx);
    let fs_ops = match crate::linux_syscall::fs_basic::fs_ops_for_rnode(&rnode) {
        Some(fs_ops) => fs_ops,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };
    let guard = step_engine::guard();
    match fs_ops.chmod_inode(rnode.fs_object_id(), new_mode, &ctx.walker_cred(), &guard) {
        StepOutcome::Done(()) => SyscallResult::Return(0),
        StepOutcome::Err(errno) => {
            SyscallResult::Error(fs_change_errno_magnitude(Errno::from(errno)))
        }
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => SyscallResult::Error(EIO_VALUE),
    }
}

fn chmod_mode_after_linux_fsetid_clear(
    requested_mode: u16,
    target_meta: &InodeMeta,
    ctx: &SyscallCtx<'_>,
) -> u16 {
    let cred = ctx.cred_snapshot().cred();
    if (requested_mode & S_ISGID) != 0 && cred.euid.raw() != 0 && cred.egid.raw() != target_meta.gid
    {
        requested_mode & !S_ISGID
    } else {
        requested_mode
    }
}

/// `fchownat(dirfd, path, uid, gid, flags)`. Linux RV64 generic ABI.
///
/// Wraps `FsOps::chown_inode` (Wave 3 Part 2). Each of `uid` / `gid`
/// decodes the `(u32) -1 == u32::MAX` "leave unchanged" sentinel to
/// `Option::None` (same convention as `setre{u,g}id`'s
/// `decode_uid_arg` / `decode_gid_arg`). Non-privileged callers may
/// only chown to their own uid/gid; arbitrary changes require
/// `CAP_FOWNER`.
pub(super) fn sys_fchownat<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    uid_arg: u32,
    gid_arg: u32,
    _flags: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    let walker_cred = ctx.walker_cred();
    let uid = decode_uid_arg(uid_arg).map(|u| u.0);
    let gid = decode_gid_arg(gid_arg).map(|g| g.0);
    let rooted_at = match dirfd_anchor_errno(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };

    // Cred check at the syscall arm using ctx.cred_snapshot(). Same
    // defense-in-depth role as sys_fchmodat — per-FS chown_inode
    // impls also enforce the rule.
    let target_dentry =
        match ResolvedPath::from_process_root(rooted_at.clone(), &path, &walker_cred, &ctx.process)
        {
            Ok(resolved) => resolved.into_dentry(),
            Err(e) => return SyscallResult::Error(e),
        };
    let target_meta = target_dentry.rnode().meta();
    if let Err(e) = cred_checks::authorize_chown(ctx.cred_snapshot(), &target_meta, uid, gid) {
        return SyscallResult::error_from(e);
    }

    let result = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = ChownOp {
            rooted_at: &rooted_at,
            path: &path,
            uid,
            gid,
            cred: &walker_cred,
            target: Some(target_dentry),
        };
        step_engine::drive_oneshot(&mut op, &mut script_ctx)
    };
    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::Error(fs_change_errno_magnitude(Errno::from(v3errno))),
    }
}

/// `fchown(fd, uid, gid)`. Linux RV64 generic ABI.
///
/// Resolves the fd's rnode and applies the same authorization,
/// `(u32)-1` sentinel decoding, and `FsOps::chown_inode` mutation path
/// as `fchownat`.
pub(super) fn sys_fchown(
    fd: u32,
    uid_arg: u32,
    gid_arg: u32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let open_file = match ctx.process.fd(fd) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let rnode = match open_file.backing() {
        OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return SyscallResult::Error(EBADF_VALUE),
    };
    let uid = decode_uid_arg(uid_arg).map(|u| u.0);
    let gid = decode_gid_arg(gid_arg).map(|g| g.0);
    let target_meta = rnode.meta();
    if let Err(e) = cred_checks::authorize_chown(ctx.cred_snapshot(), &target_meta, uid, gid) {
        return SyscallResult::error_from(e);
    }
    let fs_ops = match crate::linux_syscall::fs_basic::fs_ops_for_rnode(&rnode) {
        Some(fs_ops) => fs_ops,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };
    let guard = step_engine::guard();
    match fs_ops.chown_inode(rnode.fs_object_id(), uid, gid, &ctx.walker_cred(), &guard) {
        StepOutcome::Done(()) => SyscallResult::Return(0),
        StepOutcome::Err(errno) => {
            SyscallResult::Error(fs_change_errno_magnitude(Errno::from(errno)))
        }
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => SyscallResult::Error(EIO_VALUE),
    }
}

/// `faccessat(dirfd, path, mode)`. Linux RV64 generic ABI. POSIX
/// `access(2)` shape: the access check uses the caller's **real**
/// uid/gid (not effective). Implemented in terms of
/// [`sys_faccessat2_impl`] with `flags = 0`.
pub(super) fn sys_faccessat<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    mode: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    sys_faccessat2_impl::<P>(dirfd, path_uaddr, mode, 0, ctx)
}

/// `faccessat2(dirfd, path, mode, flags)`. Linux RV64 generic ABI.
/// Adds the `flags` argument over `faccessat`; `AT_EACCESS` switches
/// the check from real uid/gid to effective uid/gid.
pub(super) fn sys_faccessat2<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    mode: i32,
    flags: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    sys_faccessat2_impl::<P>(dirfd, path_uaddr, mode, flags, ctx)
}

/// Shared implementation for `faccessat` / `faccessat2`. The split
/// surface only exists for the dispatch ABI; both arms route here.
///
/// POSIX rule: `access(2)` defaults to checking against the caller's
/// **real** uid/gid (not effective). `AT_EACCESS` (only meaningful on
/// `faccessat2` — `faccessat` ignores `flags` entirely per Linux's
/// existing surface) switches to effective uid/gid.
///
/// `effective_caps` carries through both modes: `CAP_DAC_OVERRIDE`
/// always overrides the read/write checks. The execute-bit check
/// retains a Linux quirk: even with `CAP_DAC_OVERRIDE` set, `X_OK`
/// fails if **none** of the three octal-triplet execute bits is set
/// on the inode (matches Linux's `generic_permission` in
/// `fs/namei.c`).
///
/// `F_OK` (mode == 0) is the existence check: the syscall returns 0
/// after path resolution succeeds (no permission-bit check).
pub(super) fn sys_faccessat2_impl<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    mode: i32,
    flags: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };

    // Pick which uid/gid to check against per POSIX: `access(2)` and
    // `faccessat(2)` use the caller's real ids; `faccessat2` honours
    // the `AT_EACCESS` flag to opt into effective ids.
    let cred = ctx.cred();
    let (check_uid, check_gid) = if flags & AT_EACCESS != 0 {
        (cred.euid.raw(), cred.egid.raw())
    } else {
        (cred.uid.raw(), cred.gid.raw())
    };
    let walker_cred = Credential {
        uid: check_uid,
        gid: check_gid,
        effective_caps: cred.effective_caps,
    };

    // Resolve the path via AccessOp + drive_oneshot. Returns InodeMeta
    // for the DAC checks below.
    let rooted_at = match dirfd_anchor_for_path(dirfd, &path, ctx) {
        Ok(d) => d,
        Err(result) => return result,
    };
    let inode_meta = if is_caller_dev_tty_path(&path, &rooted_at) {
        match caller_dev_tty_stat_info(ctx) {
            Ok((meta, _, _, _)) => meta,
            Err(result) => return result,
        }
    } else {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = AccessOp {
            rooted_at: &rooted_at,
            path: &path,
            cred: &walker_cred,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(m) => m,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };
    let mode_bits = inode_meta.mode as u32;

    // F_OK: existence check only. Path resolution succeeded; return 0
    // without consulting the permission bits. Per POSIX `access(2)`
    // §RETURN VALUE.
    if mode == F_OK {
        return SyscallResult::Return(0);
    }

    // Compose the `want` mask out of the requested R/W/X bits, mapped
    // to the target inode's relevant octal triplet.
    let mut want: u32 = 0;
    if mode & R_OK != 0 {
        want |= 0o4;
    }
    if mode & W_OK != 0 {
        want |= 0o2;
    }
    if mode & X_OK != 0 {
        want |= 0o1;
    }

    let bits = if check_uid == inode_meta.uid {
        (mode_bits >> 6) & 0o7
    } else if check_gid == inode_meta.gid {
        (mode_bits >> 3) & 0o7
    } else {
        mode_bits & 0o7
    };

    let granted = if walker_cred
        .effective_caps
        .contains(Capability::DAC_OVERRIDE)
    {
        // CAP_DAC_OVERRIDE: read/write always granted; the X-bit
        // check retains the Linux quirk — `access(X_OK)` fails if no
        // execute bit is set anywhere on the inode (matches
        // `fs/namei.c::generic_permission`).
        if mode & X_OK != 0 && (mode_bits & 0o111) == 0 {
            return SyscallResult::Error(EACCES_VALUE);
        }
        want
    } else {
        bits & want
    };

    if granted == want {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(EACCES_VALUE)
    }
}

/// Decode the access-mode bits (`O_RDONLY`/`O_WRONLY`/`O_RDWR`) of an
/// `openat(2)` `flags` argument into the `(read, write)` pair. Linux's
/// `O_RDONLY = 0` reads as "read", `O_WRONLY = 1` as "write",
/// `O_RDWR = 2` as both. The historical `0o3` ("search") shape is
/// silently treated as `O_RDONLY` (we map it to `(true, false)`); no
/// shipping userspace emits it, but Linux historically tolerates it.
pub(super) fn decode_access_mode(flags: u32) -> (bool, bool) {
    match flags & numbers::O_ACCMODE {
        numbers::O_WRONLY => (false, true),
        numbers::O_RDWR => (true, true),
        // O_RDONLY (0) and the legacy "search" shape (0o3) both fall here.
        _ => (true, false),
    }
}

/// `chdir(path)`. Linux RV64 generic ABI `__NR_chdir = 49`.
///
/// Walks `path` from the caller's cwd, asserts the result is a
/// directory (`InodeKind::Directory`), then installs it via
/// `step_chdir`. Returns `0` on success.
///
/// - Empty path → `-ENOENT` (lets the walker surface the canonical
///   "no such directory" shape).
/// - Resolved path is a non-directory → `-ENOTDIR`.
/// - Walker errors forward through `errno_to_i32`.
pub(super) async fn sys_chdir<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let path_uaddr = args[0];

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
        Err(ReadCStrError::OutOfMemory) => return SyscallResult::Error(ENOMEM_VALUE),
        Err(ReadCStrError::Fault(errno)) => return SyscallResult::error_from(errno),
    };
    if path.is_empty() {
        return SyscallResult::Error(ENOENT_VALUE);
    }

    let cwd = match ctx.process.cwd_binding() {
        Some(binding) => binding,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let mount_namespace = match ctx.process.mount_namespace_cap() {
        Some(namespace) => namespace,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };
    let walker_cred = ctx.walker_cred();
    let resolved = {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = tx_subsystems::vfs::WalkInMountNamespaceWithOriginOp {
            rooted_at: cwd.dentry,
            origin_mount: cwd.mount,
            path,
            cred: walker_cred,
            mount_namespace,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(resolved) => resolved,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };

    if resolved.dentry.rnode().meta().kind() != InodeKind::Directory {
        return SyscallResult::Error(ENOTDIR_VALUE);
    }

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = ChdirOp {
        target: &ctx.process,
        new_cwd: resolved.dentry,
        new_mount: Some(resolved.mount),
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(ChdirOutcome::Replaced { .. }) => SyscallResult::Return(0),
        Ok(ChdirOutcome::ZombieIgnored) => SyscallResult::Error(ESRCH_VALUE),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `getcwd(buf, size)`. Linux RV64 generic ABI `__NR_getcwd = 17`.
///
/// Renders the cwd dentry's parent-hint chain into an absolute POSIX
/// path via `step_getcwd`, copies it (NUL terminator inclusive) into
/// the user buffer, and returns the byte count written.
///
/// - `size == 0` with `buf != NULL` → `-EINVAL` (Linux semantic).
/// - `buf == NULL` with `size != 0` → `-EFAULT`.
/// - rendered path + 1 (NUL) > size → `-ERANGE`.
/// - cwd unset / chain broken → `-ENOENT`.
pub(super) fn sys_getcwd<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    let size = args[1] as usize;

    if buf_uaddr == 0 {
        if size == 0 {
            // Linux returns -EINVAL for `getcwd(NULL, 0)` on the
            // syscall path; the glibc-side allocate-on-zero behaviour
            // is in libc, not the kernel.
            return SyscallResult::Error(EINVAL_VALUE);
        }
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if size == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = GetcwdOp {
        target: &ctx.process,
    };
    let path = match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(Some(p)) => p,
        Ok(None) => return SyscallResult::Error(ENOENT_VALUE),
        Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
    };
    // `path` is the rendered absolute path bytes (no NUL terminator);
    // `size` must accommodate `path.len() + 1` to fit the terminator.
    let needed = path.len().saturating_add(1);
    if needed > size {
        return SyscallResult::Error(ERANGE_VALUE);
    }

    // Build a NUL-terminated buffer in kernel memory, then copy out
    // through the canonical user-VA lane.
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(needed);
    buf.extend_from_slice(&path);
    buf.push(0);
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &buf) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(needed as i64)
}

/// `umask(mask)`. Linux RV64 generic ABI `__NR_umask = 166`.
///
/// Atomically swaps the per-process file-creation mask, returning the
/// previous value. Argument is silently truncated to `0o777` (the
/// bottom 9 bits — `rwxrwxrwx` only) per Linux semantics; the kernel
/// `umask(2)` ignores the kind / setuid / setgid / sticky bits.
///
/// Synchronous; never returns `-errno` (Linux's `umask(2)` always
/// succeeds in the live caller — zombies aren't reachable from a live
/// syscall arm).
pub(super) fn sys_umask<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let new_mask = args[0] as u16;
    let old = ctx.process.swap_umask(new_mask);
    SyscallResult::Return(old as i64)
}

// =====================================================================
// Slice 8 of the shell-prompt roadmap — file-mutation syscalls.
//
// Coverage:
//   - `sys_mkdirat` (NR_MKDIRAT = 34)
//   - `sys_unlinkat` (NR_UNLINKAT = 35) — `AT_REMOVEDIR` switches
//     between `FsOps::unlink` and `FsOps::rmdir`.
//   - `sys_symlinkat` (NR_SYMLINKAT = 36)
//   - `sys_linkat` (NR_LINKAT = 37) — same-FS hard link; cross-FS
//     `EXDEV` is implicit (orphan dentries surface as `-EROFS`).
//   - `sys_truncate` (NR_TRUNCATE = 45) — path-named PageBacked truncate.
//   - `sys_ftruncate` (NR_FTRUNCATE = 46) — fd-named PageBacked truncate.
//   - `sys_readlinkat` (NR_READLINKAT = 78) — walks parent directory
//     and calls `FsOps::lookup` + `FsOps::read_link` so the symlink
//     itself is returned (not its target).
//   - `sys_utimensat` (NR_UTIMENSAT = 88) — returns `-ENOSYS` (no
//     `FsOps::set_times` hook yet; deferred per slice plan).
//   - `sys_renameat2` (NR_RENAMEAT2 = 276) — `RENAME_NOREPLACE`
//     honoured via pre-walk; `RENAME_EXCHANGE` / `RENAME_WHITEOUT`
//     return `-ENOSYS` / `-EINVAL`.
//
// Each path-relative arm restricts `dirfd` to `AT_FDCWD` (matches the
// existing Slice 6 / Wave 4 Part 4 pattern; non-cwd dirfds map to
// `-EBADF` until the fd table grows directory-fd semantics under
// `TODO(phase-dirfd)`).
//
// Send-future discipline (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`):
// `Guard` is `!Send + !Sync`. Each arm is `async` so it composes with
// the dispatcher's `async fn`, but it never holds a `Guard` across an
// `.await` — `step_walk` is polled synchronously through
// `poll_walker_synchronously` (every in-tree walker resolves
// immediately) and the `FsOps` step is invoked under a freshly-taken
// guard inside the call site.
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 8.
// =====================================================================

/// Return the mount payload in scope for a dentry, ascending the
/// parent-hint chain to find the containing mount.  Used by
/// `sys_mount` / `sys_umount2`.
pub(super) fn mount_payload_for_dentry(dentry: &Cap<DEntry>) -> Option<Cap<MountPayload>> {
    MountedDentry::find_ascending(dentry).map(MountedDentry::into_payload)
}

/// Walk `path` from `cwd` synchronously, returning the resolved
/// dentry or a positive-magnitude `-errno`. Mirrors
/// `resolve_path_at`'s shape but takes the cwd directly so callers
/// that already hold it (every Slice 8 arm fetches it once for both
/// the parent walk and the optional full walk) avoid the redundant
/// `process.cwd()` lookup.
pub(super) fn walk_from(
    cwd: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
) -> Result<Cap<DEntry>, i32> {
    ResolvedPath::from_root(cwd, path, cred).map(ResolvedPath::into_dentry)
}

pub(super) fn walk_from_process(
    cwd: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    process: &Cap<ProcessIdentity>,
) -> Result<Cap<DEntry>, i32> {
    ResolvedPath::from_process_root(cwd, path, cred, process).map(ResolvedPath::into_dentry)
}
