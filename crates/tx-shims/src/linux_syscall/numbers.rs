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
/// `pread64(fd, buf, count, offset)`. Linux generic ABI `__NR_pread64`.
pub const NR_PREAD64: u64 = 67;
/// `pwrite64(fd, buf, count, offset)`. Linux generic ABI `__NR_pwrite64`.
pub const NR_PWRITE64: u64 = 68;
/// `readv(fd, iov, iovcnt)`. Linux generic ABI `__NR_readv`.
pub const NR_READV: u64 = 65;
/// `preadv(fd, iov, iovcnt, offset_lo, offset_hi)`. Linux RV64 generic ABI.
pub const NR_PREADV: u64 = 69;
/// `pwritev(fd, iov, iovcnt, offset_lo, offset_hi)`. Linux RV64 generic ABI.
pub const NR_PWRITEV: u64 = 70;
/// `socket(domain, type, protocol)`. Linux generic ABI `__NR_socket`.
pub const NR_SOCKET: u64 = 198;
/// `socketpair(domain, type, protocol, sv)`. Linux generic ABI `__NR_socketpair`.
pub const NR_SOCKETPAIR: u64 = 199;
/// `bind(sockfd, addr, addrlen)`. Linux generic ABI `__NR_bind`.
pub const NR_BIND: u64 = 200;
/// `listen(sockfd, backlog)`. Linux generic ABI `__NR_listen`.
pub const NR_LISTEN: u64 = 201;
/// `accept(sockfd, addr, addrlen)`. Linux generic ABI `__NR_accept`.
pub const NR_ACCEPT: u64 = 202;
/// `connect(sockfd, addr, addrlen)`. Linux generic ABI `__NR_connect`.
pub const NR_CONNECT: u64 = 203;
/// `getsockname(sockfd, addr, addrlen)`. Linux generic ABI `__NR_getsockname`.
pub const NR_GETSOCKNAME: u64 = 204;
/// `getpeername(sockfd, addr, addrlen)`. Linux generic ABI `__NR_getpeername`.
pub const NR_GETPEERNAME: u64 = 205;
/// `sendto(sockfd, buf, len, flags, dest_addr, addrlen)`. Linux generic ABI `__NR_sendto`.
pub const NR_SENDTO: u64 = 206;
/// `recvfrom(sockfd, buf, len, flags, src_addr, addrlen)`. Linux generic ABI `__NR_recvfrom`.
pub const NR_RECVFROM: u64 = 207;
/// `setsockopt(sockfd, level, optname, optval, optlen)`. Linux generic ABI `__NR_setsockopt`.
pub const NR_SETSOCKOPT: u64 = 208;
/// `getsockopt(sockfd, level, optname, optval, optlen)`. Linux generic ABI `__NR_getsockopt`.
pub const NR_GETSOCKOPT: u64 = 209;
/// `shutdown(sockfd, how)`. Linux generic ABI `__NR_shutdown`.
pub const NR_SHUTDOWN: u64 = 210;
/// `accept4(sockfd, addr, addrlen, flags)`. Linux generic ABI `__NR_accept4`.
pub const NR_ACCEPT4: u64 = 242;
/// `fanotify_init(flags, event_f_flags)`. Linux RV64 generic ABI.
pub const NR_FANOTIFY_INIT: u64 = 262;
/// `fanotify_mark(fanotify_fd, flags, mask, dfd, pathname)`. Linux RV64 generic ABI.
pub const NR_FANOTIFY_MARK: u64 = 263;
/// `sendfile64(out_fd, in_fd, offset, count)`. Linux generic ABI
/// `__NR_sendfile64`. Copies data from `in_fd` to `out_fd` via
/// page-level transfer without an intermediate userspace buffer.
pub const NR_SENDFILE64: u64 = 71;
/// `vmsplice(fd, iov, nr_segs, flags)`. Linux RV64 generic ABI.
pub const NR_VMSPLICE: u64 = 75;
/// `splice(fd_in, off_in, fd_out, off_out, len, flags)`. Linux RV64 generic ABI.
pub const NR_SPLICE: u64 = 76;
/// `tee(fd_in, fd_out, len, flags)`. Linux RV64 generic ABI.
pub const NR_TEE: u64 = 77;
/// `sync_file_range(fd, offset, nbytes, flags)`. Linux RV64 generic ABI.
pub const NR_SYNC_FILE_RANGE: u64 = 84;
/// `sched_setscheduler(pid, policy, param)`. Linux generic uapi
/// `__NR_sched_setscheduler = 119`. musl calls this during
/// pthread_create to set the new thread's scheduling policy.
/// v1 stub: returns 0 (success, no-op) — real priority
/// inheritance deferred to the scheduler slice.
pub const NR_SCHED_SETSCHEDULER: u64 = 119;
/// `ppoll(fds, nfds, tmo_p, sigmask)`. Linux generic ABI
/// `__NR_ppoll`. busybox sh's interactive read loop polls stdin
/// before reading. The v1 implementation is a minimal stub: walk
/// the pollfd array, mark each fd with the requested events as
/// "ready" (revents = events), return nfds. The actual block
/// happens in the subsequent `read()` if the TTY input queue is
/// empty — busybox observes the same external behaviour as on
/// Linux (poll says ready, read either returns bytes or blocks).
pub const NR_PPOLL: u64 = 73;
/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask)`.
/// Linux generic ABI `__NR_pselect6 = 72`. musl's `select(3)`
/// wrapper routes through this syscall on RISC-V, and lmbench's
/// `benchmp` pipe handshake depends on fd-set readiness.
pub const NR_PSELECT6: u64 = 72;
/// `exit(status)`. Linux generic ABI `__NR_exit`. Per-thread exit per
/// `PROCESS_v1` §7.3.1 — for a single-threaded process, the
/// `step_thread_exit` chain triggers `step_process_exit` internally.
pub const NR_EXIT: u64 = 93;
/// `personality(persona)`. Linux RV64 generic ABI `__NR_personality = 92`.
pub const NR_PERSONALITY: u64 = 92;
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
/// `restart_syscall()`. Linux RV64 generic ABI `__NR_restart_syscall = 128`.
/// Explicitly dispatched as `-ENOSYS` until interrupted-sleep restart state
/// exists in the syscall ABI.
pub const NR_RESTART_SYSCALL: u64 = 128;
/// `ioprio_set(which, who, ioprio)`. Linux RV64 generic ABI
/// `__NR_ioprio_set = 30`.
pub const NR_IOPRIO_SET: u64 = 30;
/// `ioprio_get(which, who)`. Linux RV64 generic ABI
/// `__NR_ioprio_get = 31`.
pub const NR_IOPRIO_GET: u64 = 31;
/// `sched_setparam(pid, param)`. Linux RV64 generic ABI
/// `__NR_sched_setparam = 118`.
pub const NR_SCHED_SETPARAM: u64 = 118;
/// `setpriority(which, who, niceval)`. Linux RV64 generic ABI
/// `__NR_setpriority = 140`.
pub const NR_SETPRIORITY: u64 = 140;
/// `getpriority(which, who)`. Linux RV64 generic ABI
/// `__NR_getpriority = 141`.
pub const NR_GETPRIORITY: u64 = 141;
/// `fcntl(fd, cmd, arg)`. Linux generic ABI `__NR_fcntl` (= `__NR3264_fcntl`).
///
/// Wave 2 of the ELF loader plan shipped the initial `F_GETFD` /
/// `F_SETFD` subset against the per-process CLOEXEC bitmap
/// (`ProcessPayload.fd_cloexec`). Follow-up phases now also cover the
/// duplicated-fd and status-flag commands used by fd-heavy workloads;
/// commands outside the implemented subset still return `-ENOSYS`.
pub const NR_FCNTL: u64 = 25;
/// `inotify_init1(flags)`. Linux RV64 generic ABI.
pub const NR_INOTIFY_INIT1: u64 = 26;
/// `inotify_add_watch(fd, pathname, mask)`. Linux RV64 generic ABI.
pub const NR_INOTIFY_ADD_WATCH: u64 = 27;
/// `inotify_rm_watch(fd, wd)`. Linux RV64 generic ABI.
pub const NR_INOTIFY_RM_WATCH: u64 = 28;
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
/// `getgroups(gidsetsize, grouplist)`. Linux RV64 generic ABI
/// `__NR_getgroups = 158`. txKernel v1 does not model supplementary
/// groups, so this reports zero entries and performs no writes.
pub const NR_GETGROUPS: u64 = 158;

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
/// Recognised `flags`: `O_CLOEXEC | O_NONBLOCK | O_DIRECT`.
/// `O_DIRECT` creates packet-mode pipes. Any other bits return
/// `-EINVAL`.
///
/// **SIGPIPE delivery.** Q2 DECIDED 2026-05-07: `OpenFile::step_write`
/// returns `Err(EPIPE)` when all readers have closed. The
/// `sys_write` arm intercepts `Errno::EPIPE` and dispatches SIGPIPE
/// to the calling process via `signal::step_kill_process` before
/// returning `-EPIPE` to userspace. The pipe module itself has no
/// process Cap and so cannot deliver the signal.
pub const NR_PIPE2: u64 = 59;
/// `O_DIRECT` flag bit (`0o40000`). For `pipe2`, this means Linux
/// packet mode. Other open arms currently ignore this bit.
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

pub const CLONE_SETTLS: u64 = 0x80000;
pub const CLONE_VM: u64 = 0x100;
pub const CLONE_FS: u64 = 0x200;
pub const CLONE_FILES: u64 = 0x400;
pub const CLONE_SIGHAND: u64 = 0x800;
pub const CLONE_VFORK: u64 = 0x4000;
pub const CLONE_PARENT: u64 = 0x8000;
pub const CLONE_THREAD: u64 = 0x10000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x200000;
pub const CLONE_PARENT_SETTID: u64 = 0x100000;
/// Ignored by Linux since 2.5.32; musl sets it unconditionally.
pub const CLONE_DETACHED: u64 = 0x400000;
/// System-V semaphore undo on exit; musl sets this in pthread_create.
pub const CLONE_SYSVSEM: u64 = 0x40000;
/// Namespace flags. `CLONE_NEWIPC` is wired to the process nsproxy
/// clone path; the others are still silently accepted by pthread_create
/// compatibility paths and remain namespace stubs.
pub const CLONE_NEWNS: u64 = 0x20000;
pub const CLONE_NEWCGROUP: u64 = 0x2000000;
pub const CLONE_NEWUTS: u64 = 0x4000000;
pub const CLONE_NEWIPC: u64 = 0x8000000;
pub const CLONE_NEWUSER: u64 = 0x1000_0000;
pub const CLONE_NEWNET: u64 = 0x4000_0000;

/// `getppid()`. Linux generic ABI `__NR_getppid`. Wraps
/// `ProcessIdentity::parent_pid()`. Returns `0` (`Pid::RESERVED`)
/// for orphans (init's pid 1 has no parent). Real Linux returns
/// init's pid for orphans; the trio's `sever_children` reparents to
/// init when init is registered, so under normal flows the difference
/// is invisible.
pub const NR_GETPPID: u64 = 173;
pub const NR_GETTID: u64 = 178;

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
/// `get_robust_list(pid, head, len)`. Linux generic ABI
/// `__NR_get_robust_list`.
pub const NR_GET_ROBUST_LIST: u64 = 100;
/// `sched_setaffinity(pid, cpusetsize, mask)`. Linux generic ABI.
pub const NR_SCHED_SETAFFINITY: u64 = 122;
/// `sched_getaffinity(pid, cpusetsize, mask)`. Linux generic ABI.
pub const NR_SCHED_GETAFFINITY: u64 = 123;
/// `sched_getscheduler(pid)`. Linux RV64 generic ABI.
pub const NR_SCHED_GETSCHEDULER: u64 = 120;
/// `sched_getparam(pid, param)`. Linux RV64 generic ABI.
pub const NR_SCHED_GETPARAM: u64 = 121;
/// `sched_yield()`. Linux RV64 generic ABI.
pub const NR_SCHED_YIELD: u64 = 124;
/// `sched_get_priority_max(policy)`. Linux RV64 generic ABI.
pub const NR_SCHED_GET_PRIORITY_MAX: u64 = 125;
/// `sched_get_priority_min(policy)`. Linux RV64 generic ABI.
pub const NR_SCHED_GET_PRIORITY_MIN: u64 = 126;
/// `sched_rr_get_interval(pid, timespec)`. Linux RV64 generic ABI.
pub const NR_SCHED_RR_GET_INTERVAL: u64 = 127;

// ---------------------------------------------------------------------
// Wave 3 of the fork/clone/wait4 slice — Part 3 (NR_WAIT4 syscall arm
// with blocking-wait via the per-process `exit_source` carrier wired in
// Wave 1). NR_WAITID is intentionally absent — deferred per the slice
// plan's Open Q #2 (DECIDED 2026-05-06: NR_WAITID deferred).
// ---------------------------------------------------------------------

/// `wait4(pid, status, options, rusage)`. Linux RV64 generic ABI
/// `__NR_wait4`.
///
/// Wave 3 of the fork/clone/wait4 slice ships the blocking variant —
/// when no zombie matches and `WNOHANG` is unset, the arm parks on the
/// caller's per-process `exit_source` carrier (registered at payload
/// sign time per Wave 1) via
/// [`tx_subsystems::wait_source::wait_on_token`], waking when any
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
// (`fchmod`, `fchmodat`, `fchmodat2`, `fchown`, `fchownat`,
// `faccessat`, `faccessat2`). Each wraps the
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
/// `fchmod(fd, mode)`. Linux RV64 generic ABI `__NR_fchmod = 52`.
/// Resolves the fd's `OpenFile` rnode and uses the same chmod
/// authorization and FsOps mutation path as `fchmodat`.
pub const NR_FCHMOD: u64 = 52;
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
/// `fchown(fd, uid, gid)`. Linux RV64 generic ABI `__NR_fchown = 55`.
/// Resolves the fd's `OpenFile` rnode and uses the same chown path as
/// `fchownat`, including the `(u32)-1` leave-unchanged sentinels.
pub const NR_FCHOWN: u64 = 55;
/// `faccessat2(dirfd, path, mode, flags)`. Linux RV64 generic ABI
/// `__NR_faccessat2 = 439`. Same as `faccessat` plus the `flags`
/// argument — `AT_EACCESS` switches the check from real uid/gid to
/// effective uid/gid. LTP cluster: `faccessat201..03`.
pub const NR_FACCESSAT2: u64 = 439;
/// `fchmodat2(dirfd, path, mode, flags)`. Linux RV64 generic ABI
/// `__NR_fchmodat2 = 452`. Routes through the same semantics and flag
/// handling as `fchmodat` in the current slice.
pub const NR_FCHMODAT2: u64 = 452;

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
/// generic ABI `__NR_mremap = 216`.
pub const NR_MREMAP: u64 = 216;
/// `MREMAP_MAYMOVE` — permit the kernel to move the mapping if in-place
/// resize cannot be satisfied.
pub const MREMAP_MAYMOVE: u64 = 0x1;
/// `MREMAP_FIXED` — move to the supplied fifth argument. Linux requires
/// this to be paired with `MREMAP_MAYMOVE`.
pub const MREMAP_FIXED: u64 = 0x2;
/// `MREMAP_DONTUNMAP` — accepted by newer Linux only for specialized
/// userfaultfd-style moves; txKernel does not implement it yet.
pub const MREMAP_DONTUNMAP: u64 = 0x4;
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
/// `mlock(addr, len)`. Linux RV64 generic ABI `__NR_mlock = 228`.
/// Under no-swap this is purely observational; sets
/// `VmEntryFlags.locked` for `/proc/<pid>/maps` reporting.
pub const NR_MLOCK: u64 = 228;
/// `munlock(addr, len)`. Linux RV64 generic ABI `__NR_munlock = 229`.
/// Clears `VmEntryFlags.locked`.
pub const NR_MUNLOCK: u64 = 229;
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
/// ignored. Private anonymous mappings route to `VmBacking::PrivateAnon`;
/// shared anonymous mappings allocate an anonymous `PageContainer`.
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
// `__init_libc`. v1 supports wait/wake, bitset wait/wake, requeue /
// cmp-requeue, wake-op, and best-effort PI lock/trylock/unlock
// selectors. The `FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME` flag
// bits are recognised at the syscall layer; per-process isolation is
// implicit from the per-aspace user word.
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
/// `FUTEX_REQUEUE = 3`. Best-effort wake/requeue selector.
pub const FUTEX_REQUEUE: u32 = 3;
/// `FUTEX_CMP_REQUEUE = 4`. Compare-and-requeue selector.
pub const FUTEX_CMP_REQUEUE: u32 = 4;
/// `FUTEX_WAKE_OP = 5`. Wake-op selector.
pub const FUTEX_WAKE_OP: u32 = 5;
/// `FUTEX_LOCK_PI = 6`. Best-effort PI lock selector.
pub const FUTEX_LOCK_PI: u32 = 6;
/// `FUTEX_UNLOCK_PI = 7`. Best-effort PI unlock selector.
pub const FUTEX_UNLOCK_PI: u32 = 7;
/// `FUTEX_TRYLOCK_PI = 8`. Best-effort PI trylock selector.
pub const FUTEX_TRYLOCK_PI: u32 = 8;
/// `FUTEX_WAIT_BITSET = 9`. Wait selector with bitset filtering.
pub const FUTEX_WAIT_BITSET: u32 = 9;
/// `FUTEX_WAKE_BITSET = 10`. Wake selector with bitset filtering.
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
/// wait source (so the parked task wakes on the next BSP reactor tick
/// after `read_ns() >= deadline`); that wiring is deferred — Slice 4's
/// timer-channel infrastructure lands in a follow-up slice. Real
/// non-zero durations currently return `-ENOSYS`. busybox sh barely
/// uses `nanosleep` so the deferral does not block Slice 11's QEMU
/// shell smoke.
pub const NR_NANOSLEEP: u64 = 101;
/// `clock_settime(clk_id, ts)`. Linux RV64 generic ABI
/// `__NR_clock_settime = 112`.
pub const NR_CLOCK_SETTIME: u64 = 112;
/// `clock_gettime(clk_id, ts)`. Linux RV64 generic ABI
/// `__NR_clock_gettime = 113`.
pub const NR_CLOCK_GETTIME: u64 = 113;
/// `clock_getres(clk_id, res)`. Linux RV64 generic ABI
/// `__NR_clock_getres = 114`.
pub const NR_CLOCK_GETRES: u64 = 114;
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
/// `settimeofday(tv, tz)`. Linux RV64 generic ABI
/// `__NR_settimeofday = 170`. The `tz` argument is deprecated on Linux
/// and the arm ignores it.
pub const NR_SETTIMEOFDAY: u64 = 170;

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
/// `SIOCGIFNAME = 0x8910` — resolve `struct ifreq.ifr_ifindex` to ifname.
pub const SIOCGIFNAME: u32 = 0x8910;
/// `SIOCGIFCONF = 0x8912` — enumerate interface `struct ifreq` entries.
pub const SIOCGIFCONF: u32 = 0x8912;
/// `SIOCGIFFLAGS = 0x8913` — read `struct ifreq.ifr_flags`.
pub const SIOCGIFFLAGS: u32 = 0x8913;
/// `SIOCSIFFLAGS = 0x8914` — write `struct ifreq.ifr_flags`.
pub const SIOCSIFFLAGS: u32 = 0x8914;
/// `SIOCGIFMTU = 0x8921` — read `struct ifreq.ifr_mtu`.
pub const SIOCGIFMTU: u32 = 0x8921;
/// `SIOCSIFMTU = 0x8922` — write `struct ifreq.ifr_mtu`.
pub const SIOCSIFMTU: u32 = 0x8922;
/// `SIOCGIFHWADDR = 0x8927` — read `struct ifreq.ifr_hwaddr`.
pub const SIOCGIFHWADDR: u32 = 0x8927;
/// `SIOCGIFINDEX = 0x8933` — resolve `struct ifreq.ifr_name` to ifindex.
pub const SIOCGIFINDEX: u32 = 0x8933;
/// `SIOCGIFTXQLEN = 0x8942` — query `struct ifreq.ifr_qlen`.
pub const SIOCGIFTXQLEN: u32 = 0x8942;
/// `SIOCDARP = 0x8953` — delete an IPv4 ARP cache entry via `struct arpreq`.
pub const SIOCDARP: u32 = 0x8953;
/// `SIOCSARP = 0x8955` — install an IPv4 ARP cache entry via `struct arpreq`.
pub const SIOCSARP: u32 = 0x8955;

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
/// `statfs(path, buf)`. Linux RV64 ABI `__NR_statfs = 43`.
pub const NR_STATFS: u64 = 43;
/// `fstatfs(fd, buf)`. Linux RV64 ABI `__NR_fstatfs = 44`.
pub const NR_FSTATFS: u64 = 44;
/// `sync()`. Linux RV64 ABI `__NR_sync = 81`.
pub const NR_SYNC: u64 = 81;
/// `syncfs(fd)`. Linux RV64 ABI `__NR_syncfs = 267` (same as RV64).
pub const NR_SYNCFS: u64 = 267;
/// `fsync(fd)`. Linux RV64 ABI `__NR_fsync = 82`.
pub const NR_FSYNC: u64 = 82;
/// `fdatasync(fd)`. Linux RV64 ABI `__NR_fdatasync = 83`.
pub const NR_FDATASYNC: u64 = 83;
/// `flock(fd, operation)`. Linux RV64 ABI `__NR_flock = 32`.
pub const NR_FLOCK: u64 = 32;
/// `mount(source, target, fstype, flags, data)`. Linux RV64 ABI `__NR_mount = 40`.
pub const NR_MOUNT: u64 = 40;
/// `umount2(target, flags)`. Linux RV64 ABI `__NR_umount2 = 39`.
pub const NR_UMOUNT2: u64 = 39;
/// `mknodat(dirfd, path, mode, dev)`. Linux RV64 ABI `__NR_mknodat = 33`.
pub const NR_MKNODAT: u64 = 33;
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
/// `statx(dirfd, path, flags, mask, statxbuf)`. Linux generic ABI
/// `__NR_statx = 291`. LA64 musl/busybox prefers this over the older
/// stat-family calls for directory listing metadata probes.
pub const NR_STATX: u64 = 291;
/// `umask(mask)`. Linux RV64 generic ABI `__NR_umask = 166`. Atomic
/// swap of the per-process file-creation mask, returning the
/// previous value. Mask is silently truncated to the bottom 9 bits
/// (`rwxrwxrwx` only — kernel ignores the kind / setuid / setgid /
/// sticky bits per Linux semantics).
pub const NR_UMASK: u64 = 166;
/// `getcpu(cpup, nodep, unused)`. Linux RV64 generic ABI `__NR_getcpu = 168`.
pub const NR_GETCPU: u64 = 168;

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
/// `AT_STATX_FORCE_SYNC = 0x2000`.
pub const AT_STATX_FORCE_SYNC: u32 = 0x2000;
/// `AT_STATX_DONT_SYNC = 0x4000`.
pub const AT_STATX_DONT_SYNC: u32 = 0x4000;
/// Mask of the statx-specific synchronisation bits accepted by Linux.
pub const AT_STATX_SYNC_TYPE: u32 = AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC;

/// `STATX_BASIC_STATS`: the metadata set Txv2 can currently report.
pub const STATX_BASIC_STATS: u32 = 0x0000_07ff;
pub const STATX_BTIME: u32 = 0x0000_0800;
pub const STATX_MNT_ID: u32 = 0x0000_1000;

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
/// Restores the parked pre-handler user context and returns the
/// syscall-layer `SigreturnRestored` control-flow marker. The kernel
/// syscall-return path performs the live trap-frame writeback.
pub const NR_RT_SIGRETURN: u64 = 139;
/// `rt_sigsuspend(mask, sigsetsize)` — Linux RV64 `__NR_rt_sigsuspend = 133`.
/// Phase J: returns `-ENOSYS`; TODO full implementation.
pub const NR_RT_SIGSUSPEND: u64 = 133;
/// `rt_sigpending(set, sigsetsize)` — Linux RV64 `__NR_rt_sigpending = 136`.
pub const NR_RT_SIGPENDING: u64 = 136;
/// `sigaltstack(ss, old_ss)` — Linux RV64 `__NR_sigaltstack`.
/// Phase J: returns `-ENOSYS`; TODO full implementation.
pub const NR_SIGALTSTACK: u64 = 132;
/// `rt_sigqueueinfo(pid, sig, info)` — Linux RV64 `__NR_rt_sigqueueinfo = 138`.
/// Phase J: returns `-ENOSYS`; TODO full implementation.
pub const NR_RT_SIGQUEUEINFO: u64 = 138;
/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` — Linux RV64.
/// Phase J: returns `-ENOSYS`; TODO full implementation.
pub const NR_RT_SIGTIMEDWAIT: u64 = 137;
/// `pidfd_open(pid, flags)` — Linux RV64.
/// Phase J: returns `-ENOSYS`; TODO full implementation.
pub const NR_PIDFD_OPEN: u64 = 434;
/// `pidfd_send_signal(pidfd, sig, info, flags)` — Linux RV64.
/// Phase J: returns `-ENOSYS`; TODO full implementation.
pub const NR_PIDFD_SEND_SIGNAL: u64 = 424;
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
/// `readahead(fd, offset, count)`. Linux RV64 generic ABI.
pub const NR_READAHEAD: u64 = 213;
/// `fadvise64_64(fd, offset, len, advice)`. Linux RV64 generic ABI.
pub const NR_FADVISE64_64: u64 = 223;
/// `copy_file_range(fd_in, off_in, fd_out, off_out, len, flags)`.
/// Linux RV64 generic ABI.
pub const NR_COPY_FILE_RANGE: u64 = 285;
/// `preadv2(fd, iov, iovcnt, offset_lo, offset_hi, flags)`.
/// Linux RV64 generic ABI.
pub const NR_PREADV2: u64 = 286;
/// `pwritev2(fd, iov, iovcnt, offset_lo, offset_hi, flags)`.
/// Linux RV64 generic ABI.
pub const NR_PWRITEV2: u64 = 287;

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
/// `OpenFileFlags::nonblocking`, and OR's `O_DIRECT` from
/// `OpenFileFlags::packet`. `O_CLOEXEC` is **not** included (Linux
/// semantic: cloexec is per-fd, queried via `F_GETFD`, not
/// per-OpenFile).
pub const F_GETFL: i32 = 3;
/// `F_SETFL` cmd: replace the mutable per-OpenFile status flag bits.
/// The current implementation updates `O_NONBLOCK` and pipe
/// `O_DIRECT` packet mode; other status flags are ignored until their
/// owner implements them.
pub const F_SETFL: i32 = 4;
/// `F_DUPFD_CLOEXEC` cmd: like [`F_DUPFD`] but the new fd is marked
/// close-on-exec (the per-fd CLOEXEC bit is set on the result).
pub const F_DUPFD_CLOEXEC: i32 = 1030;
/// `F_SETPIPE_SZ` cmd: resize a pipe's byte capacity, rounded to a
/// page-count power of two.
pub const F_SETPIPE_SZ: i32 = 1031;
/// `F_GETPIPE_SZ` cmd: read a pipe's current byte capacity.
pub const F_GETPIPE_SZ: i32 = 1032;

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

/// `getrlimit(resource, old_rlim)`. Linux RV64 generic ABI.
pub const NR_GETRLIMIT: u64 = 163;
/// `setrlimit(resource, new_rlim)`. Linux RV64 generic ABI.
pub const NR_SETRLIMIT: u64 = 164;
/// `getrusage(who, usage)`. Linux RV64 generic ABI.
pub const NR_GETRUSAGE: u64 = 165;
/// `unshare(flags)`. Linux RV64 generic ABI. Wired for CLONE_NEWUSER /
/// CLONE_NEWNET (network-namespace LTP setup).
pub const NR_UNSHARE: u64 = 97;
pub const NR_SETNS: u64 = 268;

/// `close_range(first, last, flags)`. Linux RV64 generic ABI.
pub const NR_CLOSE_RANGE: u64 = 436;
/// Linux `close_range` flag: unshare fd table before applying range.
pub const CLOSE_RANGE_UNSHARE: u32 = 1 << 1;
/// Linux `close_range` flag: mark fds close-on-exec instead of closing.
pub const CLOSE_RANGE_CLOEXEC: u32 = 1 << 2;

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
/// `fallocate(fd, mode, offset, len)`. Linux RV64 generic ABI.
pub const NR_FALLOCATE: u64 = 47;
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

// ---------------------------------------------------------------------
// PR-10 phase 2 — `userfaultfd(2)` + `UFFDIO_API` ioctl handshake.
//
// `__NR_userfaultfd = 282` per Linux RV64 generic ABI
// (`include/uapi/asm-generic/unistd.h`); x86_64 also uses 282 (in
// arch-specific `unistd_64.h`). The bare-`flags` syscall mints a fresh
// userfaultfd object and returns the fd. Phase 2 only validates flags
// and installs the ufd in the fd table; later phases (P-10.3 / P-10.4
// / P-10.5) wire `UFFDIO_REGISTER` / fault interception / reply
// ioctls.
//
// `UFFDIO_API` is the api-handshake ioctl on a fresh ufd:
//
//   struct uffdio_api { __u64 api; __u64 features; __u64 ioctls; }
//
// The ioctl number is `_IOWR(0xAA, 0x3F, struct uffdio_api)` =
// `0xC020_AA3F`. We pin the magic value here for grep-stability —
// later phases gain `_IOWR` / `_IOR` helpers if other UFFDIO_* numbers
// are wanted.
//
// Spec:
// - `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//   (§6 phase plan)
// - `docs/Txv3/05_DELEGATE_v1.md` §8.1 (userfaultfd worked example)
// - `man 2 userfaultfd`, `man 2 ioctl_userfaultfd`
// ---------------------------------------------------------------------

/// `userfaultfd(flags)`. Linux RV64 generic ABI
/// `__NR_userfaultfd = 282`. Mints a fresh ufd object, wraps it in an
/// `OpenFile` with `OpenFileBacking::Ufd`, installs at the lowest free
/// fd, and returns the fd. Recognised `flags`: `O_CLOEXEC`. Other bits
/// return `-EINVAL`.
pub const NR_USERFAULTFD: u64 = 282;

/// `UFFDIO_API` ioctl request number on Linux generic uapi:
/// `_IOWR('U', 0x3F, struct uffdio_api)` (`'U'` = `0xAA`, size = 24).
/// The handshake validates the api version + features fields and
/// writes back the supported `ioctls` bitmap. PR-10 phase 2 ships a
/// no-op handshake: it accepts `api == UFFD_API`, requires `features
/// == 0`, writes `0` for the supported-features mask, sets the
/// handshake bit on the ufd, and returns `0`. Later phases populate
/// the real supported-features mask.
///
/// Typed as `u32` to compare directly against `sys_ioctl`'s
/// `args[1] as u32` request word. All `UFFDIO_*` numbers fit in 32
/// bits per the `_IOWR(0xAA, _, _)` encoding.
pub const UFFDIO_API: u32 = 0xC020_AA3F;

/// `UFFD_API` magic value the agent passes in the
/// `struct uffdio_api { api: ... }` field. Linux's userfaultfd ships
/// `0xAA` as the only currently-supported API revision; rejecting any
/// other value matches Linux's "api mismatch → -EINVAL" behaviour.
pub const UFFD_API: u64 = 0xAA;

// === PR-10 phase 3 — UFFDIO_REGISTER ioctl ============================

/// `UFFDIO_REGISTER` ioctl request number on Linux generic uapi:
/// `_IOWR('U', 0x00, struct uffdio_register)`. The struct is 32 bytes
/// (16-byte `range`, an 8-byte `mode`, and an 8-byte writeback
/// `ioctls`). PR-10 phase 3 ships a MISSING-only handler — the
/// agent registers a VMA range against the ufd and the kernel
/// returns the bitmap of reply ioctls supported on that range
/// (`UFFDIO_COPY | UFFDIO_ZEROPAGE` for the canary).
///
/// Typed `u32` to match `sys_ioctl`'s `args[1] as u32` shape;
/// all `UFFDIO_*` numbers fit in 32 bits per `_IOWR(0xAA, _, _)`.
pub const UFFDIO_REGISTER: u32 = 0xC020_AA00;

/// `UFFDIO_REGISTER_MODE_MISSING` — the most common Linux mode,
/// "deliver page faults on missing-pages to the ufd's handler."
/// PR-10 phase 3 accepts this mode only.
pub const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;

/// `UFFDIO_REGISTER_MODE_WP` — write-protect mode. Phase 3 rejects
/// this with `-EINVAL`; reserved for a follow-up phase.
pub const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;

/// `UFFDIO_REGISTER_MODE_MINOR` — minor-fault mode (Linux 5.13+).
/// Phase 3 rejects this with `-EINVAL`; reserved for a follow-up
/// phase.
pub const UFFDIO_REGISTER_MODE_MINOR: u64 = 1 << 2;

/// `UFFDIO_COPY` — page-copy reply ioctl (PR-10 phase 5).
///
/// `_IOWR('U', 0x03, struct uffdio_copy)` — the struct is 40 bytes
/// (`__u64 dst, src, len, mode, copy`), so the size field is
/// `0x028` (40 in hex). Phase 5 (this) wires the handler in
/// `userfaultfd::step_uffdio_copy`.
pub const UFFDIO_COPY: u32 = 0xC028_AA03;

/// `UFFDIO_ZEROPAGE` — zero-page reply ioctl (PR-10 phase 5).
///
/// `_IOWR('U', 0x04, struct uffdio_zeropage)` — the struct is 32 bytes
/// (`struct uffdio_range range; __u64 mode; __u64 zeropage`), so the
/// size field is `0x020`. Phase 5 wires `step_uffdio_zeropage`.
pub const UFFDIO_ZEROPAGE: u32 = 0xC020_AA04;

/// `UFFDIO_CONTINUE` — page-cache continue reply ioctl (PR-10 phase 5).
///
/// `_IOWR('U', 0x07, struct uffdio_continue)` — the struct is 32 bytes
/// (`struct uffdio_range range; __u64 mode; __u64 mapped`). Used by
/// ufd-shm to install existing page-cache contents without supplying
/// a fresh source buffer.
pub const UFFDIO_CONTINUE: u32 = 0xC020_AA07;

/// `UFFD_EVENT_PAGEFAULT` — `struct uffd_msg { event }` discriminator
/// returned by `read(uffd_fd, &mut uffd_msg)` for a page-fault
/// message. Linux userfaultfd uapi (`<linux/userfaultfd.h>`):
/// `#define UFFD_EVENT_PAGEFAULT  0x12`. PR-10 phase 5 only emits
/// this event variant; future ufd events (`FORK`, `REMAP`, `REMOVE`,
/// `UNMAP`) gain their own constants when wired.
pub const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

/// Bitmap returned in `struct uffdio_register { ioctls }` on a
/// successful registration: the reply ioctls the agent may now use
/// against the registered range. Per Linux's userfaultfd uapi,
/// each bit is `_IOC_NR(UFFDIO_*)` — bit 0x03 for `UFFDIO_COPY`,
/// bit 0x04 for `UFFDIO_ZEROPAGE`, and bit 0x07 for
/// `UFFDIO_CONTINUE`. Phase 5 grows the bitmap to include
/// `UFFDIO_CONTINUE` now that the handler is wired.
pub const UFFDIO_REGISTER_REPLY_IOCTLS: u64 = (1u64 << 0x03) | (1u64 << 0x04) | (1u64 << 0x07);

// =====================================================================
// txKernel private debug syscalls
//
// These are intentionally outside the Linux generic ABI surface. The
// current RV64/generic constants wired in this file jump from 291
// (`statx`) to 424 (`pidfd_send_signal`), leaving the local 3xx range
// available for in-tree diagnostic hooks.
// =====================================================================

/// `tx_observe_begin(threshold)` — private txKernel debug hook.
///
/// Enables `tx-observe`, clears the current hart's ring, and arms a bounded
/// console dump after `threshold` subsequently emitted records. This is used
/// by benchmark wrappers to start a trace inside a specific phase instead of
/// relying on a boot- or suite-level threshold.
pub const NR_TX_OBSERVE_BEGIN: u64 = 333;

/// `tx_observe_trace_on()` — private txKernel debug hook.
///
/// Enables `tx-observe`, clears the current hart's ring, and leaves threshold
/// dumping disabled. Pair with [`NR_TX_OBSERVE_TRACE_OFF`] to bracket a guest
/// benchmark region and dump the ring after the region completes.
pub const NR_TX_OBSERVE_TRACE_ON: u64 = 334;

/// `tx_observe_trace_off()` — private txKernel debug hook.
///
/// Disables subsequent `tx-observe` emission and requests a one-shot console
/// dump at the next kernel boundary.
pub const NR_TX_OBSERVE_TRACE_OFF: u64 = 335;

// =====================================================================
// PR-11 phase 1 — AIO syscall numbers
//
// Spec:
// - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution scope)
// - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §4.1 / §7
// - `man 2 io_setup`, `man 2 io_destroy`, `man 2 io_submit`,
//   `man 2 io_getevents`
//
// **Linux divergence (per D8 §4.1):** our `sys_io_setup` returns a real
// `fd` (via `OpenFileBacking::AioContext`) rather than Linux's opaque
// pointer-shaped `aio_context_t`. The user-visible numeric value of
// the syscall return is therefore a fd, not a ring-buffer address.
// Userspace glibc shims bridge the fd back into the legacy
// `aio_context_t *` out-parameter shape with a 5-line conversion.
//
// **Linux generic/RV64 numbering reference:** `io_setup = 0`,
// `io_destroy = 1`, `io_submit = 2`, `io_cancel = 3`,
// `io_getevents = 4`. Do not use x86_64's 206..209 range here; that
// collides with generic socket syscalls such as `sendto`.
// =====================================================================

/// `io_setup(nr_events, ctx_idp)`. Linux RV64 generic ABI
/// `__NR_io_setup = 0`. Mints a fresh [`AioContext`] cap (W-Z PR-11
/// phase 1 zone), wraps it in an `OpenFile` with
/// `OpenFileBacking::AioContext`, installs at the lowest free fd, and
/// returns the fd (diverging from Linux which writes a pointer-shape
/// into `*ctx_idp`; see module-doc note above).
///
/// [`AioContext`]: tx_subsystems::aio::AioContext
pub const NR_IO_SETUP: u64 = 0;

/// `io_destroy(ctx)`. Linux RV64 generic ABI `__NR_io_destroy = 1`.
/// Phase 1 defines the constant for forward-reference; the dispatch
/// arm lands in phase 4 (close + worker abandonment via the borrow's
/// `exit_source`).
pub const NR_IO_DESTROY: u64 = 1;

/// `io_getevents(ctx, min, max, events, timeout)`. Linux RV64 generic
/// ABI `__NR_io_getevents = 4`. Phase 1 defines the constant for
/// forward-reference; the dispatch arm lands in phase 3.
pub const NR_IO_GETEVENTS: u64 = 4;

/// `io_submit(ctx, nr, iocbpp)`. Linux RV64 generic ABI
/// `__NR_io_submit = 2`. Phase 1 defines the constant for
/// forward-reference; the dispatch arm lands in phase 2 (alongside
/// the worker task spawn + `with_on_behalf_of` integration).
pub const NR_IO_SUBMIT: u64 = 2;

// =====================================================================
// Future PR-12 phase 0 — io_uring SQPOLL syscall numbers (second
// `OnBehalfOf<P>` canary)
//
// Spec:
// - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §8.1 (SQPOLL design)
// - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §13
//   (future canary discussion)
// - `man 2 io_uring_setup`, `man 2 io_uring_enter`
//
// **Linux numbering (generic uapi / x86_64 share the value for these
// post-RV64 syscalls):** `io_uring_setup = 425`, `io_uring_enter = 426`,
// `io_uring_register = 427`. The current scaffold wires setup plus a
// nonblocking enter path over Tx's in-kernel SQ/CQ queues; the real
// user-mmapped ring parser remains deferred.
// =====================================================================

/// `io_uring_setup(entries, params)`. Linux generic uapi
/// `__NR_io_uring_setup = 425`. Mints a fresh
/// [`tx_subsystems::io_uring::IoUring`] cap (W-LL phase 0 zone), wraps
/// it in an `OpenFile` with `OpenFileBacking::IoUring`, spawns the
/// SQPOLL kthread via `with_on_behalf_of`, installs at the lowest
/// free fd, and returns the fd. Phase 0 ignores `*params` per the
/// scaffold scope.
pub const NR_IO_URING_SETUP: u64 = 425;

/// `io_uring_enter(fd, to_submit, min_complete, flags, sig, sigsz)`.
/// Linux generic uapi `__NR_io_uring_enter = 426`. The scaffold arm
/// resolves an io_uring fd, drains Tx's in-kernel SQ queue into CQEs,
/// and returns the submitted count. Waiting and user-mmapped ring
/// parsing remain future extensions.
pub const NR_IO_URING_ENTER: u64 = 426;

// =====================================================================
// D9-D — signalfd syscall numbers
//
// Spec:
// - `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
//   §6 (signalfd as Option C follow-up)
// - `man 2 signalfd`, `man 2 signalfd4`
//
// Linux RV64 generic ABI exposes `signalfd4(2)` at 74. The historical
// x86_64-only `signalfd(2)` number 282 is not an RV64 signalfd surface
// here; 282 belongs to userfaultfd below.
// =====================================================================

/// `signalfd4(fd, &mask, sizemask, flags)`. Linux RV64 generic ABI
/// `__NR_signalfd4 = 74`. Mints a fresh signalfd cap (D9-D zone) if
/// `fd == -1`, or updates an existing signalfd's mask if `fd` names
/// one. Returns the fd. Recognised `flags`: `SFD_CLOEXEC`,
/// `SFD_NONBLOCK`; other bits return `-EINVAL`.
pub const NR_SIGNALFD4: u64 = 74;

/// `SFD_CLOEXEC` — set close-on-exec on the resulting fd. Same bit
/// value as `O_CLOEXEC` per Linux's signalfd4 flag convention.
pub const SFD_CLOEXEC: u32 = O_CLOEXEC;

/// `SFD_NONBLOCK` — set non-blocking mode on the resulting fd. Same
/// bit value as `O_NONBLOCK` per Linux's signalfd4 flag convention.
pub const SFD_NONBLOCK: u32 = O_NONBLOCK;

// =====================================================================
// Phase B.1 — epoll syscall numbers
//
// Spec: `docs/Txv3/03_STEP_MODEL_v2.md` §5 `YieldShape::OnEdge`.
// =====================================================================

/// `epoll_create1(flags)`. Linux generic uapi `__NR_epoll_create1 = 20`
/// on RV64 and LoongArch64. The historical x86_64 number is 291, but
/// generic targets reserve 291 for `statx`.
/// Allocates a fresh [`tx_subsystems::epoll::Epoll`] cap, wraps it in
/// an `OpenFile` with `OpenFileBacking::Epoll`, and installs it at the
/// lowest free fd.
pub const NR_EPOLL_CREATE1: u64 = 20;

/// `epoll_ctl(epfd, op, fd, event_ptr)`. Linux generic uapi
/// `__NR_epoll_ctl = 21`. ADD, MOD, or DEL a monitored fd.
pub const NR_EPOLL_CTL: u64 = 21;

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask)`.
/// Linux generic uapi `__NR_epoll_pwait = 22`. Block until ready
/// events arrive, atomically updating the signal mask. Phase B.1c
/// stubs the sigmask; real signal-mask manipulation is deferred to
/// a future signal-subsystem PR.
pub const NR_EPOLL_PWAIT: u64 = 22;

/// `epoll_pwait2(epfd, events, maxevents, timeout, sigmask, sigsetsize)`.
/// Linux RV64 generic ABI `__NR_epoll_pwait2 = 441`. Same as
/// `epoll_pwait`, but the timeout is a `struct __kernel_timespec *`
/// with nanosecond precision.
pub const NR_EPOLL_PWAIT2: u64 = 441;

// =====================================================================
// eventfd / timerfd syscall numbers
//
// `man 2 eventfd2`, `man 2 timerfd_create`.
// =====================================================================

// =====================================================================
// SysV IPC syscall numbers
//
// `man 2 shmget`, `man 2 msgget`, `man 2 semget`.
// =====================================================================

/// `shmget(key, size, shmflg)`. Linux generic uapi `__NR_shmget = 194`.
pub const NR_SHMGET: u64 = 194;
/// `shmat(shmid, shmaddr, shmflg)`. Linux generic uapi `__NR_shmat = 196`.
pub const NR_SHMAT: u64 = 196;
/// `shmdt(shmaddr)`. Linux generic uapi `__NR_shmdt = 197`.
pub const NR_SHMDT: u64 = 197;
/// `shmctl(shmid, cmd, buf)`. Linux generic uapi `__NR_shmctl = 195`.
pub const NR_SHMCTL: u64 = 195;

/// `msgget(key, msgflg)`. Linux generic uapi `__NR_msgget = 186`.
pub const NR_MSGGET: u64 = 186;
/// `msgsnd(msqid, msgp, msgsz, msgflg)`. Linux generic uapi `__NR_msgsnd = 189`.
pub const NR_MSGSND: u64 = 189;
/// `msgrcv(msqid, msgp, msgsz, msgtyp, msgflg)`. Linux generic uapi `__NR_msgrcv = 188`.
pub const NR_MSGRCV: u64 = 188;
/// `msgctl(msqid, cmd, buf)`. Linux generic uapi `__NR_msgctl = 187`.
pub const NR_MSGCTL: u64 = 187;

/// `semget(key, nsems, semflg)`. Linux generic uapi `__NR_semget = 190`.
pub const NR_SEMGET: u64 = 190;
/// `semop(semid, sops, nsops)`. Linux generic uapi `__NR_semop = 193`.
pub const NR_SEMOP: u64 = 193;
/// `semtimedop(semid, sops, nsops, timeout)`. Linux generic uapi `__NR_semtimedop = 192`.
pub const NR_SEMTIMEDOP: u64 = 192;
/// `semctl(semid, semnum, cmd, arg)`. Linux generic uapi `__NR_semctl = 191`.
pub const NR_SEMCTL: u64 = 191;

// =====================================================================
// POSIX message queue syscall numbers
//
// `man 7 mq_overview`.
// =====================================================================

/// `mq_open(name, oflag, mode, attr)`. Linux generic uapi `__NR_mq_open = 180`.
pub const NR_MQ_OPEN: u64 = 180;
/// `mq_unlink(name)`. Linux generic uapi `__NR_mq_unlink = 181`.
pub const NR_MQ_UNLINK: u64 = 181;
/// `mq_timedsend(mqdes, msg_ptr, msg_len, msg_prio, abs_timeout)`.
/// Linux generic uapi `__NR_mq_timedsend = 182`.
pub const NR_MQ_TIMEDSEND: u64 = 182;
/// `mq_timedreceive(mqdes, msg_ptr, msg_len, msg_prio, abs_timeout)`.
/// Linux generic uapi `__NR_mq_timedreceive = 183`.
pub const NR_MQ_TIMEDRECEIVE: u64 = 183;
/// `mq_notify(mqdes, sevp)`. Linux generic uapi `__NR_mq_notify = 184`.
pub const NR_MQ_NOTIFY: u64 = 184;
/// `mq_getsetattr(mqdes, newattr, oldattr)`. Linux generic uapi
/// `__NR_mq_getsetattr = 185`.
pub const NR_MQ_GETSETATTR: u64 = 185;

// =====================================================================
// eventfd / timerfd syscall numbers
// =====================================================================

/// `eventfd2(init_val, flags)`. Linux generic uapi `__NR_eventfd2 = 19`.
/// Mints a fresh [`tx_subsystems::eventfd::EventFd`] cap, wraps it in
/// an `OpenFile` with `OpenFileBacking::Eventfd`, and installs it at
/// the lowest free fd.
pub const NR_EVENTFD2: u64 = 19;

/// Recognised `eventfd2` flags. EFD_SEMAPHORE is read by the eventfd
/// subsystem; EFD_CLOEXEC / EFD_NONBLOCK are translated to OpenFileFlags.
pub const EFD_SEMAPHORE_FLAG: u32 = 0x1;
pub const EFD_CLOEXEC_FLAG: u32 = O_CLOEXEC;
pub const EFD_NONBLOCK_FLAG: u32 = O_NONBLOCK;

/// `timerfd_create(clockid, flags)`. Linux generic uapi
/// `__NR_timerfd_create = 85`. Mints a fresh
/// [`tx_subsystems::timerfd::TimerFd`] cap.
pub const NR_TIMERFD_CREATE: u64 = 85;

/// `timerfd_settime(fd, flags, new_value, old_value)`. Linux generic
/// uapi `__NR_timerfd_settime = 86`. Arms/disarms the timer.
pub const NR_TIMERFD_SETTIME: u64 = 86;

/// `timerfd_gettime(fd, curr_value)`. Linux generic uapi
/// `__NR_timerfd_gettime = 87`. Returns the current timer state.
pub const NR_TIMERFD_GETTIME: u64 = 87;

/// Recognised `timerfd_create` flags. TFD_CLOEXEC / TFD_NONBLOCK are
/// translated to OpenFileFlags.
pub const TFD_CLOEXEC_FLAG: u32 = O_CLOEXEC;
pub const TFD_NONBLOCK_FLAG: u32 = O_NONBLOCK;

/// `TFD_TIMER_ABSTIME` — interpret `it_value` as an absolute time.
pub const TFD_TIMER_ABSTIME_FLAG: u32 = 1;
/// `TFD_TIMER_CANCEL_ON_SET` — recognised for musl/Linux header
/// compatibility. txKernel has no wall-clock discontinuity event yet,
/// so the bit is accepted and otherwise ignored.
pub const TFD_TIMER_CANCEL_ON_SET_FLAG: u32 = 1 << 1;

/// `syslog(type, bufp, len)` — Linux kernel ring-buffer read / control.
/// Linux generic uapi `__NR_syslog = 116`.  Called by `dmesg(1)`.
pub const NR_SYSLOG: u64 = 116;

// =====================================================================
// membarrier syscall numbers and command flags
//
// `man 2 membarrier`. The Linux RV64 generic uapi assigns membarrier
// dedicated slot 283.
// =====================================================================

/// `membarrier(cmd, flags, cpu_id)`. RISC-V generic uapi `__NR_membarrier = 283`.
pub const NR_MEMBARRIER: u64 = 283;

/// Query supported commands. Always returns `MEMBARRIER_SUPPORTED_MASK`.
pub const MEMBARRIER_CMD_QUERY: u64 = 0;

/// Global barrier — broadcast to all online CPUs.
pub const MEMBARRIER_CMD_GLOBAL: u64 = 1 << 0;
/// Global expedited barrier.
pub const MEMBARRIER_CMD_GLOBAL_EXPEDITED: u64 = 1 << 1;
/// Register for global expedited barriers.
pub const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: u64 = 1 << 2;

/// Private expedited barrier (single-process).
pub const MEMBARRIER_CMD_PRIVATE_EXPEDITED: u64 = 1 << 3;
/// Register for private expedited barriers.
pub const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: u64 = 1 << 4;
/// Private expedited + sync_core (instruction-fetch barrier).
pub const MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE: u64 = 1 << 5;
/// Register for private expedited sync_core barriers.
pub const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: u64 = 1 << 6;

/// Bitmask of all commands this kernel supports — returned by
/// `MEMBARRIER_CMD_QUERY`.
///
/// Includes `CMD_QUERY` plus every GLOBAL/PRIVATE barrier command.
/// Registration cmds are accepted (no-op) but not advertised.
pub const MEMBARRIER_SUPPORTED_MASK: u64 = MEMBARRIER_CMD_QUERY
    | MEMBARRIER_CMD_GLOBAL
    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE;

// ---------------------------------------------------------------------------
// Socket / network constants
//
// Address families, protocols, socket-option levels, IPv6 ancillary option
// names, multicast group ops, and AF_PACKET ring versions used by the socket
// syscall layer (`linux_syscall/socket.rs` + `socket/helpers.rs`). Values are
// the Linux generic-ABI numbers. Re-homed after PR#50 stripped the net
// syscall surface; SIOCGIF* live with the ioctl block above.
// ---------------------------------------------------------------------------

/// `AF_UNIX` / `AF_LOCAL` — local interprocess sockets.
pub const AF_UNIX: u16 = 1;
/// `AF_INET` — IPv4 Internet protocols.
pub const AF_INET: u16 = 2;
/// `AF_INET6` — IPv6 Internet protocols.
pub const AF_INET6: u16 = 10;
/// `AF_NETLINK` — kernel/user-space netlink sockets.
pub const AF_NETLINK: u16 = 16;
/// `AF_PACKET` — low-level packet interface.
pub const AF_PACKET: u16 = 17;

/// `IPPROTO_IP` — dummy protocol / IPv4-level socket options.
pub const IPPROTO_IP: i32 = 0;
/// `IPPROTO_TCP` — Transmission Control Protocol.
pub const IPPROTO_TCP: i32 = 6;
/// `IPPROTO_ICMPV6` — ICMPv6.
pub const IPPROTO_ICMPV6: i32 = 58;

/// `SOL_SOCKET` — socket-level options.
pub const SOL_SOCKET: i32 = 1;
/// `SOL_SCTP` — SCTP-level options.
pub const SOL_SCTP: i32 = 132;
/// `SOL_IPV6` — IPv6-level options.
pub const SOL_IPV6: i32 = 41;
/// `SOL_RAW` — raw-socket-level options.
pub const SOL_RAW: i32 = 255;
/// `SOL_PACKET` — AF_PACKET-level options.
pub const SOL_PACKET: i32 = 263;
/// `SOL_NETLINK` — netlink-level options.
pub const SOL_NETLINK: i32 = 270;

/// `IPV6_2292PKTINFO` — RFC 2292 packet info.
pub const IPV6_2292PKTINFO: i32 = 2;
/// `IPV6_2292HOPOPTS` — RFC 2292 hop-by-hop options.
pub const IPV6_2292HOPOPTS: i32 = 3;
/// `IPV6_2292DSTOPTS` — RFC 2292 destination options.
pub const IPV6_2292DSTOPTS: i32 = 4;
/// `IPV6_2292RTHDR` — RFC 2292 routing header.
pub const IPV6_2292RTHDR: i32 = 5;
/// `IPV6_2292HOPLIMIT` — RFC 2292 hop limit.
pub const IPV6_2292HOPLIMIT: i32 = 8;
/// `IPV6_PKTINFO` — sticky/ancillary packet info.
pub const IPV6_PKTINFO: i32 = 50;
/// `IPV6_RECVPKTINFO` — receive packet info.
pub const IPV6_RECVPKTINFO: i32 = 49;
/// `IPV6_RECVHOPLIMIT` — receive hop limit.
pub const IPV6_RECVHOPLIMIT: i32 = 51;
/// `IPV6_HOPLIMIT` — ancillary hop limit.
pub const IPV6_HOPLIMIT: i32 = 52;
/// `IPV6_RECVHOPOPTS` — receive hop-by-hop options.
pub const IPV6_RECVHOPOPTS: i32 = 53;
/// `IPV6_RECVRTHDR` — receive routing header.
pub const IPV6_RECVRTHDR: i32 = 56;
/// `IPV6_RECVDSTOPTS` — receive destination options.
pub const IPV6_RECVDSTOPTS: i32 = 58;
/// `IPV6_RECVTCLASS` — receive traffic class.
pub const IPV6_RECVTCLASS: i32 = 66;
/// `IPV6_TCLASS` — ancillary/sticky traffic class.
pub const IPV6_TCLASS: i32 = 67;

/// `MCAST_JOIN_GROUP` — protocol-independent multicast join.
pub const MCAST_JOIN_GROUP: i32 = 42;
/// `MCAST_LEAVE_GROUP` — protocol-independent multicast leave.
pub const MCAST_LEAVE_GROUP: i32 = 45;

/// `TPACKET_V1` — AF_PACKET v1 ring format.
pub const TPACKET_V1: i32 = 0;
/// `TPACKET_V3` — AF_PACKET v3 ring format.
pub const TPACKET_V3: i32 = 2;

// Socket-option *names* (the `optname` arg to set/getsockopt), grouped by level.
// CRITICAL: these MUST be defined — the set/getsockopt dispatch matches on
// `(level, OPTNAME)`; an undefined OPTNAME silently becomes an irrefutable
// binding pattern, so the first arm in a level group swallows every option
// (76 unreachable arms). Re-homed after PR#50 stripped them. Values are the
// Linux generic-ABI numbers.

// SOL_SOCKET options.
pub const SO_REUSEADDR: i32 = 2;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_DONTROUTE: i32 = 5;
pub const SO_BROADCAST: i32 = 6;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_OOBINLINE: i32 = 10;
pub const SO_NO_CHECK: i32 = 11;
pub const SO_LINGER: i32 = 13;
pub const SO_REUSEPORT: i32 = 15;
pub const SO_PEERCRED: i32 = 17;
pub const SO_RCVTIMEO: i32 = 20;
pub const SO_SNDTIMEO: i32 = 21;
pub const SO_BINDTODEVICE: i32 = 25;
pub const SO_SNDBUFFORCE: i32 = 32;

// IPPROTO_IP options.
pub const IP_TTL: i32 = 2;
pub const IP_HDRINCL: i32 = 3;
pub const IP_RECVERR: i32 = 11;
pub const IP_MULTICAST_IF: i32 = 32;
pub const IP_MULTICAST_TTL: i32 = 33;
pub const IP_MULTICAST_LOOP: i32 = 34;
pub const ICMP6_FILTER: i32 = 1;

// IPPROTO_TCP options.
pub const TCP_NODELAY: i32 = 1;
pub const TCP_MAXSEG: i32 = 2;
pub const TCP_INFO: i32 = 11;
pub const TCP_CONGESTION: i32 = 13;
pub const TCP_ULP: i32 = 31;
pub const TLS_TX: i32 = 1;

// IPPROTO_SCTP options.
pub const SCTP_RTOINFO: i32 = 0;
pub const SCTP_ASSOCINFO: i32 = 1;
pub const SCTP_INITMSG: i32 = 2;
pub const SCTP_AUTOCLOSE: i32 = 4;
pub const SCTP_PRIMARY_ADDR: i32 = 6;
pub const SCTP_DISABLE_FRAGMENTS: i32 = 8;
pub const SCTP_PEER_ADDR_PARAMS: i32 = 9;
pub const SCTP_DEFAULT_SEND_PARAM: i32 = 10;
pub const SCTP_EVENTS: i32 = 11;
pub const SCTP_MAXSEG: i32 = 13;
pub const SCTP_STATUS: i32 = 14;
pub const SCTP_DELAYED_ACK_TIME: i32 = 16;
pub const SCTP_SOCKOPT_PEELOFF: i32 = 102;
pub const SCTP_GET_PEER_ADDRS: i32 = 108;
pub const SCTP_GET_LOCAL_ADDRS: i32 = 109;

// AF_PACKET (SOL_PACKET) options.
pub const PACKET_RX_RING: i32 = 5;
pub const PACKET_VERSION: i32 = 10;
pub const PACKET_RESERVE: i32 = 12;
pub const PACKET_VNET_HDR: i32 = 15;

// SOL_NETLINK options.
pub const NETLINK_EXT_ACK: i32 = 11;

// iptables (SOL_IP get/set) options.
pub const IPT_SO_SET_REPLACE: i32 = 64;
pub const IPT_SO_SET_ADD_COUNTERS: i32 = 65;
pub const IPT_SO_GET_INFO: i32 = 64;
pub const IPT_SO_GET_ENTRIES: i32 = 65;

/// `IPPROTO_UDP` — User Datagram Protocol.
pub const IPPROTO_UDP: i32 = 17;
/// `SOL_TLS` — kernel TLS socket-option level.
pub const SOL_TLS: i32 = 282;

// Message-based socket syscall numbers (RV64 generic ABI). Re-homed: their
// handlers (sys_sendmsg/recvmsg/sendmmsg/recvmmsg) exist but were undispatched,
// so sendmsg(2) fell through to ENOSYS — breaking every SCTP/UDP/TCP test that
// sends ancillary-data messages.
pub const NR_SENDMSG: u64 = 211;
pub const NR_RECVMSG: u64 = 212;
pub const NR_RECVMMSG: u64 = 243;
pub const NR_SENDMMSG: u64 = 269;

// Interval timers — setitimer arms the ITIMER_REAL deadline that bounds blocking
// recv (LTP alarm-bounded recv tests hung without it).
pub const NR_GETITIMER: u64 = 102;
pub const NR_SETITIMER: u64 = 103;
// More IPPROTO_IPV6 (SOL_IPV6) options.
pub const IPV6_ADDRFORM: i32 = 1;
pub const IPV6_CHECKSUM: i32 = 7;
pub const IPV6_UNICAST_HOPS: i32 = 16;
pub const IPV6_V6ONLY: i32 = 26;
