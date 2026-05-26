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
use tx_hal::{AuxvIf, EntropyIf, IpiKind, PmapIf, SmpIf, TimeIf};
use tx_observe::encode::{
    arg_value_tag, encode_arg_value, encode_syscall_enter, encode_syscall_exit, syscall_enter_tag,
    syscall_exit_tag,
};
use tx_observe::{EventNameId, SpanId, TxTraceLevel};
use tx_observe_types::{
    PayloadArgValue, PayloadSyscallEnter, PayloadSyscallExit, TxPayloadTag, TxValueKind,
};
use tx_scripts::process::exec::{exec_script, ExecError};
use tx_subsystems::cred::{
    Capability, CapabilitySet, CredChange, Gid, SetgidOp, SetregidOp, SetresgidOp, SetresuidOp,
    SetreuidOp, SetuidOp, Uid,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::futex::FutexWakeOp;
use tx_subsystems::page_backed::{
    AnonSwapPolicy, PageContainer, PageContainerKind, TruncateOp as FdTruncateOp,
};
use tx_subsystems::process::{
    process_by_pid, seed_child_leader_context, step_waitpid_nohang, ChdirOp, ChdirOutcome, CloseOp,
    Dup3Op, DupOp, ExitGroupOp, ExitStatus, FcntlDupFdOp, FcntlFdOp, GetcwdOp, Pgid, Pid,
    ProcessIdentity, SetpgidOp, SetsidOp, WaitError, WaitTarget,
};
use tx_subsystems::reactor_submit;
use tx_subsystems::signal::{
    DeliverSignalOp, KillProcessOp, SaFlags, SigActionEntry, SigDisposition, SigDispositionChange,
    SigactionOp, SignalMask, Signum,
};
use tx_subsystems::thread_runtime::execution::{SigmaskHow, SigprocmaskChange};
use tx_subsystems::thread_runtime::{SigprocmaskOp, ThreadExitOp, ThreadKillOp};
use tx_subsystems::tty::execution::{
    step_ioctl_tcgets, step_ioctl_tcsets, step_ioctl_tiocgpgrp, step_ioctl_tiocgwinsz,
    step_ioctl_tiocnotty, step_ioctl_tiocsctty_for_process, step_ioctl_tiocspgrp,
    step_ioctl_tiocswinsz, IoctlCaller,
};
use tx_subsystems::tty::structure::{Termios, Winsize};
use tx_subsystems::vfs::composite::{
    AccessOp, ChmodOp, ChownOp, MknodOp, NanosleepOp, StatOp, StatxOp, StatxResult,
};
use tx_subsystems::vfs::structure::{
    Credential, InodeKind, InodeMeta, OpenFileBacking, OpenFileFlags, RNodeBacking, StructPayload,
    S_ISGID,
};
use tx_subsystems::vfs::{
    step_open, step_walk, DEntry, FileFsyncOp, FlockOp, OpenFile, OpenFileGetFlOp, OpenFileSetFlOp,
    OpenOp,
};
use tx_subsystems::vm::{
    AddressSpace, MadviseAdvice, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking,
    VmEntryFlags, VmMapError, VmMapRequest, VmRemapRequest, FULL_USER_V1_TOP, USER_PAGE_SIZE,
};
use tx_subsystems::wait_source;

pub mod numbers;

mod cred;
use cred::*;
mod time;
pub use time::poll_due_itimers;
use time::*;
mod signal;
use signal::*;
mod ipc;
use ipc::*;
mod vm;
use vm::*;
pub mod io;
use io::*;
mod socket;
use socket::*;
pub mod fs_basic;
use fs_basic::*;
mod fs_path;
use fs_path::*;
mod fs_mut;
use fs_mut::*;
mod fs_handle;
use fs_handle::*;
pub mod proc;
use proc::*;
mod misc;
use misc::*;
mod userfaultfd;
use userfaultfd::*;
// `clone_op` and `exec_op` are unfinished StepOp-shaped refactors —
// both target an older API surface (Credential::euid/egid,
// SegmentFlags readable/writable, UserTrapContext::set_sepc, struct-
// variant StepOutcome::Yield(...) tuple form, etc.) that no longer
// exists. `sys_clone` and `sys_execve` drive their underlying step
// functions directly until these wrappers land.
// pub mod clone_op;
// pub mod exec_op;
pub mod aio;
use aio::*;
pub mod io_uring;
use io_uring::*;
mod signalfd;
use crate::adapter::step_engine::{self as step_engine, Cap};
use signalfd::*;
mod eventfd;
use eventfd::*;
mod timerfd;
use timerfd::*;
mod posix_timer;
pub use posix_timer::poll_due_posix_timers;
use posix_timer::*;
mod epoll;
use epoll::*;
mod net;

mod ctx;
pub use ctx::*;
mod result;
pub use result::*;
mod user_copy;
pub(super) use user_copy::*;
mod user_layout;
pub use user_layout::{
    kernel_user_layout_candidates, kernel_user_layouts, KernelToUserLayout, KernelUserCandidate,
    KernelUserField, KernelUserLayout,
};
mod helpers;
pub(super) use helpers::*;

pub use time::maybe_deliver_itimer_signal;

#[cfg(test)]
mod tests;

pub use numbers::*;

/// Maximum number of input bytes the Phase 2a `write` syscall accepts
/// in a single call. The dispatcher copies `[buf_ptr, buf_ptr+len)` into
/// a kernel-side stack-bounded slice (via `from_raw_parts`); higher-level
/// `copy_from_user` machinery is deferred per the trio plan §"Out of
/// scope". 4 KiB matches a single page; values above that should batch
/// across multiple write calls until the userspace-VA copy lane lands.
pub const TTY_WRITE_MAX_INLINE: usize = 4096;

/// Maximum socket payload bytes staged by a single `read(2)`/`write(2)`
/// call.
///
/// TTY and pipe writes stay capped at one page because they fan into
/// byte-oriented console/pipe paths. Socket payloads are already
/// backed by bounded per-socket send/receive buffers, and network
/// workloads such as iperf naturally issue 64 KiB-ish blocks. Keeping
/// those blocks intact avoids turning one socket transfer into dozens
/// of tiny syscalls while still bounding the temporary staging buffer.
pub const SOCKET_IO_MAX_INLINE: usize = 64 * 1024;

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
/// Linux generic ABI errno value for "operation not supported" (`EOPNOTSUPP`).
/// Used when a syscall surface exists but the requested object/clock flavor is
/// outside txKernel's current emulation contract.
pub(super) const EOPNOTSUPP_VALUE: i32 = 95;
pub(super) const ENODEV_VALUE: i32 = 19;
/// Linux generic ABI errno value for "bad file descriptor" (`EBADF`).
pub(super) const EBADF_VALUE: i32 = 9;
/// Linux generic ABI errno value for "too many open files" (`EMFILE`).
/// Kept in sync with the soft `RLIMIT_NOFILE` value reported by
/// `prlimit64`.
pub(super) const EMFILE_VALUE: i32 = 24;
/// Linux generic ABI errno value for "bad address" (`EFAULT`).
/// Used by Slice 4's time syscalls when a required user pointer is
/// null, and by every `bootstrap_*` user-VA bridge for invalid user
/// addresses (the canonical `aspace.copy_*_user` lane already
/// surfaces `Errno::EFAULT`; the dispatcher translates it here).
pub(super) const EFAULT_VALUE: i32 = 14;
/// Linux generic ABI errno value for "illegal seek" (`ESPIPE`).
/// Used by positioned I/O and advice syscalls on pipes/TTY-like files.
pub(super) const ESPIPE_VALUE: i32 = 29;
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
/// Linux generic ABI errno value for "connection refused" (`ECONNREFUSED`).
pub(super) const ECONNREFUSED_VALUE: i32 = 111;
/// Linux generic ABI errno value for "connection timed out" / wait timeout
/// (`ETIMEDOUT`). Used by futex wait timeout paths.
pub(super) const ETIMEDOUT_VALUE: i32 = 110;
/// Linux generic ABI errno value for "transport endpoint is not connected" (`ENOTCONN`).
pub(super) const ENOTCONN_VALUE: i32 = 107;
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
/// Fallback soft `RLIMIT_NOFILE` value used by legacy fd helpers.
pub(super) const RLIMIT_NOFILE_CUR: u32 = 1024;

pub(super) fn next_fd_below_nofile(
    process: &Cap<ProcessIdentity>,
    min: u32,
) -> Result<u32, SyscallResult> {
    if min >= RLIMIT_NOFILE_CUR {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    let fd = process.next_fd_above(min);
    if fd >= RLIMIT_NOFILE_CUR {
        return Err(SyscallResult::Error(EMFILE_VALUE));
    }
    Ok(fd)
}

pub(super) fn next_stdio_fd_below_nofile(
    process: &Cap<ProcessIdentity>,
) -> Result<u32, SyscallResult> {
    match next_fd_below_nofile(process, 0) {
        Err(SyscallResult::Error(EINVAL_VALUE)) => Err(SyscallResult::Error(EMFILE_VALUE)),
        other => other,
    }
}

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
/// Layout decision: Linux RV64 pulls in `asm-generic/signal.h` without
/// defining `SA_RESTORER`, so the optional `sa_restorer` field is absent from
/// the kernel-facing structure. The syscall therefore exchanges three 64-bit
/// fields:
///
/// ```text
/// struct sigaction {
///     __sighandler_t  sa_handler;   // 8B
///     unsigned long   sa_flags;     // 8B
///     sigset_t        sa_mask;      // 8B  (single u64 bitset, sigsetsize=8)
/// };
/// ```
///
/// So the rt_sigaction syscall takes a 24-byte buffer. The plan's
/// "16 bytes" hint applied to the legacy `__OLD_SIGACTION` shape used
/// by the (deprecated) `sigaction()` syscall — the modern
/// `rt_sigaction` syscall uses the 24-byte RV64 form. We pin the modern
/// shape because Linux RV64 has no `sigaction()` syscall at all (it only ships
/// `rt_sigaction`, NR_134), but unlike x86-64 the modern RV64 shape does not
/// carry an in-struct restorer.
///
/// Citation: linux/arch/riscv/include/uapi/asm/signal.h includes
/// `asm-generic/signal.h`; there `sa_restorer` is guarded by
/// `#ifdef SA_RESTORER`, and RV64 does not define that macro.
pub(super) const SIGACTION_BYTES: usize = 24;

/// Per-syscall context resolved by the trap-shell wrapper: the calling
/// process / thread, the bound address space, and the bookkeeping the
/// dispatch table needs to act without knowing the wrapper's shape.
///
/// Phase 2a only consumes `process` (for `getpid` / `exit_group` and
/// fd-table lookup) and `thread` (for `exit`). `aspace` is wired into
/// the surface today so the Phase 2b additions (`brk`, `read`) can
/// land without a context-shape break; the field is intentionally
/// unused by the four current arms.
///
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
pub async fn dispatch<'a, P: PmapIf + EntropyIf + TimeIf + AuxvIf + SmpIf>(
    req: SyscallRequest,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    // L0 boundary span — `08_OBSERVATION_v1.md` §6 HOOKS-1.
    // Always opened before the inner dispatch; closed after with the
    // result-shaped `PayloadSyscallExit`. A `SpanId::NONE` short-circuit
    // ensures we never emit a mismatched span-end when no emitter is
    // installed (test contexts, boards with `ObserverIf` default).
    //
    // Parent-span linkage (L0 → L2 → L4) lives in a per-hart static
    // installed via [`tx_observe::set_current_parent_span`] so the L2
    // record in `tx_scripts::drive` picks it up implicitly without
    // every syscall arm having to thread it through `ScriptCtx`.
    //
    // The threshold-based observation dump trigger is handled one level
    // up in `tx_kernel::thread_future::run_thread` so the dispatch
    // signature stays free of `ConsoleIf + PowerIf` bounds that would
    // ripple into every test-stub platform.
    let l0_span = emit_syscall_enter(&req);
    let prev = tx_observe::set_current_parent_span(l0_span);
    let result = dispatch_inner::<P>(req, ctx).await;
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);
    result
}

async fn dispatch_inner<'a, P: PmapIf + EntropyIf + TimeIf + AuxvIf + SmpIf>(
    req: SyscallRequest,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    // ── Lane 1: ImmediateSyscall (pure ABI queries, never yield) ──
    // Per `docs/Txv3/04_SYSCALL_SHAPE_v1.md §6.1`: these syscalls
    // do not call drive(), do not enter StepOp, do not construct
    // YieldShape, and do not access VFS/VM/reactor/timer.
    match req.nr {
        NR_GETPID => return sys_getpid(ctx),
        NR_GETTID => return sys_gettid(ctx),
        nr if nr == NR_GETPPID => return sys_getppid(ctx),
        nr if nr == NR_GETPGRP => return sys_getpgrp(ctx),
        nr if nr == NR_GETPGID => return sys_getpgid(req.args, ctx),
        nr if nr == NR_GETSID => return sys_getsid(req.args, ctx),
        nr if nr == NR_KCMP => return sys_kcmp(req.args, ctx),
        nr if nr == NR_PIDFD_GETFD => return sys_pidfd_getfd(req.args, ctx),
        nr if nr == NR_GETRLIMIT => return sys_getrlimit(req.args, ctx),
        nr if nr == NR_GETUID => return sys_getuid(ctx),
        nr if nr == NR_GETEUID => return sys_geteuid(ctx),
        nr if nr == NR_GETGID => return sys_getgid(ctx),
        nr if nr == NR_GETEGID => return sys_getegid(ctx),
        nr if nr == NR_GETRESUID => return sys_getresuid(req.args, ctx),
        nr if nr == NR_GETRESGID => return sys_getresgid(req.args, ctx),
        nr if nr == NR_TIMES => return sys_times::<P>(req.args, ctx),
        nr if nr == NR_GETTIMEOFDAY => return sys_gettimeofday::<P>(req.args, ctx),
        nr if nr == NR_GETITIMER => return sys_getitimer::<P>(req.args, ctx),
        nr if nr == NR_SETITIMER => return sys_setitimer::<P>(req.args, ctx),
        nr if nr == NR_UMASK => return sys_umask(req.args, ctx),
        nr if nr == NR_UNAME => return sys_uname::<P>(req.args, ctx),
        nr if nr == NR_SETHOSTNAME => return sys_sethostname(req.args, ctx),
        nr if nr == NR_GETRANDOM => return sys_getrandom(req.args, ctx),
        nr if nr == NR_PRLIMIT64 => return sys_prlimit64(req.args, ctx),
        nr if nr == NR_PERSONALITY => return sys_personality(req.args, ctx),
        nr if nr == NR_RT_SIGRETURN => return sys_rt_sigreturn(ctx),
        nr if nr == NR_SCHED_GETATTR => return sys_sched_getattr(req.args, ctx),
        nr if nr == NR_SCHED_SETATTR => return sys_sched_setattr(req.args, ctx),
        nr if nr == NR_SCHED_GETAFFINITY => return sys_sched_getaffinity(req.args, ctx),
        nr if nr == NR_SCHED_SETAFFINITY => return sys_sched_setaffinity(req.args, ctx),
        nr if nr == NR_SCHED_SETSCHEDULER => return sys_sched_setscheduler(req.args, ctx),
        nr if nr == NR_SET_TID_ADDRESS => return sys_set_tid_address(req.args, ctx),
        nr if nr == NR_SET_ROBUST_LIST => return sys_set_robust_list(req.args, ctx),
        nr if nr == NR_GET_ROBUST_LIST => return sys_get_robust_list(req.args, ctx),
        nr if nr == NR_MADVISE => return sys_madvise(req.args, ctx),
        nr if nr == NR_MLOCK => return sys_mlock(req.args, ctx).await,
        nr if nr == NR_MUNLOCK => return sys_munlock(req.args, ctx).await,
        nr if nr == NR_MLOCKALL => return sys_mlockall(req.args, ctx).await,
        nr if nr == NR_MUNLOCKALL => return sys_munlockall(req.args, ctx).await,
        nr if nr == NR_MINCORE => return sys_mincore(req.args, ctx),
        nr if nr == NR_REMAP_FILE_PAGES => return sys_remap_file_pages(req.args),
        nr if nr == NR_MLOCK2 => return sys_mlock2(req.args, ctx).await,
        nr if nr == NR_UTIMENSAT => return sys_utimensat::<P>(req.args, ctx),
        nr if nr == NR_SHMGET => return sys_shmget(req.args, ctx),
        nr if nr == NR_SHMDT => return sys_shmdt(req.args, ctx).await,
        nr if nr == NR_MSGGET => return sys_msgget(req.args, ctx),
        nr if nr == NR_MSGSND => return sys_msgsnd(req.args, ctx),
        nr if nr == NR_MSGRCV => return sys_msgrcv(req.args, ctx),
        nr if nr == NR_SEMGET => return sys_semget(req.args, ctx),
        nr if nr == NR_MQ_OPEN => return sys_mq_open(req.args, ctx),
        nr if nr == NR_MQ_UNLINK => return sys_mq_unlink(req.args, ctx),
        nr if nr == NR_MQ_GETSETATTR => return sys_mq_getsetattr(req.args, ctx),
        nr if nr == NR_MQ_NOTIFY => return sys_mq_notify(req.args, ctx),
        nr if nr == NR_MEMBARRIER => return sys_membarrier::<P>(&req.args),
        _ => {} // fall through to script lanes
    }

    // ── Lanes 2+3: Script-based (OneShotStepOp + Full async drive) ──
    match req.nr {
        nr if nr == NR_WRITE => sys_write(req.args, ctx).await,
        nr if nr == NR_WRITEV => sys_writev(req.args, ctx).await,
        nr if nr == NR_READ => sys_read::<P>(req.args, ctx).await,
        nr if nr == NR_READV => sys_readv::<P>(req.args, ctx).await,
        nr if nr == NR_MQ_TIMEDSEND => sys_mq_timedsend(req.args, ctx).await,
        nr if nr == NR_MQ_TIMEDRECEIVE => sys_mq_timedreceive(req.args, ctx).await,
        nr if nr == NR_PREAD64 => sys_pread64::<P>(req.args, ctx).await,
        nr if nr == NR_PWRITE64 => sys_pwrite64(req.args, ctx).await,
        nr if nr == NR_PREADV => sys_preadv::<P>(req.args, ctx).await,
        nr if nr == NR_PWRITEV => sys_pwritev(req.args, ctx).await,
        nr if nr == NR_PREADV2 => sys_preadv2::<P>(req.args, ctx).await,
        nr if nr == NR_PWRITEV2 => sys_pwritev2(req.args, ctx).await,
        nr if nr == NR_FADVISE64 => sys_fadvise64(req.args, ctx),
        nr if nr == NR_SPLICE => sys_splice(req.args, ctx),
        nr if nr == NR_SOCKET => sys_socket(req.args, ctx),
        nr if nr == NR_SOCKETPAIR => sys_socketpair(req.args, ctx),
        nr if nr == NR_BIND => sys_bind(req.args, ctx),
        nr if nr == NR_LISTEN => sys_listen(req.args, ctx),
        nr if nr == NR_ACCEPT => sys_accept::<P>(req.args, ctx).await,
        nr if nr == NR_ACCEPT4 => sys_accept4::<P>(req.args, ctx).await,
        nr if nr == NR_CONNECT => sys_connect(req.args, ctx).await,
        nr if nr == NR_GETSOCKNAME => sys_getsockname(req.args, ctx),
        nr if nr == NR_GETPEERNAME => sys_getpeername(req.args, ctx),
        nr if nr == NR_SENDTO => sys_sendto(req.args, ctx).await,
        nr if nr == NR_RECVFROM => sys_recvfrom::<P>(req.args, ctx).await,
        nr if nr == NR_SENDMSG => sys_sendmsg(req.args, ctx).await,
        nr if nr == NR_RECVMSG => sys_recvmsg(req.args, ctx).await,
        nr if nr == NR_RECVMMSG => sys_recvmmsg::<P>(req.args, ctx).await,
        nr if nr == NR_SENDMMSG => sys_sendmmsg(req.args, ctx).await,
        nr if nr == NR_SETSOCKOPT => sys_setsockopt(req.args, ctx),
        nr if nr == NR_GETSOCKOPT => sys_getsockopt(req.args, ctx),
        nr if nr == NR_SHUTDOWN => sys_shutdown(req.args, ctx),
        nr if nr == NR_SENDFILE64 => sys_sendfile64(req.args, ctx).await,
        nr if nr == NR_COPY_FILE_RANGE => sys_copy_file_range::<P>(req.args, ctx),
        nr if nr == NR_PPOLL => sys_ppoll::<P>(req.args, ctx).await,
        nr if nr == NR_PSELECT6 || nr == NR_PSELECT6_TIME64 => {
            sys_pselect6::<P>(req.args, ctx).await
        }
        nr if nr == NR_SCHED_YIELD => sys_sched_yield().await,
        nr if nr == NR_EXIT => sys_exit(req.args, ctx),
        nr if nr == NR_EXIT_GROUP => sys_exit_group(req.args, ctx),
        nr if nr == NR_BRK => sys_brk(req.args, ctx).await,
        nr if nr == NR_RT_SIGPROCMASK => sys_rt_sigprocmask(req.args, ctx),
        nr if nr == NR_RT_SIGACTION => sys_rt_sigaction(req.args, ctx),
        nr if nr == NR_RT_SIGPENDING => sys_rt_sigpending(req.args, ctx),
        nr if nr == NR_RT_SIGSUSPEND => sys_rt_sigsuspend::<P>(req.args, ctx).await,
        nr if nr == NR_RT_SIGQUEUEINFO => sys_rt_sigqueueinfo(req.args, ctx),
        nr if nr == NR_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait::<P>(req.args, ctx).await,
        nr if nr == NR_PIDFD_OPEN => sys_pidfd_open(req.args, ctx),
        nr if nr == NR_PIDFD_SEND_SIGNAL => sys_pidfd_send_signal(req.args, ctx),
        nr if nr == NR_SIGALTSTACK => sys_sigaltstack(req.args, ctx),
        nr if nr == NR_CAPGET => sys_capget(req.args, ctx),
        nr if nr == NR_CAPSET => sys_capset(req.args, ctx),
        nr if nr == NR_FCNTL => sys_fcntl(req.args, ctx),
        nr if nr == NR_SHMCTL => sys_shmctl(req.args, ctx),
        nr if nr == NR_MSGCTL => sys_msgctl(req.args, ctx),
        nr if nr == NR_SEMOP => sys_semop(req.args, ctx).await,
        nr if nr == NR_SEMTIMEDOP => sys_semtimedop::<P>(req.args, ctx).await,
        nr if nr == NR_SEMCTL => sys_semctl(req.args, ctx),
        nr if nr == NR_SHMAT => sys_shmat(req.args, ctx).await,
        nr if nr == NR_EXECVE => sys_execve::<P>(req.args, ctx).await,
        nr if nr == NR_CLONE => sys_clone::<P>(req.args, ctx).await,
        nr if nr == NR_UNSHARE => sys_unshare(req.args, ctx),
        nr if nr == NR_SETNS => sys_setns(req.args, ctx),
        nr if nr == NR_WAIT4 => sys_wait4(req.args, ctx).await,
        nr if nr == NR_GETRUSAGE => sys_getrusage(req.args, ctx),
        nr if nr == NR_SETPGID => sys_setpgid(req.args, ctx),
        nr if nr == NR_SETSID => sys_setsid(ctx),
        nr if nr == NR_SET_TID_ADDRESS => sys_set_tid_address(req.args, ctx),
        nr if nr == NR_SET_ROBUST_LIST => sys_set_robust_list(req.args, ctx),
        nr if nr == NR_GET_ROBUST_LIST => sys_get_robust_list(req.args, ctx),
        // Wave 2 of the DAC + setuid slice — Part 3 (cred-mutation /
        // cred-reading arms). Each wraps a Wave 1 `cred::step_*`
        // helper through the new `ctx.cred()` accessor.
        nr if nr == NR_SETUID => sys_setuid(req.args, ctx),
        nr if nr == NR_SETGID => sys_setgid(req.args, ctx),
        nr if nr == NR_SETGROUPS => sys_setgroups(req.args, ctx),
        nr if nr == NR_SETREUID => sys_setreuid(req.args, ctx),
        nr if nr == NR_SETREGID => sys_setregid(req.args, ctx),
        nr if nr == NR_SETRESUID => sys_setresuid(req.args, ctx),
        nr if nr == NR_SETRESGID => sys_setresgid(req.args, ctx),
        // Wave 4 Part 4 of the DAC + setuid slice — file-mode syscall
        // arms. Each wraps the FsOps surface Wave 3 Part 2 landed
        // (`step_chmod` / `step_chown`) plus a walker-side `access(2)`
        // predicate over the inode meta.
        nr if nr == NR_FCHMOD => sys_fchmod(req.args[0] as i32, req.args[1] as u32, ctx),
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
        nr if nr == NR_NAME_TO_HANDLE_AT => sys_name_to_handle_at::<P>(
            req.args[0] as i32,
            req.args[1],
            req.args[2],
            req.args[3],
            req.args[4] as u32,
            ctx,
        ),
        nr if nr == NR_OPEN_BY_HANDLE_AT => {
            sys_open_by_handle_at(req.args[0] as i32, req.args[1], req.args[2] as u32, ctx)
        }
        nr if nr == NR_CLOSE => sys_close(req.args[0] as u32, ctx).await,
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
        // Slice 2 of the shell-prompt roadmap — VM syscalls. mmap /
        // munmap / mprotect / mremap / madvise use StepOp wrappers
        // (VmMapOp / VmUnmapOp etc.) that yield on RangeLock
        // conflicts; the drive loop parks on WaitSource and retries.
        nr if nr == NR_MMAP => sys_mmap(req.args, ctx).await,
        nr if nr == NR_MUNMAP => sys_munmap(req.args, ctx).await,
        nr if nr == NR_MPROTECT => sys_mprotect(req.args, ctx).await,
        nr if nr == NR_MREMAP => sys_mremap(req.args, ctx).await,
        nr if nr == NR_MSYNC => sys_msync(req.args, ctx).await,
        // Slice 3 of the shell-prompt roadmap — `futex(2)`. v1 honours
        // `FUTEX_WAIT` / `FUTEX_WAKE` against a 256-bucket hash table;
        // other op selectors return `-ENOSYS`. `FUTEX_PRIVATE_FLAG` /
        // `FUTEX_CLOCK_REALTIME` are recognised but ignored. Required
        // for musl libc init.
        nr if nr == NR_FUTEX => sys_futex::<P>(req.args, ctx).await,
        // Slice 4 of the shell-prompt roadmap — time syscalls. The
        // four POSIX clock ids alias to the platform monotonic clock
        // for v1 (CLOCK_REALTIME has no boot-time RTC offset yet;
        // CPU-time clocks have no per-process accounting yet —
        // documented at the constant declarations in `numbers.rs`).
        // `nanosleep` / `clock_nanosleep` park the task on the reactor
        // timer queue for real-duration sleeps; zero-duration and
        // past-deadline cases short-circuit immediately.
        nr if nr == NR_CLOCK_GETTIME => sys_clock_gettime::<P>(req.args, ctx),
        nr if nr == NR_CLOCK_GETRES => sys_clock_getres(req.args, ctx),
        nr if nr == NR_NANOSLEEP => sys_nanosleep::<P>(req.args, ctx).await,
        nr if nr == NR_CLOCK_NANOSLEEP => sys_clock_nanosleep::<P>(req.args, ctx).await,
        nr if nr == NR_SETITIMER => sys_setitimer::<P>(req.args, ctx),
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
        nr if nr == NR_FCHDIR => sys_fchdir::<P>(req.args, ctx).await,
        nr if nr == NR_STATFS => sys_statfs::<P>(req.args, ctx).await,
        nr if nr == NR_FSTATFS => sys_fstatfs::<P>(req.args, ctx).await,
        nr if nr == NR_SYNC => sys_sync::<P>(req.args, ctx).await,
        nr if nr == NR_SYNCFS => sys_syncfs::<P>(req.args, ctx).await,
        nr if nr == NR_SYNC_FILE_RANGE => sys_sync_file_range(req.args, ctx),
        nr if nr == NR_READAHEAD => sys_readahead(req.args, ctx),
        nr if nr == NR_FSYNC => sys_fsync::<P>(req.args, ctx).await,
        nr if nr == NR_FDATASYNC => sys_fdatasync::<P>(req.args, ctx).await,
        nr if nr == NR_FLOCK => sys_flock::<P>(req.args, ctx).await,
        nr if nr == NR_MOUNT => sys_mount::<P>(req.args, ctx).await,
        nr if nr == NR_UMOUNT2 => sys_umount2::<P>(req.args, ctx).await,
        nr if nr == NR_MKNODAT => sys_mknodat::<P>(req.args, ctx).await,
        nr if nr == NR_GETDENTS64 => sys_getdents64(req.args, ctx).await,
        nr if nr == NR_STATX => sys_statx(req.args, ctx).await,
        // Slice 7 of the shell-prompt roadmap — fcntl extension +
        // day-1 misc syscalls. None individually heavy; each unblocks
        // a specific shell-startup path.
        nr if nr == NR_KILL => sys_kill(req.args, ctx),
        nr if nr == NR_TKILL => sys_tkill(req.args, ctx),
        nr if nr == NR_TGKILL => sys_tgkill(req.args, ctx),
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
        nr if nr == NR_FTRUNCATE => sys_ftruncate(req.args, ctx).await,
        nr if nr == NR_FALLOCATE => sys_fallocate(req.args, ctx),
        nr if nr == NR_READLINKAT => sys_readlinkat(req.args, ctx).await,
        nr if nr == NR_RENAMEAT2 => sys_renameat2(req.args, ctx).await,
        // syslog(2) / klogctl — kernel ring-buffer read/control.
        // Stubbed: type 2 (READ) returns 0 bytes so `dmesg(1)` exits 0.
        nr if nr == NR_SYSLOG => sys_syslog(req.args, ctx),
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
        nr if nr == NR_SIGNALFD => sys_signalfd(req.args, ctx),
        nr if nr == NR_SIGNALFD4 => sys_signalfd4(
            req.args[0] as i32,
            req.args[1],
            req.args[2],
            req.args[3] as u32,
            ctx,
        ),
        // eventfd2(init_val, flags) — mints an eventfd.
        nr if nr == NR_EVENTFD2 => sys_eventfd2(req.args[0], req.args[1] as u32, ctx),
        // timerfd_create(clockid, flags) — mints a timerfd.
        nr if nr == NR_TIMERFD_CREATE => {
            sys_timerfd_create(req.args[0] as u32, req.args[1] as u32, ctx)
        }
        // timerfd_settime(fd, flags, new_value, old_value).
        nr if nr == NR_TIMERFD_SETTIME => sys_timerfd_settime::<P>(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2],
            req.args[3],
            ctx,
        ),
        // timerfd_gettime(fd, curr_value).
        nr if nr == NR_TIMERFD_GETTIME => {
            sys_timerfd_gettime::<P>(req.args[0] as u32, req.args[1], ctx)
        }
        // POSIX timer syscalls.
        nr if nr == NR_TIMER_CREATE => {
            sys_timer_create(req.args[0] as u32, req.args[1], req.args[2], ctx)
        }
        nr if nr == NR_TIMER_DELETE => sys_timer_delete(req.args[0] as u32, ctx),
        nr if nr == NR_TIMER_GETOVERRUN => sys_timer_getoverrun(req.args[0] as u32, ctx),
        nr if nr == NR_TIMER_GETTIME => {
            sys_timer_gettime::<P>(req.args[0] as u32, req.args[1], ctx)
        }
        nr if nr == NR_TIMER_SETTIME => sys_timer_settime::<P>(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2],
            req.args[3],
            ctx,
        ),
        // epoll_create1 / epoll_ctl / epoll_pwait — generic Linux
        // numbers used by musl on RV64 and LoongArch64. musl's
        // epoll_wait wrapper calls epoll_pwait with a null mask on
        // these targets.
        nr if nr == NR_EPOLL_CREATE1 => sys_epoll_create1(req.args[0] as u32, ctx),
        nr if nr == NR_EPOLL_CTL => sys_epoll_ctl(
            req.args[0] as u32,
            req.args[1] as u32,
            req.args[2] as u32,
            req.args[3],
            ctx,
        ),
        nr if nr == NR_EPOLL_PWAIT => {
            sys_epoll_wait::<P>(
                req.args[0] as u32,
                req.args[1],
                req.args[2] as u32,
                req.args[3] as i32,
                ctx,
            )
            .await
        }
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

// ---------------------------------------------------------------------------
// membarrier(2)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct SchedParamLayout {
    sched_priority: i32,
}

/// `sched_setscheduler(pid, policy, param)` — validation-only shim.
///
/// The actual scheduler policy remains txKernel's native policy, but Linux
/// callers expect the basic errno surface for bad pid/policy/param/priority.
fn sys_sched_setscheduler<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    const SCHED_OTHER: u32 = 0;
    const SCHED_FIFO: u32 = 1;
    const SCHED_RR: u32 = 2;
    const SCHED_BATCH: u32 = 3;
    const SCHED_IDLE: u32 = 5;

    let pid = args[0];
    let policy = args[1] as u32;
    let param_ptr = args[2];

    if pid != 0 {
        let Ok(pid32) = u32::try_from(pid) else {
            return SyscallResult::Error(ESRCH_VALUE);
        };
        if process_by_pid(Pid(pid32)).is_none()
            && !matches!(
                tx_subsystems::process::numbers::resolve_pid_number(pid),
                Some(tx_subsystems::process::numbers::PidName::Thread(_))
            )
        {
            return SyscallResult::Error(ESRCH_VALUE);
        }
    }
    if !matches!(
        policy,
        SCHED_OTHER | SCHED_FIFO | SCHED_RR | SCHED_BATCH | SCHED_IDLE
    ) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if param_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let param = match bootstrap_read_user::<SchedParamLayout>(&ctx.aspace, param_ptr) {
        Ok(param) => param,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let priority = param.sched_priority;
    let valid_priority = match policy {
        SCHED_FIFO | SCHED_RR => (1..=99).contains(&priority),
        _ => priority == 0,
    };
    if !valid_priority {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    SyscallResult::Return(0)
}

/// `membarrier(cmd, flags, cpu_id)` — issue memory-ordering barriers
/// across all online harts.
///
/// Lane 1 (Immediate): never yields, never enters StepOp.
///
/// Supported commands:
/// - `MEMBARRIER_CMD_QUERY` (0) — returns a bitmask of supported commands.
/// - `MEMBARRIER_CMD_GLOBAL` (1), `_EXPEDITED` (1<<1) — broadcast a
///   [`IpiKind::Membarrier`] to every online hart and busy-wait for all
///   acks. On the receiving hart the IPI handler executes a
///   `core::sync::atomic::fence(SeqCst)` so that all prior stores are
///   globally visible.
/// - `MEMBARRIER_CMD_PRIVATE_EXPEDITED` (1<<3) — same as GLOBAL in a
///   single-address-space kernel.
/// - `MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE` (1<<5) — private
///   expedited plus instruction-fetch barrier (`fence.i` on RV64).
/// - `MEMBARRIER_CMD_REGISTER_*` — registration is a no-op; always
///   returns 0.
/// `sched_yield()` — cooperatively yield the current reactor task once.
///
/// This is not an immediate no-op: user-space race harnesses such as LTP
/// fuzzy-sync use it to let the peer pthread run on single-CPU guests.
async fn sys_sched_yield() -> SyscallResult {
    tx_reactor::yield_now().await;
    SyscallResult::Return(0)
}

///
/// `flags` and `cpu_id` are currently ignored (must be 0).
fn sys_membarrier<P: SmpIf>(args: &[u64; 6]) -> SyscallResult {
    let cmd = args[0];
    let flags = args[1] as u32;

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    match cmd {
        MEMBARRIER_CMD_QUERY => SyscallResult::Return(MEMBARRIER_SUPPORTED_MASK as i64),

        MEMBARRIER_CMD_GLOBAL
        | MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => {
            let targets = P::online_cpus();
            if targets.is_empty() {
                return SyscallResult::Return(0);
            }
            P::clear_ipi_ack_cpus(IpiKind::Membarrier, targets);
            P::broadcast_ipi(targets, IpiKind::Membarrier);
            // Synchronous wait: spin until every target hart has
            // executed the barrier and acked. The IPI handler on the
            // target hart runs `fence(SeqCst)` + ack before
            // returning to its interrupt context.
            P::wait_for_ipi_ack_cpus(targets, IpiKind::Membarrier);
            SyscallResult::Return(0)
        }

        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            // Registration is a no-op in this kernel.
            SyscallResult::Return(0)
        }

        _ => SyscallResult::Error(EINVAL_VALUE),
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
        Errno::E2BIG => 7,
        Errno::EACCES => 13,
        Errno::EALREADY => 114,
        Errno::EADDRINUSE => 98,
        Errno::EADDRNOTAVAIL => 99,
        Errno::EAFNOSUPPORT => 97,
        Errno::EAGAIN => EAGAIN_VALUE,
        Errno::EBADF => EBADF_VALUE,
        Errno::EBUSY => 16,
        Errno::ECONNREFUSED => 111,
        Errno::EDESTADDRREQ => 89,
        Errno::EDQUOT => 122,
        Errno::EEXIST => 17,
        Errno::EFBIG => 27,
        Errno::EIDRM => 43,
        Errno::EFAULT => 14,
        Errno::EINVAL => 22,
        Errno::EINPROGRESS => 115,
        Errno::EIO => 5,
        Errno::EISCONN => 106,
        Errno::EISDIR => 21,
        Errno::ELOOP => 40,
        Errno::EMLINK => 31,
        Errno::EMSGSIZE => 90,
        Errno::ENAMETOOLONG => 36,
        Errno::ENODEV => 19,
        Errno::ENOEXEC => 8,
        Errno::ENOMEM => 12,
        Errno::ENOENT => 2,
        Errno::ENOSYS => ENOSYS_VALUE,
        Errno::ENOPROTOOPT => 92,
        Errno::ENOTCONN => 107,
        Errno::ENOTDIR => 20,
        Errno::ENOTEMPTY => 39,
        Errno::ENOTTY => 25,
        Errno::EPERM => 1,
        Errno::EPIPE => 32,
        Errno::ERANGE => 34,
        Errno::EOPNOTSUPP => 95,
        Errno::EROFS => 30,
        Errno::ENOTSOCK => 88,
        Errno::EPROTONOSUPPORT => 93,
        Errno::ESPIPE => 29,
        Errno::ESRCH => 3,
        Errno::ESOCKTNOSUPPORT => 94,
        Errno::ESTALE => 116,
        Errno::ETIMEDOUT => 110,
        Errno::EINTR => 4,
    }
}

// ---------------------------------------------------------------------------
// L0 observation hooks for the syscall U/K boundary.
//
// Per `docs/Txv3/08_OBSERVATION_v1.md` §6 OBS-V1-HOOKS-1: emit a
// `SpanBegin(SyscallEnter)` at dispatch entry and a matching
// `SpanEnd(SyscallExit)` at dispatch exit. The functions are no-ops
// when no emitter is installed (test contexts, boards without an
// `ObserverIf` impl).
//
// OBS-2 compliance: raw register-shaped args are emitted as opaque
// `u64` (today: encoded only in the `argc` count; future `ArgValue`
// continuations will carry the bits). No `UserPtr<T>` deref.
// ---------------------------------------------------------------------------

#[inline]
fn emit_syscall_enter(req: &SyscallRequest) -> SpanId {
    let Some(em) = tx_observe::current() else {
        return SpanId::NONE;
    };
    let payload = PayloadSyscallEnter {
        sysno: req.nr as u32,
        // Linux RV64 = 0 today; LoongArch64 will use 1 once its shim lands.
        // Threading the per-board ABI through the call chain is OBS follow-up
        // work; emitting 0 is correct for the only board currently emitting.
        abi: 0,
        // argc reflects the register-shaped arg slots — `req.args` is `[u64; 6]`
        // for the Linux generic ABI. The daemon walks `argc` `ArgValue`
        // continuation records after this `SpanBegin`.
        argc: 6,
    };
    let (enc, len) = encode_syscall_enter(&payload);
    let syscall_span = em.span_begin(
        TxTraceLevel::Boundary,
        EventNameId::from_raw(req.nr as u32),
        SpanId::NONE,
        syscall_enter_tag(),
        &enc[..len as usize],
    );

    // Per OBS-V1 §6 / `08_OBSERVATION_SERIALIZATION_v0.md §8.6`: emit one
    // `ArgValue` `Instant` per register-shaped syscall arg right after
    // `SpanBegin(SyscallEnter)`. The daemon attaches them as debug
    // annotations on the syscall slice so each `sys_*` chip in Perfetto
    // shows `a0`/`a1`/…/`a5` with the raw u64 the userspace process
    // passed in. OBS-2 compliance: the wire carries the raw register
    // bits as `TxValueKind::U64`; no `UserPtr<T>` deref. The shim's
    // arg-parsing code later decodes individual args as `Ptr` /
    // `ObjectId` / etc. via separate `ArgValue` records once the
    // higher-fidelity arg-classification pass lands.
    if syscall_span != SpanId::NONE {
        for (i, &raw) in req.args.iter().enumerate().take(6) {
            let arg_payload = PayloadArgValue {
                key: tx_observe::fnv1a32(SYSCALL_ARG_NAMES[i].as_bytes()),
                value_kind: TxValueKind::U64 as u8,
                _pad: [0; 3],
                value0: raw,
            };
            let (enc, len) = encode_arg_value(&arg_payload);
            em.instant(
                TxTraceLevel::Boundary,
                EventNameId::from_raw(tx_observe::fnv1a32(SYSCALL_ARG_NAMES[i].as_bytes())),
                syscall_span,
                arg_value_tag(),
                &enc[..len as usize],
            );
        }
    }

    syscall_span
}

/// Stable register-position labels used as the `key` for syscall
/// `ArgValue` continuations. Matches the Linux ABI's call-clobbered
/// register names (a0..a5 on rv64, $a0..$a7 on la64, %rdi..%r9 on x86_64);
/// using the position-agnostic `aN` form keeps the names ABI-portable.
const SYSCALL_ARG_NAMES: [&str; 6] = ["a0", "a1", "a2", "a3", "a4", "a5"];

#[inline]
fn emit_syscall_exit(span: SpanId, result: &SyscallResult) {
    if span == SpanId::NONE {
        return;
    }
    let Some(em) = tx_observe::current() else {
        return;
    };
    // result_kind: 0=Ok, 1=Err, 2=Restart, 3=Fatal, 4=NoReturn (per
    // OBSERVATION_SERIALIZATION_v0 §8.1). `ExecCommitted` and
    // `Sigreturn*` are kernel-internal control-flow markers that never
    // surface as a userspace return value; classify them as NoReturn for
    // the trace so the daemon's syscall slice closes cleanly even though
    // no `a0` write occurs.
    let (ret, errno, result_kind) = match result {
        SyscallResult::Return(v) => (*v, 0, 0u8),
        SyscallResult::Error(e) => (0, *e, 1u8),
        SyscallResult::NoReturn => (0, 0, 4u8),
        SyscallResult::ExecCommitted => (0, 0, 4u8),
        SyscallResult::SigreturnRestored => (0, 0, 4u8),
        SyscallResult::SigreturnContextRestored => (0, 0, 4u8),
    };
    let payload = PayloadSyscallExit {
        ret,
        errno,
        result_kind,
        _pad: [0; 3],
    };
    let (enc, len) = encode_syscall_exit(&payload);
    em.span_end(span, syscall_exit_tag(), &enc[..len as usize]);
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
