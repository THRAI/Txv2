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

use alloc::vec::Vec;

use tx_hal::PmapIf;
use tx_reactor::userspace::SyscallRequest;
use tx_scripts::process::exec::{exec_script, ExecError};
use tx_substrate::zone::Cap;
use tx_subsystems::cred::{
    step_setgid, step_setregid, step_setresgid, step_setresuid, step_setreuid, step_setuid, Cred,
    CredChange, Gid, Uid,
};
use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::process::{
    seed_child_leader_context, step_exit_group, step_fork, step_setpgid, step_setsid,
    step_waitpid_nohang, ExitStatus, Pgid, Pid, ProcessIdentity, SetpgidError, SetsidError,
    WaitError, WaitTarget,
};
use tx_subsystems::reactor_submit;
use tx_subsystems::signal::{
    step_sigaction, SigDisposition, SigDispositionChange, SignalMask, Signum,
};
use tx_subsystems::thread_runtime::execution::{step_sigprocmask, SigmaskHow, SigprocmaskChange};
use tx_subsystems::thread_runtime::{step_thread_exit, ThreadIdentity};
use tx_subsystems::vfs::structure::Credential;
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::{AddressSpace, UserVirtAddr, VmMapError};
use tx_subsystems::wait_carrier;

pub mod numbers;

#[cfg(test)]
mod tests;

pub use numbers::{
    FD_CLOEXEC, F_GETFD, F_SETFD, NR_BRK, NR_CLONE, NR_EXECVE, NR_EXIT, NR_EXIT_GROUP, NR_FCNTL,
    NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETPGID, NR_GETPGRP, NR_GETPID, NR_GETPPID, NR_GETRESGID,
    NR_GETRESUID, NR_GETSID, NR_GETUID, NR_READ, NR_RT_SIGACTION, NR_RT_SIGPROCMASK, NR_SETGID,
    NR_SETPGID, NR_SETREGID, NR_SETRESGID, NR_SETRESUID, NR_SETREUID, NR_SETSID, NR_SETUID,
    NR_SET_ROBUST_LIST, NR_SET_TID_ADDRESS, NR_WAIT4, NR_WRITE, O_CLOEXEC, SIGCHLD, WNOHANG,
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
/// TODO(phase-userva): lift to platform `PATH_MAX` once the general
/// `copy_from_user` lane lands.
pub const EXECVE_PATH_MAX: usize = 4096;

/// Maximum total argv + envp byte budget per `execve(2)` call.
///
/// Linux's `ARG_MAX` is 128 KiB but the Phase 6 plan caps the inline
/// buffer at 8 KiB to keep the same discipline as the `write` /
/// `sigaction` arms. Overflow returns `-E2BIG`.
///
/// TODO(phase-userva): lift to 128 KiB once general `copy_from_user`
/// lands.
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
pub async fn dispatch<'a, P: PmapIf>(req: SyscallRequest, ctx: &SyscallCtx<'a>) -> SyscallResult {
    match req.nr {
        NR_WRITE => sys_write(req.args, ctx).await,
        NR_READ => sys_read(req.args, ctx).await,
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
        nr if nr == NR_GETPGRP => SyscallResult::Error(ENOSYS_VALUE),
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
    let file = match resolve_fd(&ctx.process, fd as usize) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // SAFETY: Phase 2a accepts kernel-side buffers only. Callers
    // synthesise the syscall request from a kernel-allocated slice
    // (e.g. a test's `b"hello\n".as_ptr() as u64`). General
    // `copy_from_user` over user VAs is deferred (trio plan
    // §"Out of scope" — bounded inline buffers only). Once the
    // userspace-VA copy lane lands, this `from_raw_parts` is replaced
    // by `ctx.aspace.copy_from_user(...)`.
    // TODO(phase-userva): replace with copy_from_user once available.
    let bytes: &[u8] = if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len) }
    };

    // Loop on the canonical async wait discipline pattern from
    // `vm::execution::fault_script`. Each iteration takes a fresh
    // `tx_substrate::epoch::guard()` inside the step's call site so
    // the guard never crosses an `.await`.
    let mut total: usize = 0;
    let mut remaining = bytes;
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
/// treat `args[1]` as a kernel pointer (TODO(phase-userva) bootstrap
/// exemption — same as `write`), loop on the wait-carrier discipline.
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

    let file = match resolve_fd(&ctx.process, fd as usize) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }

    // SAFETY: Phase 2b accepts kernel-side buffers only (matching the
    // Phase 2a `write` exemption — see `sys_write`'s SAFETY comment).
    // TODO(phase-userva): replace with `ctx.aspace.copy_to_user(...)`
    // once the userspace-VA copy lane lands.
    let out: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };

    let mut total: usize = 0;
    let mut cursor: usize = 0;
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            file.step_read(&mut out[cursor..], &guard)
        };
        match outcome {
            StepOutcome::Done(read) | StepOutcome::Advanced(read) => {
                total += read;
                let stop = read == 0 || cursor + read >= len;
                if stop {
                    return SyscallResult::Return(total as i64);
                }
                cursor += read;
            }
            StepOutcome::AdvancedThenBlocked(read, _token) => {
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

    // Read the user-supplied set bitset. SAFETY: Phase 2b accepts
    // kernel-side buffers only (TODO(phase-userva) bootstrap
    // exemption — same as Phase 2a's `write`).
    let next_mask = if set_ptr == 0 {
        SignalMask::EMPTY
    } else {
        // SAFETY: see SAFETY comment in `sys_write`.
        let bits = unsafe { core::ptr::read_unaligned(set_ptr as *const u64) };
        SignalMask::new(bits)
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
        // SAFETY: see SAFETY comment in `sys_write`.
        unsafe {
            core::ptr::write_unaligned(oldset_ptr as *mut u64, prev_mask.raw_bits());
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

    // Decode the new action (if any). SAFETY: Phase 2b kernel-side
    // buffer exemption applies — TODO(phase-userva) for the real
    // copy-from-user lane.
    let new_disposition: Option<SigDisposition> = if act_ptr == 0 {
        None
    } else {
        // SAFETY: see SAFETY comment in `sys_write`.
        let bytes = unsafe { core::slice::from_raw_parts(act_ptr as *const u8, SIGACTION_BYTES) };
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
        // SAFETY: see SAFETY comment in `sys_write`. Write each 8-byte
        // field individually to avoid any host-side struct-layout
        // assumption.
        unsafe {
            let base = oldact_ptr as *mut u64;
            core::ptr::write_unaligned(base, handler_value); // sa_handler
            core::ptr::write_unaligned(base.add(1), 0); // sa_flags (unused)
            core::ptr::write_unaligned(base.add(2), 0); // sa_restorer (unused)
            core::ptr::write_unaligned(base.add(3), 0); // sa_mask (unused)
        }
    }

    SyscallResult::Return(0)
}

/// `fcntl(fd, cmd, arg)` per the Wave 2 ELF-loader plan §"Part 2 —
/// Per-fd CLOEXEC bitmap + fcntl(F_SETFD) + O_CLOEXEC".
///
/// Day-1 covers only `F_GETFD` / `F_SETFD` against the per-process
/// `ProcessPayload.fd_cloexec` bitmap. Other commands return
/// `-ENOSYS` until the relevant follow-up phase
/// (`TODO(phase-fcntl-extension)`) extends the surface — `F_DUPFD`,
/// `F_GETFL`, `F_SETFL`, etc. are out of scope for Wave 2.
///
/// Validation:
/// - `fd >= FD_TABLE_SIZE` (today's day-1 fixed table size, also a
///   strict subset of the 32-bit CLOEXEC bitmap range) → `-EBADF`.
/// - Unknown `cmd` → `-ENOSYS`.
fn sys_fcntl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as u32;
    let cmd = args[1] as i32;
    let arg = args[2];

    if (fd as usize) >= tx_subsystems::process::FD_TABLE_SIZE {
        return SyscallResult::Error(EBADF_VALUE);
    }

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
        // TODO(phase-fcntl-extension): F_DUPFD, F_GETFL, F_SETFL, ...
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
/// SAFETY (kernel-buffer exemption): same Phase 2a discipline as
/// `sys_write` / `sys_read` — the userspace VAs (`path_uaddr`,
/// `argv_uaddr`, `envp_uaddr`) are read through `from_raw_parts`
/// without an `aspace.copy_from_user` indirection. Test scaffolding
/// passes kernel-side pointers directly. Once the userspace-VA copy
/// lane lands the bounded-read helpers below switch over.
/// TODO(phase-userva): replace with `ctx.aspace.copy_from_user(...)`.
async fn sys_execve<'a, P: PmapIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let path_uaddr = args[0];
    let argv_uaddr = args[1];
    let envp_uaddr = args[2];

    // ----- Step 1: bounded read of the path -----
    let path_buf = match read_user_cstr(path_uaddr, EXECVE_PATH_MAX) {
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
    let argv_buf = match read_user_cstr_vec(argv_uaddr, EXECVE_VEC_MAX, &mut remaining) {
        Ok(v) => v,
        Err(ReadVecError::TooBig) => return SyscallResult::Error(E2BIG_VALUE),
    };
    let envp_buf = match read_user_cstr_vec(envp_uaddr, EXECVE_VEC_MAX, &mut remaining) {
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
/// SAFETY: see the SAFETY comment in `sys_write` — kernel-buffer
/// bootstrap exemption applies.
fn read_user_cstr(uaddr: u64, max_len: usize) -> Result<Vec<u8>, ReadCStrError> {
    if uaddr == 0 || max_len == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<u8> = Vec::new();
    out.reserve(core::cmp::min(max_len, 256));
    for offset in 0..max_len {
        // SAFETY: bootstrap kernel-buffer exemption (TODO: phase-userva).
        let byte = unsafe { core::ptr::read_volatile((uaddr as usize + offset) as *const u8) };
        if byte == 0 {
            return Ok(out);
        }
        out.push(byte);
    }
    // Walked the full budget without seeing a NUL — too long.
    Err(ReadCStrError::TooLong)
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
/// SAFETY: see the SAFETY comment in `sys_write`.
fn read_user_cstr_vec(
    uaddr: u64,
    max_slots: usize,
    byte_budget: &mut usize,
) -> Result<Vec<Vec<u8>>, ReadVecError> {
    if uaddr == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    for slot in 0..max_slots {
        let slot_addr = uaddr as usize + slot * core::mem::size_of::<u64>();
        // SAFETY: bootstrap kernel-buffer exemption (TODO: phase-userva).
        let ptr = unsafe { core::ptr::read_volatile(slot_addr as *const u64) };
        if ptr == 0 {
            return Ok(out);
        }
        // Read the string at `ptr`, capped at the remaining byte
        // budget. We need at least one byte for the NUL terminator;
        // when `*byte_budget == 0` any non-empty string is `TooBig`.
        let cap = *byte_budget;
        let s = match read_user_cstr(ptr, cap) {
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

/// Read 8 little-endian bytes from a slice as a `u64`. Used by
/// `sys_rt_sigaction`'s `struct sigaction` decode.
fn read_u64_le(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() >= 8);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

/// Resolve fd `idx` against the process payload's day-1 stub fd table.
/// Returns `None` if the process is a zombie or the slot is empty.
fn resolve_fd(process: &Cap<ProcessIdentity>, idx: usize) -> Option<Cap<OpenFile>> {
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
        Errno::EPERM => 1,
        Errno::EROFS => 30,
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
/// `i32` to the user address. Same kernel-buffer bootstrap exemption
/// as `sys_write` / `sys_read` — `core::ptr::write_volatile` over
/// `wstatus_uaddr` directly. `TODO(phase-userva)`: replace with
/// `ctx.aspace.copy_to_user(...)` once the userspace-VA copy lane lands.
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
                    // SAFETY: kernel-buffer bootstrap exemption per the
                    // Phase 2a / Wave-3-slice plan. Writes a 4-byte
                    // little-endian (RV64-native) i32. TODO(phase-userva):
                    // replace with `ctx.aspace.copy_to_user(...)`.
                    unsafe {
                        core::ptr::write_volatile(wstatus_uaddr as *mut i32, word);
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
/// Wave 2 bootstrap exemption: the three uaddrs are treated as
/// kernel-side via inline `core::ptr::write_volatile` (mirrors
/// `sys_wait4`'s `wstatus` writeback). Linux's real semantics return
/// `-EFAULT` on any invalid pointer; user-VA validation is
/// `TODO(phase-userva)` — once `aspace.copy_to_user` lands the three
/// inline writes switch over.
///
/// SAFETY: kernel-buffer bootstrap exemption (same as `sys_wait4` /
/// `sys_write`).
fn sys_getresuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid_uaddr = args[0];
    let euid_uaddr = args[1];
    let suid_uaddr = args[2];
    let cred = ctx.cred();

    // SAFETY: bootstrap kernel-buffer exemption. TODO(phase-userva).
    if ruid_uaddr != 0 {
        unsafe { core::ptr::write_volatile(ruid_uaddr as *mut u32, cred.uid.raw()) };
    }
    if euid_uaddr != 0 {
        unsafe { core::ptr::write_volatile(euid_uaddr as *mut u32, cred.euid.raw()) };
    }
    if suid_uaddr != 0 {
        unsafe { core::ptr::write_volatile(suid_uaddr as *mut u32, cred.suid.raw()) };
    }

    SyscallResult::Return(0)
}

/// `getresgid(rgid_uaddr, egid_uaddr, sgid_uaddr)`. Gid analog of
/// `sys_getresuid`. Same bootstrap exemption applies.
fn sys_getresgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid_uaddr = args[0];
    let egid_uaddr = args[1];
    let sgid_uaddr = args[2];
    let cred = ctx.cred();

    // SAFETY: bootstrap kernel-buffer exemption. TODO(phase-userva).
    if rgid_uaddr != 0 {
        unsafe { core::ptr::write_volatile(rgid_uaddr as *mut u32, cred.gid.raw()) };
    }
    if egid_uaddr != 0 {
        unsafe { core::ptr::write_volatile(egid_uaddr as *mut u32, cred.egid.raw()) };
    }
    if sgid_uaddr != 0 {
        unsafe { core::ptr::write_volatile(sgid_uaddr as *mut u32, cred.sgid.raw()) };
    }

    SyscallResult::Return(0)
}
