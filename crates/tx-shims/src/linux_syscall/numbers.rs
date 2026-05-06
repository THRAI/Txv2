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
