//! Authoritative table of Linux RV64 syscall numbers used by txKernel.
//!
//! Source of truth: Linux RV64 generic ABI (`asm-generic/unistd.h`,
//! `__NR_*` numbers). These are the same numbers shared across arm64,
//! risc-v, and the asm-generic syscall table — txKernel's no_std
//! environment cannot pull `libc`, so the trio plan §"Part 2 —
//! Numbering policy" pins the in-tree mirror here.
//!
//! Phase 2a only ships the four numbers the syscall dispatch handles
//! today. Phase 2b adds `read` (63), `brk` (214), `rt_sigprocmask`
//! (135), and `rt_sigaction` (134).

/// `write(fd, buf, count)`. Linux generic ABI `__NR_write`.
pub const NR_WRITE: u64 = 64;
/// `read(fd, buf, count)`. Linux generic ABI `__NR_read`.
pub const NR_READ: u64 = 63;
/// `exit(status)`. Linux generic ABI `__NR_exit`. Per-thread exit per
/// `PROCESS_v1` §7.3.1 — for a single-threaded process, the
/// `step_thread_exit` chain triggers `step_process_exit` internally.
pub const NR_EXIT: u64 = 93;
/// `exit_group(status)`. Linux generic ABI `__NR_exit_group`. Routes
/// through `step_exit_group` per `PROCESS_v1` §7.3.2.
pub const NR_EXIT_GROUP: u64 = 94;
/// `getpid()`. Linux generic ABI `__NR_getpid`.
pub const NR_GETPID: u64 = 172;
/// `brk(addr)`. Linux generic ABI `__NR_brk`. Per `txdoc:VM-5-8-BRK`,
/// the dispatcher calls `AddressSpace::brk_script(brk_base,
/// current_brk, requested_brk)` and returns the new `current_brk`.
/// Linux semantics: brk *never* returns a negative errno; on failure
/// the unchanged current break is returned.
pub const NR_BRK: u64 = 214;
/// `rt_sigaction(signum, act, oldact, sigsetsize)`. Linux generic ABI
/// `__NR_rt_sigaction`. Per `SIGNAL_v1` §15.1; routes through
/// `signal::step_sigaction`. Rejects `sigsetsize != 8`.
pub const NR_RT_SIGACTION: u64 = 134;
/// `rt_sigprocmask(how, set, oldset, sigsetsize)`. Linux generic ABI
/// `__NR_rt_sigprocmask`. Per `SIGNAL_v1` §3; routes through
/// `thread_runtime::execution::step_sigprocmask`. Rejects
/// `sigsetsize != 8`.
pub const NR_RT_SIGPROCMASK: u64 = 135;
/// `fcntl(fd, cmd, arg)`. Linux generic ABI `__NR_fcntl` (= `__NR3264_fcntl`).
///
/// Wave 2 of the ELF loader plan ships a minimal subset:
/// `F_GETFD` / `F_SETFD` against the per-process CLOEXEC bitmap
/// (`ProcessPayload.fd_cloexec`). Other commands (`F_DUPFD`,
/// `F_GETFL`, `F_SETFL`, etc.) return `-ENOSYS` until the relevant
/// follow-up phases (`fcntl-extension`) wire them up.
pub const NR_FCNTL: u64 = 25;
/// `execve(path, argv, envp)`. Linux generic ABI `__NR_execve` = 221.
///
/// Wave 4 (Phase 6 of the ELF-loader plan) wires the syscall arm to
/// `tx_scripts::process::exec::exec_script`. On `Ok(())` the dispatch
/// returns `SyscallResult::ExecCommitted` — the thread future MUST NOT
/// drain `pending_syscall_return` for this iteration (the new image's
/// `_start` expects fresh GPRs; the previous trap frame's `a0` is
/// discarded). On `Err(_)` the standard `ExecError → -errno` mapping
/// applies (cite: `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`).
pub const NR_EXECVE: u64 = 221;

// ---------------------------------------------------------------------
// fcntl command numbers + flag bits.
//
// Source: Linux generic uapi `include/uapi/asm-generic/fcntl.h`
// (`F_DUPFD = 0` ... `F_GETFD = 1`, `F_SETFD = 2`, ...). `FD_CLOEXEC`
// is the only bit defined for the `arg` of `F_SETFD` / the return of
// `F_GETFD` per POSIX.
// ---------------------------------------------------------------------

/// `F_GETFD` cmd: read the close-on-exec bit for the given fd.
/// Returns `FD_CLOEXEC` if set, `0` otherwise.
pub const F_GETFD: i32 = 1;
/// `F_SETFD` cmd: set the close-on-exec bit for the given fd from
/// `arg & FD_CLOEXEC`.
pub const F_SETFD: i32 = 2;
/// `FD_CLOEXEC` flag: the (only) bit defined for the `arg` of
/// `F_SETFD` / the return of `F_GETFD`.
pub const FD_CLOEXEC: i32 = 1;

/// `O_CLOEXEC` flag for `open(2)` / future `openat(2)`. Linux generic
/// ABI: `0o2000000` (`0x80000`). Defined here so the (yet-to-land)
/// `sys_open` arm and any test that wants to construct an
/// `OpenFileFlags { cloexec: true, ... }` from the user-visible bit
/// can share one canonical constant.
pub const O_CLOEXEC: u32 = 0o2000000;

// ---------------------------------------------------------------------
// Wave 2 of the fd-ops slice — `openat(2)` flag bits.
//
// Authoritative source: Linux generic uapi `include/uapi/asm-generic/fcntl.h`.
// Only the bit set Wave 2 acts on is named here; `O_DIRECTORY`,
// `O_DSYNC`, `O_SYNC`, `O_DIRECT`, etc. are out of scope for the slice
// (the existing `OpenFileFlags` shape only carries read/write/append/
// cloexec; growing the surface lives with `pipe2` / `getdents64` slices
// when those flags are exercised). Unrecognised bits are accept-and-ignore
// matching Linux's lenient open-flag policy. See
// `docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md` Part 2.
// ---------------------------------------------------------------------

/// Access-mode mask for `openat(2)` flags. The bottom two bits encode
/// the access mode — `O_RDONLY` / `O_WRONLY` / `O_RDWR` — with `0o3`
/// being the historical "search-only" shape Linux ignores. Decoders
/// extract this via `flags & O_ACCMODE` per `man 2 open`.
pub const O_ACCMODE: u32 = 0o3;
/// `openat(2)` access mode: read-only (`O_RDONLY = 0`).
pub const O_RDONLY: u32 = 0o0;
/// `openat(2)` access mode: write-only (`O_WRONLY = 1`).
pub const O_WRONLY: u32 = 0o1;
/// `openat(2)` access mode: read-write (`O_RDWR = 2`).
pub const O_RDWR: u32 = 0o2;
/// `openat(2)` flag bit: create the file if missing. With `O_EXCL`,
/// fail with `-EEXIST` when the file already exists.
pub const O_CREAT: u32 = 0o100;
/// `openat(2)` flag bit: when paired with `O_CREAT`, fail with
/// `-EEXIST` if the target file already exists. The combination is the
/// canonical lock-file primitive.
pub const O_EXCL: u32 = 0o200;
/// `openat(2)` flag bit: truncate the file to size 0 on open. Wave 2
/// supports it for tmpfs-backed regular files via the in-scope
/// `FsPageBacking::truncate` hook; backends without truncate support
/// surface `-ENOSYS`.
pub const O_TRUNC: u32 = 0o1000;
/// `openat(2)` flag bit: open with append-only semantics — every write
/// is positioned at end-of-file regardless of the per-fd offset.
/// Threads through to `OpenFileFlags::append`.
pub const O_APPEND: u32 = 0o2000;
/// `openat(2)` flag bit: non-blocking open + non-blocking I/O on the
/// resulting fd. Wave 2 accepts but ignores this bit — there is no
/// blocking-flag plumbing on `OpenFile` yet (`TODO(phase-nonblock)`).
pub const O_NONBLOCK: u32 = 0o4000;

// ---------------------------------------------------------------------
// Wave 2 of the fd-ops slice — fd-management syscall numbers.
//
// `NR_OPENAT = 56`, `NR_CLOSE = 57`, `NR_DUP = 23`, `NR_DUP3 = 24`.
// `NR_DUP2` is **absent** on the Linux RV64 generic ABI — musl emits
// `dup3(oldfd, newfd, 0)` for the legacy `dup2(oldfd, newfd)` shape
// per its `src/unistd/dup2.c` arch-generic shim. See
// `docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md` Parts 2–4.
// ---------------------------------------------------------------------

/// `dup(oldfd)`. Linux RV64 generic ABI `__NR_dup = 23`. Returns the
/// lowest unused fd ≥ 0 referring to the same `OpenFile` as `oldfd`.
/// The returned fd has `cloexec` cleared per POSIX — `dup` never
/// inherits the cloexec bit; only `dup3(.., O_CLOEXEC)` sets it.
pub const NR_DUP: u64 = 23;
/// `dup3(oldfd, newfd, flags)`. Linux RV64 generic ABI
/// `__NR_dup3 = 24`. The atomic-replace form: any existing `newfd`
/// is silently closed and `newfd` is bound to the same `OpenFile` as
/// `oldfd`. `oldfd == newfd` is `-EINVAL` (Linux dup3 rejects the
/// no-op shape that legacy dup2 accepts). `flags` accepts only
/// `O_CLOEXEC`; other bits return `-EINVAL`.
pub const NR_DUP3: u64 = 24;
/// `openat(dirfd, path, flags, mode)`. Linux RV64 generic ABI
/// `__NR_openat = 56`. Wave 2's slice surface only supports
/// `dirfd == AT_FDCWD`; non-cwd dirfds return `-EBADF` (the slice's
/// fd table doesn't carry directory-fd semantics yet). The walker
/// resolves the path via `vfs::step_open` using the caller's
/// `walker_cred()` (effective ids per POSIX). On `O_CREAT` against a
/// missing file, the syscall arm walks the parent dir and calls
/// `FsOps::create_inode` before re-running `step_open`.
pub const NR_OPENAT: u64 = 56;
/// `close(fd)`. Linux RV64 generic ABI `__NR_close = 57`. Removes
/// the `OpenFile` cap from the fd table (EBR-deferred reclamation
/// fires the OpenFile's `Drop`) and clears the cloexec bit. `-EBADF`
/// for closed fds.
pub const NR_CLOSE: u64 = 57;
/// `pipe2(int pipefd[2], int flags)`. Linux RV64 generic ABI
/// `__NR_pipe2 = 59`.
///
/// Wave 3 of the fd-ops slice. Builds a (reader, writer) `OpenFile`
/// pair sharing a single `Cap<PipePayload>` via
/// `tx_subsystems::pipe::step_pipe2`, allocates two `(reader_fd,
/// writer_fd)` slots via `process.allocate_fd()`, installs them in
/// the BTreeMap, and writes the pair back to userspace at
/// `pipefd_uaddr` as `[u32; 2]` little-endian.
///
/// Recognised `flags`: `O_CLOEXEC | O_NONBLOCK`. `O_DIRECT`
/// (packet-mode pipes) is recognised but returns `-ENOSYS`. Any
/// other bits return `-EINVAL`.
///
/// **SIGPIPE delivery.** Q2 DECIDED 2026-05-07: `OpenFile::step_write`
/// returns `Err(EPIPE)` when all readers have closed. The
/// `sys_write` arm intercepts `Errno::EPIPE` and dispatches SIGPIPE
/// to the calling process via `signal::step_kill_process` before
/// returning `-EPIPE` to userspace. The pipe module itself has no
/// process Cap and so cannot deliver the signal.
pub const NR_PIPE2: u64 = 59;
/// `O_DIRECT` flag bit (`0o40000`). Recognised by `sys_pipe2` but
/// not implemented (packet-mode pipes are out of scope). Any other
/// open arm currently ignores this bit.
pub const O_DIRECT: u32 = 0o40000;

// ---------------------------------------------------------------------
// Wave 2 of the fork/clone/wait4 slice — Part 2 (NR_CLONE) +
// Part 4 (process-tree introspection arms) + Part 5 (musl-startup
// stubs). NR_WAIT4 is intentionally absent — it lives in Wave 3 with
// the blocking-wait scaffolding. See
// `docs/progress/plans/2026-05-06-fork-clone-wait4.md`.
// ---------------------------------------------------------------------

/// `clone(flags, stack, parent_tidptr, tls, child_tidptr)`.
/// Linux RV64 generic ABI `__NR_clone`.
///
/// Wave 2 of the fork/clone/wait4 slice ships only the bare-`SIGCHLD`
/// shape that musl's `_Fork.c:35` issues
/// (`__syscall(SYS_clone, SIGCHLD, 0)`). Anything else (`CLONE_VM`,
/// `CLONE_VFORK`, the pthread_create flag set, non-zero stack)
/// returns `-EINVAL`. See `txdoc:PROCESS-CLONE-FLAGS` /
/// `txdoc:PROCESS-CLONE-FLAG-SUPPORT-V1-1`.
pub const NR_CLONE: u64 = 220;

/// Linux signal number for `SIGCHLD` (matches the trio's
/// `signal::Signum::SIGCHLD` encoding). Used as the termination-signal
/// low-byte of `clone()`'s `flags` argument; bare-`SIGCHLD` is the
/// only flag combination Wave 2's `sys_clone` accepts.
pub const SIGCHLD: u64 = 17;

/// `getppid()`. Linux generic ABI `__NR_getppid`. Wraps
/// `ProcessIdentity::parent_pid()`. Returns `0` (`Pid::RESERVED`)
/// for orphans (init's pid 1 has no parent). Real Linux returns
/// init's pid for orphans; the trio's `sever_children` reparents to
/// init when init is registered, so under normal flows the difference
/// is invisible.
pub const NR_GETPPID: u64 = 173;

/// `setpgid(pid, pgid)`. Linux generic ABI `__NR_setpgid`. Wraps
/// `step_setpgid`. The trio's day-1 step only supports
/// `pid == self` and `pgid == self.pid` (creates a fresh process
/// group inside the caller's session); cross-process and joining an
/// existing pgid return `-EPERM` per Linux semantics.
pub const NR_SETPGID: u64 = 154;

/// `getpgid(pid)`. Linux generic ABI `__NR_getpgid`. Returns the
/// process group id of the process with pid `pid`, or the caller's
/// pgid if `pid == 0`. Day-1 only supports `pid == 0` /
/// `pid == self.pid`; cross-pid lookup is deferred (no pid → Cap
/// resolver yet).
pub const NR_GETPGID: u64 = 155;

/// `getpgrp()`. Linux **legacy** glibc-only call; the RV64 generic
/// ABI does not ship this number, but glibc emulates `getpgrp()` as
/// `getpgid(0)`. We carve out the constant for grep-stability and
/// dispatch returns `-ENOSYS` deliberately. musl uses `getpgid(0)`
/// directly and never issues this number.
pub const NR_GETPGRP: u64 = 81;

/// `getsid(pid)`. Linux generic ABI `__NR_getsid`. Returns the session
/// id of the process with pid `pid`, or the caller's sid if
/// `pid == 0`. Day-1 only supports `pid == 0` / `pid == self.pid`.
pub const NR_GETSID: u64 = 156;

/// `setsid()`. Linux generic ABI `__NR_setsid`. Wraps `step_setsid` —
/// creates a fresh `Session` + leader `ProcessGroup` rooted at the
/// caller's pid. Day-1 does not enforce Linux's "already a process
/// group leader → -EPERM" rule (follow-up).
pub const NR_SETSID: u64 = 157;

/// `set_tid_address(tidptr)`. Linux generic ABI
/// `__NR_set_tid_address`. Wave 2 ships a stub-success arm that
/// returns the calling thread's tid and ignores `tidptr` — the real
/// semantic (futex wakeup on thread exit via `clear_child_tid`) is
/// deferred to the pthread/futex slice (`TODO(phase-tls)`).
pub const NR_SET_TID_ADDRESS: u64 = 96;

/// `set_robust_list(head, len)`. Linux generic ABI
/// `__NR_set_robust_list`. Wave 2 ships a stub-success arm that
/// returns `0` and ignores `head`/`len` — the real semantic
/// (futex robust-list registration) is deferred to the futex slice
/// (`TODO(phase-futex)`).
pub const NR_SET_ROBUST_LIST: u64 = 99;

// ---------------------------------------------------------------------
// Wave 3 of the fork/clone/wait4 slice — Part 3 (NR_WAIT4 syscall arm
// with blocking-wait via the per-process `exit_port` carrier wired in
// Wave 1). NR_WAITID is intentionally absent — deferred per the slice
// plan's Open Q #2 (DECIDED 2026-05-06: NR_WAITID deferred).
// ---------------------------------------------------------------------

/// `wait4(pid, status, options, rusage)`. Linux RV64 generic ABI
/// `__NR_wait4`.
///
/// Wave 3 of the fork/clone/wait4 slice ships the blocking variant —
/// when no zombie matches and `WNOHANG` is unset, the arm parks on the
/// caller's per-process `exit_port` carrier (registered at payload
/// sign time per Wave 1) via
/// [`tx_subsystems::wait_carrier::wait_on_token`], waking when any
/// child of this process zombifies. See
/// `txdoc:PROCESS-WAIT-FAMILY-1`.
pub const NR_WAIT4: u64 = 260;

/// `WNOHANG` — only options bit Wave 3 acts on. Other defined bits
/// (`WUNTRACED = 0x2`, `WCONTINUED = 0x8`) are accepted but ignored;
/// they need stop/cont signal infrastructure to surface
/// `Stopped`/`Continued` `ExitStatus` values, which is a deferred slice.
pub const WNOHANG: i32 = 0x1;

// ---------------------------------------------------------------------
// Wave 2 of the DAC + setuid slice — Part 7 (`SyscallCtx::cred()`
// accessor) + Part 3 (process-side cred-mutation / cred-reading
// syscall arms). Numbers verified against Linux's RV64 generic ABI
// (`include/uapi/asm-generic/unistd.h`); each one cites the shipping
// `tx_subsystems::cred::step_*` helper landed in Wave 1
// (`txdoc:PROCESS-CREDENTIAL-SERVICE-DRAFT-1`). See
// `docs/progress/plans/2026-05-06-dac-and-setuid.md` Part 3.
// ---------------------------------------------------------------------

/// `setgid(gid)`. Linux RV64 generic ABI `__NR_setgid = 144`. Wraps
/// `cred::step_setgid`. LTP cluster: `setgid01..03`,
/// `setregid01..04` (regression cross-check).
pub const NR_SETGID: u64 = 144;
/// `setregid(rgid, egid)`. Linux RV64 generic ABI
/// `__NR_setregid = 143`. Wraps `cred::step_setregid` (Wave 1).
/// `(u32) -1` (== `u32::MAX` after the i32→u32 cast) means "leave
/// unchanged" per Linux's sentinel convention. LTP cluster:
/// `setregid01..04`.
pub const NR_SETREGID: u64 = 143;
/// `setreuid(ruid, euid)`. Linux RV64 generic ABI
/// `__NR_setreuid = 145`. Wraps `cred::step_setreuid` (Wave 1).
/// `(u32) -1` sentinel as above. LTP cluster: `setreuid01..05`.
pub const NR_SETREUID: u64 = 145;
/// `setuid(uid)`. Linux RV64 generic ABI `__NR_setuid = 146`. Wraps
/// `cred::step_setuid`. LTP cluster: `setuid01..04`.
pub const NR_SETUID: u64 = 146;
/// `setresuid(ruid, euid, suid)`. Linux RV64 generic ABI
/// `__NR_setresuid = 147`. Wraps `cred::step_setresuid` (Wave 1).
/// `(u32) -1` sentinel decodes to `None` in each of the three
/// argument slots. LTP cluster: `setresuid01..05`.
pub const NR_SETRESUID: u64 = 147;
/// `getresuid(ruid_uaddr, euid_uaddr, suid_uaddr)`. Linux RV64
/// generic ABI `__NR_getresuid = 148`. Writes
/// `(uid.raw(), euid.raw(), suid.raw())` to the three user
/// pointers. LTP cluster: `getresuid01..03`.
///
/// Wave 2 bootstrap exemption: the three uaddrs are treated as
/// kernel-side via inline `write_volatile` (mirrors `sys_wait4`'s
/// `wstatus` writeback). Linux's real semantics return `-EFAULT` on
/// any invalid pointer; user-VA validation is `TODO(phase-userva)`.
pub const NR_GETRESUID: u64 = 148;
/// `setresgid(rgid, egid, sgid)`. Linux RV64 generic ABI
/// `__NR_setresgid = 149`. Wraps `cred::step_setresgid` (Wave 1).
/// `(u32) -1` sentinel as for `setresuid`. LTP cluster:
/// `setresgid01..04`.
pub const NR_SETRESGID: u64 = 149;
/// `getresgid(rgid_uaddr, egid_uaddr, sgid_uaddr)`. Linux RV64
/// generic ABI `__NR_getresgid = 150`. Companion of `getresuid`;
/// writes the three gids to user pointers. Same Wave 2 bootstrap
/// exemption applies. LTP cluster: `getresgid01..02`.
pub const NR_GETRESGID: u64 = 150;
/// `getuid()`. Linux RV64 generic ABI `__NR_getuid = 174`. Reads
/// `cred.uid`. LTP cluster: `getuid01..03`.
pub const NR_GETUID: u64 = 174;
/// `geteuid()`. Linux RV64 generic ABI `__NR_geteuid = 175`. Reads
/// `cred.euid`. LTP cluster: `geteuid01..02`.
pub const NR_GETEUID: u64 = 175;
/// `getgid()`. Linux RV64 generic ABI `__NR_getgid = 176`. Reads
/// `cred.gid`. LTP cluster: `getgid01..03`.
pub const NR_GETGID: u64 = 176;
/// `getegid()`. Linux RV64 generic ABI `__NR_getegid = 177`. Reads
/// `cred.egid`. LTP cluster: `getegid01..02`.
pub const NR_GETEGID: u64 = 177;

// ---------------------------------------------------------------------
// Wave 4 Part 4 of the DAC + setuid slice — file-mode syscall arms
// (`fchmodat`, `fchownat`, `faccessat`, `faccessat2`). Each wraps the
// `FsOps::step_chmod` / `step_chown` trait method Wave 3 Part 2 landed
// (tmpfs has the real impl; devfs returns EROFS) plus a walker-side
// `access(2)` predicate over the inode meta. Only the `AT_FDCWD`
// dirfd shape is supported in this slice — real dirfd-relative
// resolution requires directory file descriptors which the slice's
// fd table does not yet carry.
//
// See `docs/progress/plans/2026-05-06-dac-and-setuid.md` Part 4 and
// `txdoc:VFS-CHECKS-PERMISSIONS-1`.
// ---------------------------------------------------------------------

/// `faccessat(dirfd, path, mode)`. Linux RV64 generic ABI
/// `__NR_faccessat = 48`. POSIX `access(2)` semantics: the requested
/// permission bits in `mode` are checked against the inode using the
/// caller's **real** uid/gid (not effective), unless paired with
/// `AT_EACCESS` via `faccessat2`. LTP cluster: `access01..04`,
/// `faccessat01..02`.
pub const NR_FACCESSAT: u64 = 48;
/// `fchmodat(dirfd, path, mode, flags)`. Linux RV64 generic ABI
/// `__NR_fchmodat = 53`. Wraps `FsOps::step_chmod` (Wave 3 Part 2).
/// `flags` (`AT_SYMLINK_NOFOLLOW`) is accepted but ignored — the slice
/// doesn't follow symlinks at chmod time anyway. LTP cluster:
/// `fchmodat01..02`.
pub const NR_FCHMODAT: u64 = 53;
/// `fchownat(dirfd, path, uid, gid, flags)`. Linux RV64 generic ABI
/// `__NR_fchownat = 54`. Wraps `FsOps::step_chown` (Wave 3 Part 2).
/// Each of `uid` / `gid` decodes the `(u32) -1 == u32::MAX` "leave
/// unchanged" sentinel to `Option::None` (same convention as
/// `setre{u,g}id` / `setres{u,g}id`). LTP cluster: `fchownat01..02`.
pub const NR_FCHOWNAT: u64 = 54;
/// `faccessat2(dirfd, path, mode, flags)`. Linux RV64 generic ABI
/// `__NR_faccessat2 = 439`. Same as `faccessat` plus the `flags`
/// argument — `AT_EACCESS` switches the check from real uid/gid to
/// effective uid/gid. LTP cluster: `faccessat201..03`.
pub const NR_FACCESSAT2: u64 = 439;

/// `AT_FDCWD = -100` cast to i32. Kernel-side sentinel for "interpret
/// `path` relative to the caller's cwd"; musl passes this as the first
/// arg to `fchmodat` / `fchownat` / `faccessat` / `faccessat2` when the
/// caller wants the cwd-relative shape (`chmod` / `chown` / `access`).
pub const AT_FDCWD: i32 = -100;

/// `access(2)` mode bits — passed through `faccessat` / `faccessat2`'s
/// `mode` argument.
///
/// `F_OK = 0` is the existence-only check; the syscall arm short-
/// circuits to `Return(0)` after path resolution succeeds (no
/// permission-bit check). `R_OK` / `W_OK` / `X_OK` correspond to the
/// POSIX read / write / execute permission checks; the syscall arm
/// translates them to the matching octal triplet bits (`0o4` / `0o2`
/// / `0o1`).
pub const F_OK: i32 = 0;
pub const R_OK: i32 = 4;
pub const W_OK: i32 = 2;
pub const X_OK: i32 = 1;

/// `AT_EACCESS = 0x200` flag bit (4th arg of `faccessat2`). When set,
/// the access check uses the caller's **effective** uid/gid; when
/// clear (the POSIX `access(2)` default) the check uses the **real**
/// uid/gid. Linux's `faccessat(2)` (no flags) is fixed to the real-id
/// path; only `faccessat2` exposes this knob.
pub const AT_EACCESS: i32 = 0x200;
/// `AT_SYMLINK_NOFOLLOW = 0x100` flag bit (4th arg of `fchmodat` /
/// `fchownat` / `faccessat2`). The slice accepts this bit silently —
/// chmod/chown don't follow symlinks anyway in the current tmpfs
/// surface, and the walker's symlink budget already guards against
/// cycles at resolution time. Plumbing the bit through the walker
/// is `TODO(phase-symlink-flag)`.
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
