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
/// `writev(fd, iov, iovcnt)`. Linux generic ABI `__NR_writev`.
/// Loops `step_write` over the iovec array. musl's stdio buffered
/// output goes through `writev` (not `write`), so this is on the
/// busybox-startup hot path.
pub const NR_WRITEV: u64 = 66;
/// `readv(fd, iov, iovcnt)`. Linux generic ABI `__NR_readv`.
pub const NR_READV: u64 = 65;
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
// Wave 4 of the fd-ops slice — `lseek(2)`.
//
// Linux RV64 generic ABI `__NR_lseek = 62`. The `_llseek` 32-bit ABI
// (`__NR__llseek = 140` on legacy archs) is not defined for RV64 and
// is intentionally absent here. Non-seekable backings (TTY, chardev,
// pipe) return `-ESPIPE`; directories return `-EISDIR`. See
// `docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md` Part 6.
// ---------------------------------------------------------------------

/// `lseek(fd, offset, whence)`. Linux RV64 generic ABI `__NR_lseek = 62`.
/// Returns the resulting absolute offset on success, or `-errno`.
pub const NR_LSEEK: u64 = 62;

/// `lseek` whence: set the offset to the absolute value `offset`.
/// Linux uapi `<unistd.h>` `SEEK_SET`.
pub const SEEK_SET: u32 = 0;
/// `lseek` whence: add `offset` to the current per-fd offset. Linux
/// uapi `<unistd.h>` `SEEK_CUR`.
pub const SEEK_CUR: u32 = 1;
/// `lseek` whence: add `offset` to the file's current size (only
/// meaningful for `RNodeBacking::PageBacked`). Linux uapi
/// `<unistd.h>` `SEEK_END`.
pub const SEEK_END: u32 = 2;

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

// ---------------------------------------------------------------------
// Slice 2 of the shell-prompt roadmap — VM syscalls
// (`mmap` / `munmap` / `mprotect` / `mremap` / `madvise` / `msync`).
//
// Numbers verified against Linux's RV64 generic ABI
// (`include/uapi/asm-generic/unistd.h`). Each one wraps a pre-existing
// `vm::execution::*` primitive (`AddressSpace::try_mmap`,
// `try_munmap`, `try_mprotect`, `try_mremap`, `madvise`, `msync`)
// landed earlier; the slice is pure plumbing — flag decode + errno
// mapping. See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md`
// Slice 2.
// ---------------------------------------------------------------------

/// `munmap(addr, length)`. Linux RV64 generic ABI `__NR_munmap = 215`.
/// Wraps `AddressSpace::try_munmap` over a page-aligned `[addr,
/// addr+length)` window.
pub const NR_MUNMAP: u64 = 215;
/// `mremap(old_addr, old_size, new_size, flags, new_addr)`. Linux RV64
/// generic ABI `__NR_mremap = 216`. Slice 2 only honours the
/// `MREMAP_FIXED | MREMAP_MAYMOVE` shape musl emits — pure-grow without
/// `MAYMOVE` returns `-ENOMEM` if the new size doesn't fit in place
/// (which `try_mremap`'s disjoint-only contract always reports).
pub const NR_MREMAP: u64 = 216;
/// `mmap(addr, length, prot, flags, fd, offset)`. Linux RV64 generic
/// ABI `__NR_mmap = 222`.
pub const NR_MMAP: u64 = 222;
/// `mprotect(addr, length, prot)`. Linux RV64 generic ABI
/// `__NR_mprotect = 226`. Wraps `AddressSpace::try_mprotect`.
pub const NR_MPROTECT: u64 = 226;
/// `msync(addr, length, flags)`. Linux RV64 generic ABI
/// `__NR_msync = 227`. Wraps `AddressSpace::msync` (the StepOutcome
/// shape — async over the `step_fsync` blocking lane).
pub const NR_MSYNC: u64 = 227;
/// `madvise(addr, length, advice)`. Linux RV64 generic ABI
/// `__NR_madvise = 233`. Wraps `AddressSpace::madvise`.
pub const NR_MADVISE: u64 = 233;

// ---------------------------------------------------------------------
// `PROT_*` flag bits — Linux generic uapi `<sys/mman.h>`. Slice 2 acts
// on the three-permission-bit set (`READ`/`WRITE`/`EXEC`); `PROT_NONE`
// is the zero pattern. `PROT_GROWSDOWN`/`PROT_GROWSUP` are recognised
// (so the high bits don't trip the unknown-bit `-EINVAL` reject) but
// return `-ENOSYS` because the underlying `Prot` shape has no
// equivalent.
// ---------------------------------------------------------------------

/// `PROT_NONE` — page is inaccessible. Equivalent to `Prot::NONE`.
pub const PROT_NONE: u64 = 0x0;
/// `PROT_READ` — page may be read.
pub const PROT_READ: u64 = 0x1;
/// `PROT_WRITE` — page may be written.
pub const PROT_WRITE: u64 = 0x2;
/// `PROT_EXEC` — page may be executed.
pub const PROT_EXEC: u64 = 0x4;
/// `PROT_GROWSDOWN` — apply the `prot` to one page below the
/// `VmEntry::grows_down` mapping. Slice 2 returns `-ENOSYS`.
pub const PROT_GROWSDOWN: u64 = 0x0100_0000;
/// `PROT_GROWSUP` — symmetrical extension to the `grows_down`
/// counterpart. Slice 2 returns `-ENOSYS`.
pub const PROT_GROWSUP: u64 = 0x0200_0000;

// ---------------------------------------------------------------------
// `MAP_*` flag bits — Linux generic uapi `<sys/mman.h>`. Slice 2 acts
// on `SHARED`/`PRIVATE` (mutual exclusion enforced — exactly one
// required), `FIXED` (FixedReplace placement), `FIXED_NOREPLACE`
// (RequireFree placement at the requested addr), `ANONYMOUS`
// (`VmBacking::PrivateAnon`), `GROWSDOWN`/`LOCKED` (threaded into
// `VmEntryFlags`). The remaining bits are recognised but ignored
// (best-effort hints) so userspace builds that pass them through
// don't trip `-EINVAL`.
// ---------------------------------------------------------------------

/// `MAP_SHARED` — share modifications with other mappings of the same
/// backing.
pub const MAP_SHARED: u64 = 0x01;
/// `MAP_PRIVATE` — copy-on-write: modifications never propagate to the
/// backing. Mutually exclusive with `MAP_SHARED`.
pub const MAP_PRIVATE: u64 = 0x02;
/// `MAP_FIXED` — interpret `addr` as the exact placement; any existing
/// mapping in the requested range is silently replaced
/// (`MapPlacement::FixedReplace`).
pub const MAP_FIXED: u64 = 0x10;
/// `MAP_ANONYMOUS` — mapping is not file-backed; `fd` and `offset` are
/// ignored. Routes to `VmBacking::PrivateAnon` (or
/// `VmBacking::None`-shaped Shared, which Slice 2 does not yet wire —
/// shared anon is treated like private anon for now).
pub const MAP_ANONYMOUS: u64 = 0x20;
/// `MAP_GROWSDOWN` — stack-style mapping, threads through to
/// `VmEntryFlags.grows_down`.
pub const MAP_GROWSDOWN: u64 = 0x0100;
/// `MAP_DENYWRITE` — historical no-op on Linux since 2.0; recognised so
/// userspace builds that still pass it don't trip `-EINVAL`.
pub const MAP_DENYWRITE: u64 = 0x0800;
/// `MAP_EXECUTABLE` — historical no-op on Linux; recognised but
/// ignored.
pub const MAP_EXECUTABLE: u64 = 0x1000;
/// `MAP_LOCKED` — mlock the mapping (best-effort hint).
pub const MAP_LOCKED: u64 = 0x2000;
/// `MAP_NORESERVE` — don't reserve swap space (best-effort hint;
/// txKernel doesn't model swap reservations).
pub const MAP_NORESERVE: u64 = 0x4000;
/// `MAP_POPULATE` — pre-fault the pages (best-effort hint; Slice 2
/// installs the recipe without forcing materialisation).
pub const MAP_POPULATE: u64 = 0x8000;
/// `MAP_NONBLOCK` — only meaningful with `MAP_POPULATE`; silently
/// ignored.
pub const MAP_NONBLOCK: u64 = 0x1_0000;
/// `MAP_STACK` — historical no-op on Linux; recognised but ignored.
pub const MAP_STACK: u64 = 0x2_0000;
/// `MAP_HUGETLB` — request hugepages. Slice 2 honours the bit by
/// accepting it (no `-EINVAL`) but does not allocate hugepages.
pub const MAP_HUGETLB: u64 = 0x4_0000;
/// `MAP_SYNC` — for `MAP_SHARED_VALIDATE` against a DAX-backed file;
/// recognised but ignored (txKernel has no DAX backing yet).
pub const MAP_SYNC: u64 = 0x8_0000;
/// `MAP_FIXED_NOREPLACE` — like `MAP_FIXED` but error with `-EEXIST`
/// on overlap rather than silently replace
/// (`MapPlacement::RequireFree`).
pub const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

// ---------------------------------------------------------------------
// `MADV_*` advice values — Linux generic uapi `<sys/mman.h>`. Slice 2
// recognises the subset `AddressSpace::madvise` consumes (`Normal`,
// `Random`, `Sequential`, `WillNeed`, `DontNeed`, `Free`); other
// values return `-ENOSYS`.
// ---------------------------------------------------------------------

/// `MADV_NORMAL = 0` — no special treatment (default).
pub const MADV_NORMAL: u64 = 0;
/// `MADV_RANDOM = 1` — expect random access.
pub const MADV_RANDOM: u64 = 1;
/// `MADV_SEQUENTIAL = 2` — expect sequential access.
pub const MADV_SEQUENTIAL: u64 = 2;
/// `MADV_WILLNEED = 3` — readahead hint.
pub const MADV_WILLNEED: u64 = 3;
/// `MADV_DONTNEED = 4` — release pages (mini-munmap; recipes preserved
/// so next access refaults clean).
pub const MADV_DONTNEED: u64 = 4;
/// `MADV_FREE = 8` — same as `DONTNEED` for txKernel's day-1 surface
/// (Linux distinguishes lazy vs. eager release; we treat both as
/// eager).
pub const MADV_FREE: u64 = 8;

// ---------------------------------------------------------------------
// Slice 3 of the shell-prompt roadmap — `futex(2)`.
//
// musl's libc init issues `FUTEX_WAIT` / `FUTEX_WAKE` for its
// `pthread_once`-style guards even in single-threaded programs, so
// without this number wired the busybox shell can't get past
// `__init_libc`. v1 supports `FUTEX_WAIT` / `FUTEX_WAKE` only; other
// op selectors return `-ENOSYS`. The `FUTEX_PRIVATE_FLAG` and
// `FUTEX_CLOCK_REALTIME` flag bits are recognised but ignored
// (per-process isolation is implicit from the per-aspace user word;
// timeout support is deferred to Slice 4 with the timer-wait carrier).
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 3.
// ---------------------------------------------------------------------

/// `futex(uaddr, op, val, timeout, uaddr2, val3)`. Linux RV64 generic
/// ABI `__NR_futex = 98`. Wraps `tx_subsystems::futex::step_futex_wait`
/// / `step_futex_wake`.
pub const NR_FUTEX: u64 = 98;

/// `FUTEX_WAIT = 0` op selector. Park if `*uaddr == val`, otherwise
/// return `-EAGAIN` immediately.
pub const FUTEX_WAIT: u32 = 0;
/// `FUTEX_WAKE = 1` op selector. Wake up to `val` waiters parked on
/// `uaddr`'s bucket. Returns the (best-effort) number woken.
pub const FUTEX_WAKE: u32 = 1;
/// `FUTEX_REQUEUE = 3`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_REQUEUE: u32 = 3;
/// `FUTEX_CMP_REQUEUE = 4`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_CMP_REQUEUE: u32 = 4;
/// `FUTEX_WAKE_OP = 5`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_WAKE_OP: u32 = 5;
/// `FUTEX_LOCK_PI = 6`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_LOCK_PI: u32 = 6;
/// `FUTEX_UNLOCK_PI = 7`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_UNLOCK_PI: u32 = 7;
/// `FUTEX_TRYLOCK_PI = 8`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_TRYLOCK_PI: u32 = 8;
/// `FUTEX_WAIT_BITSET = 9`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_WAIT_BITSET: u32 = 9;
/// `FUTEX_WAKE_BITSET = 10`. Out of scope for v1 — returns `-ENOSYS`.
pub const FUTEX_WAKE_BITSET: u32 = 10;

/// `FUTEX_PRIVATE_FLAG = 0x80` flag bit OR'd into the op word.
/// Recognised but ignored — per-process isolation falls out of the
/// per-aspace user word naturally. musl emits `FUTEX_WAIT_PRIVATE`
/// (`= FUTEX_WAIT | FUTEX_PRIVATE_FLAG`) for in-process guards.
pub const FUTEX_PRIVATE_FLAG: u32 = 0x80;
/// `FUTEX_CLOCK_REALTIME = 0x100` flag bit OR'd into the op word.
/// Recognised but ignored (timeout support is deferred to Slice 4).
pub const FUTEX_CLOCK_REALTIME: u32 = 0x100;
/// Mask applied to the `op` argument before matching the op
/// selector — strips `FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME`.
pub const FUTEX_CMD_MASK: u32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

// ---------------------------------------------------------------------
// Slice 4 of the shell-prompt roadmap — time syscalls
// (`clock_gettime` / `gettimeofday` / `nanosleep` / `clock_nanosleep` /
// `times`).
//
// Numbers verified against Linux's RV64 generic ABI
// (`include/uapi/asm-generic/unistd.h`). All arms read the platform
// monotonic clock via `<P as TimeIf>::read_ns()`. Day-1 clock-id
// surface aliases all four POSIX clocks to the platform monotonic
// (CLOCK_REALTIME has no boot-time RTC offset yet; CPU-time clocks
// have no per-process accounting yet — both are TODOs documented at
// the syscall arms). See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 4.
// ---------------------------------------------------------------------

/// `nanosleep(req, rem)`. Linux RV64 generic ABI `__NR_nanosleep = 101`.
///
/// Slice 4 ships the zero-duration short-circuit only: if `*req` is
/// already past (`tv_sec == 0 && tv_nsec == 0`, or the deadline has
/// already elapsed by the time we sample `read_ns()`), the arm returns
/// `0` immediately. Real-duration nanosleep needs a per-task timer-fire
/// wait carrier (so the parked task wakes on the next BSP reactor tick
/// after `read_ns() >= deadline`); that wiring is deferred — Slice 4's
/// timer-channel infrastructure lands in a follow-up slice. Real
/// non-zero durations currently return `-ENOSYS`. busybox sh barely
/// uses `nanosleep` so the deferral does not block Slice 11's QEMU
/// shell smoke.
pub const NR_NANOSLEEP: u64 = 101;
/// `clock_gettime(clk_id, ts)`. Linux RV64 generic ABI
/// `__NR_clock_gettime = 113`.
pub const NR_CLOCK_GETTIME: u64 = 113;
/// `clock_nanosleep(clk_id, flags, req, rem)`. Linux RV64 generic ABI
/// `__NR_clock_nanosleep = 115`. Same deferral as `nanosleep` — only
/// the zero-duration / past-deadline short-circuit ships in Slice 4.
pub const NR_CLOCK_NANOSLEEP: u64 = 115;
/// `times(buf)`. Linux RV64 generic ABI `__NR_times = 153`. Returns
/// the monotonic tick count (100Hz — `_SC_CLK_TCK` per Linux's RV64
/// uapi); writes a `struct tms` to `buf` (zero `stime`/`cutime`/`cstime`
/// since CPU-time accounting is not yet wired). Null `buf` is OK per
/// Linux semantics — only the return value matters.
pub const NR_TIMES: u64 = 153;
/// `gettimeofday(tv, tz)`. Linux RV64 generic ABI
/// `__NR_gettimeofday = 169`. The `tz` argument is deprecated on Linux
/// and the arm ignores it.
pub const NR_GETTIMEOFDAY: u64 = 169;

/// `clock_gettime` clock id: `CLOCK_REALTIME = 0`. Day-1 surface
/// aliases this to the platform monotonic clock — no boot-time RTC
/// offset yet (`TODO(phase-rtc)`).
pub const CLOCK_REALTIME: u32 = 0;
/// `clock_gettime` clock id: `CLOCK_MONOTONIC = 1`. Maps directly to
/// `<P as TimeIf>::read_ns()`.
pub const CLOCK_MONOTONIC: u32 = 1;
/// `clock_gettime` clock id: `CLOCK_PROCESS_CPUTIME_ID = 2`. Day-1
/// surface aliases this to the platform monotonic clock — no
/// per-process CPU-time accounting yet (`TODO(phase-cputime)`).
pub const CLOCK_PROCESS_CPUTIME_ID: u32 = 2;
/// `clock_gettime` clock id: `CLOCK_THREAD_CPUTIME_ID = 3`. Same
/// per-process aliasing as `CLOCK_PROCESS_CPUTIME_ID` — no per-thread
/// CPU-time accounting yet.
pub const CLOCK_THREAD_CPUTIME_ID: u32 = 3;
/// `clock_gettime` clock id: `CLOCK_MONOTONIC_RAW = 4`. Same as
/// `CLOCK_MONOTONIC` for v1 (txKernel doesn't NTP-discipline the
/// monotonic clock).
pub const CLOCK_MONOTONIC_RAW: u32 = 4;
/// `clock_gettime` clock id: `CLOCK_REALTIME_COARSE = 5`. Same as
/// `CLOCK_REALTIME`.
pub const CLOCK_REALTIME_COARSE: u32 = 5;
/// `clock_gettime` clock id: `CLOCK_MONOTONIC_COARSE = 6`. Same as
/// `CLOCK_MONOTONIC`.
pub const CLOCK_MONOTONIC_COARSE: u32 = 6;
/// `clock_gettime` clock id: `CLOCK_BOOTTIME = 7`. Same as
/// `CLOCK_MONOTONIC` — txKernel's monotonic clock starts at boot, so
/// "boot time" and "monotonic" are equivalent.
pub const CLOCK_BOOTTIME: u32 = 7;

/// `clock_nanosleep` flag bit: `TIMER_ABSTIME = 0x1`. When set, the
/// `req` value is interpreted as an absolute deadline (against the
/// selected clock) rather than a relative duration. Slice 4 honours
/// the bit for the zero-duration / past-deadline short-circuit
/// (which is independent of relative-vs-absolute interpretation —
/// already-past deadlines short-circuit either way).
pub const TIMER_ABSTIME: u32 = 0x1;

/// Tick frequency for `times(2)`'s return value (Linux's
/// `_SC_CLK_TCK`). Linux's RV64 generic ABI ships this as 100Hz —
/// `times` returns ticks-since-boot at this granularity.
pub const TIMES_TICK_HZ: u64 = 100;
/// Number of nanoseconds per `times(2)` tick (10ms at 100Hz).
pub const TIMES_NS_PER_TICK: u64 = 1_000_000_000 / TIMES_TICK_HZ;

// ---------------------------------------------------------------------
// Slice 5 of the shell-prompt roadmap — `ioctl(2)` + TTY routing.
//
// Without `ioctl`, musl's `isatty(STDIN_FILENO)` returns false, the
// shell starts in non-interactive mode and never prints a prompt. The
// arm decodes `request` against the eight TTY ioctls v1 supports; non-
// TTY fds and unknown request codes return `-ENOTTY` per Linux's
// `man ioctl_tty`. See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 5.
//
// Authoritative source: Linux generic uapi `<asm-generic/ioctls.h>` —
// musl's `<sys/ioctl.h>` for `riscv64-linux-musl` resolves to the same
// hex values.
// ---------------------------------------------------------------------

/// `ioctl(fd, request, argp)`. Linux RV64 generic ABI `__NR_ioctl = 29`.
pub const NR_IOCTL: u64 = 29;

/// `TCGETS = 0x5401` — read termios (`struct termios *argp`).
pub const TCGETS: u32 = 0x5401;
/// `TCSETS = 0x5402` — install termios immediately
/// (`struct termios *argp`).
pub const TCSETS: u32 = 0x5402;
/// `TCSETSW = 0x5403` — install termios after draining the output
/// queue. v1 aliases to `TCSETS` (drain semantics not yet implemented).
pub const TCSETSW: u32 = 0x5403;
/// `TCSETSF = 0x5404` — install termios after draining the output
/// queue and flushing the input queue. v1 aliases to `TCSETS`.
pub const TCSETSF: u32 = 0x5404;
/// `TIOCSCTTY = 0x540E` — make the calling process's session use this
/// TTY as its controlling terminal. Caller must be a session leader
/// without an existing controlling TTY.
pub const TIOCSCTTY: u32 = 0x540E;
/// `TIOCGPGRP = 0x540F` — read the foreground process-group id of
/// this TTY (`u32 *argp`).
pub const TIOCGPGRP: u32 = 0x540F;
/// `TIOCSPGRP = 0x5410` — set the foreground process-group id of this
/// TTY (`u32 *argp`).
pub const TIOCSPGRP: u32 = 0x5410;
/// `TIOCGWINSZ = 0x5413` — read the TTY's window size
/// (`struct winsize *argp`).
pub const TIOCGWINSZ: u32 = 0x5413;
/// `TIOCSWINSZ = 0x5414` — set the TTY's window size
/// (`struct winsize *argp`). Fires SIGWINCH at the foreground pgrp.
pub const TIOCSWINSZ: u32 = 0x5414;
/// `TIOCNOTTY = 0x5422` — detach this TTY as the calling session's
/// controlling terminal.
pub const TIOCNOTTY: u32 = 0x5422;

// ---------------------------------------------------------------------
// Slice 6 of the shell-prompt roadmap — stat family syscalls.
//
// Without these arms `ls` cannot enumerate (`getdents64`), `pwd`
// cannot render (`getcwd`), `cd` cannot change cwd (`chdir`), and the
// shell's `fstat(0)`/`fstat(1)`/`fstat(2)` startup probes (used to
// decide interactive mode) all fail. Numbers verified against Linux's
// RV64 generic ABI (`include/uapi/asm-generic/unistd.h`). See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 6.
// ---------------------------------------------------------------------

/// `getcwd(buf, size)`. Linux RV64 generic ABI `__NR_getcwd = 17`.
/// Renders the calling process's cwd `Cap<DEntry>` as an absolute
/// POSIX path via `step_getcwd` and copies the bytes (NUL terminator
/// included) into the user buffer. Returns the byte count written
/// on success; `-ERANGE` if `size` is too small for the rendered
/// path; `-ENOENT` if the cwd dentry chain is broken.
pub const NR_GETCWD: u64 = 17;
/// `chdir(path)`. Linux RV64 generic ABI `__NR_chdir = 49`. Walks
/// `path` from the caller's cwd, asserts the result is a directory,
/// then installs it via `step_chdir`. Returns `0` on success.
pub const NR_CHDIR: u64 = 49;
/// `fchdir(fd)`. Linux RV64 generic ABI `__NR_fchdir = 50`.
///
/// **Slice 6 carryover.** Returns `-ENOSYS` for now: `OpenFile` only
/// carries `Cap<RNode>`, not `Cap<DEntry>`, so we have no way to
/// recover the named-path edge `step_chdir` consumes from a directory
/// fd alone. Wiring the dentry hint onto OpenFile (or on a separate
/// per-fd metadata sidecar) is a follow-up slice — `fchdir` is rarely
/// used by shells, so the carryover does not block Slice 11's QEMU
/// shell smoke.
pub const NR_FCHDIR: u64 = 50;
/// `getdents64(fd, dirp, count)`. Linux RV64 generic ABI
/// `__NR_getdents64 = 61`. Calls `FsOps::readdir` with the per-fd
/// readdir cursor and encodes each `DirEntry` into the user buffer
/// as a `linux_dirent64` record. Returns the number of bytes
/// written; `0` at end-of-directory; `-EINVAL` if even the first
/// record won't fit in the supplied buffer; `-ENOTDIR` if the fd
/// refers to a non-directory backing.
pub const NR_GETDENTS64: u64 = 61;
/// `newfstatat(dirfd, path, statbuf, flags)`. Linux RV64 generic ABI
/// `__NR_newfstatat = 79`.
///
/// Slice 6's surface: `dirfd == AT_FDCWD` only (non-cwd dirfds return
/// `-EBADF`). `AT_EMPTY_PATH` paired with an empty path stats the
/// caller's cwd directly. `AT_SYMLINK_NOFOLLOW` is **deferred** —
/// the walker always follows symlinks at resolution time today
/// (Wave 3's symlink budget guards cycles, but a "stop on terminal
/// symlink" flag isn't plumbed yet). Documented carryover.
pub const NR_NEWFSTATAT: u64 = 79;
/// `fstat(fd, statbuf)`. Linux RV64 generic ABI `__NR_fstat = 80`.
/// On RV64 generic the `fstat` syscall's struct layout is identical
/// to `fstat64` (one shape, no 32/64 split). Reads the inode meta
/// from `OpenFile.rnode().meta()` and writes the Linux `struct stat`
/// layout into the user buffer.
pub const NR_FSTAT: u64 = 80;
/// `umask(mask)`. Linux RV64 generic ABI `__NR_umask = 166`. Atomic
/// swap of the per-process file-creation mask, returning the
/// previous value. Mask is silently truncated to the bottom 9 bits
/// (`rwxrwxrwx` only — kernel ignores the kind / setuid / setgid /
/// sticky bits per Linux semantics).
pub const NR_UMASK: u64 = 166;

// AT_* flag bits used by the stat-family arms. `AT_FDCWD = -100` and
// `AT_SYMLINK_NOFOLLOW = 0x100` are defined earlier in this file
// (DAC + setuid Wave 4 Part 4 introduced them for the file-mode
// arms). Slice 6 only needs the additional `AT_EMPTY_PATH` /
// `AT_NO_AUTOMOUNT` bits below.

/// `AT_EMPTY_PATH = 0x1000` flag bit (4th arg of `newfstatat`). When
/// set with an empty path, the syscall operates on the dirfd itself
/// (or, for `AT_FDCWD`, the caller's cwd). Slice 6 honours this only
/// for `AT_FDCWD + ""`; non-cwd dirfds with `AT_EMPTY_PATH` return
/// `-EBADF` (same as the dirfd-rejection path).
pub const AT_EMPTY_PATH: u32 = 0x1000;
/// `AT_NO_AUTOMOUNT = 0x800` flag bit (4th arg of `newfstatat`).
/// Slice 6 has no automount machinery — the bit is recognised but
/// ignored. Documented carryover; matches Linux's
/// "ignored-when-no-automount" lenience.
pub const AT_NO_AUTOMOUNT: u32 = 0x800;

// `linux_dirent64` `d_type` byte values per
// `include/uapi/linux/dirent.h`. Encoded into each record's
// `d_type` byte by `sys_getdents64` after mapping the
// `vfs::structure::InodeKind` enum.

/// `DT_UNKNOWN = 0` — backend has no kind information for the entry.
/// Not produced by the in-tree backends (tmpfs / devfs always know
/// the kind from the inode meta), but the constant exists for ABI
/// completeness.
pub const DT_UNKNOWN: u8 = 0;
/// `DT_FIFO = 1` — `InodeKind::Fifo`.
pub const DT_FIFO: u8 = 1;
/// `DT_CHR = 2` — `InodeKind::CharDevice`.
pub const DT_CHR: u8 = 2;
/// `DT_DIR = 4` — `InodeKind::Directory`.
pub const DT_DIR: u8 = 4;
/// `DT_BLK = 6` — `InodeKind::BlockDevice`.
pub const DT_BLK: u8 = 6;
/// `DT_REG = 8` — `InodeKind::Regular`.
pub const DT_REG: u8 = 8;
/// `DT_LNK = 10` — `InodeKind::Symlink`.
pub const DT_LNK: u8 = 10;
/// `DT_SOCK = 12` — `InodeKind::Socket`.
pub const DT_SOCK: u8 = 12;

// ---------------------------------------------------------------------
// Slice 7 of the shell-prompt roadmap — fcntl extension + day-1 misc.
//
// Numbers verified against Linux's RV64 generic ABI
// (`include/uapi/asm-generic/unistd.h`). This slice ships a grab-bag
// of small day-1-blocking syscalls — none individually heavy; each
// unblocks a specific shell-startup path. See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 7.
// ---------------------------------------------------------------------

/// `kill(pid, sig)`. Linux RV64 generic ABI `__NR_kill = 129`. Routes
/// through `tx_subsystems::signal::step_kill_process` after resolving
/// the target via the init-rooted process tree walk
/// (`process::execution::process_by_pid`).
///
/// Slice 7 v1 surface: only `pid > 0` is supported. Negative / zero
/// pids (process-group / all-process targets) return `-ENOSYS` —
/// pgrp-targeted kills need a global pid-to-pgrp lookup the slice
/// does not yet wire. `sig == 0` is the existence-probe shape: the
/// arm returns `0` for live targets, `-ESRCH` for unknown / zombie
/// targets.
pub const NR_KILL: u64 = 129;
/// `tkill(tid, sig)`. Linux RV64 generic ABI `__NR_tkill = 130`.
///
/// Slice 7 v1 aliases this to [`NR_KILL`]: txKernel has no per-thread
/// signal state machine yet, so `tkill(tid, sig)` is treated as
/// `kill(tid, sig)` (the tid is interpreted as a pid). Real per-thread
/// signal posting is `TODO(phase-thread-signals)`.
pub const NR_TKILL: u64 = 130;
/// `tgkill(tgid, tid, sig)`. Linux RV64 generic ABI `__NR_tgkill = 131`.
///
/// Slice 7 v1 aliases this to [`NR_KILL`]: `tgid` is interpreted as a
/// pid, `tid` is ignored. Real tgid+tid resolution is
/// `TODO(phase-thread-signals)`.
pub const NR_TGKILL: u64 = 131;
/// `rt_sigreturn(...)`. Linux RV64 generic ABI `__NR_rt_sigreturn = 139`.
///
/// **Slice 7 carryover.** Returns `-ENOSYS` for now. The
/// `SignalFrameIf::restore_signal_frame` surface in `tx-hal` requires
/// a `TrapFrameMut<'_>` on the live trap frame and the user-stack
/// pointer the kernel parked at signal-frame setup time; the
/// `SyscallCtx` shape does not yet expose the trap frame to the
/// dispatcher. Wiring rt_sigreturn end-to-end requires the trap-shell
/// to either invoke the SignalFrameIf method directly (bypassing the
/// dispatcher) or pass the trap-frame pointer through the syscall
/// context — both are out of scope for Slice 7. Real signal handlers
/// are not yet wired anyway (no userspace handler trampoline path),
/// so the carryover does not block any day-1 shell flow.
/// `TODO(phase-signal-frame)`.
pub const NR_RT_SIGRETURN: u64 = 139;
/// `uname(buf)`. Linux RV64 generic ABI `__NR_uname = 160`. Writes
/// the static utsname (`sysname` / `nodename` / `release` / `version`
/// / `machine` / `domainname`, each `[u8; 65]`) to `buf`. Slice 7
/// pins `release = "6.1.0-txkernel"` so musl's runtime version probes
/// see a Linux 2.6.16+ kernel.
pub const NR_UNAME: u64 = 160;
/// `prlimit64(pid, resource, new_rlim, old_rlim)`. Linux RV64 generic
/// ABI `__NR_prlimit64 = 261`.
///
/// Slice 7 v1 ships a read-only static rlimit table for the calling
/// process only (`pid == 0` or `pid == self.pid`); cross-pid queries
/// return `-EPERM`. `new_rlim` is silently ignored — limits are not
/// actually enforced by any in-tree subsystem yet
/// (`TODO(phase-rlimit-enforcement)`). The static table is generous
/// (`RLIMIT_NOFILE = 1024 / 4096`, `RLIMIT_STACK = 8 MiB`, the rest
/// `RLIM_INFINITY`) — matches the values musl probes and accepts as
/// non-restrictive.
pub const NR_PRLIMIT64: u64 = 261;
/// `getrandom(buf, buflen, flags)`. Linux RV64 generic ABI
/// `__NR_getrandom = 278`. Fills `buf` with `buflen` bytes from the
/// platform entropy source via `<P as EntropyIf>::fill_random`.
///
/// Slice 7 v1: `flags` (`GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE`)
/// are recognised but ignored — the in-tree EntropyIf default is
/// already deterministic + non-blocking. Returns the number of bytes
/// written (always equals `buflen`); never short-reads on the v1
/// surface. Null `buf` with non-zero `buflen` returns `-EFAULT`.
pub const NR_GETRANDOM: u64 = 278;

// ---------------------------------------------------------------------
// fcntl command numbers — Slice 7 extension. F_GETFD / F_SETFD live
// earlier in this file (Wave 2 of the ELF-loader plan); Slice 7 adds
// F_DUPFD / F_DUPFD_CLOEXEC / F_GETFL / F_SETFL.
// ---------------------------------------------------------------------

/// `F_DUPFD` cmd: duplicate `fd` into the lowest-numbered slot
/// `≥ arg`. Returns the new fd. The new fd has the close-on-exec bit
/// **cleared** per POSIX (only `F_DUPFD_CLOEXEC` sets it).
pub const F_DUPFD: i32 = 0;
/// `F_GETFL` cmd: read the per-OpenFile access mode + open-flag bits.
/// Returns the bits as a non-negative `i32`; never errors on a valid
/// open fd.
///
/// Slice 7 surface: composes the access mode (`O_RDONLY` / `O_WRONLY`
/// / `O_RDWR`) from `OpenFileFlags::{read,write}`, OR's `O_APPEND`
/// from `OpenFileFlags::append`, OR's `O_NONBLOCK` from
/// `OpenFileFlags::nonblocking`. `O_CLOEXEC` is **not** included
/// (Linux semantic: cloexec is per-fd, queried via `F_GETFD`, not
/// per-OpenFile).
pub const F_GETFL: i32 = 3;
/// `F_SETFL` cmd: replace the per-OpenFile open-flag bits.
///
/// **Slice 7 carryover.** Returns `-ENOSYS` for now. The
/// `OpenFileFlags` struct in `vfs::structure` is a plain `Copy`-struct
/// field on `OpenFile` (not behind an atomic / mutex), so the
/// "replace flags atomically" semantic F_SETFL needs is not safe under
/// the current shape. Wiring interior mutability onto OpenFileFlags is
/// the gating change; once it lands, this command moves to the
/// mutator side. `TODO(phase-fcntl-setfl)`.
pub const F_SETFL: i32 = 4;
/// `F_DUPFD_CLOEXEC` cmd: like [`F_DUPFD`] but the new fd is marked
/// close-on-exec (the per-fd CLOEXEC bit is set on the result).
pub const F_DUPFD_CLOEXEC: i32 = 1030;

// ---------------------------------------------------------------------
// `RLIMIT_*` resource ids — Linux generic uapi `<sys/resource.h>`.
// Used by [`NR_PRLIMIT64`] to index a read-only static table of
// `(rlim_cur, rlim_max)` pairs.
// ---------------------------------------------------------------------

/// `RLIMIT_CPU = 0` — CPU-time limit in seconds.
pub const RLIMIT_CPU: u32 = 0;
/// `RLIMIT_FSIZE = 1` — maximum file size.
pub const RLIMIT_FSIZE: u32 = 1;
/// `RLIMIT_DATA = 2` — maximum data segment size.
pub const RLIMIT_DATA: u32 = 2;
/// `RLIMIT_STACK = 3` — maximum stack size. Slice 7 reports 8 MiB
/// (musl's startup probe accepts this as non-restrictive).
pub const RLIMIT_STACK: u32 = 3;
/// `RLIMIT_CORE = 4` — maximum core-file size.
pub const RLIMIT_CORE: u32 = 4;
/// `RLIMIT_RSS = 5` — maximum resident set size.
pub const RLIMIT_RSS: u32 = 5;
/// `RLIMIT_NPROC = 6` — maximum number of processes per real uid.
pub const RLIMIT_NPROC: u32 = 6;
/// `RLIMIT_NOFILE = 7` — maximum open file descriptors. Slice 7
/// reports `(1024, 4096)`.
pub const RLIMIT_NOFILE: u32 = 7;
/// `RLIMIT_MEMLOCK = 8` — maximum locked-in-memory bytes.
pub const RLIMIT_MEMLOCK: u32 = 8;
/// `RLIMIT_AS = 9` — maximum address-space size.
pub const RLIMIT_AS: u32 = 9;
/// `RLIMIT_LOCKS = 10` — maximum file locks held.
pub const RLIMIT_LOCKS: u32 = 10;
/// `RLIMIT_SIGPENDING = 11` — maximum queued signals.
pub const RLIMIT_SIGPENDING: u32 = 11;
/// `RLIMIT_MSGQUEUE = 12` — maximum POSIX message-queue bytes.
pub const RLIMIT_MSGQUEUE: u32 = 12;
/// `RLIMIT_NICE = 13` — ceiling on nice value (offset by 20).
pub const RLIMIT_NICE: u32 = 13;
/// `RLIMIT_RTPRIO = 14` — ceiling on real-time scheduling priority.
pub const RLIMIT_RTPRIO: u32 = 14;
/// `RLIMIT_RTTIME = 15` — maximum realtime-priority CPU time without
/// blocking.
pub const RLIMIT_RTTIME: u32 = 15;

/// `RLIM_INFINITY` sentinel — `u64::MAX`. Linux uapi
/// `<sys/resource.h>` `RLIM_INFINITY = (~0UL)`. Used in the static
/// table for limits txKernel does not enforce.
pub const RLIM_INFINITY: u64 = u64::MAX;

// ---------------------------------------------------------------------
// `getrandom(2)` flag bits — Linux uapi `<sys/random.h>`. All three
// are recognised and silently ignored by Slice 7's arm; the in-tree
// EntropyIf default is already deterministic + non-blocking, so
// `GRND_NONBLOCK` is implicit and `GRND_RANDOM` (urandom vs random
// pool) has no meaning when there is no urandom pool.
// ---------------------------------------------------------------------

/// `GRND_NONBLOCK = 0x1` — return `-EAGAIN` rather than blocking when
/// the entropy pool is uninitialised. Slice 7: ignored (the default
/// EntropyIf never blocks).
pub const GRND_NONBLOCK: u32 = 0x1;
/// `GRND_RANDOM = 0x2` — read from the random pool instead of urandom.
/// Slice 7: ignored (single entropy source).
pub const GRND_RANDOM: u32 = 0x2;
/// `GRND_INSECURE = 0x4` — return whatever bytes the kernel has even
/// if the pool is not yet seeded. Slice 7: ignored.
pub const GRND_INSECURE: u32 = 0x4;

// ---------------------------------------------------------------------
// Slice 8 — file-mutation syscalls. NR_MKDIRAT, NR_UNLINKAT,
// NR_SYMLINKAT, NR_LINKAT, NR_TRUNCATE, NR_FTRUNCATE, NR_READLINKAT,
// NR_UTIMENSAT, NR_RENAMEAT2 plus the `AT_REMOVEDIR`, `RENAME_*`,
// `UTIME_NOW` / `UTIME_OMIT` flag constants. Each arm wraps the
// in-tree `FsOps::*` step bodies (`unlink` / `rename` / `link` /
// `mkdir` / `rmdir` / `symlink` / `read_link`) plus the
// `page_backed::lifecycle::step_truncate` body for truncate /
// ftruncate. See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 8.
// ---------------------------------------------------------------------

/// `NR_MKDIRAT = 34` — Linux RV64 generic ABI `__NR_mkdirat`.
pub const NR_MKDIRAT: u64 = 34;
/// `NR_UNLINKAT = 35` — Linux RV64 generic ABI `__NR_unlinkat`.
pub const NR_UNLINKAT: u64 = 35;
/// `NR_SYMLINKAT = 36` — Linux RV64 generic ABI `__NR_symlinkat`.
pub const NR_SYMLINKAT: u64 = 36;
/// `NR_LINKAT = 37` — Linux RV64 generic ABI `__NR_linkat`.
pub const NR_LINKAT: u64 = 37;
/// `NR_TRUNCATE = 45` — Linux RV64 generic ABI `__NR_truncate`. Slice 8
/// wires the path-named form against `step_truncate` for PageBacked
/// regular files.
pub const NR_TRUNCATE: u64 = 45;
/// `NR_FTRUNCATE = 46` — Linux RV64 generic ABI `__NR_ftruncate`.
/// Slice 8: PageBacked fds only; non-page-backed surfaces return
/// `-EINVAL` per `step_truncate`.
pub const NR_FTRUNCATE: u64 = 46;
/// `NR_READLINKAT = 78` — Linux RV64 generic ABI `__NR_readlinkat`.
/// Slice 8 walks the link's parent directory and calls
/// `FsOps::lookup` + `read_link` directly so the symlink's target
/// bytes are returned without the walker following the link.
pub const NR_READLINKAT: u64 = 78;
/// `NR_UTIMENSAT = 88` — Linux RV64 generic ABI `__NR_utimensat`.
/// Slice 8 returns `-ENOSYS` (no `FsOps::set_times` hook yet); see
/// the slice plan §"Out of scope".
pub const NR_UTIMENSAT: u64 = 88;
/// `NR_RENAMEAT2 = 276` — Linux RV64 generic ABI `__NR_renameat2`.
/// Slice 8: `RENAME_NOREPLACE` honoured via a pre-walk existence
/// check; `RENAME_EXCHANGE` and `RENAME_WHITEOUT` return `-ENOSYS`
/// (no atomic-swap surface yet).
pub const NR_RENAMEAT2: u64 = 276;

/// `AT_REMOVEDIR = 0x200` — `unlinkat(2)` flag bit. When set the arm
/// dispatches through `FsOps::rmdir` instead of `unlink` (matching
/// Linux's `unlinkat(.., AT_REMOVEDIR)` shape). Without this bit a
/// directory target surfaces `-EISDIR`.
pub const AT_REMOVEDIR: u32 = 0x200;

/// `RENAME_NOREPLACE = 1` — `renameat2(2)` flag. The kernel rejects
/// the rename with `-EEXIST` if `newpath` already exists. Slice 8
/// implements this via a pre-walk: if the new path resolves
/// successfully, the arm short-circuits without touching `FsOps::rename`.
pub const RENAME_NOREPLACE: u32 = 1;
/// `RENAME_EXCHANGE = 2` — atomically swap two existing paths.
/// Slice 8: returns `-ENOSYS` (no FsOps surface for atomic swap).
pub const RENAME_EXCHANGE: u32 = 2;
/// `RENAME_WHITEOUT = 4` — overlayfs whiteout-creating rename.
/// Slice 8: returns `-EINVAL` (recognised but unsupported flag bit).
pub const RENAME_WHITEOUT: u32 = 4;

/// `UTIME_NOW = (1 << 30) - 1` — `utimensat(2)` "use current time"
/// sentinel. Slice 8 carries the constant for grep-stability;
/// `sys_utimensat` returns `-ENOSYS` regardless.
pub const UTIME_NOW: i64 = (1 << 30) - 1;
/// `UTIME_OMIT = (1 << 30) - 2` — `utimensat(2)` "leave unchanged"
/// sentinel.
pub const UTIME_OMIT: i64 = (1 << 30) - 2;
