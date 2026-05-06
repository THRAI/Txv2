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
