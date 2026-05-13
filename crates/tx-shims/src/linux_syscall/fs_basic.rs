//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;

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

    match cmd {
        F_GETFD => {
            // POSIX: return `FD_CLOEXEC` if bit set, `0` otherwise.
            let value = if ctx.process.fd_cloexec(fd) {
                FD_CLOEXEC as i64
            } else {
                0
            };
            SyscallResult::Return(value)
        }
        F_SETFD => {
            // POSIX: set the close-on-exec bit from `arg & FD_CLOEXEC`.
            // Other bits in `arg` are silently ignored (this matches
            // Linux's behaviour — `FD_CLOEXEC` is the only bit
            // defined on this command's `arg`).
            let on = (arg & FD_CLOEXEC as u64) != 0;
            ctx.process.set_fd_cloexec(fd, on);
            SyscallResult::Return(0)
        }
        F_DUPFD => {
            // Duplicate `fd` into the lowest-numbered slot ≥ `arg`.
            // Per POSIX: the result clears the cloexec bit
            // (`F_DUPFD_CLOEXEC` is the variant that sets it).
            let min = arg as u32;
            let new_fd = ctx.process.allocate_fd_at_least(min);
            // install_fd returns the previous occupant, if any (in
            // practice always None because allocate_fd_at_least
            // returns the lowest *unused* slot). Drop it under the
            // caller's EBR if it surfaces.
            let _previous = ctx.process.install_fd(new_fd, file);
            ctx.process.set_fd_cloexec(new_fd, false);
            SyscallResult::Return(new_fd as i64)
        }
        F_DUPFD_CLOEXEC => {
            // Like F_DUPFD but sets the cloexec bit on the new fd.
            let min = arg as u32;
            let new_fd = ctx.process.allocate_fd_at_least(min);
            let _previous = ctx.process.install_fd(new_fd, file);
            ctx.process.set_fd_cloexec(new_fd, true);
            SyscallResult::Return(new_fd as i64)
        }
        F_GETFL => {
            // Compose access-mode + per-OpenFile open-flag bits.
            // `O_CLOEXEC` is **not** included — Linux's F_GETFL only
            // reports the per-OpenFile bits, while CLOEXEC is per-fd
            // (read via F_GETFD).
            let f = file.flags();
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
            // TODO(phase-fcntl-setfl): F_SETFL needs interior-mutable
            // OpenFileFlags. Future slice owns this — the plain
            // `Copy`-struct field on OpenFile cannot be mutated
            // atomically without a structural change.
            SyscallResult::Error(ENOSYS_VALUE)
        }
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
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
    // Wave 2 slice: AT_FDCWD only. Real dirfd-relative resolution
    // requires directory file descriptors — the slice's fd table
    // doesn't carry them yet. (TODO(phase-dirfd): mirror Wave 4
    // Part 4's `resolve_path_at` once dirfds land.)
    if dirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
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
    let want_append = flags & O_APPEND != 0;
    let want_cloexec = flags & O_CLOEXEC != 0;
    let want_create = flags & O_CREAT != 0;
    let want_excl = flags & O_EXCL != 0;
    let want_trunc = flags & O_TRUNC != 0;
    // O_NONBLOCK and other unrecognised bits: silently dropped.

    let open_flags = OpenFileFlags {
        read: want_read,
        write: want_write,
        append: want_append,
        cloexec: want_cloexec,
        nonblocking: flags & O_NONBLOCK != 0,
    };

    // Resolve the cwd anchor. Zombies + uninitialised init pre-rootfs
    // both surface `cwd() == None`; the alive caller of `openat` always
    // has a cwd installed by `step_chdir` / bootstrap. No-cwd is a
    // defensive `-ENOENT` (matches Linux's "no such directory" shape
    // for an unreachable cwd).
    let cwd: Cap<DEntry> = match ctx.process.cwd() {
        Some(d) => d,
        None => return SyscallResult::Error(ENOENT_VALUE),
    };

    let walker_cred = ctx.walker_cred();

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
    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3};
    let walk_first = {
        let guard = tx_substrate::epoch::guard();
        let outcome =
            poll_walker_synchronously(step_walk(cwd.clone(), &path, &walker_cred, &guard));
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
            match create_then_walk::<P>(&cwd, &path, mode as u16, &walker_cred) {
                Ok(d) => d,
                Err(e) => return SyscallResult::Error(e),
            }
        }
        V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
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
            use tx_substrate::step_v3::StepOutcome as V3Trunc;
            let fs_page_backing = match fs_page_backing_for_dentry(&dentry) {
                Some(b) => b,
                None => return SyscallResult::Error(ENOSYS_VALUE),
            };
            let fs_object_id = dentry.rnode().fs_object_id();
            let guard = tx_substrate::epoch::guard();
            match fs_page_backing.truncate(fs_object_id, 0, &guard) {
                V3Trunc::Done(()) => {}
                V3Trunc::Continue { .. } | V3Trunc::Yield { .. } => {
                    return SyscallResult::Error(EIO_VALUE);
                }
                V3Trunc::Err(errno) => {
                    return SyscallResult::Error(errno_to_i32(Errno::from(errno)));
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
        let guard = tx_substrate::epoch::guard();
        let outcome = poll_walker_synchronously(step_open(
            cwd,
            &path,
            open_flags,
            mode as u16,
            &walker_cred,
            &guard,
        ));
        drop(guard);
        match outcome {
            V3::Done(file) => file,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };

    // Step 4: install at the lowest unused fd ≥ 0. Per fd-ops Wave 1
    // the fd table is a sparse `BTreeMap<u32, Cap<OpenFile>>`;
    // `allocate_fd()` scans for the lowest unused key.
    let fd = ctx.process.allocate_fd();
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
pub(super) fn sys_close<'a>(fd: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    if ctx.process.fd(fd).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let _previous = ctx.process.set_fd(fd, None);
    // Clear the cloexec bit defensively. The bitmap is a sibling of
    // the fd-table BTreeMap (not folded into OpenFile.flags), so the
    // close arm explicitly clears it even though the fd-table entry
    // is gone — matches Linux's `close(2)` "cloexec disposition is
    // forgotten" semantic.
    ctx.process.set_fd_cloexec(fd, false);
    SyscallResult::Return(0)
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
    let file = match ctx.process.fd(oldfd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let newfd = ctx.process.allocate_fd();
    let _ = ctx.process.set_fd(newfd, Some(file));
    // POSIX: the duplicate fd has its cloexec bit cleared. Defensively
    // clear it (allocate_fd returned an unused slot, so the bit
    // should already be clear, but `BTreeSet<u32>::remove` is cheap
    // and guards against a stale bit from a previous lifecycle).
    ctx.process.set_fd_cloexec(newfd, false);
    SyscallResult::Return(newfd as i64)
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
    if oldfd == newfd {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    // Validate flags: only O_CLOEXEC is meaningful. Other bits =
    // -EINVAL. (Linux dup3 specifically rejects junk flags rather
    // than ignoring them, unlike open().)
    if flags & !O_CLOEXEC != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match ctx.process.fd(oldfd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    // install_fd returns the previous occupant. We drop it
    // immediately — the Cap goes through EBR-deferred reclamation,
    // matching `sys_close`'s semantic.
    let _previous = ctx.process.install_fd(newfd, file);
    let want_cloexec = flags & O_CLOEXEC != 0;
    ctx.process.set_fd_cloexec(newfd, want_cloexec);
    SyscallResult::Return(newfd as i64)
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
    use tx_substrate::step_v3::{StepOp, StepOutcome as V3Pipe};
    use tx_subsystems::pipe::Pipe2Op;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let (reader_cap, writer_cap) = {
        let mut op = Pipe2Op { flags: pipe_flags };
        match op.step(&mut script_ctx) {
            V3Pipe::Done(pair) => pair,
            V3Pipe::Err(v3errno) => {
                let errno: Errno = v3errno.into();
                return SyscallResult::Error(errno_to_i32(errno));
            }
            V3Pipe::Continue { .. } | V3Pipe::Yield { .. } => {
                // `step_pipe2` is allocation-only; Continue/Yield
                // are unreachable. Surface as -EIO defensively.
                return SyscallResult::Error(EIO_VALUE);
            }
        }
    };

    // Install at the lowest two unused fds. `allocate_fd()` returns
    // the lowest unused slot; install_fd() commits. Allocate the
    // reader first so on a fresh process it lands at 0 and the
    // writer at 1, matching Linux's user-visible (3, 4) pattern
    // post-stdin/out/err.
    let reader_fd = ctx.process.allocate_fd();
    let _ = ctx.process.install_fd(reader_fd, reader_cap);
    let writer_fd = ctx.process.allocate_fd();
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
        return SyscallResult::Error(errno_to_i32(errno));
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
    let guard = tx_substrate::epoch::guard();
    use tx_substrate::step_v3::StepOutcome as V3Out;
    match file.step_lseek(offset, whence, &guard) {
        V3Out::Done(new_offset) => SyscallResult::Return(new_offset as i64),
        V3Out::Continue { .. } | V3Out::Yield { .. } => {
            // Unreachable in practice — see the comment on the
            // function header.
            SyscallResult::Error(errno_to_i32(Errno::EIO))
        }
        V3Out::Err(v3errno) => {
            let errno: Errno = v3errno.into();
            SyscallResult::Error(errno_to_i32(errno))
        }
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
/// DIAGNOSTIC (temp, 2026-05-12): counts of ioctl requests and the
/// last seen request word. Useful for tracing the post-prompt EOF
/// gap, where the TTY's termios is somehow ICANON-cleared before
/// busybox's first read.
pub static SYS_IOCTL_INVOCATIONS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static SYS_IOCTL_LAST_REQUEST: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
pub static SYS_IOCTL_TCSETS_CALLS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static SYS_IOCTL_TCGETS_CALLS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
/// On the first TCGETS, record the lflag the kernel handed back —
/// this reveals what state the TTY was in *before* busybox touched it.
pub static FIRST_TCGETS_LFLAG: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);
pub static FIRST_TCGETS_VMIN: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);
/// On every TCSETS, record the lflag/vmin/vtime busybox installed.
/// We watch the latest one because the call right before the read is
/// the one that puts the TTY into the VMIN=0 polling shape.
pub static LAST_TCSETS_LFLAG: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);
pub static LAST_TCSETS_VMIN: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);
pub static LAST_TCSETS_VTIME: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);
pub static FIRST_TCSETS_LFLAG: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);
pub static FIRST_TCSETS_VMIN: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0xdead_beef);

pub(super) fn sys_ioctl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let request = args[1] as u32;
    let argp = args[2];
    SYS_IOCTL_INVOCATIONS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    SYS_IOCTL_LAST_REQUEST.store(request, core::sync::atomic::Ordering::Relaxed);
    match request {
        TCSETS | TCSETSW | TCSETSF => {
            SYS_IOCTL_TCSETS_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        TCGETS => {
            SYS_IOCTL_TCGETS_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        _ => {}
    }

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
            _ => SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
        };
    }

    // Resolve to a TTY. Non-TTY fds → -ENOTTY for terminal-shape ioctls
    // (Linux semantic — even pipes / regular files return ENOTTY for
    // these requests, per `man ioctl_tty`).
    let tty = match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        _ => return SyscallResult::Error(errno_to_i32(Errno::ENOTTY)),
    };

    // v3 step_ioctl_* return Done/Err only in practice; helper to
    // collapse the four-variant catalog into a v4 Errno-or-value.
    use tx_substrate::step_v3::StepOutcome as V3Out;
    fn unwrap_v3<T>(v: V3Out<T, tx_substrate::step_v3::NoProgress>) -> Result<T, Errno> {
        match v {
            V3Out::Done(t) => Ok(t),
            V3Out::Err(e) => Err(e.into()),
            V3Out::Continue { .. } | V3Out::Yield { .. } => Err(Errno::EIO),
        }
    }

    match request {
        TCGETS => {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tcgets(&tty, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(termios) => {
                    if SYS_IOCTL_TCGETS_CALLS.load(core::sync::atomic::Ordering::Relaxed) == 1 {
                        FIRST_TCGETS_LFLAG
                            .store(termios.c_lflag, core::sync::atomic::Ordering::Relaxed);
                        FIRST_TCGETS_VMIN.store(
                            termios.c_cc[tx_subsystems::tty::structure::termios::VMIN] as u32,
                            core::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<Termios>(&ctx.aspace, argp, termios)
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
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
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            LAST_TCSETS_LFLAG.store(new_termios.c_lflag, core::sync::atomic::Ordering::Relaxed);
            LAST_TCSETS_VMIN.store(
                new_termios.c_cc[tx_subsystems::tty::structure::termios::VMIN] as u32,
                core::sync::atomic::Ordering::Relaxed,
            );
            LAST_TCSETS_VTIME.store(
                new_termios.c_cc[tx_subsystems::tty::structure::termios::VTIME] as u32,
                core::sync::atomic::Ordering::Relaxed,
            );
            if SYS_IOCTL_TCSETS_CALLS.load(core::sync::atomic::Ordering::Relaxed) == 1 {
                FIRST_TCSETS_LFLAG
                    .store(new_termios.c_lflag, core::sync::atomic::Ordering::Relaxed);
                FIRST_TCSETS_VMIN.store(
                    new_termios.c_cc[tx_subsystems::tty::structure::termios::VMIN] as u32,
                    core::sync::atomic::Ordering::Relaxed,
                );
            }
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tcsets(&tty, new_termios, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        TIOCGPGRP => {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocgpgrp(&tty, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(pgid) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, argp, pgid) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        TIOCSPGRP => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let new_pgrp: u32 = match bootstrap_read_user::<u32>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let caller = make_ioctl_caller(ctx);
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocspgrp(&tty, caller, new_pgrp, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        TIOCGWINSZ => {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocgwinsz(&tty, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(ws) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<Winsize>(&ctx.aspace, argp, ws) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        TIOCSWINSZ => {
            if argp == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let ws: Winsize = match bootstrap_read_user::<Winsize>(&ctx.aspace, argp) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocswinsz(&tty, ws, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        TIOCSCTTY => {
            // The `argp` for TIOCSCTTY is a "force" bit (0 or 1) on
            // Linux, used to steal the TTY from another session when
            // the caller is root. v1 ignores it — the underlying
            // `step_ioctl_tiocsctty` rejects already-bound TTYs with
            // -EBUSY regardless of the force flag.
            let caller = make_ioctl_caller(ctx);
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocsctty(&tty, caller, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        TIOCNOTTY => {
            let caller = make_ioctl_caller(ctx);
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocnotty(&tty, caller, &guard)
            };
            match unwrap_v3(outcome) {
                Ok(_) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        // Unknown ioctl request → -ENOTTY (the POSIX `man ioctl_tty`
        // semantic). musl's `isatty(3)` resolves to TCGETS so it never
        // hits this arm, but other libc paths (or buggy userspace)
        // observing -ENOTTY here is the canonical Linux signal that
        // the request is not a terminal ioctl on this fd.
        _ => SyscallResult::Error(errno_to_i32(Errno::ENOTTY)),
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
struct LinuxDirent64Header {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
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
        stx_mode: (meta.mode & 0xffff) as u16,
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
    let meta = rnode.meta();
    let ino = rnode.fs_object_id().as_u64();
    let stat = inode_meta_to_stat(&meta, ino, 0);

    if let Err(errno) = bootstrap_write_user::<StatLayout>(&ctx.aspace, statbuf_uaddr, stat) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `statx(dirfd, path, flags, mask, statxbuf)`. Linux generic ABI
/// `__NR_statx = 291`.
///
/// This is the metadata probe LA64 musl/busybox uses before `ls`
/// opens a directory. Txv2 reports the same inode metadata already
/// used by `newfstatat`; unsupported sync policy bits are accepted
/// because there is no cache coherency distinction in the current VFS
/// layer.
pub(super) async fn sys_statx<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let dirfd = args[0] as i32;
    let path_uaddr = args[1];
    let flags = args[2] as u32;
    let _mask = args[3] as u32;
    let statxbuf_uaddr = args[4];

    if dirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
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

    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };

    let dentry: Cap<DEntry> = if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
        match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        }
    } else {
        let cwd = match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        };
        let walker_cred = ctx.walker_cred();
        use tx_substrate::step_v3::StepOutcome as V3;
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            poll_walker_synchronously(step_walk(cwd, &path, &walker_cred, &guard))
        };
        match outcome {
            V3::Done(d) => d,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };

    let rnode = dentry.rnode();
    let meta = rnode.meta();
    let statx = inode_meta_to_statx(&meta, rnode.fs_object_id().as_u64());
    if let Err(errno) = bootstrap_write_user::<StatxLayout>(&ctx.aspace, statxbuf_uaddr, statx) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `newfstatat(dirfd, path, statbuf, flags)`. Linux RV64 generic ABI
/// `__NR_newfstatat = 79`.
///
/// Slice 6 surface:
/// - `dirfd == AT_FDCWD` only; non-cwd dirfds → `-EBADF`.
/// - `flags & AT_EMPTY_PATH` paired with empty path stats the cwd
///   directly (no walker invocation).
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

    if dirfd != AT_FDCWD {
        return SyscallResult::Error(EBADF_VALUE);
    }
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

    // AT_EMPTY_PATH + empty path: stat the cwd itself. No walker
    // invocation — the cwd dentry's rnode meta is the answer.
    let dentry: Cap<DEntry> = if path.is_empty() && (flags & AT_EMPTY_PATH != 0) {
        match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        }
    } else {
        let cwd = match ctx.process.cwd() {
            Some(d) => d,
            None => return SyscallResult::Error(ENOENT_VALUE),
        };
        use tx_substrate::step_v3::StepOutcome as V3;
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            poll_walker_synchronously(step_walk(cwd, &path, &walker_cred, &guard))
        };
        match outcome {
            V3::Done(d) => d,
            V3::Continue { .. } | V3::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
            V3::Err(errno) => return SyscallResult::Error(errno_to_i32(Errno::from(errno))),
        }
    };

    let rnode = dentry.rnode();
    let meta = rnode.meta();
    let ino = rnode.fs_object_id().as_u64();
    let stat = inode_meta_to_stat(&meta, ino, 0);

    if let Err(errno) = bootstrap_write_user::<StatLayout>(&ctx.aspace, statbuf_uaddr, stat) {
        return SyscallResult::Error(errno_to_i32(errno));
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

    use tx_substrate::step_v3::StepOutcome as V3;
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
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
                    return SyscallResult::Error(errno_to_i32(errno));
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
                return SyscallResult::Error(errno_to_i32(Errno::from(errno)));
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
    let guard = tx_substrate::epoch::guard();
    let weak = rnode.containing_mount_weak()?;
    let payload = weak.upgrade(&guard)?;
    Some(payload.fs_ops.clone())
}
