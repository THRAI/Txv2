//! Linux syscall dispatch table — Phase 2a + 2b slice.
//!
//! Phase 2a deliverable per the Trio plan
//! (`docs/progress/plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md`
//! §"Phasing" item 2): `NR_WRITE`, `NR_EXIT`, `NR_EXIT_GROUP`,
//! `NR_GETPID`. Phase 2b (§"Phasing" item 4) extends with `NR_READ`,
//! `NR_BRK`, `NR_RT_SIGPROCMASK`, `NR_RT_SIGACTION`. Everything else
//! still returns `-ENOSYS`.
//!
//! ## Plan B writeback discipline
//!
//! Per the plan's "Cross-cutting risks #1" the dispatcher only writes
//! the syscall return into `ThreadPayload.pending_syscall_return`; the
//! userspace-entry shim drains the slot and writes it into the *fresh*
//! trap frame before `enter_userspace`. The `dispatch` function itself
//! returns a `SyscallResult` so the trap-shell-side wrapper (Phase 6
//! territory) can decide between writing to the payload slot
//! (`Return`/`Error`) and never re-entering userspace (`NoReturn`).
//!
//! ## Cap vs IdentRef
//!
//! Per `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` and the plan's
//! "Cross-cutting risks #2", every step that needs an epoch guard
//! takes a fresh `step_engine::guard()` *inside* the call
//! site. Guards never cross `.await`; `Cap<T>` does (it's
//! epoch-managed). This mirrors `vm::execution::fault_script`.
//!
//! ## Doc anchors
//!
//! - `txdoc:PROCESS-WHAT-THIS-DOCUMENT-PINS-1`,
//!   `txdoc:PROCESS-RELATIONSHIP-OTHER-SUBSYSTEMS-1`,
//!   `txdoc:PROCESS-STEP-THREAD-EXIT-1` (`PROCESS_v1` §7.3.1, §8.4)
//!   — exit ordering and the `step_thread_exit` → `step_process_exit`
//!   chain.
//! - `txdoc:THREAD-5-1-STATE-PLACEMENT` (`THREAD_RUNTIME_v1`) —
//!   per-thread state placement that this dispatcher consumes.
//! - `txdoc:VFS-CHECKS-WALKER-MODES-1` (`VFS_CHECKS_V2.1`) —
//!   `OpenFile::step_write` semantics.

#![allow(clippy::module_inception)]

extern crate alloc;

use crate::adapter::reactor_entry;
use alloc::sync::Arc;
use alloc::vec::Vec;

use reactor_entry::userspace::SyscallRequest;
use tx_hal::{EntropyIf, PmapIf, TimeIf, UserPtr};
use tx_scripts::process::exec::{exec_script, ExecError};
use tx_subsystems::cred::{
    step_setgid, step_setregid, step_setresgid, step_setresuid, step_setreuid, step_setuid,
    Capability, Cred, CredChange, Gid, SetgidOp, SetregidOp, SetresgidOp, SetresuidOp,
    SetreuidOp, SetuidOp, Uid,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::process::{
    process_by_pid, seed_child_leader_context, step_chdir, step_getcwd,
    step_setpgid, step_setsid, step_waitpid_nohang, ChdirOutcome, ExitGroupOp, ExitStatus,
    Pgid, Pid, ProcessIdentity, SetpgidError, SetsidError, WaitError, WaitTarget,
};
use tx_subsystems::reactor_submit;
use tx_subsystems::signal::{
    step_kill_process, step_sigaction, KillOutcome, SigDisposition, SigDispositionChange,
    SignalMask, Signum,
};
use tx_subsystems::thread_runtime::execution::{step_sigprocmask, SigmaskHow, SigprocmaskChange};
use tx_subsystems::thread_runtime::{step_thread_exit, ThreadIdentity};
use tx_subsystems::tty::execution::{
    step_ioctl_tcgets, step_ioctl_tcsets, step_ioctl_tiocgpgrp, step_ioctl_tiocgwinsz,
    step_ioctl_tiocnotty, step_ioctl_tiocsctty_for_process, step_ioctl_tiocspgrp,
    step_ioctl_tiocswinsz, IoctlCaller,
};
use tx_subsystems::tty::structure::{Termios, Winsize};
use tx_subsystems::vfs::structure::{
    Credential, InodeKind, InodeMeta, OpenFileFlags, RNodeBacking, StructPayload,
};
use tx_subsystems::vfs::{step_open, step_walk, DEntry, OpenFile};
use tx_subsystems::vm::{
    AddressSpace, MadviseAdvice, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking,
    VmEntryFlags, VmMapError, VmMapRequest, VmRemapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::wait_source;

pub mod numbers;

mod cred;
use cred::*;
mod time;
use time::*;
mod signal;
use signal::*;
mod vm;
use vm::*;
pub mod io;
use io::*;
pub mod fs_basic;
use fs_basic::*;
mod fs_path;
use fs_path::*;
mod fs_mut;
use fs_mut::*;
pub mod proc;
use proc::*;
mod misc;
use misc::*;
mod userfaultfd;
use userfaultfd::*;
pub mod aio;
use aio::*;
pub mod io_uring;
use io_uring::*;
mod signalfd;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
use signalfd::*;

mod ctx;
pub use ctx::*;
mod result;
pub use result::*;
mod user_copy;
pub(super) use user_copy::*;
mod helpers;
pub(super) use helpers::*;

#[cfg(test)]
mod tests;

pub use numbers::{
    AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW,
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW,
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE, CLOCK_THREAD_CPUTIME_ID,
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, FD_CLOEXEC,
    FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_LOCK_PI, FUTEX_PRIVATE_FLAG,
    FUTEX_REQUEUE, FUTEX_TRYLOCK_PI, FUTEX_UNLOCK_PI, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE,
    FUTEX_WAKE_BITSET, FUTEX_WAKE_OP, F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_OK, F_SETFD,
    F_SETFL, GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, MADV_DONTNEED, MADV_FREE, MADV_NORMAL,
    MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED, MAP_ANONYMOUS, MAP_DENYWRITE, MAP_EXECUTABLE,
    MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_GROWSDOWN, MAP_HUGETLB, MAP_LOCKED, MAP_NONBLOCK,
    MAP_NORESERVE, MAP_POPULATE, MAP_PRIVATE, MAP_SHARED, MAP_STACK, MAP_SYNC, NR_BRK, NR_CHDIR,
    NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_CLONE, NR_CLOSE, NR_DUP, NR_DUP3, NR_EXECVE, NR_EXIT,
    NR_EXIT_GROUP, NR_FACCESSAT, NR_FACCESSAT2, NR_FCHDIR, NR_FCHMODAT, NR_FCHOWNAT, NR_FCNTL,
    NR_FSTAT, NR_FTRUNCATE, NR_FUTEX, NR_GETCWD, NR_GETDENTS64, NR_GETEGID, NR_GETEUID, NR_GETGID,
    NR_GETPGID, NR_GETPGRP, NR_GETPID, NR_GETPPID, NR_GETRANDOM, NR_GETRESGID, NR_GETRESUID,
    NR_GETSID, NR_GETTIMEOFDAY, NR_GETUID, NR_IOCTL, NR_IO_DESTROY, NR_IO_GETEVENTS, NR_IO_SETUP,
    NR_IO_SUBMIT, NR_IO_URING_ENTER, NR_IO_URING_SETUP, NR_KILL, NR_LINKAT, NR_LSEEK, NR_MADVISE,
    NR_MKDIRAT, NR_MLOCK, NR_MMAP, NR_MPROTECT, NR_MREMAP, NR_MSYNC, NR_MUNLOCK, NR_MUNMAP, NR_NANOSLEEP, NR_NEWFSTATAT,
    NR_OPENAT, NR_PIPE2, NR_PPOLL, NR_PRLIMIT64, NR_READ, NR_READLINKAT, NR_READV, NR_RENAMEAT2,
    NR_RT_SIGACTION, NR_RT_SIGPENDING, NR_RT_SIGPROCMASK, NR_RT_SIGQUEUEINFO,
    NR_RT_SIGRETURN, NR_RT_SIGSUSPEND, NR_RT_SIGTIMEDWAIT, NR_SIGALTSTACK, NR_PIDFD_OPEN,
    NR_PIDFD_SEND_SIGNAL, NR_SETGID, NR_SETPGID, NR_SETREGID,
    NR_SETRESGID, NR_SETRESUID, NR_SETREUID, NR_SETSID, NR_SETUID, NR_SET_ROBUST_LIST,
    NR_SET_TID_ADDRESS, NR_SIGNALFD, NR_SIGNALFD4, NR_STATX, NR_SYMLINKAT, NR_TGKILL, NR_TIMES,
    NR_TKILL, NR_TRUNCATE, NR_UMASK, NR_UNAME, NR_UNLINKAT, NR_USERFAULTFD, NR_UTIMENSAT, NR_WAIT4,
    NR_WRITE, NR_WRITEV, O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECT, O_EXCL, O_NONBLOCK,
    O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, PROT_EXEC, PROT_GROWSDOWN, PROT_GROWSUP, PROT_NONE,
    PROT_READ, PROT_WRITE, RENAME_EXCHANGE, RENAME_NOREPLACE, RENAME_WHITEOUT, RLIMIT_AS,
    RLIMIT_CORE, RLIMIT_CPU, RLIMIT_DATA, RLIMIT_FSIZE, RLIMIT_LOCKS, RLIMIT_MEMLOCK,
    RLIMIT_MSGQUEUE, RLIMIT_NICE, RLIMIT_NOFILE, RLIMIT_NPROC, RLIMIT_RSS, RLIMIT_RTPRIO,
    RLIMIT_RTTIME, RLIMIT_SIGPENDING, RLIMIT_STACK, RLIM_INFINITY, R_OK, SEEK_CUR, SEEK_END,
    SEEK_SET, SIGCHLD, TCGETS, TCSETS, TCSETSF, TCSETSW, TIMER_ABSTIME, TIMES_NS_PER_TICK,
    TIOCGPGRP, TIOCGWINSZ, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP, TIOCSWINSZ, UTIME_NOW, UTIME_OMIT,
    WNOHANG, W_OK, X_OK,
};

/// Maximum number of input bytes the Phase 2a `write` syscall accepts
/// in a single call. The dispatcher copies `[buf_ptr, buf_ptr+len)` into
/// a kernel-side stack-bounded slice (via `from_raw_parts`); higher-level
/// `copy_from_user` machinery is deferred per the trio plan §"Out of
/// scope". 4 KiB matches a single page; values above that should batch
/// across multiple write calls until the userspace-VA copy lane lands.
pub const TTY_WRITE_MAX_INLINE: usize = 4096;

/// Maximum path-name length accepted by `execve(2)` (Linux's
/// `PATH_MAX`). Mirrors the `TTY_WRITE_MAX_INLINE = 4096` discipline
/// for inline buffer copies. A longer path returns `-ENAMETOOLONG`
/// per the Phase 6 plan.
///
/// Could be lifted to platform `PATH_MAX` (typically 4096 across
/// Linux ABIs, so this is already at the canonical ceiling).
pub const EXECVE_PATH_MAX: usize = 4096;

/// Maximum total argv + envp byte budget per `execve(2)` call.
///
/// Linux's `ARG_MAX` is 128 KiB but the Phase 6 plan caps the inline
/// buffer at 8 KiB to keep the same discipline as the `write` /
/// `sigaction` arms. Overflow returns `-E2BIG`. Could be lifted to
/// 128 KiB now that the user-VA `copy_from_user` lane has landed.
pub const EXECVE_ARG_MAX_INLINE: usize = 8192;

/// Maximum number of pointer slots walked through `argv` / `envp`
/// before we give up. The Phase 6 plan caps at 256; in practice the
/// total-byte cap (`EXECVE_ARG_MAX_INLINE`) bounds well below this.
pub const EXECVE_VEC_MAX: usize = 256;

/// Linux generic ABI errno value for "function not implemented" (`ENOSYS`).
/// Used as the `-ENOSYS` magnitude returned from `dispatch` for every
/// syscall number not handled by Phase 2a / 2b.
pub(super) const ENOSYS_VALUE: i32 = 38;
/// Linux generic ABI errno value for "bad file descriptor" (`EBADF`).
pub(super) const EBADF_VALUE: i32 = 9;
/// Linux generic ABI errno value for "bad address" (`EFAULT`).
/// Used by Slice 4's time syscalls when a required user pointer is
/// null, and by every `bootstrap_*` user-VA bridge for invalid user
/// addresses (the canonical `aspace.copy_*_user` lane already
/// surfaces `Errno::EFAULT`; the dispatcher translates it here).
pub(super) const EFAULT_VALUE: i32 = 14;
/// Linux generic ABI errno value for "argument list too long" (`E2BIG`).
/// Used when a syscall argument violates a Phase 2a slice bound (e.g.
/// `write(len > TTY_WRITE_MAX_INLINE)`).
pub(super) const E2BIG_VALUE: i32 = 7;
/// Linux generic ABI errno value for "filename too long" (`ENAMETOOLONG`).
/// Used by Phase 6's `execve(path)` arm when the path overflows
/// `EXECVE_PATH_MAX`.
pub(super) const ENAMETOOLONG_VALUE: i32 = 36;
/// Linux generic ABI errno value for "invalid argument" (`EINVAL`).
/// Used by Phase 2b's `rt_sigprocmask` / `rt_sigaction` for the
/// `sigsetsize != 8` rejection per `SIGNAL_v1` §3 / §15.1, and for
/// any signum out of the 1..=64 range.
pub(super) const EINVAL_VALUE: i32 = 22;
/// Linux generic ABI errno value for "no such process" (`ESRCH`).
/// Used by `rt_sigprocmask` / `rt_sigaction` when the target thread /
/// process is a zombie (no payload to install state on).
pub(super) const ESRCH_VALUE: i32 = 3;
/// Linux generic ABI errno value for "operation not permitted" (`EPERM`).
/// Used by `setpgid` / `setsid` when the caller is not allowed to
/// perform the requested process-group / session change (Wave 2's
/// day-1 surface only supports the self-pid / self-pgid form;
/// cross-process and join-existing-pgid map to `-EPERM`).
pub(super) const EPERM_VALUE: i32 = 1;
/// Linux generic ABI errno value for "out of memory" (`ENOMEM`).
/// Used by `setpgid` / `setsid` when zone allocation fails minting a
/// fresh `ProcessGroup` / `Session`.
pub(super) const ENOMEM_VALUE: i32 = 12;
/// Linux generic ABI errno value for "resource temporarily
/// unavailable" (`EAGAIN`). Reserved for `sys_clone` to surface
/// retriable allocator failures from `step_fork`'s VM-side clone path
/// (`fork_aspace`'s `WouldBlock`); current `step_fork` only surfaces
/// `Zone(_)` / `ParentZombie`, but EAGAIN is the canonical Linux
/// errno for fork's transient-failure case.
pub(super) const EAGAIN_VALUE: i32 = 11;
/// Linux generic ABI errno value for "no child processes" (`ECHILD`).
/// Used by `sys_wait4` when the caller has no children matching the
/// requested selector (Wave 3 of the fork/clone/wait4 slice).
pub(super) const ECHILD_VALUE: i32 = 10;
/// Linux generic ABI errno value for "permission denied" (`EACCES`).
/// Used by Wave 4 Part 4's file-mode arms (`fchmodat`, `fchownat`,
/// `faccessat`, `faccessat2`) when the DAC predicate denies the
/// requested permission bits.
pub(super) const EACCES_VALUE: i32 = 13;
/// Linux generic ABI errno value for "read-only file system"
/// (`EROFS`). Used by `fchmodat` / `fchownat` against devfs (which
/// returns `Errno::EROFS` from `step_chmod` / `step_chown` per the
/// Wave 3 slice's projection-only contract).
pub(super) const EROFS_VALUE: i32 = 30;
/// Linux generic ABI errno value for "I/O error" (`EIO`). Used as the
/// fall-through magnitude for `StepOutcome::Yield { shape:
/// YieldShape::OnWaitSource { .. } }` shapes the file-mode arms
/// cannot produce today (chmod/chown/access never block in
/// tmpfs/devfs); matches `errno_to_i32`'s `Errno::EIO` row.
pub(super) const EIO_VALUE: i32 = 5;
/// Required sigsetsize per Linux RV64 generic ABI: 8 bytes (a single
/// `u64` bitset matching `tx_subsystems::signal::SignalMask`'s
/// internal representation). `rt_sigprocmask` / `rt_sigaction`
/// reject any other value with `-EINVAL`.
pub(super) const SIGSETSIZE_BYTES: u64 = 8;
/// Minimum alternate signal stack size (Linux: MINSIGSTKSZ = 2048).
pub(super) const MINSIGSTKSZ: u64 = 2048;
/// Size of the kernel `struct sigaction` exchanged via `rt_sigaction`
/// on RV64 generic ABI.
///
/// Layout decision: Linux's `arch/riscv/include/uapi/asm/signal.h`
/// pulls in `asm-generic/signal.h`, which defines the kernel
/// (uapi) `struct sigaction` as four 64-bit fields:
///
/// ```text
/// struct sigaction {
///     __sighandler_t  sa_handler;   // 8B
///     unsigned long   sa_flags;     // 8B
///     __sigrestore_t  sa_restorer;  // 8B  (present under SA_RESTORER)
///     sigset_t        sa_mask;      // 8B  (single u64 bitset, sigsetsize=8)
/// };
/// ```
///
/// So the rt_sigaction syscall takes a 32-byte buffer. The plan's
/// "16 bytes" hint applied to the legacy `__OLD_SIGACTION` shape used
/// by the (deprecated) `sigaction()` syscall — the modern
/// `rt_sigaction` syscall uses the 32-byte form. We pin the modern
/// shape because (a) Linux RV64 has no `sigaction()` syscall at all
/// (it only ships `rt_sigaction`, NR_134) and (b) `__sa_restorer` is
/// part of the ABI even when SA_RESTORER is unset (kernel reads all
/// four words and ignores the restorer bits unless the flag is set).
///
/// Citation: linux/include/uapi/asm-generic/signal.h
/// `struct sigaction { __sighandler_t sa_handler; unsigned long
///  sa_flags; __ARCH_HAS_SA_RESTORER ? __sigrestore_t sa_restorer;
///  sigset_t sa_mask; };` — RV64 enables `__ARCH_HAS_SA_RESTORER`
/// transitively (the field is always emitted at the ABI level).
pub(super) const SIGACTION_BYTES: usize = 32;

/// Per-syscall context resolved by the trap-shell wrapper: the calling
/// process / thread, the bound address space, and the bookkeeping the
/// dispatch table needs to act without knowing the wrapper's shape.
///
/// Phase 2a only consumes `process` (for `getpid` / `exit_group` and
/// fd-table lookup) and `thread` (for `exit`). `aspace` is wired into
/// the surface today so the Phase 2b additions (`brk`, `read`) can
/// land without a context-shape break; the field is intentionally
/// unused by the four current arms.

/// Dispatch a Phase 2a syscall.
///
/// This is the single entry point that maps a `SyscallRequest` to a
/// concrete `step_*` call. The function is `async` because some arms
/// (notably `NR_WRITE`) loop on `StepOutcome::Yield { shape:
/// YieldShape::OnWaitSource { .. } }` and `.await` the wait-source
/// release per `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`. The four currently
/// implemented arms return synchronously today; the `async` shape
/// stays so Phase 2b's additions (`read`, `brk`) can return
/// `SyscallResult::Return` after one or more `.await` points without
/// changing the surface.
pub async fn dispatch<'a, P: PmapIf + EntropyIf + TimeIf>(
    req: SyscallRequest,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    // ── Lane 1: ImmediateSyscall (pure ABI queries, never yield) ──
    // Per `docs/Txv3/04_SYSCALL_SHAPE_v1.md §6.1`: these syscalls
    // do not call drive(), do not enter StepOp, do not construct
    // YieldShape, and do not access VFS/VM/reactor/timer.
    match req.nr {
        NR_GETPID => return sys_getpid(ctx),
        nr if nr == NR_GETPPID => return sys_getppid(ctx),
        nr if nr == NR_GETPGRP => return sys_getpgrp(ctx),
        nr if nr == NR_GETPGID => return sys_getpgid(req.args, ctx),
        nr if nr == NR_GETSID => return sys_getsid(req.args, ctx),
        nr if nr == NR_GETUID => return sys_getuid(ctx),
        nr if nr == NR_GETEUID => return sys_geteuid(ctx),
        nr if nr == NR_GETGID => return sys_getgid(ctx),
        nr if nr == NR_GETEGID => return sys_getegid(ctx),
        nr if nr == NR_GETRESUID => return sys_getresuid(req.args, ctx),
        nr if nr == NR_GETRESGID => return sys_getresgid(req.args, ctx),
        nr if nr == NR_TIMES => return sys_times::<P>(req.args, ctx),
        nr if nr == NR_GETTIMEOFDAY => return sys_gettimeofday::<P>(req.args, ctx),
        nr if nr == NR_UMASK => return sys_umask(req.args, ctx),
        nr if nr == NR_UNAME => return sys_uname(req.args, ctx),
        nr if nr == NR_PRLIMIT64 => return sys_prlimit64(req.args, ctx),
        nr if nr == NR_RT_SIGRETURN => return sys_rt_sigreturn(ctx),
        _ => {} // fall through to script lanes
    }

    // ── Lanes 2+3: Script-based (OneShotStepOp + Full async drive) ──
    match req.nr {
        NR_WRITE => sys_write(req.args, ctx).await,
        NR_WRITEV => sys_writev(req.args, ctx).await,
        NR_READ => sys_read(req.args, ctx).await,
        NR_READV => sys_readv(req.args, ctx).await,
        NR_PPOLL => sys_ppoll(req.args, ctx).await,
        NR_EXIT => sys_exit(req.args, ctx),
        NR_EXIT_GROUP => sys_exit_group(req.args, ctx),
        NR_BRK => sys_brk(req.args, ctx).await,
        NR_RT_SIGPROCMASK => sys_rt_sigprocmask(req.args, ctx),
        NR_RT_SIGACTION => sys_rt_sigaction(req.args, ctx),
        NR_FCNTL => sys_fcntl(req.args, ctx),
        nr if nr == NR_EXECVE => sys_execve::<P>(req.args, ctx).await,
        nr if nr == NR_CLONE => sys_clone::<P>(req.args, ctx),
        nr if nr == NR_WAIT4 => sys_wait4(req.args, ctx).await,
        nr if nr == NR_SETPGID => sys_setpgid(req.args, ctx),
        nr if nr == NR_SETSID => sys_setsid(ctx),
        nr if nr == NR_SET_TID_ADDRESS => sys_set_tid_address(req.args, ctx),
        nr if nr == NR_SET_ROBUST_LIST => sys_set_robust_list(req.args),
        // Wave 2 of the DAC + setuid slice — Part 3 (cred-mutation /
        // cred-reading arms). Each wraps a Wave 1 `cred::step_*`
        // helper through the new `ctx.cred()` accessor.
        nr if nr == NR_SETUID => sys_setuid(req.args, ctx),
        nr if nr == NR_SETGID => sys_setgid(req.args, ctx),
        nr if nr == NR_SETREUID => sys_setreuid(req.args, ctx),
        nr if nr == NR_SETREGID => sys_setregid(req.args, ctx),
        nr if nr == NR_SETRESUID => sys_setresuid(req.args, ctx),
        nr if nr == NR_SETRESGID => sys_setresgid(req.args, ctx),
        // Wave 4 Part 4 of the DAC + setuid slice — file-mode syscall
        // arms. Each wraps the FsOps surface Wave 3 Part 2 landed
        // (`step_chmod` / `step_chown`) plus a walker-side `access(2)`
        // predicate over the inode meta.
        nr if nr == NR_FCHMODAT => sys_fchmodat::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as u32,
            req.args[3] as i32,
            ctx,
        ),
        nr if nr == NR_FCHOWNAT => sys_fchownat::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as u32,
            req.args[3] as u32,
            req.args[4] as i32,
            ctx,
        ),
        nr if nr == NR_FACCESSAT => {
            sys_faccessat::<P>(req.args[0] as i32, req.args[1], req.args[2] as i32, ctx)
        }
        nr if nr == NR_FACCESSAT2 => sys_faccessat2::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2] as i32,
            req.args[3] as i32,
            ctx,
        ),
        // Wave 2 of the fd-ops slice — fd-management arms
        // (`openat` / `close` / `dup` / `dup3`). `sys_openat` needs
        // `<P>` because the walker's `resolve_path_at` is generic over
        // `PmapIf`; the others operate purely on the fd table and the
        // `Cap<OpenFile>` slot it carries.
        nr if nr == NR_OPENAT => {
            sys_openat::<P>(
                req.args[0] as i32,
                req.args[1],
                req.args[2] as u32,
                req.args[3] as u32,
                ctx,
            )
            .await
        }
        nr if nr == NR_CLOSE => sys_close(req.args[0] as u32, ctx),
        nr if nr == NR_DUP => sys_dup(req.args[0] as u32, ctx),
        nr if nr == NR_DUP3 => sys_dup3(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2] as u32,
            ctx,
        ),
        // fd-ops Wave 3 — anonymous pipe.
        nr if nr == NR_PIPE2 => sys_pipe2(req.args[0], req.args[1] as u32, ctx),
        // fd-ops Wave 4 — `lseek(2)`. Non-async; pure offset compute
        // through `OpenFile::step_lseek`. ESPIPE for non-seekable
        // backings (TTY / chardev / pipe), EISDIR for directories,
        // EINVAL for negative result / overflow / unknown whence.
        nr if nr == NR_LSEEK => sys_lseek(
            req.args[0] as u32,
            req.args[1] as i64,
            req.args[2] as u32,
            ctx,
        ),
        // Slice 2 of the shell-prompt roadmap — VM syscalls. Pure
        // plumbing on top of `vm::execution::*` primitives. mmap /
        // munmap / mprotect / mremap / madvise are synchronous (the
        // underlying `try_*` step variants never `.await`); msync
        // calls into `step_fsync` for File-backed page containers and
        // is the only one that may block.
        nr if nr == NR_MMAP => sys_mmap(req.args, ctx),
        nr if nr == NR_MUNMAP => sys_munmap(req.args, ctx),
        // mlock / munlock are synchronous; the underlying try_mlock
        // step never yields.
        nr if nr == NR_MLOCK => sys_mlock(req.args, ctx),
        nr if nr == NR_MUNLOCK => sys_munlock(req.args, ctx),
        nr if nr == NR_MPROTECT => sys_mprotect(req.args, ctx),
        nr if nr == NR_MREMAP => sys_mremap(req.args, ctx),
        nr if nr == NR_MADVISE => sys_madvise(req.args, ctx),
        nr if nr == NR_MSYNC => sys_msync(req.args, ctx).await,
        // Slice 3 of the shell-prompt roadmap — `futex(2)`. v1 honours
        // `FUTEX_WAIT` / `FUTEX_WAKE` against a 256-bucket hash table;
        // other op selectors return `-ENOSYS`. `FUTEX_PRIVATE_FLAG` /
        // `FUTEX_CLOCK_REALTIME` are recognised but ignored. Required
        // for musl libc init.
        nr if nr == NR_FUTEX => sys_futex(req.args, ctx).await,
        // Slice 4 of the shell-prompt roadmap — time syscalls. The
        // four POSIX clock ids alias to the platform monotonic clock
        // for v1 (CLOCK_REALTIME has no boot-time RTC offset yet;
        // CPU-time clocks have no per-process accounting yet —
        // documented at the constant declarations in `numbers.rs`).
        // `nanosleep` / `clock_nanosleep` park the task on the reactor
        // timer queue for real-duration sleeps; zero-duration and
        // past-deadline cases short-circuit immediately.
        nr if nr == NR_CLOCK_GETTIME => sys_clock_gettime::<P>(req.args, ctx),
        nr if nr == NR_NANOSLEEP => sys_nanosleep::<P>(req.args, ctx).await,
        nr if nr == NR_CLOCK_NANOSLEEP => sys_clock_nanosleep::<P>(req.args, ctx).await,
        // Slice 5 of the shell-prompt roadmap — `ioctl(2)` + TTY
        // routing. Without this, musl's `isatty(STDIN_FILENO)` check
        // returns false, the shell starts in non-interactive mode, no
        // prompt is printed. Pure plumbing — all eight TTY ioctl
        // step functions exist; the arm decodes `request` and
        // dispatches. Non-TTY fds and unknown request codes return
        // `-ENOTTY` per Linux's `man ioctl_tty`.
        nr if nr == NR_IOCTL => sys_ioctl(req.args, ctx),
        // Slice 6 of the shell-prompt roadmap — stat family
        // (`fstat` / `newfstatat` / `getdents64` / `getcwd` / `chdir`
        // / `umask`). `fchdir` returns `-ENOSYS` (carryover; OpenFile
        // has no DEntry hint to install via step_chdir).
        nr if nr == NR_FSTAT => sys_fstat(req.args, ctx),
        nr if nr == NR_NEWFSTATAT => sys_newfstatat(req.args, ctx).await,
        nr if nr == NR_GETCWD => sys_getcwd(req.args, ctx),
        nr if nr == NR_CHDIR => sys_chdir(req.args, ctx).await,
        nr if nr == NR_FCHDIR => SyscallResult::Error(ENOSYS_VALUE),
        nr if nr == NR_GETDENTS64 => sys_getdents64(req.args, ctx).await,
        nr if nr == NR_STATX => sys_statx(req.args, ctx).await,
        // Slice 7 of the shell-prompt roadmap — fcntl extension +
        // day-1 misc syscalls. None individually heavy; each unblocks
        // a specific shell-startup path.
        nr if nr == NR_KILL => sys_kill(req.args, ctx),
        nr if nr == NR_TKILL => sys_tkill(req.args, ctx),
        nr if nr == NR_TGKILL => sys_tgkill(req.args, ctx),
        nr if nr == NR_GETRANDOM => sys_getrandom::<P>(req.args, ctx),
        // rt_sigreturn: deferred. Returns -ENOSYS — the
        // SignalFrameIf::restore_signal_frame surface needs the trap
        // frame which the dispatcher does not yet pass through. The
        // dispatcher ENOSYS path matches; arm explicitly written for
        // grep-stability and future wiring.
        // Slice 8 of the shell-prompt roadmap — file-mutation syscalls.
        // Each arm wraps an in-tree `FsOps::*` step body
        // (`mkdir`/`rmdir`/`unlink`/`rename`/`link`/`symlink`/
        // `read_link`) plus, for the truncate pair, the
        // `page_backed::lifecycle::step_truncate` body. The walker
        // resolves the target path(s); the syscall arm dispatches
        // through `fs_ops_for_dentry` against the parent's dentry to
        // find the in-scope FS surface. `utimensat` is deferred
        // (`-ENOSYS`); `RENAME_EXCHANGE` / `RENAME_WHITEOUT` are
        // recognised flag bits but unsupported. See
        // `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md`
        // Slice 8.
        nr if nr == NR_MKDIRAT => sys_mkdirat(req.args, ctx).await,
        nr if nr == NR_UNLINKAT => sys_unlinkat(req.args, ctx).await,
        nr if nr == NR_SYMLINKAT => sys_symlinkat(req.args, ctx).await,
        nr if nr == NR_LINKAT => sys_linkat(req.args, ctx).await,
        nr if nr == NR_TRUNCATE => sys_truncate(req.args, ctx).await,
        nr if nr == NR_FTRUNCATE => sys_ftruncate(req.args, ctx),
        nr if nr == NR_READLINKAT => sys_readlinkat(req.args, ctx).await,
        nr if nr == NR_UTIMENSAT => sys_utimensat(req.args, ctx),
        nr if nr == NR_RENAMEAT2 => sys_renameat2(req.args, ctx).await,
        // PR-10 phase 2 — `userfaultfd(2)` scaffold. Mints a fresh
        // `Cap<UserfaultFd>` (W-Q phase 0 zone), wraps in an
        // `OpenFile` with `OpenFileBacking::Ufd`, installs in the fd
        // table, returns the fd. The companion `UFFDIO_API` ioctl
        // handshake is dispatched from `sys_ioctl` when the resolved
        // fd carries an `OpenFileBacking::Ufd` (see
        // `super::userfaultfd::step_uffdio_api`). Later phases
        // (P-10.3 / .4 / .5) land `UFFDIO_REGISTER` / fault
        // interception / reply ioctls.
        nr if nr == NR_USERFAULTFD => sys_userfaultfd(req.args[0] as u32, ctx),
        // PR-11 phase 1 — `io_setup(2)` scaffold. Mints a fresh
        // `Cap<AioContext>` (W-Z phase 1 zone), wraps in an
        // `OpenFile` with `OpenFileBacking::AioContext`, installs in
        // the fd table, returns the fd. Diverges from Linux which
        // writes a pointer-shape into the `aio_context_t *`
        // out-parameter; per D8 §4.1 we return a real fd and
        // userspace bridges. Phases 2–4 land `io_submit` (worker +
        // `with_on_behalf_of` borrow), `io_getevents`, and
        // `io_destroy`.
        nr if nr == NR_IO_SETUP => sys_io_setup(req.args[0] as u32, req.args[1], ctx),
        // PR-11 phase 2 — `io_submit(2)` dispatch. Resolves the AIO
        // fd, copies each iocb in, validates + pushes onto the
        // context's submit queue, and returns the count admitted.
        // The per-context worker future is the one
        // `sys_io_setup` spawned + stashed for phase 2's deferred-
        // pump model.
        nr if nr == NR_IO_SUBMIT => sys_io_submit(req.args, ctx),
        // PR-11 phase 4 — `io_getevents(2)` dispatch. Drains up to
        // `nr` completions from the AIO context's completion queue;
        // blocks on the `events_available` carrier until `min_nr` is
        // satisfied when `timeout == NULL`. Serialises each drained
        // event into a 32-byte `struct io_event` and writes through
        // P's address space.
        nr if nr == NR_IO_GETEVENTS => sys_io_getevents(req.args, ctx).await,
        // PR-11 phase 5 — `io_destroy(2)` dispatch. Trips the
        // worker's abort signal (cooperative cancel), drops the
        // worker future, removes the fd-table entry. Mirrors
        // `sys_close(2)` on the AIO fd plus the worker teardown.
        nr if nr == NR_IO_DESTROY => sys_io_destroy(req.args[0] as u32, ctx),
        // Future PR-12 phase 0 — `io_uring_setup(2)` scaffold (second
        // `OnBehalfOf<P>` canary). Mints a fresh `Cap<IoUring>`
        // (W-LL phase 0 zone), wraps in an `OpenFile` with
        // `OpenFileBacking::IoUring`, spawns the SQPOLL kthread via
        // `with_on_behalf_of`, installs the fd, returns the fd.
        // Diverges from Linux which writes ring offsets into
        // `*params`; per the scaffold scope the in-kernel `VecDeque`
        // ring doesn't yet need user-mmaps. Phase 1 will land the
        // user-mmapped ring + real SQE dispatch.
        nr if nr == NR_IO_URING_SETUP => sys_io_uring_setup(req.args[0] as u32, req.args[1], ctx),
        // Future PR-12 phase 1 — `io_uring_enter(2)` dispatch. SQPOLL
        // by definition does not need this for SQE submission (the
        // kthread polls); deferred to the future PR that handles
        // non-SQPOLL setups + the user-mmapped ring path.
        nr if nr == NR_IO_URING_ENTER => SyscallResult::Error(ENOSYS_VALUE),
        // D9-D — `signalfd4(fd, &mask, sizemask, flags)` dispatch.
        // `fd == -1` mints a fresh signalfd cap and installs it at
        // the lowest free fd; `fd >= 0` updates the mask on an
        // existing signalfd. Returns the fd. The companion
        // signalfd-shaped read(2) arm lives in sys_read after the
        // ufd discriminator.
        nr if nr == NR_SIGNALFD4 => sys_signalfd4(
            req.args[0] as i32,
            req.args[1],
            req.args[2],
            req.args[3] as u32,
            ctx,
        ),
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

/// Translate the subsystem-shared `Errno` enum into the Linux RV64
/// generic ABI errno number used in `-errno` returns.
///
/// Phase 2a covers only the errnos `OpenFile::step_write` /
/// `tty::execution::step_write` / `CharDeviceOps::write` can
/// produce. Anything outside that set falls back to `EIO`; future
/// phases extend the table in lockstep with the syscall arms.
pub(super) fn errno_to_i32(errno: Errno) -> i32 {
    match errno {
        Errno::EACCES => 13,
        Errno::EAGAIN => EAGAIN_VALUE,
        Errno::EBADF => EBADF_VALUE,
        Errno::EBUSY => 16,
        Errno::EDQUOT => 122,
        Errno::EEXIST => 17,
        Errno::EFAULT => 14,
        Errno::EINVAL => 22,
        Errno::EIO => 5,
        Errno::EISDIR => 21,
        Errno::ELOOP => 40,
        Errno::ENAMETOOLONG => 36,
        Errno::ENODEV => 19,
        Errno::ENOEXEC => 8,
        Errno::ENOMEM => 12,
        Errno::ENOENT => 2,
        Errno::ENOSYS => ENOSYS_VALUE,
        Errno::ENOTDIR => 20,
        Errno::ENOTEMPTY => 39,
        Errno::ENOTTY => 25,
        Errno::EPERM => 1,
        Errno::EPIPE => 32,
        Errno::ERANGE => 34,
        Errno::EROFS => 30,
        Errno::ESPIPE => 29,
        Errno::ESRCH => 3,
        Errno::ESTALE => 116,
    }
}

/// Linux generic ABI errno value for "no such file or directory"
/// (`ENOENT`). Used by `sys_openat` when the walker reports the file
/// is missing and `O_CREAT` is unset.
pub(super) const ENOENT_VALUE: i32 = 2;
/// Linux generic ABI errno value for "file exists" (`EEXIST`). Used by
/// `sys_openat` when `O_CREAT | O_EXCL` is set and the file already
/// exists.
pub(super) const EEXIST_VALUE: i32 = 17;
/// Linux generic ABI errno value for "is a directory" (`EISDIR`).
/// Used by `sys_openat` when `O_TRUNC` is requested against a
/// directory inode.
pub(super) const EISDIR_VALUE: i32 = 21;
/// Linux generic ABI errno value for "not a directory" (`ENOTDIR`).
/// Used by Slice 6's `sys_chdir` when the resolved path is not a
/// directory and by `sys_getdents64` for a non-directory fd.
pub(super) const ENOTDIR_VALUE: i32 = 20;
/// Linux generic ABI errno value for "interrupted system call" (`EINTR`).
/// Used by `sys_rt_sigsuspend`.
pub(super) const EINTR_VALUE: i32 = 4;
/// Linux generic ABI errno value for "result out of range" (`ERANGE`).
/// Used by Slice 6's `sys_getcwd` when the user buffer is too small
/// for the rendered cwd path (NUL terminator inclusive).
pub(super) const ERANGE_VALUE: i32 = 34;
