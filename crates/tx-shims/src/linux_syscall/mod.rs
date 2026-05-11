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
use tx_subsystems::execution::Errno;
use tx_subsystems::process::{
    process_by_pid, seed_child_leader_context, step_chdir, step_exit_group, step_getcwd,
    step_setpgid, step_setsid, step_waitpid_nohang, ChdirOutcome, ExitStatus, Pgid, Pid,
    ProcessIdentity, SetpgidError, SetsidError, WaitError, WaitTarget,
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
mod io;
use io::*;
mod fs_basic;
use fs_basic::*;
mod fs_path;
use fs_path::*;
mod fs_mut;
use fs_mut::*;
mod proc;
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
use signalfd::*;

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
    NR_MKDIRAT, NR_MMAP, NR_MPROTECT, NR_MREMAP, NR_MSYNC, NR_MUNMAP, NR_NANOSLEEP, NR_NEWFSTATAT,
    NR_OPENAT, NR_PIPE2, NR_PPOLL, NR_PRLIMIT64, NR_READ, NR_READLINKAT, NR_READV, NR_RENAMEAT2,
    NR_RT_SIGACTION, NR_RT_SIGPROCMASK, NR_RT_SIGRETURN, NR_SETGID, NR_SETPGID, NR_SETREGID,
    NR_SETRESGID, NR_SETRESUID, NR_SETREUID, NR_SETSID, NR_SETUID, NR_SET_ROBUST_LIST,
    NR_SET_TID_ADDRESS, NR_SIGNALFD, NR_SIGNALFD4, NR_SYMLINKAT, NR_TGKILL, NR_TIMES, NR_TKILL,
    NR_TRUNCATE, NR_UMASK,
    NR_UNAME, NR_UNLINKAT, NR_USERFAULTFD, NR_UTIMENSAT, NR_WAIT4, NR_WRITE, NR_WRITEV, O_ACCMODE,
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
    /// `AtomicSlot<Cap<Cred>>` is loaded once (PR-9 phase 5 / D5 Path
    /// A — was `SpinMutex<Cred>`), the resulting cap is derefed to
    /// `&Cred`, and the value is copied out; the cap clone drops
    /// before return, so the snapshot is independent of the slot and
    /// safe to hold across `.await` points. Holding a reference
    /// through the cap would still be sound (the cap retain-count
    /// keeps the slab entry live), but callers that need the
    /// long-lived cap shape should use [`Self::cred_cap`] instead.
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

    /// Snapshot the current process's `Cap<Cred>`. Returns a cloned
    /// strong cap; the slab entry stays live until the cap drops.
    ///
    /// PR-9 phase 5 (D5 Path A): the cred-mutators
    /// (`step_setuid` / `step_setgid` / ...) replace the slot's
    /// inhabitant per call; this accessor reads whichever cap is
    /// current. Concurrent mutators between this read and the
    /// `SubjectContext` construction yield a cap pointing at the
    /// pre-mutation cred — the syscall arm sees a coherent snapshot
    /// for the duration of its script frame.
    ///
    /// Defensive fallback: zombies have no payload, so no cred-cap.
    /// In that case we mint a fresh `Cap<Cred>` from `Cred::root()`.
    /// Reaching this fallback inside a live syscall is impossible by
    /// construction (the calling process is by definition alive).
    pub fn cred_cap(&self) -> Cap<Cred> {
        self.process.cred_cap().unwrap_or_else(|| {
            tx_subsystems::cred::sign_cred(Cred::root())
                .expect("zone slab has capacity for defensive root cred")
        })
    }
}

/// PR-9 phase 5 (D5 Path A) — build a `KernelScriptCtx` whose subject
/// is populated from `SyscallCtx`. The four wired arms (sys_read,
/// sys_write, sys_pipe2, sys_clone) call this at script entry so the
/// step body receives `ctx.subject()` rather than `None`.
///
/// Restrictions cap is a fresh placeholder per call until PR-K lands
/// the real append-only stack (D5 §7). Each call mints one
/// `Cap<RestrictionStackHandle>` from the substrate placeholder zone;
/// the cap drops at script-frame exit (EBR retires the slab).
///
/// **Failure mode**: zone-slab exhaustion mints a defensive
/// placeholder cap from `Cred::root()` and panics on
/// restrictions-cap failure (the placeholder zone is sized for one
/// cap per concurrent syscall — exhaustion is a kernel-wide pressure
/// event PR-K will revisit). Production callers should not hit this
/// path; for now the conservative-panic matches today's
/// `expect("zone slab has capacity")` discipline elsewhere in this
/// module.
pub fn build_subject_script_ctx(ctx: &SyscallCtx<'_>) -> crate::KernelScriptCtx {
    let cred_cap = ctx.cred_cap();
    let restrictions_cap = tx_subsystems::cred::placeholder_restrictions_cap()
        .expect("placeholder restrictions zone has capacity per syscall entry");
    let authority = crate::KernelSubjectAuthority::new(cred_cap, restrictions_cap);
    let subject = crate::KernelSubjectContext::from_thread(
        ctx.process.clone(),
        ctx.thread.clone(),
        authority,
    );
    crate::KernelScriptCtx::new().with_subject(subject)
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
    match req.nr {
        NR_WRITE => sys_write(req.args, ctx).await,
        NR_WRITEV => sys_writev(req.args, ctx).await,
        NR_READ => sys_read(req.args, ctx).await,
        NR_READV => sys_readv(req.args, ctx).await,
        NR_PPOLL => sys_ppoll(req.args, ctx).await,
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
        nr if nr == NR_IO_URING_SETUP => {
            sys_io_uring_setup(req.args[0] as u32, req.args[1], ctx)
        }
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

/// Outcome of `read_user_cstr` — distinguishes "no NUL within budget"
/// from a successful copy. The successful arm yields the bytes up to
/// (not including) the NUL terminator, allocated as a kernel-owned
/// `Vec<u8>`.
pub(super) enum ReadCStrError {
    /// No NUL within `max_len` — surface as `-ENAMETOOLONG`.
    TooLong,
}

/// Outcome of `read_user_cstr_vec`. `TooBig` covers both
/// pointer-array overflow and aggregate-byte overflow; both surface
/// as `-E2BIG` per the Phase 6 plan.
pub(super) enum ReadVecError {
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
pub(super) fn read_user_cstr(
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
pub(super) fn read_user_cstr_vec(
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
pub(super) fn bootstrap_read_user<T: Copy>(aspace: &AddressSpace, uaddr: u64) -> Result<T, Errno> {
    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3};
    let guard = tx_substrate::epoch::guard();
    match aspace.read_user(UserPtr::<T>::new(uaddr as usize), &guard) {
        V3::Done(v) => Ok(v),
        V3::Err(V3Errno::EFAULT) => {
            drop(guard);
            // Fallback: kernel-pointer bootstrap exemption.
            // SAFETY: existing dispatch tests pass kernel-side pointers
            // directly. The fallback is a bridge until tests migrate.
            Ok(unsafe { core::ptr::read_volatile(uaddr as *const T) })
        }
        V3::Err(e) => Err(e.into()),
        V3::Yield { .. } | V3::Continue { .. } => Err(Errno::EIO),
    }
}

/// Write a `T: Copy` value to `uaddr` through the canonical
/// `aspace.write_user` lane, falling back to the bootstrap
/// kernel-pointer dance on `EFAULT`.
pub(super) fn bootstrap_write_user<T: Copy>(
    aspace: &AddressSpace,
    uaddr: u64,
    value: T,
) -> Result<(), Errno> {
    use tx_substrate::step_v3::StepOutcome as V3;
    let guard = tx_substrate::epoch::guard();
    match aspace.write_user(UserPtr::<T>::new(uaddr as usize), value, &guard) {
        V3::Done(()) | V3::Continue { .. } => Ok(()),
        V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
            drop(guard);
            // SAFETY: see `bootstrap_read_user`.
            unsafe {
                core::ptr::write_volatile(uaddr as *mut T, value);
            }
            Ok(())
        }
        V3::Err(e) => Err(Errno::from(e)),
        V3::Yield { .. } => Err(Errno::EIO),
    }
}

/// Copy `dst.len()` bytes from user-space `uaddr` into the kernel-side
/// buffer `dst`. Bridges through `aspace.copy_from_user`, falling back
/// to a kernel-pointer memcpy on `EFAULT`.
pub(super) fn bootstrap_copy_from_user(
    aspace: &AddressSpace,
    dst: &mut [u8],
    uaddr: u64,
) -> Result<(), Errno> {
    use tx_substrate::step_v3::StepOutcome as V3;
    if dst.is_empty() {
        return Ok(());
    }
    let guard = tx_substrate::epoch::guard();
    match aspace.copy_from_user(dst, UserPtr::<u8>::new(uaddr as usize), &guard) {
        V3::Done(_) | V3::Continue { .. } => Ok(()),
        V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
            drop(guard);
            // SAFETY: see `bootstrap_read_user`.
            unsafe {
                core::ptr::copy_nonoverlapping(uaddr as *const u8, dst.as_mut_ptr(), dst.len());
            }
            Ok(())
        }
        V3::Err(e) => Err(Errno::from(e)),
        V3::Yield { .. } => Err(Errno::EIO),
    }
}

/// Copy `src.len()` bytes from the kernel-side buffer `src` to
/// user-space `uaddr`. Bridges through `aspace.copy_to_user`, falling
/// back to a kernel-pointer memcpy on `EFAULT`.
pub(super) fn bootstrap_copy_to_user(
    aspace: &AddressSpace,
    uaddr: u64,
    src: &[u8],
) -> Result<(), Errno> {
    use tx_substrate::step_v3::StepOutcome as V3;
    if src.is_empty() {
        return Ok(());
    }
    let guard = tx_substrate::epoch::guard();
    match aspace.copy_to_user(UserPtr::<u8>::new(uaddr as usize), src, &guard) {
        V3::Done(_) | V3::Continue { .. } => Ok(()),
        V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
            drop(guard);
            // SAFETY: see `bootstrap_read_user`.
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), uaddr as *mut u8, src.len());
            }
            Ok(())
        }
        V3::Err(e) => Err(Errno::from(e)),
        V3::Yield { .. } => Err(Errno::EIO),
    }
}

/// Read a NUL-terminated user string at `uaddr`, capped at `max_len`
/// bytes. Bridges through `aspace.read_user_cstr`, falling back to the
/// bootstrap byte-by-byte scan on `EFAULT`.
///
/// Returns `Ok(bytes)` (without the NUL terminator). `Err(Errno)`
/// surfaces other errors; `Errno::ENAMETOOLONG` indicates `max_len`
/// bytes were walked without finding a NUL.
pub(super) fn bootstrap_read_user_cstr(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, Errno> {
    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3};
    if uaddr == 0 || max_len == 0 {
        return Ok(Vec::new());
    }
    let guard = tx_substrate::epoch::guard();
    match aspace.read_user_cstr(UserPtr::<u8>::new(uaddr as usize), max_len, &guard) {
        V3::Done(v) => Ok(v),
        V3::Err(V3Errno::EFAULT) => {
            drop(guard);
            // Fallback bootstrap scan — matches the previous inline
            // helper.
            let mut out: Vec<u8> = Vec::with_capacity(core::cmp::min(max_len, 256));
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
        V3::Err(e) => Err(e.into()),
        V3::Yield { .. } | V3::Continue { .. } => Err(Errno::EIO),
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
pub(super) const UID_LEAVE_UNCHANGED: u32 = u32::MAX;

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
/// Linux generic ABI errno value for "result out of range" (`ERANGE`).
/// Used by Slice 6's `sys_getcwd` when the user buffer is too small
/// for the rendered cwd path (NUL terminator inclusive).
pub(super) const ERANGE_VALUE: i32 = 34;

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
// timer-fire wait source (i.e. a Channel attached to the reactor's
// TimerQueue, fired when `step_hart_loop_at`'s `advance_time_to` walks
// past the parked deadline). That wiring requires either exposing
// `Reactor::channel()` through `wait_source` (a tx-kernel ↔
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

/// Fixed header byte size of `linux_dirent64`. Used by `sys_getdents64`
/// to compute the trailing name-and-pad offset.
pub(super) const LINUX_DIRENT64_HEADER_BYTES: usize = 19;

/// Default `st_blksize` reported by Slice 6's stat arms. Linux's
/// page-backed filesystems all report 4096; txKernel has no
/// per-FS blocksize hint to override this with today.
pub(super) const STAT_BLKSIZE: i32 = 4096;

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

// ---------------------------------------------------------------------
// Layout structs for Slice 7 syscall arms.
// ---------------------------------------------------------------------

/// Linux uapi `struct utsname` field width (`__NEW_UTS_LEN + 1 = 65`).
pub(super) const UTSNAME_FIELD: usize = 65;
