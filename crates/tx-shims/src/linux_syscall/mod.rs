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
//! takes a fresh `tx_substrate::epoch::guard()` *inside* the call
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

use alloc::sync::Arc;
use alloc::vec::Vec;

use tx_hal::{EntropyIf, PmapIf, TimeIf, UserPtr};
use tx_reactor::userspace::SyscallRequest;
use tx_scripts::process::exec::{exec_script, ExecError};
use tx_substrate::zone::Cap;
use tx_subsystems::cred::{
    step_setgid, step_setregid, step_setresgid, step_setresuid, step_setreuid, step_setuid,
    Capability, Cred, CredChange, Gid, Uid,
};
use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::process::{
    process_by_pid, seed_child_leader_context, step_chdir, step_exit_group, step_fork,
    step_getcwd, step_setpgid, step_setsid, step_waitpid_nohang, ChdirOutcome, ExitStatus, Pgid,
    Pid, ProcessIdentity, SetpgidError, SetsidError, WaitError, WaitTarget,
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
    step_ioctl_tiocnotty, step_ioctl_tiocsctty, step_ioctl_tiocspgrp, step_ioctl_tiocswinsz,
    IoctlCaller,
};
use tx_subsystems::tty::structure::{Termios, Winsize};
use tx_subsystems::vfs::structure::{
    Credential, InodeKind, InodeMeta, OpenFileFlags, RNodeBacking, StructPayload,
};
use tx_subsystems::vfs::{step_open, step_walk, DEntry, OpenFile};
use tx_subsystems::vm::{
    AddressSpace, MadviseAdvice, MapPlacement, Prot, UserRange, UserRangeError, UserVirtAddr,
    VmBacking, VmEntryFlags, VmMapError, VmMapRequest, VmRemapRequest, USER_PAGE_SIZE,
};
use tx_subsystems::wait_carrier;

pub mod numbers;

#[cfg(test)]
mod tests;

pub use numbers::{
    AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, CLOCK_BOOTTIME,
    CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID,
    CLOCK_REALTIME, CLOCK_REALTIME_COARSE, CLOCK_THREAD_CPUTIME_ID, DT_BLK, DT_CHR, DT_DIR,
    DT_FIFO, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, FD_CLOEXEC, FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK,
    FUTEX_CMP_REQUEUE, FUTEX_LOCK_PI, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_TRYLOCK_PI,
    FUTEX_UNLOCK_PI, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, FUTEX_WAKE_OP,
    F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_OK, F_SETFD, F_SETFL, GRND_INSECURE, GRND_NONBLOCK,
    GRND_RANDOM, MADV_DONTNEED, MADV_FREE, MADV_NORMAL, MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED,
    MAP_ANONYMOUS, MAP_DENYWRITE, MAP_EXECUTABLE, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_GROWSDOWN,
    MAP_HUGETLB, MAP_LOCKED, MAP_NONBLOCK, MAP_NORESERVE, MAP_POPULATE, MAP_PRIVATE, MAP_SHARED,
    MAP_STACK, MAP_SYNC, NR_BRK, NR_CHDIR, NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_CLONE, NR_CLOSE,
    NR_DUP, NR_DUP3, NR_EXECVE, NR_EXIT, NR_EXIT_GROUP, NR_FACCESSAT, NR_FACCESSAT2, NR_FCHDIR,
    NR_FCHMODAT, NR_FCHOWNAT, NR_FCNTL, NR_FSTAT, NR_FUTEX, NR_GETCWD, NR_GETDENTS64, NR_GETEGID,
    NR_GETEUID, NR_GETGID, NR_GETPGID, NR_GETPGRP, NR_GETPID, NR_GETPPID, NR_GETRANDOM, NR_GETRESGID,
    NR_GETRESUID, NR_GETSID, NR_GETTIMEOFDAY, NR_GETUID, NR_IOCTL, NR_KILL, NR_LINKAT, NR_LSEEK,
    NR_MADVISE, NR_MKDIRAT, NR_MMAP, NR_MPROTECT, NR_MREMAP, NR_MSYNC, NR_MUNMAP, NR_NANOSLEEP,
    NR_NEWFSTATAT, NR_OPENAT, NR_PIPE2, NR_PRLIMIT64, NR_READ, NR_READV, NR_READLINKAT, NR_RENAMEAT2,
    NR_RT_SIGACTION, NR_RT_SIGPROCMASK, NR_RT_SIGRETURN, NR_SETGID, NR_SETPGID, NR_SETREGID,
    NR_SETRESGID, NR_SETRESUID, NR_SETREUID, NR_SETSID, NR_SETUID, NR_SET_ROBUST_LIST,
    NR_SET_TID_ADDRESS, NR_SYMLINKAT, NR_TGKILL, NR_TIMES, NR_TKILL, NR_TRUNCATE, NR_UMASK,
    NR_UNAME, NR_UNLINKAT, NR_UTIMENSAT, NR_FTRUNCATE, NR_WAIT4, NR_WRITE, NR_WRITEV, O_ACCMODE,
    O_APPEND,
    O_CLOEXEC, O_CREAT, O_DIRECT, O_EXCL, O_NONBLOCK, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY,
    PROT_EXEC, PROT_GROWSDOWN, PROT_GROWSUP, PROT_NONE, PROT_READ, PROT_WRITE, RENAME_EXCHANGE,
    RENAME_NOREPLACE, RENAME_WHITEOUT, RLIMIT_AS, RLIMIT_CORE, RLIMIT_CPU, RLIMIT_DATA,
    RLIMIT_FSIZE, RLIMIT_LOCKS, RLIMIT_MEMLOCK, RLIMIT_MSGQUEUE, RLIMIT_NICE, RLIMIT_NOFILE,
    RLIMIT_NPROC, RLIMIT_RSS, RLIMIT_RTPRIO, RLIMIT_RTTIME, RLIMIT_SIGPENDING, RLIMIT_STACK,
    RLIM_INFINITY, R_OK, SEEK_CUR, SEEK_END, SEEK_SET, SIGCHLD, TCGETS, TCSETS, TCSETSF, TCSETSW,
    TIMER_ABSTIME, TIMES_NS_PER_TICK, TIOCGPGRP, TIOCGWINSZ, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP,
    TIOCSWINSZ, UTIME_NOW, UTIME_OMIT, WNOHANG, W_OK, X_OK,
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
const ENOSYS_VALUE: i32 = 38;
/// Linux generic ABI errno value for "bad file descriptor" (`EBADF`).
const EBADF_VALUE: i32 = 9;
/// Linux generic ABI errno value for "bad address" (`EFAULT`).
/// Used by Slice 4's time syscalls when a required user pointer is
/// null, and by every `bootstrap_*` user-VA bridge for invalid user
/// addresses (the canonical `aspace.copy_*_user` lane already
/// surfaces `Errno::EFAULT`; the dispatcher translates it here).
const EFAULT_VALUE: i32 = 14;
/// Linux generic ABI errno value for "argument list too long" (`E2BIG`).
/// Used when a syscall argument violates a Phase 2a slice bound (e.g.
/// `write(len > TTY_WRITE_MAX_INLINE)`).
const E2BIG_VALUE: i32 = 7;
/// Linux generic ABI errno value for "filename too long" (`ENAMETOOLONG`).
/// Used by Phase 6's `execve(path)` arm when the path overflows
/// `EXECVE_PATH_MAX`.
const ENAMETOOLONG_VALUE: i32 = 36;
/// Linux generic ABI errno value for "invalid argument" (`EINVAL`).
/// Used by Phase 2b's `rt_sigprocmask` / `rt_sigaction` for the
/// `sigsetsize != 8` rejection per `SIGNAL_v1` §3 / §15.1, and for
/// any signum out of the 1..=64 range.
const EINVAL_VALUE: i32 = 22;
/// Linux generic ABI errno value for "no such process" (`ESRCH`).
/// Used by `rt_sigprocmask` / `rt_sigaction` when the target thread /
/// process is a zombie (no payload to install state on).
const ESRCH_VALUE: i32 = 3;
/// Linux generic ABI errno value for "operation not permitted" (`EPERM`).
/// Used by `setpgid` / `setsid` when the caller is not allowed to
/// perform the requested process-group / session change (Wave 2's
/// day-1 surface only supports the self-pid / self-pgid form;
/// cross-process and join-existing-pgid map to `-EPERM`).
const EPERM_VALUE: i32 = 1;
/// Linux generic ABI errno value for "out of memory" (`ENOMEM`).
/// Used by `setpgid` / `setsid` when zone allocation fails minting a
/// fresh `ProcessGroup` / `Session`.
const ENOMEM_VALUE: i32 = 12;
/// Linux generic ABI errno value for "resource temporarily
/// unavailable" (`EAGAIN`). Reserved for `sys_clone` to surface
/// retriable allocator failures from `step_fork`'s VM-side clone path
/// (`fork_aspace`'s `WouldBlock`); current `step_fork` only surfaces
/// `Zone(_)` / `ParentZombie`, but EAGAIN is the canonical Linux
/// errno for fork's transient-failure case.
const EAGAIN_VALUE: i32 = 11;
/// Linux generic ABI errno value for "no child processes" (`ECHILD`).
/// Used by `sys_wait4` when the caller has no children matching the
/// requested selector (Wave 3 of the fork/clone/wait4 slice).
const ECHILD_VALUE: i32 = 10;
/// Linux generic ABI errno value for "permission denied" (`EACCES`).
/// Used by Wave 4 Part 4's file-mode arms (`fchmodat`, `fchownat`,
/// `faccessat`, `faccessat2`) when the DAC predicate denies the
/// requested permission bits.
const EACCES_VALUE: i32 = 13;
/// Linux generic ABI errno value for "read-only file system"
/// (`EROFS`). Used by `fchmodat` / `fchownat` against devfs (which
/// returns `Errno::EROFS` from `step_chmod` / `step_chown` per the
/// Wave 3 slice's projection-only contract).
const EROFS_VALUE: i32 = 30;
/// Linux generic ABI errno value for "I/O error" (`EIO`). Used as the
/// fall-through magnitude for `StepOutcome::Blocked` /
/// `AdvancedThenBlocked` shapes the file-mode arms cannot produce
/// today (chmod/chown/access never block in tmpfs/devfs); matches
/// `errno_to_i32`'s `Errno::EIO` row.
const EIO_VALUE: i32 = 5;
/// Required sigsetsize per Linux RV64 generic ABI: 8 bytes (a single
/// `u64` bitset matching `tx_subsystems::signal::SignalMask`'s
/// internal representation). `rt_sigprocmask` / `rt_sigaction`
/// reject any other value with `-EINVAL`.
const SIGSETSIZE_BYTES: u64 = 8;
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
const SIGACTION_BYTES: usize = 32;

/// Per-syscall context resolved by the trap-shell wrapper: the calling
/// process / thread, the bound address space, and the bookkeeping the
/// dispatch table needs to act without knowing the wrapper's shape.
///
/// Phase 2a only consumes `process` (for `getpid` / `exit_group` and
/// fd-table lookup) and `thread` (for `exit`). `aspace` is wired into
/// the surface today so the Phase 2b additions (`brk`, `read`) can
/// land without a context-shape break; the field is intentionally
/// unused by the four current arms.
pub struct SyscallCtx<'a> {
    pub process: Cap<ProcessIdentity>,
    pub thread: Cap<ThreadIdentity>,
    pub aspace: Cap<AddressSpace>,
    /// Sliced lifetime so future fields (signal-mask snapshot, cred
    /// snapshot) can be added without ripping every call site.
    pub _lifetime: core::marker::PhantomData<&'a ()>,
}

impl<'a> SyscallCtx<'a> {
    /// Construct a fresh context. Phase 2a callers (the syscall
    /// dispatch tests; the future trap-shell wrapper in Phase 6) take
    /// the three Caps from the resolved per-thread payload and pass
    /// them in.
    pub fn new(
        process: Cap<ProcessIdentity>,
        thread: Cap<ThreadIdentity>,
        aspace: Cap<AddressSpace>,
    ) -> Self {
        Self {
            process,
            thread,
            aspace,
            _lifetime: core::marker::PhantomData,
        }
    }

    /// Snapshot the current process's full credential.
    ///
    /// Returns a fresh [`Cred`] value (`Copy`); the payload's
    /// `SpinMutex<Cred>` is acquired once and released before return,
    /// so the snapshot is independent of the lock and safe to hold
    /// across `.await` points. Holding a reference into the lock
    /// would be unsound — `step_setuid` / `step_setresuid` etc. can
    /// mutate the cred while a syscall arm is `.await`-ing.
    ///
    /// Falls back to [`Cred::root`] for zombies (impossible in
    /// practice from inside a live syscall arm — the caller is by
    /// definition alive). The defensive default keeps every
    /// downstream arm's signature noise-free; callers that need to
    /// distinguish zombie vs. alive use `ctx.process.is_zombie()`
    /// directly.
    ///
    /// Companion to [`Self::walker_cred`] (the walker-side
    /// projection consumed by VFS path resolution).
    pub fn cred(&self) -> Cred {
        self.process.cred().unwrap_or_else(Cred::root)
    }

    /// Walker-side projection of the current cred. Builds a fresh
    /// [`Credential`] from `self.cred()` via the
    /// `From<&Cred> for Credential` bridge (Wave 1) — uses **euid**
    /// and **egid** (the POSIX rule for DAC checks), and forwards
    /// `effective_caps` so the walker can short-circuit on
    /// `CAP_DAC_OVERRIDE` without re-locking the per-process cred.
    ///
    /// Returned by value (never as a reference into the lock) so the
    /// snapshot can be held across `.await` points in callers like
    /// `sys_execve` that drive the multi-phase `exec_script`.
    pub fn walker_cred(&self) -> Credential {
        Credential::from(&self.cred())
    }
}

/// Outcome of a syscall dispatch.
///
/// Plan B (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`): the dispatcher
/// reports its outcome to the caller; the userspace-entry shim writes
/// the encoded `i64`/`-i32` value into a fresh trap frame's `a0` slot
/// just before `sret`. Phase 2a only ever produces these three
/// variants — `Blocked` from a step is awaited internally and never
/// surfaces to the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyscallResult {
    /// Success — encode `value` into `a0` (positive return path).
    Return(i64),
    /// Failure — encode `-errno` into `a0`. `errno` is the positive
    /// magnitude (e.g. 38 for `ENOSYS`); the userspace-entry shim is
    /// responsible for negating before writing.
    Error(i32),
    /// Thread (or process) ended; the future driving this syscall does
    /// not return to userspace. Used by `NR_EXIT` and `NR_EXIT_GROUP`.
    NoReturn,
    /// `execve` succeeded and the process's `AddressSpace` plus the
    /// thread's `saved_user_context` have been replaced. The syscall
    /// arm returned this; the thread future MUST NOT drain
    /// `pending_syscall_return` for this iteration — the next
    /// userspace re-entry runs the new image via the new
    /// `saved_user_context`. The previous trap frame's `a0` is
    /// effectively discarded (the new image's `_start` expects a
    /// fresh stack and zero-initialised gprs).
    ///
    /// Cites: `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`.
    ExecCommitted,
}

/// Dispatch a Phase 2a syscall.
///
/// This is the single entry point that maps a `SyscallRequest` to a
/// concrete `step_*` call. The function is `async` because some arms
/// (notably `NR_WRITE`) loop on `StepOutcome::Blocked` and `.await`
/// the wait-carrier release per
/// `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`. The four currently
/// implemented arms return synchronously today; the `async` shape
/// stays so Phase 2b's additions (`read`, `brk`) can return
/// `SyscallResult::Return` after one or more `.await` points without
/// changing the surface.
pub async fn dispatch<'a, P: PmapIf + EntropyIf + TimeIf>(
    req: SyscallRequest,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    match req.nr {
        NR_WRITE => sys_write(req.args, ctx).await,
        NR_WRITEV => sys_writev(req.args, ctx).await,
        NR_READ => sys_read(req.args, ctx).await,
        NR_READV => sys_readv(req.args, ctx).await,
        NR_EXIT => sys_exit(req.args, ctx),
        NR_EXIT_GROUP => sys_exit_group(req.args, ctx),
        NR_GETPID => sys_getpid(ctx),
        NR_BRK => sys_brk(req.args, ctx).await,
        NR_RT_SIGPROCMASK => sys_rt_sigprocmask(req.args, ctx),
        NR_RT_SIGACTION => sys_rt_sigaction(req.args, ctx),
        NR_FCNTL => sys_fcntl(req.args, ctx),
        nr if nr == NR_EXECVE => sys_execve::<P>(req.args, ctx).await,
        nr if nr == NR_CLONE => sys_clone::<P>(req.args, ctx),
        nr if nr == NR_WAIT4 => sys_wait4(req.args, ctx).await,
        nr if nr == NR_GETPPID => sys_getppid(ctx),
        nr if nr == NR_SETPGID => sys_setpgid(req.args, ctx),
        nr if nr == NR_GETPGID => sys_getpgid(req.args, ctx),
        nr if nr == NR_GETPGRP => sys_getpgrp(ctx),
        nr if nr == NR_GETSID => sys_getsid(req.args, ctx),
        nr if nr == NR_SETSID => sys_setsid(ctx),
        nr if nr == NR_SET_TID_ADDRESS => sys_set_tid_address(req.args, ctx),
        nr if nr == NR_SET_ROBUST_LIST => sys_set_robust_list(req.args),
        // Wave 2 of the DAC + setuid slice — Part 3 (cred-mutation /
        // cred-reading arms). Each wraps a Wave 1 `cred::step_*`
        // helper through the new `ctx.cred()` accessor.
        nr if nr == NR_GETUID => sys_getuid(ctx),
        nr if nr == NR_GETEUID => sys_geteuid(ctx),
        nr if nr == NR_GETGID => sys_getgid(ctx),
        nr if nr == NR_GETEGID => sys_getegid(ctx),
        nr if nr == NR_SETUID => sys_setuid(req.args, ctx),
        nr if nr == NR_SETGID => sys_setgid(req.args, ctx),
        nr if nr == NR_SETREUID => sys_setreuid(req.args, ctx),
        nr if nr == NR_SETREGID => sys_setregid(req.args, ctx),
        nr if nr == NR_SETRESUID => sys_setresuid(req.args, ctx),
        nr if nr == NR_SETRESGID => sys_setresgid(req.args, ctx),
        nr if nr == NR_GETRESUID => sys_getresuid(req.args, ctx),
        nr if nr == NR_GETRESGID => sys_getresgid(req.args, ctx),
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
        // `nanosleep` / `clock_nanosleep` ship the zero-duration /
        // past-deadline short-circuit only; non-zero durations return
        // `-ENOSYS` (deferred — needs a per-task timer-fire wait
        // carrier the slice does not yet wire).
        nr if nr == NR_CLOCK_GETTIME => sys_clock_gettime::<P>(req.args, ctx),
        nr if nr == NR_GETTIMEOFDAY => sys_gettimeofday::<P>(req.args, ctx),
        nr if nr == NR_TIMES => sys_times::<P>(req.args, ctx),
        nr if nr == NR_NANOSLEEP => sys_nanosleep::<P>(req.args, ctx),
        nr if nr == NR_CLOCK_NANOSLEEP => sys_clock_nanosleep::<P>(req.args, ctx),
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
        nr if nr == NR_UMASK => sys_umask(req.args, ctx),
        // Slice 7 of the shell-prompt roadmap — fcntl extension +
        // day-1 misc syscalls. None individually heavy; each unblocks
        // a specific shell-startup path.
        nr if nr == NR_KILL => sys_kill(req.args),
        nr if nr == NR_TKILL => sys_tkill(req.args),
        nr if nr == NR_TGKILL => sys_tgkill(req.args),
        nr if nr == NR_GETRANDOM => sys_getrandom::<P>(req.args, ctx),
        nr if nr == NR_UNAME => sys_uname(req.args, ctx),
        nr if nr == NR_PRLIMIT64 => sys_prlimit64(req.args, ctx),
        // rt_sigreturn: deferred. Returns -ENOSYS — the
        // SignalFrameIf::restore_signal_frame surface needs the trap
        // frame which the dispatcher does not yet pass through. The
        // dispatcher ENOSYS path matches; arm explicitly written for
        // grep-stability and future wiring.
        nr if nr == NR_RT_SIGRETURN => sys_rt_sigreturn(),
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
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

/// `write(fd, buf, count)`.
///
/// Phase 2a restriction (per the trio plan §"Part 2 — Syscall table"
/// `write` row): the buffer is treated as kernel-side bytes, not a
/// user VA. `args[1]` is taken as a kernel pointer that already points
/// into kernel-readable memory (the test scaffolding allocates from
/// the test's stack/heap). General `copy_from_user` is out of scope
/// per §"Out of scope".
/// `writev(fd, iov, iovcnt)` — gather-write per `man 2 writev`.
///
/// musl's stdio (`fwrite` / `fputs` / etc.) uses `writev` rather than
/// `write` to flush its line-buffered stdio, so this is on busybox's
/// startup hot path: without it, every stdio write returns `-ENOSYS`,
/// busybox treats the negative return as a fatal error and crashes
/// while trying to print a diagnostic.
///
/// Implementation: walk the user's `struct iovec[iovcnt]`
/// (`[*const u8; 8]` + `usize`, 16 bytes per entry on RV64), copy
/// each entry into a kernel `iovec_local` and forward to `sys_write`.
/// Returns the cumulative byte count, with Linux's standard partial-
/// success policy: a short or failed write on entry N returns the
/// running total if `total > 0`, or the error from entry N otherwise.
async fn sys_writev<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;

    if iovcnt < 0 || iovcnt > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        let write_args = [args[0], base, len, 0, 0, 0];
        match sys_write(write_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                // Short write: stop here and return what we got. Linux
                // does the same — writev never silently combines past
                // a short write.
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

/// `readv(fd, iov, iovcnt)` — scatter-read counterpart of `sys_writev`.
async fn sys_readv<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;

    if iovcnt < 0 || iovcnt > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        let read_args = [args[0], base, len, 0, 0, 0];
        match sys_read(read_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

async fn sys_write<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len > TTY_WRITE_MAX_INLINE {
        return SyscallResult::Error(E2BIG_VALUE);
    }

    // Resolve fd → Cap<OpenFile> against the process payload's stub
    // fd table. Holding the payload guard across the lookup is fine —
    // the resulting Cap is independent and the lock is released before
    // any `.await`.
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // Pull the user buffer into kernel memory through the canonical
    // user-VA lane (`bootstrap_copy_from_user` bridges via
    // `aspace.copy_from_user`, falling back to the kernel-pointer
    // deref the trio's earlier exemption used). The Vec is owned for
    // the duration of the step loop so the underlying user pages
    // can be re-mapped without affecting the byte stream we feed to
    // `step_write`.
    let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    if len > 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, buf_ptr as u64) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    // Loop on the canonical async wait discipline pattern from
    // `vm::execution::fault_script`. Each iteration takes a fresh
    // `tx_substrate::epoch::guard()` inside the step's call site so
    // the guard never crosses an `.await`.
    let mut total: usize = 0;
    let mut remaining = bytes.as_slice();
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            file.step_write(remaining, &guard)
        };
        match outcome {
            StepOutcome::Done(written) | StepOutcome::Advanced(written) => {
                total += written;
                let stop = written == 0 || written >= remaining.len();
                if stop {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[written..];
            }
            StepOutcome::AdvancedThenBlocked(written, token) => {
                total += written;
                if written >= remaining.len() {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[written..];
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
                // Otherwise the carrier has been retired or is a test
                // placeholder; fall through and retry immediately.
            }
            StepOutcome::Blocked(token) => {
                // No progress made yet; await the carrier and retry.
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
            }
            StepOutcome::Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                // fd-ops Wave 3 — Q2 DECIDED 2026-05-07. SIGPIPE is
                // delivered to the calling process before returning
                // `-EPIPE` to userspace. The pipe `step_write` cannot
                // do this itself (no process Cap); the syscall arm
                // is the right boundary because it has `ctx.process`.
                if errno == Errno::EPIPE {
                    let _ = tx_subsystems::signal::step_kill_process(
                        &ctx.process,
                        tx_subsystems::signal::Signum::SIGPIPE,
                    );
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

/// `exit(status)` — per-thread exit per `PROCESS_v1` §7.3.1.
///
/// The implementation of `step_thread_exit` (in
/// `crates/tx-subsystems/src/thread_runtime/execution.rs`) already
/// chains internally: when the exiting thread is the last in its
/// process, `step_thread_exit` invokes `step_process_exit` directly.
/// Per the trio plan's open question #6 and the doc citation in
/// `PROCESS_v1` §7.3.1 step 3 ("If `thread_count == 0`: trigger
/// step_process_exit"), the dispatcher therefore calls **only**
/// `step_thread_exit`. Calling `step_exit_group` here would
/// double-zombify the payload and corrupt the recorded exit status.
fn sys_exit<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let status = args[0] as i32;
    step_thread_exit(ctx.thread.clone(), status);
    SyscallResult::NoReturn
}

/// `exit_group(status)` — per `PROCESS_v1` §7.3.2.
fn sys_exit_group<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let status = args[0] as i32;
    step_exit_group(&ctx.process, ExitStatus::Exited(status));
    SyscallResult::NoReturn
}

/// `getpid()` — direct read of `process.pid` per `PROCESS_v1`
/// §"Step catalog" / `getpid` row in the trio plan.
///
/// No `.await`, no guard — `Pid` is `Copy` and the `pid` field on
/// `ProcessIdentity` is plainly addressable (it does not change after
/// construction).
fn sys_getpid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.pid.0 as i64)
}

/// `read(fd, buf, count)`.
///
/// Mirrors `sys_write`'s structure: resolve fd → `Cap<OpenFile>`,
/// route the user buffer through `bootstrap_copy_to_user`, and loop
/// on the wait-carrier discipline.
///
/// **Blocking semantic.** Pre-ELF Phase 5 (item 9) wires the UART RX
/// path so a blocked `read(0, ...)` actually parks until bytes arrive:
/// `tty::execution::step_read` returns `Blocked(token)` on an empty
/// input queue, the dispatcher awaits `wait_carrier::wait_on_token`,
/// and `tx_kernel::irq::uart_rx_irq_handler` drives
/// `tty::execution::step_ingest` from the IRQ side, which fires the
/// TTY's wait `Channel`. On any partial progress (`total > 0`)
/// the dispatcher returns what it has rather than block again,
/// matching `sys_write`'s partial-success policy.
async fn sys_read<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len > TTY_WRITE_MAX_INLINE {
        return SyscallResult::Error(E2BIG_VALUE);
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }

    // Read into a kernel-side staging buffer, then copy out through
    // the canonical user-VA lane (`bootstrap_copy_to_user` bridges
    // via `aspace.copy_to_user`, falling back to the kernel-pointer
    // dance the trio's earlier exemption used).
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];

    let mut total: usize = 0;
    let mut cursor: usize = 0;
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            file.step_read(&mut staging[cursor..], &guard)
        };
        match outcome {
            StepOutcome::Done(read) | StepOutcome::Advanced(read) => {
                if read > 0 {
                    if let Err(errno) = bootstrap_copy_to_user(
                        &ctx.aspace,
                        buf_ptr as u64 + cursor as u64,
                        &staging[cursor..cursor + read],
                    ) {
                        if total > 0 {
                            return SyscallResult::Return(total as i64);
                        }
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                total += read;
                let stop = read == 0 || cursor + read >= len;
                if stop {
                    return SyscallResult::Return(total as i64);
                }
                cursor += read;
            }
            StepOutcome::AdvancedThenBlocked(read, _token) => {
                if read > 0 {
                    if let Err(errno) = bootstrap_copy_to_user(
                        &ctx.aspace,
                        buf_ptr as u64 + cursor as u64,
                        &staging[cursor..cursor + read],
                    ) {
                        if total > 0 {
                            return SyscallResult::Return(total as i64);
                        }
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                total += read;
                if total > 0 {
                    // Partial-success policy: same as `write`. Return
                    // what we got rather than blocking; userspace
                    // re-issues the syscall to drain more.
                    return SyscallResult::Return(total as i64);
                }
                // total == 0 here is unreachable in practice (Advanced
                // implies progress) but fall through defensively to
                // the `Blocked` arm below.
                return SyscallResult::Return(0);
            }
            StepOutcome::Blocked(token) => {
                // Pre-ELF Phase 5 (item 9): no input buffered yet.
                // Park on the registered TTY wait carrier (fired
                // from `tty::execution::step_ingest` after UART RX
                // bytes land via `irq::uart_rx_irq_handler`), then
                // re-poll. Mirrors the canonical async wait
                // discipline pattern from
                // `vm::execution::fault_script` /
                // `RangeLock::WouldBlock`.
                //
                // `wait_on_token` returns `None` for test
                // placeholder tokens (carrier id not registered);
                // in that case fall through and re-poll
                // immediately. Production carriers are always
                // registered (see `TtyIdentity::new`). If a partial
                // read already happened on a prior iteration
                // (`total > 0`) we return what we have rather than
                // block, matching `sys_write`'s partial-success
                // policy.
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
            }
            StepOutcome::Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

/// `brk(requested)` per `txdoc:VM-5-8-BRK`.
///
/// - `requested == 0`: report the current break (Linux's "brk(0)
///   returns current_brk" idiom; matches glibc's `__sbrk(0)` probe).
/// - On any error from `brk_script` (including `InvalidRange` for
///   `requested < brk_base`): return the *unchanged* current break.
///   Linux's brk(2) **never** returns a negative errno; on failure
///   userspace observes "the break didn't move" and is responsible
///   for noticing.
async fn sys_brk<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let requested = args[0];

    let brk_base = ctx.process.brk_base();
    let current_brk = ctx.process.current_brk();

    // Requested == 0 is the "report current" idiom; never call into
    // the script (which would treat zero as `requested < brk_base`
    // and return InvalidRange).
    if requested == 0 {
        return SyscallResult::Return(current_brk as i64);
    }

    let base = UserVirtAddr(brk_base as usize);
    let cur = UserVirtAddr(current_brk as usize);
    let req = UserVirtAddr(requested as usize);

    match ctx.aspace.brk_script(base, cur, req).await {
        Ok(new_brk) => {
            ctx.process.set_current_brk(new_brk.0 as u64);
            SyscallResult::Return(new_brk.0 as i64)
        }
        Err(VmMapError::InvalidRange) | Err(_) => {
            // Linux: brk(2) never returns -errno. On failure (range
            // below brk_base, OOM, mapping conflict) report the
            // unchanged current break. Userspace detects "no
            // movement" by comparing against the prior break.
            SyscallResult::Return(current_brk as i64)
        }
    }
}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)` per `SIGNAL_v1` §3.
///
/// `sigsetsize` is rejected with `-EINVAL` for any value other than
/// `8` (the kernel's only supported sigset width on RV64 — a single
/// `u64` bitset). `set_ptr == 0` means "query only"; `oldset_ptr == 0`
/// means "don't return the previous mask".
fn sys_rt_sigprocmask<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let how_raw = args[0] as i32;
    let set_ptr = args[1] as usize;
    let oldset_ptr = args[2] as usize;
    let sigsetsize = args[3];

    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Decode `how` per Linux generic ABI: 0 = SIG_BLOCK, 1 = SIG_UNBLOCK,
    // 2 = SIG_SETMASK. `set_ptr == 0` short-circuits to a query-only
    // path — `step_sigprocmask` doesn't need to run because the mask
    // doesn't change; we only need to read the current value out for
    // `oldset_ptr` writeback.
    let how = match (how_raw, set_ptr) {
        (_, 0) => None,
        (0, _) => Some(SigmaskHow::Block),
        (1, _) => Some(SigmaskHow::Unblock),
        (2, _) => Some(SigmaskHow::SetMask),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Read the user-supplied set bitset through the canonical
    // user-VA lane (`bootstrap_read_user` bridges via
    // `aspace.read_user`, falling back to a kernel-pointer read on
    // EFAULT).
    let next_mask = if set_ptr == 0 {
        SignalMask::EMPTY
    } else {
        match bootstrap_read_user::<u64>(&ctx.aspace, set_ptr as u64) {
            Ok(bits) => SignalMask::new(bits),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };

    // If `set` is null we still need the previous mask to satisfy
    // `oldset_ptr`. `step_sigprocmask` returns `prev` from the
    // change record, so call it with `SetMask` of the *current* bits
    // (a no-op, plus it uniformly produces a `Replaced` record). The
    // simpler approach: skip the call and read the mask directly via
    // `step_sigprocmask` invoked with a no-op `SetMask` of `prev`...
    // but the cleanest shape is to call `step_sigprocmask` always
    // when `how` is Some, and for the query-only branch bypass it.
    let prev_mask: SignalMask = match how {
        Some(how) => match step_sigprocmask(&ctx.thread, how, next_mask) {
            SigprocmaskChange::Replaced { prev, .. } => prev,
            SigprocmaskChange::ZombieIgnored => {
                return SyscallResult::Error(ESRCH_VALUE);
            }
        },
        None => {
            // Query-only path. Use `Block` of EMPTY (a no-op) to
            // pull the current mask out without changing it. SIG_BLOCK
            // with empty `next` cannot alter the mask: `new = prev | 0`.
            match step_sigprocmask(&ctx.thread, SigmaskHow::Block, SignalMask::EMPTY) {
                SigprocmaskChange::Replaced { prev, .. } => prev,
                SigprocmaskChange::ZombieIgnored => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
            }
        }
    };

    if oldset_ptr != 0 {
        if let Err(errno) =
            bootstrap_write_user::<u64>(&ctx.aspace, oldset_ptr as u64, prev_mask.raw_bits())
        {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}

/// `rt_sigaction(signum, act, oldact, sigsetsize)` per `SIGNAL_v1`
/// §15.1.
///
/// Decodes a 32-byte kernel `struct sigaction` (see `SIGACTION_BYTES`
/// for the layout citation). `act_ptr == 0` queries the current
/// disposition without changing it; `oldact_ptr == 0` discards the
/// previous disposition.
fn sys_rt_sigaction<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let signum_raw = args[0] as u32;
    let act_ptr = args[1] as usize;
    let oldact_ptr = args[2] as usize;
    let sigsetsize = args[3];

    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let Some(sig) = (if signum_raw <= u8::MAX as u32 {
        Signum::new(signum_raw as u8)
    } else {
        None
    }) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    // Decode the new action (if any) through the canonical user-VA
    // lane (`bootstrap_copy_from_user` bridges via
    // `aspace.copy_from_user`).
    let new_disposition: Option<SigDisposition> = if act_ptr == 0 {
        None
    } else {
        let mut bytes = [0u8; SIGACTION_BYTES];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, act_ptr as u64) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let handler = read_u64_le(&bytes[0..8]);
        // sa_flags / sa_restorer / sa_mask are decoded but unused at
        // this layer — `SigDisposition` only stores the handler shape.
        // Once SA_SIGINFO / SA_RESTORER / per-handler mask wiring
        // lands these fields will materialise on `SigDisposition`.
        let _flags = read_u64_le(&bytes[8..16]);
        let _restorer = read_u64_le(&bytes[16..24]);
        let _mask = read_u64_le(&bytes[24..32]);

        // SIG_DFL == 0, SIG_IGN == 1 per Linux generic ABI; everything
        // else is a userspace function-pointer handler.
        let disp = match handler {
            0 => SigDisposition::Default,
            1 => SigDisposition::Ignore,
            other => SigDisposition::Handler(other as usize),
        };
        Some(disp)
    };

    // If the caller wants the previous disposition, snapshot it
    // *before* installing the new one. `step_sigaction` returns the
    // prev as part of `SigDispositionChange`, so a single call suffices
    // for both install and query — but `act_ptr == 0` is "query only",
    // and we must not mutate. Read the live disposition through the
    // process's `sig_actions` table accessor in that case.
    let prev_disposition: SigDisposition = match new_disposition {
        Some(disp) => match step_sigaction(&ctx.process, sig, disp) {
            SigDispositionChange::Replaced { prev } => prev,
            SigDispositionChange::Uncatchable(prev) => {
                // SIGKILL/SIGSTOP — `step_sigaction` silently keeps
                // them at default. Treat the call as a successful
                // query: return the (unchanged) prev to oldact, and
                // the syscall returns 0. Linux allows installing
                // SIG_DFL on these; installing handlers fails. For
                // simplicity (and matching `step_sigaction`'s shape)
                // we report success either way.
                prev
            }
            SigDispositionChange::ZombieIgnored => {
                return SyscallResult::Error(ESRCH_VALUE);
            }
        },
        None => {
            // Query-only: read directly via the process's
            // `sig_disposition` accessor. Returns `None` for zombies
            // — surface as `-ESRCH`.
            match ctx.process.sig_disposition(sig) {
                Some(d) => d,
                None => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
            }
        }
    };

    if oldact_ptr != 0 {
        let handler_value: u64 = match prev_disposition {
            SigDisposition::Default => 0, // SIG_DFL
            SigDisposition::Ignore => 1,  // SIG_IGN
            SigDisposition::Handler(addr) => addr as u64,
        };
        // Build a 32-byte image and copy out through the canonical
        // user-VA lane. Layout: 4×u64 little-endian (sa_handler,
        // sa_flags, sa_restorer, sa_mask). All but sa_handler are 0
        // until SA_SIGINFO / SA_RESTORER / per-handler mask wiring
        // lands.
        let mut image = [0u8; SIGACTION_BYTES];
        image[0..8].copy_from_slice(&handler_value.to_le_bytes());
        // image[8..32] already zero.
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, oldact_ptr as u64, &image) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}

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
fn sys_fcntl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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

/// `execve(path, argv, envp)` — Wave 4 / Phase 6 of the ELF-loader
/// plan.
///
/// Bounded user-buffer copy discipline (matches the existing
/// `TTY_WRITE_MAX_INLINE = 4096` / Phase 2a "kernel-side `from_raw_parts`"
/// pattern, with an explicit `EXECVE_PATH_MAX` / `EXECVE_ARG_MAX_INLINE`
/// cap):
///
/// 1. `path_uaddr` — read up to `EXECVE_PATH_MAX = 4096` bytes,
///    stopping at the first NUL byte. No NUL within budget →
///    `-ENAMETOOLONG`.
/// 2. `argv_uaddr` / `envp_uaddr` — each is a NULL-terminated array
///    of `*const u8` pointers (8 bytes each on RV64). Walk up to
///    `EXECVE_VEC_MAX = 256` slots; for each non-NULL pointer, read
///    a NUL-terminated string. Total string bytes across argv + envp
///    are bounded by `EXECVE_ARG_MAX_INLINE = 8192`. Overflow →
///    `-E2BIG`.
///
/// On `Ok(())` from `exec_script`, return `SyscallResult::ExecCommitted`.
/// The thread future MUST NOT drain `pending_syscall_return` for this
/// iteration — the new image's `_start` reads from a fresh
/// `saved_user_context` (entry pc / initial sp) and zero-initialised
/// gprs (System V psABI). On `Err(_)` map to a Linux negative errno
/// via `ExecError::to_errno_i32`.
///
/// User-buffer reads (`path_uaddr`, `argv_uaddr`, `envp_uaddr`) flow
/// through `read_user_cstr` / `read_user_cstr_vec`, which bridge via
/// the canonical `aspace.read_user` / `aspace.read_user_cstr` lane
/// (with a kernel-pointer fallback for test scaffolding).
async fn sys_execve<'a, P: PmapIf + EntropyIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let path_uaddr = args[0];
    let argv_uaddr = args[1];
    let envp_uaddr = args[2];

    // ----- Step 1: bounded read of the path -----
    let path_buf = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(buf) => buf,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };

    // ----- Step 2 + 3: bounded reads of argv and envp -----
    //
    // The byte budget is shared across argv and envp per Linux's
    // ARG_MAX semantics. Track `remaining` across both vector reads so
    // an oversized envp following a normal argv still triggers
    // `-E2BIG`.
    let mut remaining: usize = EXECVE_ARG_MAX_INLINE;
    let argv_buf = match read_user_cstr_vec(&ctx.aspace, argv_uaddr, EXECVE_VEC_MAX, &mut remaining)
    {
        Ok(v) => v,
        Err(ReadVecError::TooBig) => return SyscallResult::Error(E2BIG_VALUE),
    };
    let envp_buf = match read_user_cstr_vec(&ctx.aspace, envp_uaddr, EXECVE_VEC_MAX, &mut remaining)
    {
        Ok(v) => v,
        Err(ReadVecError::TooBig) => return SyscallResult::Error(E2BIG_VALUE),
    };

    // ----- Build kernel-side `&[&[u8]]` slices for `exec_script`. -----
    //
    // The owned `Vec<Vec<u8>>` outlives the `&[&[u8]]` snapshot
    // — both are local to this function so the lifetimes are
    // straightforward. `exec_script` only reads from the slices
    // during the stack-image build, well before any aspace swap.
    let argv_slices: Vec<&[u8]> = argv_buf.iter().map(|s| s.as_slice()).collect();
    let envp_slices: Vec<&[u8]> = envp_buf.iter().map(|s| s.as_slice()).collect();

    // Wave 2 (cred-on-ctx): consume the caller's cred through
    // `ctx.walker_cred()`. The walker projection uses euid/egid +
    // effective_caps per the POSIX DAC rule (Wave 1's
    // `From<&Cred> for Credential` bridge). `init.rs`'s bootstrap
    // exec stays on `Credential::root()` because it runs outside a
    // `SyscallCtx` (kernel-side bootstrap path).
    let cred = ctx.walker_cred();

    let outcome = exec_script::<P>(
        &ctx.process,
        &ctx.thread,
        &path_buf,
        &argv_slices,
        &envp_slices,
        &cred,
    )
    .await;

    match outcome {
        Ok(()) => SyscallResult::ExecCommitted,
        Err(e) => SyscallResult::Error(execve_errno_magnitude(e)),
    }
}

/// Outcome of `read_user_cstr` — distinguishes "no NUL within budget"
/// from a successful copy. The successful arm yields the bytes up to
/// (not including) the NUL terminator, allocated as a kernel-owned
/// `Vec<u8>`.
enum ReadCStrError {
    /// No NUL within `max_len` — surface as `-ENAMETOOLONG`.
    TooLong,
}

/// Outcome of `read_user_cstr_vec`. `TooBig` covers both
/// pointer-array overflow and aggregate-byte overflow; both surface
/// as `-E2BIG` per the Phase 6 plan.
enum ReadVecError {
    TooBig,
}

/// Bounded copy of a NUL-terminated user string into a kernel-owned
/// `Vec<u8>` (NUL terminator stripped). `uaddr == 0` produces an empty
/// vector — matches Linux's "execve(NULL, ...)" lenience for path =
/// NULL (which would actually surface as `EFAULT` in real Linux; the
/// trio Phase 2a bootstrap exemption pre-dates the EFAULT plumbing,
/// so we treat NULL as "empty").
///
/// Bridges through `bootstrap_read_user_cstr` (which delegates to
/// `aspace.read_user_cstr` and falls back to a kernel-pointer scan on
/// `EFAULT`).
fn read_user_cstr(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, ReadCStrError> {
    match bootstrap_read_user_cstr(aspace, uaddr, max_len) {
        Ok(v) => Ok(v),
        Err(Errno::ENAMETOOLONG) => Err(ReadCStrError::TooLong),
        // Other errnos collapse to TooLong defensively — the caller's
        // Result shape only carries the "too long" axis. Production
        // paths surface clean Done; the EFAULT fallback inside
        // `bootstrap_read_user_cstr` covers test scaffolding pointers.
        Err(_) => Err(ReadCStrError::TooLong),
    }
}

/// Bounded copy of a NULL-terminated array of `*const u8` user
/// pointers into a kernel-owned `Vec<Vec<u8>>`. Each non-NULL entry
/// resolves to its own NUL-terminated string. The aggregate-byte
/// budget shared across argv + envp is passed in through
/// `byte_budget` (decremented in place).
///
/// `uaddr == 0` produces an empty vector — matches Linux's lenience
/// for `execve(path, NULL, NULL)` per the Phase 6 plan.
///
/// Each pointer slot and each string read bridges through the
/// canonical user-VA lane (`bootstrap_read_user` /
/// `bootstrap_read_user_cstr`), falling back to the kernel-pointer
/// dance on EFAULT for test scaffolding.
fn read_user_cstr_vec(
    aspace: &AddressSpace,
    uaddr: u64,
    max_slots: usize,
    byte_budget: &mut usize,
) -> Result<Vec<Vec<u8>>, ReadVecError> {
    if uaddr == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    for slot in 0..max_slots {
        let slot_addr = uaddr.wrapping_add((slot * core::mem::size_of::<u64>()) as u64);
        let ptr = match bootstrap_read_user::<u64>(aspace, slot_addr) {
            Ok(p) => p,
            Err(_) => return Err(ReadVecError::TooBig),
        };
        if ptr == 0 {
            return Ok(out);
        }
        // Read the string at `ptr`, capped at the remaining byte
        // budget. We need at least one byte for the NUL terminator;
        // when `*byte_budget == 0` any non-empty string is `TooBig`.
        let cap = *byte_budget;
        let s = match read_user_cstr(aspace, ptr, cap) {
            Ok(s) => s,
            Err(ReadCStrError::TooLong) => return Err(ReadVecError::TooBig),
        };
        // Account `s.len() + 1` for the implicit NUL byte we read but
        // did not store, matching Linux's `ARG_MAX` accounting.
        let charged = s.len().saturating_add(1);
        if charged > *byte_budget {
            return Err(ReadVecError::TooBig);
        }
        *byte_budget -= charged;
        out.push(s);
    }
    // Hit the slot cap without observing a NULL terminator — treat
    // as oversized argv per the plan.
    Err(ReadVecError::TooBig)
}

/// Translate `ExecError` to the dispatched `-errno` magnitude the
/// Phase 6 syscall arm hands back through `SyscallResult::Error`.
///
/// `ExecError::to_errno_i32` returns the *signed* `-errno`
/// (`-2` for `ENOENT`); `SyscallResult::Error` carries the *positive*
/// magnitude (the userspace-entry shim negates before writing). We
/// flip the sign here so the existing `Error(i32)` discipline is
/// unchanged.
fn execve_errno_magnitude(e: ExecError) -> i32 {
    -e.to_errno_i32()
}

// =====================================================================
// User-VA bridging helpers.
//
// Phase userva-sweep: every syscall arm that previously dereferenced a
// userspace pointer through the bootstrap `core::ptr::read_volatile` /
// `write_volatile` exemption now routes through one of the bridging
// helpers below. Each helper:
//
// 1. Calls the canonical `aspace.copy_*_user` / `read_user` /
//    `write_user` / `read_user_cstr` lane which walks the AddressSpace's
//    recipes, materialises every covered page through its
//    `VmBacking`, publishes the page to pmap (so subsequent calls see
//    the same frame — see `vm/user_access.rs` module header), and
//    copies through the kernel direct-map view. This is the "real"
//    user-VA path that exec'd processes (and the bake-in fixture
//    after exec) follow.
// 2. On `Errno::EFAULT` (no recipe covers the address — typical for
//    unit-test scaffolding that passes kernel stack/heap pointers
//    directly), falls back to the bootstrap kernel-pointer dance
//    (`core::ptr::read_volatile` / `write_volatile`) the previous
//    user-VA-deferred sites used inline before the userva sweep.
//
// The fallback exists because the existing dispatch tests pass kernel
// pointers (e.g. `buf.as_ptr() as u64`, `&mut set as *mut u64 as u64`)
// directly: a fresh `AddressSpace` has no recipes covering them, so a
// pure `aspace.copy_*_user` call would EFAULT. The fallback is a
// bridge until those tests migrate to user-VA-shaped fixtures
// (`map_user_buffer + seed`); for the bake-in `init` fixture (which
// runs through `exec_script`) the user-VA path always succeeds and
// the fallback is never exercised.
//
// `Blocked` outcomes from the canonical path are awaited inside the
// bridge for sync helpers; async-context helpers surface the token to
// the caller. Today no in-tree backend produces `Blocked` from a
// user-buffer copy on the synchronous path (anon page-cache
// materialisation is sync, file-backed reads await up at the file's
// `step_read` lane), so the awaiting code is a defensive scaffold for
// future async-aware backings.
// =====================================================================

/// Read a `T: Copy` value from `uaddr` through the canonical
/// `aspace.read_user` lane, falling back to the bootstrap
/// kernel-pointer dance on `EFAULT`.
fn bootstrap_read_user<T: Copy>(aspace: &AddressSpace, uaddr: u64) -> Result<T, Errno> {
    let guard = tx_substrate::epoch::guard();
    match aspace.read_user(UserPtr::<T>::new(uaddr as usize), &guard) {
        StepOutcome::Done(v) | StepOutcome::Advanced(v) => Ok(v),
        StepOutcome::Err(Errno::EFAULT) => {
            drop(guard);
            // Fallback: kernel-pointer bootstrap exemption.
            // SAFETY: existing dispatch tests pass kernel-side pointers
            // directly. The fallback is a bridge until tests migrate.
            Ok(unsafe { core::ptr::read_volatile(uaddr as *const T) })
        }
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => Err(Errno::EIO),
    }
}

/// Write a `T: Copy` value to `uaddr` through the canonical
/// `aspace.write_user` lane, falling back to the bootstrap
/// kernel-pointer dance on `EFAULT`.
fn bootstrap_write_user<T: Copy>(aspace: &AddressSpace, uaddr: u64, value: T) -> Result<(), Errno> {
    let guard = tx_substrate::epoch::guard();
    match aspace.write_user(UserPtr::<T>::new(uaddr as usize), value, &guard) {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => Ok(()),
        StepOutcome::Err(Errno::EFAULT) => {
            drop(guard);
            // SAFETY: see `bootstrap_read_user`.
            unsafe {
                core::ptr::write_volatile(uaddr as *mut T, value);
            }
            Ok(())
        }
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => Err(Errno::EIO),
    }
}

/// Copy `dst.len()` bytes from user-space `uaddr` into the kernel-side
/// buffer `dst`. Bridges through `aspace.copy_from_user`, falling back
/// to a kernel-pointer memcpy on `EFAULT`.
fn bootstrap_copy_from_user(aspace: &AddressSpace, dst: &mut [u8], uaddr: u64) -> Result<(), Errno> {
    if dst.is_empty() {
        return Ok(());
    }
    let guard = tx_substrate::epoch::guard();
    match aspace.copy_from_user(dst, UserPtr::<u8>::new(uaddr as usize), &guard) {
        StepOutcome::Done(_) | StepOutcome::Advanced(_) => Ok(()),
        StepOutcome::Err(Errno::EFAULT) => {
            drop(guard);
            // SAFETY: see `bootstrap_read_user`.
            unsafe {
                core::ptr::copy_nonoverlapping(uaddr as *const u8, dst.as_mut_ptr(), dst.len());
            }
            Ok(())
        }
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => Err(Errno::EIO),
    }
}

/// Copy `src.len()` bytes from the kernel-side buffer `src` to
/// user-space `uaddr`. Bridges through `aspace.copy_to_user`, falling
/// back to a kernel-pointer memcpy on `EFAULT`.
fn bootstrap_copy_to_user(aspace: &AddressSpace, uaddr: u64, src: &[u8]) -> Result<(), Errno> {
    if src.is_empty() {
        return Ok(());
    }
    let guard = tx_substrate::epoch::guard();
    match aspace.copy_to_user(UserPtr::<u8>::new(uaddr as usize), src, &guard) {
        StepOutcome::Done(_) | StepOutcome::Advanced(_) => Ok(()),
        StepOutcome::Err(Errno::EFAULT) => {
            drop(guard);
            // SAFETY: see `bootstrap_read_user`.
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), uaddr as *mut u8, src.len());
            }
            Ok(())
        }
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => Err(Errno::EIO),
    }
}

/// Read a NUL-terminated user string at `uaddr`, capped at `max_len`
/// bytes. Bridges through `aspace.read_user_cstr`, falling back to the
/// bootstrap byte-by-byte scan on `EFAULT`.
///
/// Returns `Ok(bytes)` (without the NUL terminator). `Err(Errno)`
/// surfaces other errors; `Errno::ENAMETOOLONG` indicates `max_len`
/// bytes were walked without finding a NUL.
fn bootstrap_read_user_cstr(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, Errno> {
    if uaddr == 0 || max_len == 0 {
        return Ok(Vec::new());
    }
    let guard = tx_substrate::epoch::guard();
    match aspace.read_user_cstr(UserPtr::<u8>::new(uaddr as usize), max_len, &guard) {
        StepOutcome::Done(v) | StepOutcome::Advanced(v) => Ok(v),
        StepOutcome::Err(Errno::EFAULT) => {
            drop(guard);
            // Fallback bootstrap scan — matches the previous inline
            // helper.
            let mut out: Vec<u8> = Vec::new();
            out.reserve(core::cmp::min(max_len, 256));
            for offset in 0..max_len {
                // SAFETY: see `bootstrap_read_user`.
                let byte =
                    unsafe { core::ptr::read_volatile((uaddr as usize + offset) as *const u8) };
                if byte == 0 {
                    return Ok(out);
                }
                out.push(byte);
            }
            Err(Errno::ENAMETOOLONG)
        }
        StepOutcome::Err(e) => Err(e),
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => Err(Errno::EIO),
    }
}

/// Read 8 little-endian bytes from a slice as a `u64`. Used by
/// `sys_rt_sigaction`'s `struct sigaction` decode.
fn read_u64_le(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() >= 8);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

/// Resolve fd `idx` against the process payload's fd table. Returns
/// `None` if the process is a zombie or the slot is empty.
///
/// Per fd-ops Wave 1 the table is a sparse `BTreeMap<u32, Cap<OpenFile>>`;
/// any `u32` fd value is a valid key.
fn resolve_fd(process: &Cap<ProcessIdentity>, idx: u32) -> Option<Cap<OpenFile>> {
    process.fd(idx)
}

/// Translate the subsystem-shared `Errno` enum into the Linux RV64
/// generic ABI errno number used in `-errno` returns.
///
/// Phase 2a covers only the errnos `OpenFile::step_write` /
/// `tty::execution::step_write` / `CharDeviceOps::write` can
/// produce. Anything outside that set falls back to `EIO`; future
/// phases extend the table in lockstep with the syscall arms.
fn errno_to_i32(errno: Errno) -> i32 {
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

// =====================================================================
// Wave 2 of the fork/clone/wait4 slice — Part 2 (NR_CLONE) +
// Part 4 (process-tree introspection arms) + Part 5 (musl-startup
// stubs).
//
// NR_WAIT4 is intentionally absent — it lives in Wave 3 with the
// blocking-wait scaffolding (`exit_port` `WaitToken` await loop). See
// `docs/progress/plans/2026-05-06-fork-clone-wait4.md`.
// =====================================================================

/// `clone(flags, stack, parent_tidptr, tls, child_tidptr)`.
///
/// Wave 2 of the fork/clone/wait4 slice ships only the bare-`SIGCHLD`
/// shape musl's `_Fork.c:35` issues
/// (`__syscall(SYS_clone, SIGCHLD, 0)`):
///
/// - `args[0]` (`flags`) **must** equal [`SIGCHLD`] — anything else
///   (including `SIGCHLD | CLONE_VM`, `CLONE_VFORK`, the
///   pthread_create flag set, or zero flags) returns `-EINVAL`.
/// - `args[1]` (`stack`) **must** be `0` — non-zero stack is the
///   posix_spawn / pthread_create path, deferred.
/// - `args[2..5]` (`parent_tidptr`, `tls`, `child_tidptr`) are
///   ignored (they're only meaningful with the CLONE flags we
///   reject).
///
/// On success:
/// 1. The parent's `saved_user_context` is read off the calling
///    thread's payload (a kernel invariant — the trap shell stored it
///    at trap entry per Plan B). `None` here is a kernel-bug panic
///    with the stable `:clone:no-context` sentinel (decided 2026-05-06
///    open Q #2).
/// 2. `step_fork::<P>` mints a child `Cap<ProcessIdentity>` and a
///    leader `Cap<ThreadIdentity>` (the leader is at index 0 of the
///    new process's thread list per `step_fork`'s post-condition).
/// 3. `seed_child_leader_context` stamps the child leader's
///    `saved_user_context` with the parent's GPRs except
///    `regs[10] = 0` (RV64 a0) and `pc + 4` (skip past `ecall`).
/// 4. `reactor_submit::submit_child_thread` hands the child's
///    leader-thread future to the kernel-side reactor seam (installed
///    at boot by `tx-kernel`'s `CoreInit::install_reactor_submit_seam`).
///    Reaching the seam without an installer is a kernel-invariant
///    violation — the seam panics with `:clone:no-reactor-seam`.
/// 5. The parent's syscall return is the child's pid (the trap-shell
///    writeback drains `pending_syscall_return` into the parent's
///    fresh trap frame's `a0`); the child re-enters userspace with
///    `a0 == 0` from the seed.
///
/// Synchronous (no `.await`): `step_fork` is itself synchronous in
/// Wave 1's surface (`fork_aspace`'s `WouldBlock` cannot fire under
/// v1's single-thread-per-process model). The function is non-`async`
/// to keep the seam minimal.
fn sys_clone<'a, P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let flags = args[0];
    let stack = args[1];

    // Validation: bare-SIGCHLD only. Reject any other flag combo
    // (CLONE_VM, CLONE_VFORK, pthread_create OR-set, zero, etc.) and
    // any non-zero stack.
    if flags != SIGCHLD {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if stack != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Snapshot parent's saved trap context. Plan B discipline: the
    // trap shell stored this at trap entry. `None` here means the
    // shell never stored it — a kernel-invariant violation. Panic
    // with the stable `:clone:no-context` sentinel (matches the ELF
    // loader's `:bootstrap-exec:fail` precedent — decision recorded
    // 2026-05-06 in the Wave 2 plan, Open Q #2).
    let parent_user_ctx = ctx
        .thread
        .payload_cap()
        .expect(":clone:no-payload: kernel-invariant violation, calling thread had no payload")
        .saved_user_context()
        .expect(":clone:no-context: kernel-invariant violation, parent thread had no saved_user_context");

    // step_fork: mint a child ProcessIdentity + leader ThreadIdentity
    // + payload + parent.children/pgrp wiring.
    let child = match step_fork::<P>(&ctx.process) {
        Ok(c) => c,
        Err(tx_subsystems::process::ForkError::ParentZombie) => {
            // Impossible by construction — the calling process is the
            // parent and is alive (we're servicing its syscall). Map
            // to ESRCH defensively.
            return SyscallResult::Error(ESRCH_VALUE);
        }
        Err(tx_subsystems::process::ForkError::Vm(_)) => {
            // VmMapError (e.g. a transient WouldBlock or OOM during
            // fork_aspace). Map to EAGAIN — Linux's canonical
            // transient-fork-failure errno.
            return SyscallResult::Error(EAGAIN_VALUE);
        }
        Err(tx_subsystems::process::ForkError::Zone(_)) => {
            return SyscallResult::Error(ENOMEM_VALUE);
        }
    };

    // Resolve the child's leader thread (always at slot 0 by
    // `step_fork`'s post-condition).
    let child_thread = child
        .nth_thread(0)
        .expect(":clone:no-leader: kernel-invariant violation, fresh child has no leader thread");

    // Seed the child's leader trap context with the parent's GPRs
    // (a0 := 0, pc := pc + 4). Infallible.
    seed_child_leader_context(&child_thread, &parent_user_ctx);

    // Hand the child's leader thread to the reactor. Panics with
    // `:clone:no-reactor-seam` if the boot path didn't install the
    // seam — that's a boot-time invariant violation.
    reactor_submit::submit_child_thread(child.clone(), child_thread.clone());

    // Parent observes the child's pid. The trap shell drains
    // `pending_syscall_return` into the parent's fresh trap frame's
    // a0 before re-entry per Plan B.
    SyscallResult::Return(child.pid.0 as i64)
}

/// `wait4(pid, status, options, rusage)` — Wave 3 of the fork/clone/wait4
/// slice. The blocking variant: when no zombie matches and `WNOHANG`
/// is unset, the arm parks on the caller's per-process `exit_port`
/// carrier (registered at payload sign time per Wave 1) until any
/// child of this process zombifies, then re-polls.
///
/// ## pid → `WaitTarget`
///
/// Per the existing comment at `process/execution.rs:123-145`:
///
/// - `pid > 0` → [`WaitTarget::Pid`]
/// - `pid == 0` → [`WaitTarget::CallerPgrp`]
/// - `pid == -1` → [`WaitTarget::Any`]
/// - `pid < -1` → [`WaitTarget::Pgrp`] with `Pgid(-pid as u32)`
/// - `pid == i32::MIN` → `-EINVAL` (overflow on negate; matches Linux
///   per LTP `wait403`).
///
/// ## Options
///
/// - `WNOHANG = 0x1` — short-circuit: if no zombie ready, return `0`
///   instead of blocking. Acted on.
/// - `WUNTRACED = 0x2` / `WCONTINUED = 0x8` — Linux ignores unknown
///   bits silently for `wait4`; we mirror that behaviour. Stop/cont
///   surface needs the stop/cont signal slice (deferred).
///
/// ## rusage
///
/// Wave 3 rejects non-NULL `rusage` with `-EINVAL` per the slice
/// plan. txKernel doesn't track per-process resource usage today;
/// zero-fill is busy-work that doesn't unblock anything LTP exercises.
/// `TODO(phase-rusage)`: zero-fill or populate once rusage state lands.
///
/// ## wstatus write
///
/// If `wstatus_uaddr != 0`, the wait-status word
/// ([`ExitStatus::wait_status_word`]) is written as a little-endian
/// `i32` to the user address through `bootstrap_write_user::<i32>`
/// (canonical `aspace.write_user` lane with kernel-pointer fallback).
///
/// ## Blocking shape
///
/// The loop pattern matches `sys_read` / `vm::execution::fault_script`:
/// each iteration calls the synchronous `step_waitpid_nohang` walker
/// (no guard parameter — it takes its own snapshot internally). On
/// `Err(WaitError::NoneReady)` without `WNOHANG`, build a `WaitToken`
/// from `ctx.process.exit_port_wait_token()` and `wait_carrier::wait_on_token`
/// it. Post-wake, loop and re-poll: a third party may have reaped the
/// same zombie (e.g. another wait4 caller in the same process; or the
/// shared `INIT_PROCESS` reaper if init wakes first), so the second
/// poll may still return `NoneReady` — re-park.
///
/// Cites: `txdoc:PROCESS-WAIT-FAMILY-1`
/// (`docs/design/04_process-signals/PROCESS_v1.md` §7.4).
async fn sys_wait4<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as i64 as i32;
    let wstatus_uaddr = args[1];
    let options = args[2] as i32;
    let rusage_uaddr = args[3];

    // rusage: Wave 3 rejects non-NULL with -EINVAL. txKernel doesn't
    // track rusage today.
    if rusage_uaddr != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // pid → WaitTarget. i32::MIN's negate overflows; reject upfront.
    let target = match pid {
        i32::MIN => return SyscallResult::Error(EINVAL_VALUE),
        p if p == -1 => WaitTarget::Any,
        0 => WaitTarget::CallerPgrp,
        p if p > 0 => WaitTarget::Pid(Pid(p as u32)),
        p => {
            // p < -1: any child whose pgid matches `-p`.
            let pgid = (-p) as u32;
            WaitTarget::Pgrp(Pgid(pgid))
        }
    };

    let wnohang = (options & WNOHANG) != 0;

    // Polling loop with the canonical async-wait double-check shape.
    // Each iteration: poll → if Done(zombie) reap+return; if NoneReady
    // and WNOHANG return 0; else build a WaitToken and await.
    loop {
        let outcome = step_waitpid_nohang(&ctx.process, target);
        match outcome {
            Ok((child_pid, status)) => {
                if wstatus_uaddr != 0 {
                    let word = status.wait_status_word();
                    if let Err(errno) =
                        bootstrap_write_user::<i32>(&ctx.aspace, wstatus_uaddr, word)
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                return SyscallResult::Return(child_pid.0 as i64);
            }
            Err(WaitError::NoChildren) => {
                return SyscallResult::Error(ECHILD_VALUE);
            }
            Err(WaitError::NoneReady) => {
                if wnohang {
                    return SyscallResult::Return(0);
                }
                // Build the WaitToken from the parent's exit_port
                // carrier id (registered at payload-sign time, Wave 1).
                // `None` means the calling process is itself a zombie
                // — race against our own exit; surface as -ECHILD per
                // POSIX (no children to wait for from a dead process).
                let Some(token) = ctx.process.exit_port_wait_token() else {
                    return SyscallResult::Error(ECHILD_VALUE);
                };
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
                // Either `wait_on_token` returned None (test placeholder
                // carrier; should be `Some` for the live process payload)
                // or the future resolved. Loop and re-poll. The wake
                // races a third party reaping the same zombie, so the
                // re-poll may still observe NoneReady — fine, we re-park.
            }
        }
    }
}

/// `getppid()` — return the parent's pid, or `0` (`Pid::RESERVED`)
/// for orphans.
///
/// Wraps `ProcessIdentity::parent_pid()` — see
/// `crates/tx-subsystems/src/process/structure.rs:197`. Returns `0`
/// for init (no parent) and for processes whose parent has been
/// reclaimed. Real Linux returns init's pid for orphans; the trio's
/// `sever_children` reparents to init when init is registered, so
/// under normal flows the difference is invisible.
fn sys_getppid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.parent_pid().0 as i64)
}

/// `setpgid(pid, pgid)`.
///
/// Wraps `step_setpgid` (`process/execution.rs:721`). Day-1 only
/// supports `pid == 0` / `pid == self.pid` (setpgid on self) and
/// `pgid == 0` / `pgid == self.pid` (create a fresh process group
/// rooted at the caller's pid inside the caller's session). Anything
/// else returns `-EPERM` (matches Linux's errno for cross-pgrp
/// setpgid). Cross-process setpgid needs a pid → `Cap<ProcessIdentity>`
/// resolver that day-1 doesn't ship.
fn sys_setpgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as i32;
    let pgid = args[1] as i32;

    // Day-1: only "self" target supported (cross-process setpgid is
    // deferred). pid == 0 means "self" per Linux convention.
    if pid != 0 && (pid as u32) != ctx.process.pid.0 {
        return SyscallResult::Error(EPERM_VALUE);
    }

    // pgid == 0 means "use the caller's pid" — exactly what the trio's
    // step_setpgid supports.
    let new_pgid_raw = if pgid == 0 {
        ctx.process.pid.0
    } else {
        pgid as u32
    };

    match step_setpgid(&ctx.process, Pgid(new_pgid_raw)) {
        Ok(()) => SyscallResult::Return(0),
        Err(SetpgidError::Unimplemented) => SyscallResult::Error(EPERM_VALUE),
        Err(SetpgidError::Zone(_)) => SyscallResult::Error(ENOMEM_VALUE),
    }
}

/// `getpgid(pid)`.
///
/// Day-1 only supports `pid == 0` (self) and `pid == self.pid`.
/// Cross-pid lookup needs a pid → `Cap<ProcessIdentity>` resolver
/// that day-1 doesn't ship; cross-pid queries return `-ESRCH`.
///
/// Reads through `ProcessIdentity::pgrp_cap` (already used by the
/// trio's signal-permission machinery).
fn sys_getpgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as i32;
    if pid != 0 && (pid as u32) != ctx.process.pid.0 {
        // TODO(phase-pid-resolver): cross-pid getpgid once a global
        // pid → Cap<ProcessIdentity> table is wired.
        return SyscallResult::Error(ESRCH_VALUE);
    }
    SyscallResult::Return(ctx.process.pgrp_cap().pgid.0 as i64)
}

/// `getsid(pid)`.
///
/// Same shape as `getpgid` but reports the session id. Day-1 only
/// supports `pid == 0` / `pid == self.pid`; cross-pid queries return
/// `-ESRCH`.
fn sys_getsid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as i32;
    if pid != 0 && (pid as u32) != ctx.process.pid.0 {
        // TODO(phase-pid-resolver): cross-pid getsid once a global
        // pid → Cap<ProcessIdentity> table is wired.
        return SyscallResult::Error(ESRCH_VALUE);
    }
    SyscallResult::Return(ctx.process.pgrp_cap().session_cap().sid.0 as i64)
}

/// `setsid()` — create a new session rooted at the caller.
///
/// Wraps `step_setsid` (`process/execution.rs:746`). Returns the new
/// session id on success, `-ENOMEM` on zone-allocation failure.
///
/// Note: real Linux returns `-EPERM` if the caller is already a
/// process-group leader. The trio's `step_setsid` doesn't enforce
/// this and the slice ships the trio's behaviour. Flagged as a
/// follow-up (`TODO(phase-process-topology)`).
fn sys_setsid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    match step_setsid(&ctx.process) {
        Ok(sid) => SyscallResult::Return(sid.0 as i64),
        Err(SetsidError::Zone(_)) => SyscallResult::Error(ENOMEM_VALUE),
    }
}

/// `set_tid_address(tidptr)` — Wave 2 stub.
///
/// Returns the calling thread's tid (Linux's documented return for
/// this syscall). Ignores `tidptr` — the real semantic
/// (`clear_child_tid` slot + futex wakeup on thread exit) is deferred
/// to the pthread/futex slice.
///
/// TODO(phase-tls): wire `tidptr` through to a per-thread
/// `clear_child_tid` slot per `THREAD_RUNTIME_v1` §2.6.
fn sys_set_tid_address<'a>(_args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.thread.tid.0 as i64)
}

/// `set_robust_list(head, len)` — Wave 2 stub.
///
/// Returns `0` unconditionally. Ignores `head`/`len` — the real
/// semantic (futex robust-list registration + walk on thread exit)
/// is deferred to the futex slice.
///
/// TODO(phase-futex): register the robust-list head per-thread once
/// futex infrastructure lands.
fn sys_set_robust_list(_args: [u64; 6]) -> SyscallResult {
    SyscallResult::Return(0)
}

// =====================================================================
// Wave 2 of the DAC + setuid slice — Part 3 (process-side cred-mutation
// / cred-reading syscall arms). Each arm reads the caller's cred via
// `ctx.cred()` (Part 7) for the privilege check; setters wrap the
// already-shipping `tx_subsystems::cred::step_set*` family (Wave 1) and
// translate the Linux `(u32) -1` ("leave unchanged") sentinel to
// `Option::None` before calling.
//
// The translation `u32::MAX → None` is overflow-safe: userspace passes
// a `uid_t` (`u32`) sign-extended from the i32 sentinel, so `(u32) -1`
// arrives in our `args[i]` as `u32::MAX = 0xFFFF_FFFF`. The pattern
// `if v == u32::MAX { None } else { Some(Uid::new(v)) }` never reaches
// `Uid::new(u32::MAX)` for the sentinel branch and never wraps.
//
// See `docs/progress/plans/2026-05-06-dac-and-setuid.md` Part 3 and
// `txdoc:PROCESS-CREDENTIAL-SERVICE-DRAFT-1`.
// =====================================================================

/// Translate the Linux `(u32) -1 == u32::MAX` "leave unchanged"
/// sentinel into `Option::None`. Used by every two- and three-arg
/// setter (`setre{u,g}id`, `setres{u,g}id`).
///
/// Userspace passes `uid_t` (an unsigned 32-bit type), so the
/// `setresuid(-1, -1, -1)` call shape arrives in the kernel with each
/// arg holding `0xFFFF_FFFF`. Decoding to `Option::None` lets the
/// `cred::step_set*` family receive a clean "leave the corresponding
/// field alone" signal without any further sign-extension dance.
const UID_LEAVE_UNCHANGED: u32 = u32::MAX;

#[inline]
fn decode_uid_arg(raw: u32) -> Option<Uid> {
    if raw == UID_LEAVE_UNCHANGED {
        None
    } else {
        Some(Uid(raw))
    }
}

#[inline]
fn decode_gid_arg(raw: u32) -> Option<Gid> {
    if raw == UID_LEAVE_UNCHANGED {
        None
    } else {
        Some(Gid(raw))
    }
}

/// Map a `CredChange` outcome from a setter helper to the dispatched
/// `SyscallResult`. `Replaced` → `Return(0)`; `PermissionDenied` →
/// `-EPERM`; `Zombie` → `-ESRCH` (impossible in practice — the caller
/// is by definition alive — but defensive).
fn cred_change_to_result(change: CredChange) -> SyscallResult {
    match change {
        CredChange::Replaced { .. } => SyscallResult::Return(0),
        CredChange::PermissionDenied => SyscallResult::Error(EPERM_VALUE),
        CredChange::Zombie => SyscallResult::Error(ESRCH_VALUE),
    }
}

/// `getuid()`. Linux RV64 generic ABI `__NR_getuid`. Returns the
/// caller's real uid. Reads `ctx.cred()` once; no `.await`, no
/// privilege check (everyone can read their own uid).
fn sys_getuid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().uid.raw() as i64)
}

/// `geteuid()`. Linux RV64 generic ABI `__NR_geteuid`. Returns the
/// caller's effective uid.
fn sys_geteuid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().euid.raw() as i64)
}

/// `getgid()`. Linux RV64 generic ABI `__NR_getgid`. Returns the
/// caller's real gid.
fn sys_getgid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().gid.raw() as i64)
}

/// `getegid()`. Linux RV64 generic ABI `__NR_getegid`. Returns the
/// caller's effective gid.
fn sys_getegid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().egid.raw() as i64)
}

/// `setuid(uid)`. Wraps `cred::step_setuid` (Wave 1).
///
/// Privileged callers (`euid == 0` or `CAP_SETUID`) get all four of
/// `uid`, `euid`, `suid` set to `uid`. Non-privileged callers may
/// only swap `euid` among `(uid, euid, suid)`; any other target
/// returns `-EPERM`.
fn sys_setuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let target = Uid(args[0] as u32);
    cred_change_to_result(step_setuid(&ctx.process, target))
}

/// `setgid(gid)`. Wraps `cred::step_setgid` (Wave 1). Same privilege
/// rules as `setuid` applied to the gid family.
fn sys_setgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let target = Gid(args[0] as u32);
    cred_change_to_result(step_setgid(&ctx.process, target))
}

/// `setreuid(ruid, euid)`. Wraps `cred::step_setreuid` (Wave 1).
///
/// Each argument: `(u32) -1` (= `u32::MAX`) means "leave unchanged".
/// Privileged callers may set arbitrary values. Non-privileged
/// callers must each (when not the sentinel) supply a value
/// currently in `{uid, euid, suid}`. Linux quirk: when `ruid` is
/// supplied OR the post-call `euid` differs from the pre-call real
/// uid, the saved-set `suid` is bumped to the post-call effective
/// uid (the rule that distinguishes `setreuid` from `setresuid`).
fn sys_setreuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid = decode_uid_arg(args[0] as u32);
    let euid = decode_uid_arg(args[1] as u32);
    cred_change_to_result(step_setreuid(&ctx.process, ruid, euid))
}

/// `setregid(rgid, egid)`. Gid analog of `sys_setreuid`. Wraps
/// `cred::step_setregid` (Wave 1).
fn sys_setregid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid = decode_gid_arg(args[0] as u32);
    let egid = decode_gid_arg(args[1] as u32);
    cred_change_to_result(step_setregid(&ctx.process, rgid, egid))
}

/// `setresuid(ruid, euid, suid)`. Wraps `cred::step_setresuid`
/// (Wave 1). Each argument decodes the `(u32) -1` sentinel to
/// `Option::None` ("leave unchanged"). Privileged callers may set
/// any combination. Non-privileged callers must each (when not the
/// sentinel) supply a value currently in `{uid, euid, suid}`; if any
/// one fails the rule, no field changes and the call returns
/// `-EPERM` (atomic per `step_setresuid`'s contract).
fn sys_setresuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid = decode_uid_arg(args[0] as u32);
    let euid = decode_uid_arg(args[1] as u32);
    let suid = decode_uid_arg(args[2] as u32);
    cred_change_to_result(step_setresuid(&ctx.process, ruid, euid, suid))
}

/// `setresgid(rgid, egid, sgid)`. Gid analog of `sys_setresuid`.
/// Wraps `cred::step_setresgid` (Wave 1).
fn sys_setresgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid = decode_gid_arg(args[0] as u32);
    let egid = decode_gid_arg(args[1] as u32);
    let sgid = decode_gid_arg(args[2] as u32);
    cred_change_to_result(step_setresgid(&ctx.process, rgid, egid, sgid))
}

/// `getresuid(ruid_uaddr, euid_uaddr, suid_uaddr)`. Reads
/// `ctx.cred()` once and writes each `u32` raw uid to the
/// corresponding user pointer. NULL pointers skip that write.
///
/// Each uaddr is written through `bootstrap_write_user::<u32>`
/// (canonical `aspace.write_user` lane with kernel-pointer fallback
/// for test scaffolding). NULL pointers skip the write.
fn sys_getresuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid_uaddr = args[0];
    let euid_uaddr = args[1];
    let suid_uaddr = args[2];
    let cred = ctx.cred();

    if ruid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, ruid_uaddr, cred.uid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if euid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, euid_uaddr, cred.euid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if suid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, suid_uaddr, cred.suid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}

/// `getresgid(rgid_uaddr, egid_uaddr, sgid_uaddr)`. Gid analog of
/// `sys_getresuid`. Same bridging through the user-VA lane applies.
fn sys_getresgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid_uaddr = args[0];
    let egid_uaddr = args[1];
    let sgid_uaddr = args[2];
    let cred = ctx.cred();

    if rgid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, rgid_uaddr, cred.gid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if egid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, egid_uaddr, cred.egid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if sgid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, sgid_uaddr, cred.sgid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}

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
/// dispatch through `FsOps::step_chmod` etc. must ascend the dentry
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
fn resolve_path_at<P: PmapIf>(
    dirfd: i32,
    path: &[u8],
    cred: &Credential,
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, i32> {
    if dirfd != AT_FDCWD {
        // TODO(phase-dirfd): real dirfd-relative paths once the fd
        // table grows directory-fd semantics.
        return Err(EBADF_VALUE);
    }
    let cwd: Cap<DEntry> = match ctx.process.cwd() {
        Some(d) => d,
        None => return Err(EBADF_VALUE),
    };
    let guard = tx_substrate::epoch::guard();
    let outcome = poll_walker_synchronously(step_walk(cwd, path, cred, &guard));
    let dentry = match outcome {
        StepOutcome::Done(d) | StepOutcome::Advanced(d) => d,
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            return Err(EIO_VALUE);
        }
        StepOutcome::Err(errno) => return Err(errno_to_i32(errno)),
    };
    drop(guard);
    Ok(dentry)
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
fn poll_walker_synchronously<F: core::future::Future>(future: F) -> F::Output {
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

/// Translate an `Errno` from `step_chmod` / `step_chown` to the
/// dispatched `-errno` magnitude. Mirrors the existing
/// `errno_to_i32` table; the inline match keeps the file-mode arms
/// readable (only the four errnos the FsOps surface produces are
/// listed; everything else collapses to `-EINVAL`).
fn fs_change_errno_magnitude(errno: Errno) -> i32 {
    match errno {
        Errno::EPERM => EPERM_VALUE,
        Errno::EROFS => EROFS_VALUE,
        Errno::EACCES => EACCES_VALUE,
        Errno::ENOENT => 2,
        Errno::ENOSYS => ENOSYS_VALUE,
        _ => EINVAL_VALUE,
    }
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
fn fs_ops_for_dentry(dentry: &Cap<DEntry>) -> Option<Arc<dyn tx_subsystems::vfs::FsOps>> {
    let guard = tx_substrate::epoch::guard();
    let mut cursor: Cap<DEntry> = dentry.clone();
    loop {
        if let Some(weak) = cursor.rnode().containing_mount_weak() {
            if let Some(payload) = weak.upgrade(&guard) {
                return Some(payload.fs_ops.clone());
            }
        }
        let next = cursor.parent_hint().and_then(|w| w.upgrade(&guard));
        match next {
            Some(p) => cursor = p,
            None => return None,
        }
    }
}

/// `fchmodat(dirfd, path, mode, flags)`. Linux RV64 generic ABI.
///
/// Wraps `FsOps::step_chmod` (Wave 3 Part 2). Permission failures
/// surface as `-EPERM`; read-only filesystems (e.g. devfs) return
/// `-EROFS`. The `flags` argument (`AT_SYMLINK_NOFOLLOW`) is accepted
/// silently — chmod doesn't follow symlinks at this layer in the
/// slice anyway. Mode is masked to the bottom 12 bits (preserving
/// `S_ISUID`, `S_ISGID`, `S_ISVTX` plus `rwxrwxrwx`).
fn sys_fchmodat<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    mode: u32,
    _flags: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
    };
    let walker_cred = ctx.walker_cred();
    let dentry = match resolve_path_at::<P>(dirfd, &path, &walker_cred, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let fs_object_id = dentry.rnode().fs_object_id();
    let fs_ops = match fs_ops_for_dentry(&dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    // Mask to the bottom 12 bits (rwx + S_ISUID/S_ISGID/S_ISVTX);
    // callers can't change S_IFMT bits via chmod.
    let new_mode = (mode & 0o7777) as u16;
    let guard = tx_substrate::epoch::guard();
    match fs_ops.step_chmod(fs_object_id, new_mode, &walker_cred, &guard) {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(fs_change_errno_magnitude(errno)),
    }
}

/// `fchownat(dirfd, path, uid, gid, flags)`. Linux RV64 generic ABI.
///
/// Wraps `FsOps::step_chown` (Wave 3 Part 2). Each of `uid` / `gid`
/// decodes the `(u32) -1 == u32::MAX` "leave unchanged" sentinel to
/// `Option::None` (same convention as `setre{u,g}id`'s
/// `decode_uid_arg` / `decode_gid_arg`). Non-privileged callers may
/// only chown to their own uid/gid; arbitrary changes require
/// `CAP_FOWNER`.
fn sys_fchownat<P: PmapIf>(
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
    };
    let walker_cred = ctx.walker_cred();
    let dentry = match resolve_path_at::<P>(dirfd, &path, &walker_cred, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };
    let fs_object_id = dentry.rnode().fs_object_id();
    let fs_ops = match fs_ops_for_dentry(&dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let uid_opt = decode_uid_arg(uid_arg).map(|u| u.raw());
    let gid_opt = decode_gid_arg(gid_arg).map(|g| g.raw());
    let guard = tx_substrate::epoch::guard();
    match fs_ops.step_chown(fs_object_id, uid_opt, gid_opt, &walker_cred, &guard) {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(fs_change_errno_magnitude(errno)),
    }
}

/// `faccessat(dirfd, path, mode)`. Linux RV64 generic ABI. POSIX
/// `access(2)` shape: the access check uses the caller's **real**
/// uid/gid (not effective). Implemented in terms of
/// [`sys_faccessat2_impl`] with `flags = 0`.
fn sys_faccessat<P: PmapIf>(
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
fn sys_faccessat2<P: PmapIf>(
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
fn sys_faccessat2_impl<P: PmapIf>(
    dirfd: i32,
    path_uaddr: u64,
    mode: i32,
    flags: i32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let path = match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(p) => p,
        Err(ReadCStrError::TooLong) => return SyscallResult::Error(ENAMETOOLONG_VALUE),
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

    // Resolve the path. Note: the walker's interior-directory descend
    // check uses `walker_cred`'s ids — for the AT_EACCESS=0 default
    // this is the **real** id walk. POSIX `access(2)` is documented
    // as exactly this shape ("uses the real uid/gid for both the
    // access check and the path resolution"); no separate walk is
    // required.
    let dentry = match resolve_path_at::<P>(dirfd, &path, &walker_cred, ctx) {
        Ok(d) => d,
        Err(e) => return SyscallResult::Error(e),
    };

    let inode_meta = dentry.rnode().meta();
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

// =====================================================================
// Wave 2 of the fd-ops slice — fd-management syscall arms.
//
// Coverage:
//   - `sys_openat` (NR_OPENAT = 56). Wave 2's slice surface only
//     supports `dirfd == AT_FDCWD`; non-cwd dirfds map to `-EBADF`. The
//     walker resolves the path via `vfs::step_open` using the caller's
//     `walker_cred()` (effective ids per POSIX). On `O_CREAT` against a
//     missing file, the syscall arm walks to the parent directory,
//     calls `FsOps::create_inode`, and re-runs `step_open` (the
//     create-on-open path is implemented at the syscall arm rather
//     than baked into the walker — keeps the walker resolve-only per
//     the slice plan §"Cross-cutting risks #6").
//   - `sys_close` (NR_CLOSE = 57). Removes the `Cap<OpenFile>` from
//     the fd table; EBR-deferred reclamation fires the `OpenFile`'s
//     `Drop`.
//   - `sys_dup` (NR_DUP = 23) and `sys_dup3` (NR_DUP3 = 24). `NR_DUP2`
//     is absent on the RV64 generic ABI; musl emits `dup3(.., 0)` for
//     the legacy `dup2(oldfd, newfd)` shape.
//
// See `docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md`
// Parts 2–4 and `txdoc:VFS-CHECKS-OPEN-FLAGS-1`.
// =====================================================================

/// Linux generic ABI errno value for "no such file or directory"
/// (`ENOENT`). Used by `sys_openat` when the walker reports the file
/// is missing and `O_CREAT` is unset.
const ENOENT_VALUE: i32 = 2;
/// Linux generic ABI errno value for "file exists" (`EEXIST`). Used by
/// `sys_openat` when `O_CREAT | O_EXCL` is set and the file already
/// exists.
const EEXIST_VALUE: i32 = 17;
/// Linux generic ABI errno value for "is a directory" (`EISDIR`).
/// Used by `sys_openat` when `O_TRUNC` is requested against a
/// directory inode.
const EISDIR_VALUE: i32 = 21;
/// Linux generic ABI errno value for "not a directory" (`ENOTDIR`).
/// Used by Slice 6's `sys_chdir` when the resolved path is not a
/// directory and by `sys_getdents64` for a non-directory fd.
const ENOTDIR_VALUE: i32 = 20;
/// Linux generic ABI errno value for "result out of range" (`ERANGE`).
/// Used by Slice 6's `sys_getcwd` when the user buffer is too small
/// for the rendered cwd path (NUL terminator inclusive).
const ERANGE_VALUE: i32 = 34;

/// Decode the access-mode bits (`O_RDONLY`/`O_WRONLY`/`O_RDWR`) of an
/// `openat(2)` `flags` argument into the `(read, write)` pair. Linux's
/// `O_RDONLY = 0` reads as "read", `O_WRONLY = 1` as "write",
/// `O_RDWR = 2` as both. The historical `0o3` ("search") shape is
/// silently treated as `O_RDONLY` (we map it to `(true, false)`); no
/// shipping userspace emits it, but Linux historically tolerates it.
fn decode_access_mode(flags: u32) -> (bool, bool) {
    match flags & numbers::O_ACCMODE {
        numbers::O_WRONLY => (false, true),
        numbers::O_RDWR => (true, true),
        // O_RDONLY (0) and the legacy "search" shape (0o3) both fall here.
        _ => (true, false),
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
fn split_path(path: &[u8]) -> (&[u8], &[u8]) {
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
async fn sys_openat<'a, P: PmapIf>(
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
    // `poll_walker_synchronously` helper that the Wave 4 file-mode
    // arms also use; every in-tree walker backend resolves
    // immediately so the noop-waker poll always returns `Ready`.
    let walk_first = {
        let guard = tx_substrate::epoch::guard();
        let outcome =
            poll_walker_synchronously(step_walk(cwd.clone(), &path, &walker_cred, &guard));
        drop(guard);
        outcome
    };

    let dentry: Cap<DEntry> = match walk_first {
        StepOutcome::Done(d) | StepOutcome::Advanced(d) => {
            if want_create && want_excl {
                return SyscallResult::Error(EEXIST_VALUE);
            }
            d
        }
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            return SyscallResult::Error(EIO_VALUE);
        }
        StepOutcome::Err(Errno::ENOENT) if want_create => {
            match create_then_walk::<P>(&cwd, &path, mode as u16, &walker_cred) {
                Ok(d) => d,
                Err(e) => return SyscallResult::Error(e),
            }
        }
        StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
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
            let fs_page_backing = match fs_page_backing_for_dentry(&dentry) {
                Some(b) => b,
                None => return SyscallResult::Error(ENOSYS_VALUE),
            };
            let fs_object_id = dentry.rnode().fs_object_id();
            let guard = tx_substrate::epoch::guard();
            match fs_page_backing.truncate(fs_object_id, 0, &guard) {
                StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
                StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                    return SyscallResult::Error(EIO_VALUE);
                }
                StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
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
            StepOutcome::Done(file) | StepOutcome::Advanced(file) => file,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
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
fn create_then_walk<P: PmapIf>(
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
        let guard = tx_substrate::epoch::guard();
        let outcome = poll_walker_synchronously(step_walk(cwd.clone(), parent_path, cred, &guard));
        drop(guard);
        match outcome {
            StepOutcome::Done(d) | StepOutcome::Advanced(d) => d,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return Err(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return Err(errno_to_i32(errno)),
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
        let guard = tx_substrate::epoch::guard();
        let outcome = fs_ops.create_inode(parent_fs_object_id, basename, new_mode, cred, &guard);
        match outcome {
            StepOutcome::Done(_) | StepOutcome::Advanced(_) => {}
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return Err(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return Err(errno_to_i32(errno)),
        }
    }

    // Re-walk the full path. Lookup now resolves the freshly-created
    // inode; the resulting dentry carries the proper parent-hint
    // chain back to the mount root.
    let guard = tx_substrate::epoch::guard();
    let outcome = poll_walker_synchronously(step_walk(cwd.clone(), path, cred, &guard));
    drop(guard);
    match outcome {
        StepOutcome::Done(d) | StepOutcome::Advanced(d) => Ok(d),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => Err(EIO_VALUE),
        StepOutcome::Err(errno) => Err(errno_to_i32(errno)),
    }
}

/// Resolve the `Arc<dyn FsPageBacking>` in scope for a dentry by
/// ascending its parent-hint chain to find an rnode that carries
/// `with_containing_mount`. Mirrors `fs_ops_for_dentry`'s shape but
/// reads the mount payload's `fs_page_backing` field instead of
/// `fs_ops`. Used by `sys_openat`'s O_TRUNC arm to call
/// `FsPageBacking::truncate(0)` on the resolved file.
///
/// Returns `None` for orphan dentries (no parent-hint chain reaches
/// a rnode with a mount weak); the `O_TRUNC` arm surfaces that as
/// `-ENOSYS` (no backing → no truncate).
fn fs_page_backing_for_dentry(
    dentry: &Cap<DEntry>,
) -> Option<Arc<dyn tx_subsystems::page_backed::FsPageBacking>> {
    let guard = tx_substrate::epoch::guard();
    let mut cursor: Cap<DEntry> = dentry.clone();
    loop {
        if let Some(weak) = cursor.rnode().containing_mount_weak() {
            if let Some(payload) = weak.upgrade(&guard) {
                return Some(payload.fs_page_backing.clone());
            }
        }
        let next = cursor.parent_hint().and_then(|w| w.upgrade(&guard));
        match next {
            Some(p) => cursor = p,
            None => return None,
        }
    }
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
fn sys_close<'a>(fd: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
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
fn sys_dup<'a>(oldfd: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
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
fn sys_dup3<'a>(oldfd: u32, newfd: u32, flags: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
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
fn sys_pipe2<'a>(pipefd_uaddr: u64, flags: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let (reader_cap, writer_cap) = match tx_subsystems::pipe::step_pipe2(pipe_flags) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
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
fn sys_lseek<'a>(
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
    match file.step_lseek(offset, whence, &guard) {
        StepOutcome::Done(new_offset) | StepOutcome::Advanced(new_offset) => {
            SyscallResult::Return(new_offset as i64)
        }
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
            // Unreachable in practice — see the comment on the
            // function header.
            SyscallResult::Error(errno_to_i32(Errno::EIO))
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
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
fn make_ioctl_caller(ctx: &SyscallCtx<'_>) -> IoctlCaller {
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
fn sys_ioctl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let request = args[1] as u32;
    let argp = args[2];

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // Resolve to a TTY. Non-TTY fds → -ENOTTY for terminal-shape ioctls
    // (Linux semantic — even pipes / regular files return ENOTTY for
    // these requests, per `man ioctl_tty`).
    let tty = match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        _ => return SyscallResult::Error(errno_to_i32(Errno::ENOTTY)),
    };

    match request {
        TCGETS => {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tcgets(&tty, &guard)
            };
            match outcome {
                StepOutcome::Done(termios) | StepOutcome::Advanced(termios) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<Termios>(&ctx.aspace, argp, termios)
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
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
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tcsets(&tty, new_termios, &guard)
            };
            match outcome {
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
            }
        }
        TIOCGPGRP => {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocgpgrp(&tty, &guard)
            };
            match outcome {
                StepOutcome::Done(pgid) | StepOutcome::Advanced(pgid) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, argp, pgid) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
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
            match outcome {
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
            }
        }
        TIOCGWINSZ => {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocgwinsz(&tty, &guard)
            };
            match outcome {
                StepOutcome::Done(ws) | StepOutcome::Advanced(ws) => {
                    if argp == 0 {
                        return SyscallResult::Error(EFAULT_VALUE);
                    }
                    if let Err(errno) = bootstrap_write_user::<Winsize>(&ctx.aspace, argp, ws) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
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
            match outcome {
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
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
            match outcome {
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
            }
        }
        TIOCNOTTY => {
            let caller = make_ioctl_caller(ctx);
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_ioctl_tiocnotty(&tty, caller, &guard)
            };
            match outcome {
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    SyscallResult::Error(errno_to_i32(Errno::EIO))
                }
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

// =====================================================================
// Slice 2 of the shell-prompt roadmap — VM syscalls.
//
// `mmap` / `munmap` / `mprotect` / `mremap` / `madvise` / `msync` are
// pure plumbing on top of the VM execution primitives landed earlier
// (`AddressSpace::try_mmap`, `try_munmap`, `try_mprotect`, `try_mremap`,
// `madvise`, `msync`). The arms below decode the Linux PROT_* / MAP_* /
// MADV_* / MREMAP_* / MS_* flag bytes into the subsystem-shared `Prot` /
// `VmEntryFlags` / `MapPlacement` / `MadviseAdvice` / `VmBacking`
// shapes, build the request value, and dispatch — there is no
// step-loop discipline because the inner VM steps are themselves
// synchronous (msync is the lone exception, awaiting `step_fsync` per
// File-backed page container).
//
// User-VA discipline: `addr` (mmap/munmap/mprotect/madvise) is an
// integer hint, not a dereferenced pointer; msync's `addr/length`
// shape only walks existing recipes and never dereferences the user
// VA either. No user-VA copy lane is invoked from these arms.
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 2.
// =====================================================================

/// `mmap(addr, length, prot, flags, fd, offset)` — Linux RV64 generic
/// syscall #222.
///
/// Decodes Linux `PROT_*` / `MAP_*` into the subsystem-shared `Prot` /
/// `VmEntryFlags` shapes, builds a `VmBacking` (`PrivateAnon` for
/// `MAP_ANONYMOUS`; `Page { pc, offset }` for file-backed via the fd's
/// resolved `RNodeBacking::PageBacked`), and dispatches to
/// `AddressSpace::try_mmap`. Returns the chosen user VA on success
/// (matches Linux's `void *mmap(...)` shape — caller treats negative
/// returns as `-errno`).
///
/// Slice 2 scope (2026-05-07):
///
/// - `PROT_NONE` is the zero pattern (`Prot::NONE`); `PROT_GROWSDOWN` /
///   `PROT_GROWSUP` are recognised but return `-ENOSYS` (the underlying
///   `Prot` value type has no equivalent and they pair with
///   `MAP_GROWSDOWN`, which is rare in practice).
/// - Exactly one of `MAP_SHARED` / `MAP_PRIVATE` is required.
/// - `MAP_FIXED` → `MapPlacement::FixedReplace` (silently overwrites).
/// - `MAP_FIXED_NOREPLACE` → `MapPlacement::RequireFree` at the
///   requested addr; overlap returns `-EEXIST`.
/// - `MAP_ANONYMOUS` without `MAP_PRIVATE | MAP_SHARED` rejects
///   (`-EINVAL`). With either, the backing is `VmBacking::PrivateAnon`
///   regardless of shared/private (Slice 2 does not yet model shared
///   anon as distinct).
/// - `MAP_HUGETLB` / `MAP_LOCKED` / `MAP_POPULATE` / `MAP_STACK` etc.
///   are recognised but ignored (best-effort hints).
/// - File-backed mmap requires `fd` to resolve to an `OpenFile` whose
///   rnode is `RNodeBacking::PageBacked` — TTY / pipe / chardev /
///   directory / symlink → `-ENODEV`.
fn sys_mmap<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let prot_bits = args[2];
    let flags = args[3];
    let fd = args[4] as i32;
    let offset = args[4 + 1]; // args[5]

    // Length validation. Linux rounds the byte length up to a whole
    // page; addr (when MAP_FIXED is set) must already be page-aligned.
    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Decode `prot`. Only the documented bits are accepted; anything
    // else is `-EINVAL`. PROT_GROWSDOWN/GROWSUP recognised but
    // unsupported (`-ENOSYS`) — the underlying `Prot` shape has no
    // equivalent.
    let prot_recognised = PROT_READ | PROT_WRITE | PROT_EXEC | PROT_NONE | PROT_GROWSDOWN | PROT_GROWSUP;
    if prot_bits & !prot_recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if prot_bits & (PROT_GROWSDOWN | PROT_GROWSUP) != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    let prot = Prot::new(
        prot_bits & PROT_READ != 0,
        prot_bits & PROT_WRITE != 0,
        prot_bits & PROT_EXEC != 0,
    );

    // Decode `flags`. Exactly one of MAP_SHARED / MAP_PRIVATE required.
    let private = flags & MAP_PRIVATE != 0;
    let shared = flags & MAP_SHARED != 0;
    if private == shared {
        // both unset, or both set
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let fixed = flags & MAP_FIXED != 0;
    let fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    let anonymous = flags & MAP_ANONYMOUS != 0;
    let entry_flags = VmEntryFlags::new(
        shared,
        flags & MAP_GROWSDOWN != 0,
        flags & MAP_LOCKED != 0,
    );

    // Build the backing.
    let backing = if anonymous {
        VmBacking::PrivateAnon
    } else {
        if fd < 0 {
            return SyscallResult::Error(EBADF_VALUE);
        }
        let file = match resolve_fd(&ctx.process, fd as u32) {
            Some(f) => f,
            None => return SyscallResult::Error(EBADF_VALUE),
        };
        match extract_page_container(&file) {
            Some(pc) => VmBacking::Page { pc, offset },
            None => return SyscallResult::Error(errno_to_i32(Errno::ENODEV)),
        }
    };

    // Build the request. MAP_FIXED → FixedReplace; MAP_FIXED_NOREPLACE
    // → RequireFree at the requested addr; otherwise Anywhere over the
    // full V1 user range.
    let request = if fixed || fixed_noreplace {
        if !UserVirtAddr::new(addr as usize).is_page_aligned() {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
            Ok(r) => r,
            Err(_) => return SyscallResult::Error(EINVAL_VALUE),
        };
        let placement = if fixed_noreplace {
            MapPlacement::RequireFree
        } else {
            MapPlacement::FixedReplace
        };
        VmMapRequest::fixed(range, placement, prot, entry_flags, backing)
    } else {
        let window = UserRange::full_user_v1();
        let page_count = length / USER_PAGE_SIZE;
        VmMapRequest::anywhere(window, page_count, prot, entry_flags, backing)
    };

    match ctx.aspace.try_mmap(request) {
        Ok(outcome) => SyscallResult::Return(outcome.range.start().as_usize() as i64),
        Err(error) => {
            // MAP_FIXED_NOREPLACE → AlreadyMapped maps to EEXIST per
            // Linux's distinct semantic for that flag.
            let errno = if fixed_noreplace && error == VmMapError::AlreadyMapped {
                errno_to_i32(Errno::EEXIST)
            } else {
                vmmap_error_to_i32(error)
            };
            SyscallResult::Error(errno)
        }
    }
}

/// `munmap(addr, length)` — Linux RV64 generic syscall #215.
///
/// `addr` must be page-aligned and `length` is rounded up to a whole
/// page (matching Linux). Wraps `AddressSpace::try_munmap`.
fn sys_munmap<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    match ctx.aspace.try_munmap(range) {
        Ok(_commit) => SyscallResult::Return(0),
        Err(error) => SyscallResult::Error(vmmap_error_to_i32(error)),
    }
}

/// `mprotect(addr, length, prot)` — Linux RV64 generic syscall #226.
///
/// Wraps `AddressSpace::try_mprotect`. PROT_GROWSDOWN/GROWSUP not
/// supported (returns `-ENOSYS`); other prot validation matches
/// `sys_mmap`.
fn sys_mprotect<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let prot_bits = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let prot_recognised = PROT_READ | PROT_WRITE | PROT_EXEC | PROT_NONE | PROT_GROWSDOWN | PROT_GROWSUP;
    if prot_bits & !prot_recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if prot_bits & (PROT_GROWSDOWN | PROT_GROWSUP) != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    let prot = Prot::new(
        prot_bits & PROT_READ != 0,
        prot_bits & PROT_WRITE != 0,
        prot_bits & PROT_EXEC != 0,
    );

    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    match ctx.aspace.try_mprotect(range, prot) {
        Ok(_commit) => SyscallResult::Return(0),
        Err(error) => SyscallResult::Error(vmmap_error_to_i32(error)),
    }
}

/// `mremap(old_addr, old_size, new_size, flags, new_addr)` — Linux
/// RV64 generic syscall #216.
///
/// Slice 2 wraps `AddressSpace::try_mremap`, which only supports the
/// disjoint-range form (`old_range ∩ new_range == ∅`). The Linux
/// `MREMAP_FIXED | MREMAP_MAYMOVE` shape musl emits maps cleanly to
/// this contract; in-place grow without `MAYMOVE` would need
/// `try_mremap`'s contract extended (deferred).
fn sys_mremap<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let old_addr = args[0];
    let old_size_in = args[1] as usize;
    let new_size_in = args[2] as usize;
    let _flags = args[3];
    let new_addr = args[4];

    if old_size_in == 0 || new_size_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let old_size = match old_size_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let new_size = match new_size_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(old_addr as usize).is_page_aligned()
        || !UserVirtAddr::new(new_addr as usize).is_page_aligned()
    {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let old_range = match UserRange::new_aligned(UserVirtAddr::new(old_addr as usize), old_size) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    let new_range = match UserRange::new_aligned(UserVirtAddr::new(new_addr as usize), new_size) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    match ctx.aspace.try_mremap(VmRemapRequest::new(old_range, new_range)) {
        Ok(outcome) => SyscallResult::Return(outcome.new_range.start().as_usize() as i64),
        Err(error) => SyscallResult::Error(vmmap_error_to_i32(error)),
    }
}

/// `madvise(addr, length, advice)` — Linux RV64 generic syscall #233.
///
/// Slice 2 honours `MADV_NORMAL` / `RANDOM` / `SEQUENTIAL` / `WILLNEED`
/// (all observation-only no-ops in `AddressSpace::madvise`) and
/// `MADV_DONTNEED` / `MADV_FREE` (range-scoped pmap teardown). Other
/// advice values return `-ENOSYS` — the underlying `MadviseAdvice`
/// enum has no slot for them.
fn sys_madvise<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let advice_raw = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let advice = match advice_raw {
        MADV_NORMAL => MadviseAdvice::Normal,
        MADV_RANDOM => MadviseAdvice::Random,
        MADV_SEQUENTIAL => MadviseAdvice::Sequential,
        MADV_WILLNEED => MadviseAdvice::WillNeed,
        MADV_DONTNEED => MadviseAdvice::DontNeed,
        MADV_FREE => MadviseAdvice::Free,
        _ => return SyscallResult::Error(ENOSYS_VALUE),
    };
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    match ctx.aspace.madvise(range, advice) {
        Ok(()) => SyscallResult::Return(0),
        Err(error) => SyscallResult::Error(vmmap_error_to_i32(error)),
    }
}

/// `msync(addr, length, flags)` — Linux RV64 generic syscall #227.
///
/// The lone async VM arm: `AddressSpace::msync` calls into
/// `step_fsync` per File-backed page container, which can return
/// `Blocked` against the page-cache wait carrier. The arm awaits the
/// carrier and re-polls until `step_fsync` reaches `Done`.
///
/// `flags` (`MS_ASYNC` / `MS_SYNC` / `MS_INVALIDATE`) is recognised
/// but ignored — Slice 2 always behaves as `MS_SYNC` (synchronous
/// flush) and never invalidates non-flushed cache state.
async fn sys_msync<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let _flags = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Loop on the canonical wait-carrier discipline mirroring
    // `sys_write` — fresh epoch guard inside the call site, never
    // crossing an `.await`.
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            ctx.aspace.msync(range, &guard)
        };
        match outcome {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                return SyscallResult::Return(0);
            }
            StepOutcome::AdvancedThenBlocked((), token) | StepOutcome::Blocked(token) => {
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
                // Otherwise re-poll immediately.
            }
            StepOutcome::Err(errno) => {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

/// Resolve an `OpenFile` to its underlying `Cap<PageContainer>` if
/// the rnode backing is `RNodeBacking::PageBacked`. TTY / pipe /
/// chardev / directory / symlink rnodes return `None`; the caller
/// surfaces this as `-ENODEV` per Linux's `mmap(2)` errno surface for
/// unsupported file types.
fn extract_page_container(
    file: &Cap<OpenFile>,
) -> Option<Cap<tx_subsystems::page_backed::PageContainer>> {
    use tx_subsystems::vfs::structure::RNodeBacking;
    match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => Some(pc.clone()),
        _ => None,
    }
}

/// Translate `VmMapError` into a Linux RV64 generic ABI errno
/// magnitude. Slice 2 mapping:
///
/// - `AlreadyMapped` → `EEXIST` (17). Real Linux returns `EEXIST`
///   only for `MAP_FIXED_NOREPLACE`; other shapes silently replace.
///   `sys_mmap` overrides this for non-`FIXED_NOREPLACE` paths
///   before calling here, but `try_mmap`'s contract still surfaces
///   `AlreadyMapped` for the `RequireFree` placement, so EEXIST is
///   the right magnitude for the error-class on its own.
/// - `InvalidRange` → `EINVAL` (22).
/// - `MissingMapping` → `EINVAL` (22). Linux's `munmap` returns 0
///   for an unmapped region; `try_munmap`'s `MissingMapping` is the
///   "fully-disjoint range" shape that `mprotect` / `madvise` /
///   `mremap` need to flag.
/// - `NoFreeRange` → `ENOMEM` (12). Linux's "no usable address
///   range" magnitude.
/// - `WouldBlock` → `EAGAIN` (11). Slice 2 only reaches this for
///   in-flight VM contention; the synchronous arms surface the
///   raw `WouldBlock` rather than spinning.
/// - `BackingOffsetOverflow` → `EINVAL` (22). Bad offset arithmetic.
/// - `Pmap(_)` → `EIO` (5). Catch-all for the lower-level pmap
///   error variants; never produced by the `try_*` step's
///   non-async lane in practice.
fn vmmap_error_to_i32(error: VmMapError) -> i32 {
    match error {
        VmMapError::AlreadyMapped => errno_to_i32(Errno::EEXIST),
        VmMapError::InvalidRange => EINVAL_VALUE,
        VmMapError::MissingMapping => EINVAL_VALUE,
        VmMapError::NoFreeRange => errno_to_i32(Errno::ENOMEM),
        VmMapError::WouldBlock => EAGAIN_VALUE,
        VmMapError::BackingOffsetOverflow => EINVAL_VALUE,
        VmMapError::Pmap(_) => errno_to_i32(Errno::EIO),
    }
}

/// Required for the `UserRangeError` side of the VM-arm error
/// surface; centralised here so each arm doesn't repeat the match.
#[allow(dead_code)]
fn user_range_error_to_i32(_error: UserRangeError) -> i32 {
    EINVAL_VALUE
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)` — Linux RV64
/// generic syscall #98.
///
/// Slice 3 of the shell-prompt roadmap (2026-05-07). Required for
/// musl libc init: musl uses futex internally for `pthread_once`-
/// style guards even in single-threaded programs, and would otherwise
/// trip on `-ENOSYS` within the first few thousand instructions of
/// `__init_libc`.
///
/// v1 supports `FUTEX_WAIT` and `FUTEX_WAKE` only; other ops
/// (`REQUEUE`, `CMP_REQUEUE`, `WAKE_OP`, `LOCK_PI`, `WAIT_BITSET`
/// etc.) return `-ENOSYS`. `FUTEX_PRIVATE_FLAG` and
/// `FUTEX_CLOCK_REALTIME` flag bits are accepted but ignored —
/// per-process isolation is implicit (each process has its own
/// aspace and the user word at `uaddr` lives in that aspace), and
/// timeout support is deferred to Slice 4 with the timer-wait
/// carrier. The `timeout` (args[3]), `uaddr2` (args[4]), and
/// `val3` (args[5]) arguments are ignored in v1.
///
/// **`FUTEX_WAIT` semantics.** Loops on the canonical wait-carrier
/// discipline:
///
/// 1. Take a fresh `epoch::guard()` and call `step_futex_wait`.
/// 2. `Blocked(token)` → set `parked = true`, `await` the wait
///    future, loop back to (1).
/// 3. `Done(())` / `Advanced(())` → return `0`.
/// 4. `Err(EAGAIN)` → distinguishes "first-call mismatch" (return
///    `-EAGAIN` to userspace per Linux) from "post-wake re-check
///    showed the word changed" (return `0` per the WAIT contract)
///    via the `parked` flag tracked across loop iterations.
/// 5. Other `Err(errno)` → return `-errno`.
async fn sys_futex<'a>(args: [u64; 6], _ctx: &SyscallCtx<'a>) -> SyscallResult {
    let uaddr = args[0];
    let op_full = args[1] as u32;
    let val = args[2] as u32;
    // args[3] = timeout pointer (ignored — Slice 4 carryover).
    // args[4] = uaddr2 (REQUEUE-family only).
    // args[5] = val3 (BITSET-family only).

    let op = op_full & FUTEX_CMD_MASK;

    match op {
        FUTEX_WAIT => {
            // Track whether we've parked at least once. EAGAIN
            // from `step_futex_wait` means "user word != val". If
            // parked is false, this is the first-call mismatch
            // (return -EAGAIN). If parked is true, this is a
            // post-wake re-check showing the word changed (the
            // wake was meaningful — return 0).
            let mut parked = false;
            loop {
                let outcome = {
                    let guard = tx_substrate::epoch::guard();
                    tx_subsystems::futex::step_futex_wait(uaddr, val, &guard)
                };
                match outcome {
                    StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                        return SyscallResult::Return(0);
                    }
                    StepOutcome::AdvancedThenBlocked((), token)
                    | StepOutcome::Blocked(token) => {
                        parked = true;
                        if let Some(future) = wait_carrier::wait_on_token(token) {
                            let _ = future.await;
                        }
                        // Otherwise re-poll immediately (no
                        // registered carrier — should not happen
                        // for production-built tokens).
                        continue;
                    }
                    StepOutcome::Err(Errno::EAGAIN) => {
                        return if parked {
                            // Post-wake re-check showed the word
                            // changed; the wake was meaningful.
                            SyscallResult::Return(0)
                        } else {
                            // First-call mismatch — return -EAGAIN
                            // to userspace per Linux.
                            SyscallResult::Error(errno_to_i32(Errno::EAGAIN))
                        };
                    }
                    StepOutcome::Err(errno) => {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
            }
        }
        FUTEX_WAKE => {
            let n = val;
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                tx_subsystems::futex::step_futex_wake(uaddr, n, &guard)
            };
            match outcome {
                StepOutcome::Done(woken) | StepOutcome::Advanced(woken) => {
                    SyscallResult::Return(woken as i64)
                }
                StepOutcome::AdvancedThenBlocked(woken, _) => {
                    SyscallResult::Return(woken as i64)
                }
                StepOutcome::Blocked(_) => {
                    // FUTEX_WAKE is not a blocking op. The step
                    // never returns `Blocked` in practice; map to
                    // `EIO` defensively rather than panic.
                    SyscallResult::Error(EIO_VALUE)
                }
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        // FUTEX_REQUEUE / CMP_REQUEUE / WAKE_OP / LOCK_PI /
        // UNLOCK_PI / TRYLOCK_PI / WAIT_BITSET / WAKE_BITSET — out
        // of scope for v1. musl's libc init only emits FUTEX_WAIT
        // and FUTEX_WAKE so these are not on the critical path.
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

// ===========================================================================
// Slice 4 of the shell-prompt roadmap (2026-05-07) — time syscalls.
//
// `clock_gettime`, `gettimeofday`, `times` ship the read-side surface
// against `<P as TimeIf>::read_ns()`. All four POSIX clocks
// (`CLOCK_REALTIME` / `CLOCK_MONOTONIC` / `CLOCK_PROCESS_CPUTIME_ID`
// / `CLOCK_THREAD_CPUTIME_ID`) and their `*_RAW` / `*_COARSE` /
// `BOOTTIME` aliases route to the platform monotonic — no boot-time
// RTC offset and no per-process CPU-time accounting yet (TODOs at the
// constant declarations).
//
// `nanosleep` / `clock_nanosleep` ship the zero-duration / past-
// deadline short-circuit only. Real-duration sleeps need a per-task
// timer-fire wait carrier (i.e. a Channel attached to the reactor's
// TimerQueue, fired when `step_hart_loop_at`'s `advance_time_to` walks
// past the parked deadline). That wiring requires either exposing
// `Reactor::channel()` through `wait_carrier` (a tx-kernel ↔
// tx-subsystems plumbing change, since the BSP reactor lives in
// tx-kernel) or adding a global timer queue to tx-subsystems and
// driving it from the BSP loop. Both are out of scope for Slice 4 —
// busybox sh's syscall trace barely uses `nanosleep` and Slice 11's
// QEMU shell smoke can land without it. The deferred follow-up is
// tracked in `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md`.
//
// Per the dispatch convention, all writes flow through the
// `bootstrap_*` user-VA bridges (`bootstrap_write_user::<T>` for
// fixed-size structs, `bootstrap_copy_to_user` for byte buffers),
// which delegate to the canonical `aspace.write_user` /
// `aspace.copy_to_user` lane.
// ===========================================================================

/// Layout of a POSIX `struct timespec` written by `clock_gettime` /
/// read by `nanosleep` / `clock_nanosleep`. Field order and widths
/// match Linux's RV64 generic ABI (`include/uapi/linux/time.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct TimespecLayout {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Layout of a POSIX `struct timeval` written by `gettimeofday`. Field
/// order and widths match Linux's RV64 generic ABI.
#[repr(C)]
#[derive(Clone, Copy)]
struct TimevalLayout {
    tv_sec: i64,
    tv_usec: i64,
}

/// Layout of a POSIX `struct tms` written by `times(2)`. Slice 4
/// populates `tms_utime` with the monotonic tick count and zeros the
/// other three fields (no per-process system / child-time accounting
/// in v1 — `TODO(phase-cputime)`).
#[repr(C)]
#[derive(Clone, Copy)]
struct TmsLayout {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

/// Convert a nanosecond count to a Linux-shaped `(tv_sec, tv_nsec)`
/// pair. Both fields are signed 64-bit per the uapi.
fn ns_to_timespec(ns: u64) -> TimespecLayout {
    TimespecLayout {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    }
}

/// Convert a nanosecond count to a Linux-shaped `(tv_sec, tv_usec)`
/// pair (microsecond resolution — `gettimeofday` truncates the
/// sub-microsecond residue).
fn ns_to_timeval(ns: u64) -> TimevalLayout {
    TimevalLayout {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_usec: ((ns % 1_000_000_000) / 1_000) as i64,
    }
}

/// Read a Linux-shaped `(tv_sec, tv_nsec)` pair from user memory and
/// fold it back into a nanosecond count. Returns `None` if either
/// field is negative or `tv_nsec` overflows the canonical
/// `[0, 1_000_000_000)` range — those are the two `-EINVAL` cases
/// `nanosleep(2)` documents (`req->tv_nsec >= 1_000_000_000` or
/// either field negative).
///
/// Bridges through `bootstrap_read_user::<TimespecLayout>` for the
/// user-VA copy.
fn read_timespec_at(aspace: &AddressSpace, uaddr: u64) -> Option<u64> {
    if uaddr == 0 {
        return None;
    }
    let ts: TimespecLayout = match bootstrap_read_user::<TimespecLayout>(aspace, uaddr) {
        Ok(v) => v,
        Err(_) => return None,
    };
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return None;
    }
    Some((ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64))
}

/// `clock_gettime(clk_id, tp)`. Linux RV64 generic ABI
/// `__NR_clock_gettime = 113`.
///
/// Day-1 surface: every recognised clock id (REALTIME / MONOTONIC /
/// PROCESS_CPUTIME / THREAD_CPUTIME plus the *_RAW / *_COARSE /
/// BOOTTIME aliases) routes to `<P as TimeIf>::read_ns()`. Unknown
/// clock ids return `-EINVAL`. Null `tp` returns `-EFAULT`.
fn sys_clock_gettime<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let clk_id = args[0] as u32;
    let ts_uaddr = args[1];
    if ts_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let ns = match clk_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME => <P as TimeIf>::read_ns(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let ts = ns_to_timespec(ns);
    if let Err(errno) = bootstrap_write_user::<TimespecLayout>(&ctx.aspace, ts_uaddr, ts) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `gettimeofday(tv, tz)`. Linux RV64 generic ABI
/// `__NR_gettimeofday = 169`.
///
/// The `tz` argument (args[1]) is deprecated on Linux and ignored.
/// Null `tv` returns `-EFAULT`.
fn sys_gettimeofday<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let tv_uaddr = args[0];
    // args[1] = tz (ignored — deprecated on Linux).
    if tv_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let tv = ns_to_timeval(<P as TimeIf>::read_ns());
    if let Err(errno) = bootstrap_write_user::<TimevalLayout>(&ctx.aspace, tv_uaddr, tv) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `times(buf)`. Linux RV64 generic ABI `__NR_times = 153`.
///
/// Returns the monotonic tick count at `_SC_CLK_TCK = 100Hz`. Writes
/// `tms_utime = ticks` and zeros the other three fields when `buf` is
/// non-null. Null `buf` is permitted per Linux semantics — only the
/// return value matters in that case (LTP `times02` covers this).
fn sys_times<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    let ns = <P as TimeIf>::read_ns();
    let ticks = (ns / TIMES_NS_PER_TICK) as i64;
    if buf_uaddr != 0 {
        let tms = TmsLayout {
            tms_utime: ticks,
            tms_stime: 0,
            tms_cutime: 0,
            tms_cstime: 0,
        };
        if let Err(errno) = bootstrap_write_user::<TmsLayout>(&ctx.aspace, buf_uaddr, tms) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    SyscallResult::Return(ticks)
}

/// `nanosleep(req, rem)`. Linux RV64 generic ABI
/// `__NR_nanosleep = 101`.
///
/// **Slice 4 surface.** Validates `*req` (returns `-EINVAL` on
/// negative fields or `tv_nsec >= 1_000_000_000`); short-circuits to
/// `Return(0)` on a zero-duration request. Real non-zero durations
/// return `-ENOSYS` — the per-task timer-fire wait carrier needed for
/// proper park-until-deadline semantics is deferred (see the slice
/// header comment). Null `req` returns `-EFAULT`.
///
/// `rem` (args[1]) is currently ignored — only the EINTR-with-leftover
/// path needs to populate it, and the slice does not yet have signal
/// interruption of nanosleep wired.
fn sys_nanosleep<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let req_uaddr = args[0];
    // args[1] = rem (ignored — no EINTR path in Slice 4).
    let req_ns = match read_timespec_at(&ctx.aspace, req_uaddr) {
        Some(ns) => ns,
        None if req_uaddr == 0 => return SyscallResult::Error(EFAULT_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if req_ns == 0 {
        return SyscallResult::Return(0);
    }
    // Real-duration sleeps deferred — see slice header. busybox sh
    // does not exercise this on the critical path, so returning
    // -ENOSYS keeps the contract honest while the timer-channel
    // wiring lands in a follow-up slice.
    SyscallResult::Error(ENOSYS_VALUE)
}

/// `clock_nanosleep(clk_id, flags, req, rem)`. Linux RV64 generic ABI
/// `__NR_clock_nanosleep = 115`.
///
/// **Slice 4 surface.** Same deferral as `nanosleep`: the
/// zero-duration / past-deadline short-circuit ships, real
/// non-zero-future deadlines return `-ENOSYS`. Honours
/// `TIMER_ABSTIME` for the past-deadline check (when set, `req`
/// is interpreted as an absolute deadline — past deadlines short-
/// circuit immediately to `Return(0)`).
///
/// Recognised clock ids match `clock_gettime`. Unknown clock ids and
/// unknown flag bits return `-EINVAL`. Null `req` returns `-EFAULT`.
fn sys_clock_nanosleep<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let clk_id = args[0] as u32;
    let flags = args[1] as u32;
    let req_uaddr = args[2];
    // args[3] = rem (ignored — no EINTR path in Slice 4).

    match clk_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME => {}
        _ => return SyscallResult::Error(EINVAL_VALUE),
    }
    if (flags & !TIMER_ABSTIME) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let req_ns = match read_timespec_at(&ctx.aspace, req_uaddr) {
        Some(ns) => ns,
        None if req_uaddr == 0 => return SyscallResult::Error(EFAULT_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let now = <P as TimeIf>::read_ns();
    let deadline_ns = if (flags & TIMER_ABSTIME) != 0 {
        req_ns
    } else {
        now.saturating_add(req_ns)
    };
    if now >= deadline_ns {
        return SyscallResult::Return(0);
    }
    // Real-duration sleeps deferred — see `sys_nanosleep`.
    SyscallResult::Error(ENOSYS_VALUE)
}

// =====================================================================
// Slice 6 of the shell-prompt roadmap — stat family
// (`fstat` / `newfstatat` / `getdents64` / `getcwd` / `chdir` /
// `fchdir` / `umask`).
//
// These arms unblock four shell-startup-blocking surfaces:
//   - `ls` calls `getdents64(fd)` to enumerate directory contents.
//   - `pwd` calls `getcwd(buf, size)` to render the cwd.
//   - `cd` calls `chdir(path)` to change the cwd.
//   - musl's shell startup calls `fstat(0)` / `fstat(1)` / `fstat(2)`
//     to decide interactive mode.
//
// Carryovers documented at the constants in `numbers.rs`:
//   - `fchdir` returns `-ENOSYS` (OpenFile carries `Cap<RNode>`, not
//     `Cap<DEntry>` — no path-edge to install via `step_chdir`).
//   - `AT_SYMLINK_NOFOLLOW` accepted but ignored (the walker always
//     follows symlinks at resolution time today).
//
// User-VA discipline: buffer pointers flow through the `bootstrap_*`
// user-VA bridges (`bootstrap_write_user::<StatLayout>` for `fstat` /
// `newfstatat`; `bootstrap_copy_to_user` for `getcwd` /
// `getdents64` byte streams), which delegate to the canonical
// `aspace.write_user` / `aspace.copy_to_user` lane.
// =====================================================================

/// Linux RV64 generic ABI `struct stat` layout (matches `struct stat64`
/// — the RV64 generic ABI ships one shape for both `stat` and
/// `fstat64`). Source of truth: `arch/riscv/include/uapi/asm/stat.h`
/// pulls in `asm-generic/stat.h`. Field order and padding are
/// load-bearing; the dispatcher writes the byte image into the user
/// buffer via `write_volatile`.
#[repr(C)]
#[derive(Clone, Copy)]
struct StatLayout {
    st_dev: u64,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    __pad1: u64,
    st_size: i64,
    st_blksize: i32,
    __pad2: i32,
    st_blocks: i64,
    st_atime_sec: i64,
    st_atime_nsec: u64,
    st_mtime_sec: i64,
    st_mtime_nsec: u64,
    st_ctime_sec: i64,
    st_ctime_nsec: u64,
    __unused: [u32; 2],
}

/// Fixed header of the Linux `linux_dirent64` record produced by
/// `getdents64(2)`. The record is followed by a NUL-terminated `d_name`
/// byte string and zero padding to align the next record on an 8-byte
/// boundary.
///
/// Header size = `8 + 8 + 2 + 1 = 19` bytes; total record =
/// `align_up(19 + name_len + 1, 8)`. Source: linux uapi
/// `include/uapi/linux/dirent.h`.
#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxDirent64Header {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
}

/// Fixed header byte size of `linux_dirent64`. Used by `sys_getdents64`
/// to compute the trailing name-and-pad offset.
const LINUX_DIRENT64_HEADER_BYTES: usize = 19;

/// Default `st_blksize` reported by Slice 6's stat arms. Linux's
/// page-backed filesystems all report 4096; txKernel has no
/// per-FS blocksize hint to override this with today.
const STAT_BLKSIZE: i32 = 4096;

/// Round `x` up to the nearest multiple of 8. Used by `getdents64` to
/// pad records to the 8-byte boundary the ABI requires.
const fn align_up_8(x: usize) -> usize {
    (x + 7) & !7
}

/// Project an `InodeKind` onto the `linux_dirent64` `d_type` byte. The
/// match exhausts every variant of the enum (verified from
/// `vfs::structure::InodeKind`).
const fn inode_kind_to_dt(kind: InodeKind) -> u8 {
    match kind {
        InodeKind::Regular => DT_REG,
        InodeKind::Directory => DT_DIR,
        InodeKind::Symlink => DT_LNK,
        InodeKind::CharDevice => DT_CHR,
        InodeKind::BlockDevice => DT_BLK,
        InodeKind::Fifo => DT_FIFO,
        InodeKind::Socket => DT_SOCK,
    }
}

/// Map an `InodeMeta` + (`fs_object_id`, `rdev`) pair onto the Linux
/// `struct stat` byte image. Single-device kernel today
/// (`st_dev = 0`); `st_blksize = 4096` is the universal page size on
/// the platforms txKernel supports. `rdev` is `0` for non-device
/// inodes; future device-fs work can plumb the major/minor encoding
/// through this argument.
fn inode_meta_to_stat(meta: &InodeMeta, ino: u64, rdev: u64) -> StatLayout {
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
fn sys_fstat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
async fn sys_newfstatat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            poll_walker_synchronously(step_walk(cwd, &path, &walker_cred, &guard))
        };
        match outcome {
            StepOutcome::Done(d) | StepOutcome::Advanced(d) => d,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
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
async fn sys_chdir<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let path_uaddr = args[0];

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
    let walker_cred = ctx.walker_cred();
    let dentry: Cap<DEntry> = {
        let guard = tx_substrate::epoch::guard();
        let outcome = poll_walker_synchronously(step_walk(cwd, &path, &walker_cred, &guard));
        drop(guard);
        match outcome {
            StepOutcome::Done(d) | StepOutcome::Advanced(d) => d,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };

    if dentry.rnode().meta().kind() != InodeKind::Directory {
        return SyscallResult::Error(ENOTDIR_VALUE);
    }

    match step_chdir(&ctx.process, dentry) {
        ChdirOutcome::Replaced { .. } => SyscallResult::Return(0),
        ChdirOutcome::ZombieIgnored => SyscallResult::Error(ESRCH_VALUE),
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
fn sys_getcwd<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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

    let path = match step_getcwd(&ctx.process) {
        Some(p) => p,
        None => return SyscallResult::Error(ENOENT_VALUE),
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
        return SyscallResult::Error(errno_to_i32(errno));
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
fn sys_umask<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let new_mask = args[0] as u16;
    let old = ctx.process.swap_umask(new_mask);
    SyscallResult::Return(old as i64)
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
/// require shifting to the `wait_carrier::wait_on_token` pattern; the
/// arm panics defensively on `Blocked` / `AdvancedThenBlocked` per the
/// `Guard` send-future discipline (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
async fn sys_getdents64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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

    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            fs_ops.readdir(dir_fs_object_id, cursor, &guard)
        };
        match outcome {
            StepOutcome::Done(Some((entry, next_cursor)))
            | StepOutcome::Advanced(Some((entry, next_cursor))) => {
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
                record[LINUX_DIRENT64_HEADER_BYTES
                    ..LINUX_DIRENT64_HEADER_BYTES + name_bytes.len()]
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
            StepOutcome::Done(None) | StepOutcome::Advanced(None) => {
                // End of directory — durable cursor advance is
                // unnecessary (the readdir backend's cursor is
                // self-terminating).
                break;
            }
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                // No in-tree backend produces these. Surface as
                // `-EIO` defensively if the partial-progress shape
                // ever fires.
                if written > 0 {
                    return SyscallResult::Return(written as i64);
                }
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => {
                if written > 0 {
                    return SyscallResult::Return(written as i64);
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }

    SyscallResult::Return(written as i64)
}

// =====================================================================
// Slice 7 of the shell-prompt roadmap — fcntl extension + day-1 misc
// syscalls (`getpgrp` / `kill` / `tkill` / `tgkill` / `getrandom` /
// `uname` / `prlimit64` / `rt_sigreturn`). Each is a small, isolated
// arm that unblocks a specific shell-startup path. F_DUPFD /
// F_DUPFD_CLOEXEC / F_GETFL extensions to fcntl live inside `sys_fcntl`
// itself (see above). See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 7.
// =====================================================================

/// `getpgrp()` — Linux RV64 generic ABI `__NR_getpgrp = 81`.
///
/// glibc-only legacy call: glibc emulates `getpgrp()` as `getpgid(0)`.
/// musl uses `getpgid(0)` directly and never issues this number, but
/// shipping a real implementation is cheap and removes a startup
/// `-ENOSYS` from any glibc-built binary that lands later.
fn sys_getpgrp<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.pgrp_cap().pgid.0 as i64)
}

/// `kill(pid, sig)` — Linux RV64 generic ABI `__NR_kill = 129`.
///
/// Slice 7 v1 surface:
/// - `pid > 0`: deliver `sig` to the matching process via
///   `tx_subsystems::signal::step_kill_process`. Resolved through
///   `process_by_pid`'s init-rooted tree walk.
/// - `pid <= 0`: pgrp / all-processes targets — out of scope for v1
///   (`-ENOSYS`; needs a global pid-to-pgrp lookup the slice does
///   not yet wire).
/// - `sig == 0`: existence probe — return `0` if the target exists
///   (live or zombie), `-ESRCH` otherwise. Linux semantic.
/// - Unknown signum (outside 1..=64): `-EINVAL`.
/// - Target zombie / no live thread: `-ESRCH` (matches Linux's
///   "kill returns ESRCH if no signal could be delivered").
fn sys_kill(args: [u64; 6]) -> SyscallResult {
    let pid = args[0] as i32;
    let sig = args[1] as u32;

    if pid <= 0 {
        // TODO(phase-pgrp-kill): pgrp-targeted (`pid < 0` /
        // `pid == 0` / `pid == -1`) kills need a global pid-to-pgrp
        // lookup the slice does not yet wire.
        return SyscallResult::Error(ENOSYS_VALUE);
    }

    let target = match process_by_pid(Pid(pid as u32)) {
        Some(t) => t,
        None => return SyscallResult::Error(ESRCH_VALUE),
    };

    if sig == 0 {
        // Existence probe: 0 for live or zombie targets, -ESRCH for
        // missing (handled above by the `process_by_pid` None branch).
        return SyscallResult::Return(0);
    }

    // Bound check: Linux signums are 1..=64 (the realtime range
    // shares the same encoding as `Signum`). Anything outside that
    // is `-EINVAL`.
    let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
        Some(s) => s,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    match step_kill_process(&target, signum) {
        KillOutcome::Delivered => SyscallResult::Return(0),
        KillOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
    }
}

/// `tkill(tid, sig)` — Linux RV64 generic ABI `__NR_tkill = 130`.
///
/// Slice 7 v1 aliases this to [`sys_kill`]: txKernel has no
/// per-thread signal state machine yet, so `tkill(tid, sig)` is
/// treated as `kill(tid, sig)` (the tid is interpreted as a pid).
/// `TODO(phase-thread-signals)`.
fn sys_tkill(args: [u64; 6]) -> SyscallResult {
    sys_kill(args)
}

/// `tgkill(tgid, tid, sig)` — Linux RV64 generic ABI
/// `__NR_tgkill = 131`.
///
/// Slice 7 v1 aliases this to [`sys_kill`]: `tgid` (args[0]) is
/// interpreted as a pid, `tid` (args[1]) is ignored, and `sig`
/// (args[2]) is shifted to the kill arg slot.
/// `TODO(phase-thread-signals)`.
fn sys_tgkill(args: [u64; 6]) -> SyscallResult {
    let mut k_args = args;
    // sys_kill expects (pid, sig) at args[0]/args[1]. tgkill places
    // sig at args[2]; shift it down for the alias.
    k_args[1] = args[2];
    sys_kill(k_args)
}

/// `getrandom(buf, buflen, flags)` — Linux RV64 generic ABI
/// `__NR_getrandom = 278`.
///
/// Slice 7 v1: fills `buflen` bytes at `buf` from
/// `<P as EntropyIf>::fill_random`. The `flags` arg is recognised
/// (`GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE`) but ignored — the
/// in-tree default impl is deterministic + non-blocking.
///
/// User-VA writeback flows through `bootstrap_copy_to_user`
/// (canonical `aspace.copy_to_user` lane with kernel-pointer fallback
/// for test scaffolding). Null `buf` with non-zero `buflen` returns
/// `-EFAULT`; `buflen == 0` is a successful no-op (`Return(0)`).
fn sys_getrandom<'a, P: EntropyIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    let buf_len = args[1] as usize;
    let _flags = args[2] as u32; // GRND_* recognised but ignored.

    if buf_len == 0 {
        return SyscallResult::Return(0);
    }
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    // Fill into a temporary kernel buffer, then copy out through the
    // canonical user-VA lane.
    let mut tmp = alloc::vec![0u8; buf_len];
    <P as EntropyIf>::fill_random(&mut tmp);
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &tmp) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(buf_len as i64)
}

/// `uname(buf)` — Linux RV64 generic ABI `__NR_uname = 160`.
///
/// Writes a static utsname (`sysname` / `nodename` / `release` /
/// `version` / `machine` / `domainname`) to `buf`. Each field is a
/// `[u8; 65]` NUL-padded string. Slice 7 pins:
///
/// - `sysname = "Linux"` so musl's runtime "is this Linux?" probe
///   succeeds.
/// - `release = "6.1.0-txkernel"` so the version-triple parser at the
///   front of the string sees a Linux 2.6.16+ kernel (musl's
///   kernel-feature gating reads only the leading digits).
/// - `machine = "riscv64"` matching the target ABI.
///
/// SAFETY: kernel-buffer exemption (mirrors `sys_getresuid`).
fn sys_uname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let utsname = build_utsname();
    if let Err(errno) = bootstrap_write_user::<UtsnameLayout>(&ctx.aspace, buf_uaddr, utsname) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `prlimit64(pid, resource, new_rlim, old_rlim)` — Linux RV64
/// generic ABI `__NR_prlimit64 = 261`.
///
/// Slice 7 v1: read-only static rlimit table for the calling process.
/// `pid == 0` or `pid == self.pid` is the only supported target;
/// cross-pid queries return `-EPERM`. `new_rlim` is silently ignored
/// — limits are not actually enforced by any in-tree subsystem yet
/// (`TODO(phase-rlimit-enforcement)`). The static table is generous
/// (`RLIMIT_NOFILE = (1024, 4096)`, `RLIMIT_STACK = 8 MiB`, the rest
/// `RLIM_INFINITY`).
///
/// Unknown resource ids return `-EINVAL`. Null `old_rlim` is OK (the
/// arm just reports back via the return value) — Linux only requires
/// the writeback when `old_rlim` is non-null.
fn sys_prlimit64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as u32;
    let resource = args[1] as u32;
    let _new_uaddr = args[2]; // ignored — limits not enforced today.
    let old_uaddr = args[3];

    if pid != 0 && pid != ctx.process.pid.0 {
        // TODO(phase-pid-resolver): cross-pid prlimit64 once a global
        // pid → Cap<ProcessIdentity> table is wired.
        return SyscallResult::Error(EPERM_VALUE);
    }

    let limit = match resource {
        RLIMIT_NOFILE => RlimitLayout {
            rlim_cur: 1024,
            rlim_max: 4096,
        },
        RLIMIT_STACK => RlimitLayout {
            rlim_cur: 8 * 1024 * 1024,
            rlim_max: RLIM_INFINITY,
        },
        RLIMIT_CORE => RlimitLayout {
            rlim_cur: 0,
            rlim_max: RLIM_INFINITY,
        },
        RLIMIT_CPU
        | RLIMIT_FSIZE
        | RLIMIT_DATA
        | RLIMIT_RSS
        | RLIMIT_NPROC
        | RLIMIT_MEMLOCK
        | RLIMIT_AS
        | RLIMIT_LOCKS
        | RLIMIT_SIGPENDING
        | RLIMIT_MSGQUEUE
        | RLIMIT_NICE
        | RLIMIT_RTPRIO
        | RLIMIT_RTTIME => RlimitLayout {
            rlim_cur: RLIM_INFINITY,
            rlim_max: RLIM_INFINITY,
        },
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    if old_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<RlimitLayout>(&ctx.aspace, old_uaddr, limit) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    SyscallResult::Return(0)
}

/// `rt_sigreturn(...)` — Linux RV64 generic ABI
/// `__NR_rt_sigreturn = 139`.
///
/// **Slice 7 carryover.** Returns `-ENOSYS` for now. The
/// `SignalFrameIf::restore_signal_frame` / `read_signal_frame`
/// surface in `tx-hal` requires a `TrapFrameMut<'_>` on the live
/// trap frame and the user-stack pointer the kernel parked at
/// signal-frame setup time; the `SyscallCtx` shape does not yet
/// expose either. End-to-end wiring requires the trap-shell to invoke
/// `SignalFrameIf` directly (bypassing this dispatcher) or pass the
/// trap-frame pointer through the syscall context — both are out of
/// scope for Slice 7. Real signal handlers are also not yet wired
/// (no userspace handler trampoline path), so the carryover does not
/// block any day-1 shell flow. `TODO(phase-signal-frame)`.
fn sys_rt_sigreturn() -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

// ---------------------------------------------------------------------
// Layout structs for Slice 7 syscall arms.
// ---------------------------------------------------------------------

/// Linux uapi `struct utsname` field width (`__NEW_UTS_LEN + 1 = 65`).
const UTSNAME_FIELD: usize = 65;

/// Linux RV64 generic ABI `struct utsname` layout — six fields,
/// each `[u8; 65]` NUL-padded. The field count and width are fixed
/// across architectures (Linux's `<sys/utsname.h>`).
#[repr(C)]
#[derive(Clone, Copy)]
struct UtsnameLayout {
    sysname: [u8; UTSNAME_FIELD],
    nodename: [u8; UTSNAME_FIELD],
    release: [u8; UTSNAME_FIELD],
    version: [u8; UTSNAME_FIELD],
    machine: [u8; UTSNAME_FIELD],
    domainname: [u8; UTSNAME_FIELD],
}

fn build_utsname() -> UtsnameLayout {
    fn pad(s: &str) -> [u8; UTSNAME_FIELD] {
        let mut out = [0u8; UTSNAME_FIELD];
        let bytes = s.as_bytes();
        // Reserve the trailing NUL byte. `min(len, 64)` clamps the
        // copy so `out[64] = 0` always.
        let n = core::cmp::min(bytes.len(), UTSNAME_FIELD - 1);
        let (head, _) = out.split_at_mut(n);
        head.copy_from_slice(&bytes[..n]);
        out
    }
    UtsnameLayout {
        sysname: pad("Linux"),
        nodename: pad("txkernel"),
        // Linux 6.1.0 is the LTS line musl 1.2.x runtime probes treat
        // as fully featured.
        release: pad("6.1.0-txkernel"),
        version: pad("#1 SMP txkernel"),
        machine: pad("riscv64"),
        domainname: pad("(none)"),
    }
}

/// Linux uapi `struct rlimit64` layout — two `u64` fields. Used as
/// the writeback shape for `sys_prlimit64`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RlimitLayout {
    rlim_cur: u64,
    rlim_max: u64,
}

/// Resolve the `Arc<dyn FsOps>` in scope for a directory rnode.
/// Mirrors `fs_ops_for_dentry`'s shape but operates on the rnode
/// directly (the OpenFile carries `Cap<RNode>`, not `Cap<DEntry>`).
///
/// Returns `None` if the rnode does not carry a `containing_mount`
/// weak (descendant rnodes minted by `materialise_child_rnode` don't
/// — only mount-root rnodes do). The Slice 6 `getdents64` arm
/// surfaces this as `-ENOSYS` defensively (no FsOps to dispatch
/// through). In practice every tested directory is the mount root,
/// matching tmpfs's day-1 surface.
///
/// TODO(phase-readdir-mount): forward the mount weak to descendants
/// during `materialise_child_rnode` so this fallback is unnecessary.
fn fs_ops_for_rnode(
    rnode: &Cap<tx_subsystems::vfs::structure::RNode>,
) -> Option<Arc<dyn tx_subsystems::vfs::FsOps>> {
    let guard = tx_substrate::epoch::guard();
    let weak = rnode.containing_mount_weak()?;
    let payload = weak.upgrade(&guard)?;
    Some(payload.fs_ops.clone())
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

/// Walk `path` from `cwd` synchronously, returning the resolved
/// dentry or a positive-magnitude `-errno`. Mirrors
/// `resolve_path_at`'s shape but takes the cwd directly so callers
/// that already hold it (every Slice 8 arm fetches it once for both
/// the parent walk and the optional full walk) avoid the redundant
/// `process.cwd()` lookup.
fn walk_from(cwd: Cap<DEntry>, path: &[u8], cred: &Credential) -> Result<Cap<DEntry>, i32> {
    let guard = tx_substrate::epoch::guard();
    let outcome = poll_walker_synchronously(step_walk(cwd, path, cred, &guard));
    drop(guard);
    match outcome {
        StepOutcome::Done(d) | StepOutcome::Advanced(d) => Ok(d),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => Err(EIO_VALUE),
        StepOutcome::Err(errno) => Err(errno_to_i32(errno)),
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
async fn sys_mkdirat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    // Apply umask: effective_mode = mode & !umask. Linux semantics
    // (umask is the bottom 9 bits — `rwxrwxrwx`).
    let umask = ctx.process.umask();
    let effective_mode = mode & !umask & 0o7777;
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        fs_ops.mkdir(parent_id, basename, effective_mode, &cred, &guard)
    };
    match outcome {
        StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

/// `unlinkat(dirfd, pathname, flags)`. Linux RV64 generic ABI
/// `__NR_unlinkat = 35`.
///
/// Without `AT_REMOVEDIR` the arm dispatches through `FsOps::unlink`
/// (rejects directory targets with `-EISDIR`); with `AT_REMOVEDIR` it
/// dispatches through `FsOps::rmdir` (rejects non-directory targets
/// with `-ENOTDIR`).
async fn sys_unlinkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        if want_rmdir {
            fs_ops.rmdir(parent_id, basename, target_id, &guard)
        } else {
            fs_ops.unlink(parent_id, basename, target_id, &guard)
        }
    };
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

/// `symlinkat(target, newdirfd, linkpath)`. Linux RV64 generic ABI
/// `__NR_symlinkat = 36`.
///
/// `target` is the symlink's textual content (no path resolution).
/// `linkpath` is split into `(parent_path, basename)`; the parent is
/// walked, then `FsOps::symlink(parent, basename, target, &cred,
/// &guard)` is dispatched.
async fn sys_symlinkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        fs_ops.symlink(parent_id, basename, &target, &cred, &guard)
    };
    match outcome {
        StepOutcome::Done(_) | StepOutcome::Advanced(_) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
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
async fn sys_linkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let fs_ops = match fs_ops_for_dentry(&new_parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        fs_ops.link(new_parent_id, new_basename, source_id, &guard)
    };
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
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
async fn sys_truncate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
        let guard = tx_substrate::epoch::guard();
        tx_subsystems::page_backed::step_truncate(&pc, new_size, &guard)
    };
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
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
fn sys_ftruncate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        tx_subsystems::page_backed::step_truncate(&pc, new_size, &guard)
    };
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
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
async fn sys_readlinkat<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let fs_ops = match fs_ops_for_dentry(&parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };
    // Resolve the basename in the parent directly via `FsOps::lookup`
    // — bypasses the walker's symlink-chase loop so the symlink's
    // own inode (not its target's) is what we read.
    let target_id = {
        let guard = tx_substrate::epoch::guard();
        match fs_ops.lookup(parent_id, basename, &guard) {
            StepOutcome::Done(id) | StepOutcome::Advanced(id) => id,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };
    let target_meta = {
        let guard = tx_substrate::epoch::guard();
        match fs_ops.load_inode_meta(target_id, &guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };
    if target_meta.kind() != InodeKind::Symlink {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let link_bytes = {
        let guard = tx_substrate::epoch::guard();
        match fs_ops.read_link(target_id, &guard) {
            StepOutcome::Done(b) | StepOutcome::Advanced(b) => b,
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                return SyscallResult::Error(EIO_VALUE);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };
    let to_copy = core::cmp::min(link_bytes.len(), buf_len);
    if to_copy > 0 {
        if let Err(errno) =
            bootstrap_copy_to_user(&ctx.aspace, buf_uaddr, &link_bytes[..to_copy])
        {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    SyscallResult::Return(to_copy as i64)
}

/// `utimensat(dirfd, pathname, times, flags)`. Linux RV64 generic ABI
/// `__NR_utimensat = 88`.
///
/// Slice 8: returns `-ENOSYS`. The `FsOps` surface does not yet expose
/// a `set_times` hook (`InodeMeta` carries `atime`/`mtime`/`ctime`
/// fields, but the backend trait has no method to mutate them).
/// Most shells ignore `utimensat` failures — the deferred
/// implementation is documented under
/// `TODO(phase-vfs-utimens)` in the slice plan.
fn sys_utimensat<'a>(_args: [u64; 6], _ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
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
/// new_parent, new_name, &guard)`. The in-tree tmpfs surface only
/// supports same-directory rename today; cross-directory rename
/// surfaces as `-ENOSYS` from the backend.
async fn sys_renameat2<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    let cred = ctx.walker_cred();
    let (old_parent_path, old_basename) = split_path(&oldpath);
    let (new_parent_path, new_basename) = split_path(&newpath);
    if old_basename.is_empty() || new_basename.is_empty() {
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
    let new_parent_dentry = if new_parent_path.is_empty() {
        cwd.clone()
    } else {
        match walk_from(cwd.clone(), new_parent_path, &cred) {
            Ok(d) => d,
            Err(e) => return SyscallResult::Error(e),
        }
    };
    // RENAME_NOREPLACE pre-check: walk the full new path; if it
    // resolves, the rename must fail with -EEXIST (Linux semantic).
    if (flags & RENAME_NOREPLACE) != 0 {
        if walk_from(cwd, &newpath, &cred).is_ok() {
            return SyscallResult::Error(EEXIST_VALUE);
        }
    }
    let old_parent_id = old_parent_dentry.rnode().fs_object_id();
    let new_parent_id = new_parent_dentry.rnode().fs_object_id();
    let fs_ops = match fs_ops_for_dentry(&old_parent_dentry) {
        Some(o) => o,
        None => return SyscallResult::Error(EROFS_VALUE),
    };
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        fs_ops.rename(
            old_parent_id,
            old_basename,
            new_parent_id,
            new_basename,
            &guard,
        )
    };
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => SyscallResult::Return(0),
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            SyscallResult::Error(EIO_VALUE)
        }
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}
