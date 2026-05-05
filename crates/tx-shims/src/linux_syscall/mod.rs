//! Linux syscall dispatch table — Phase 2a slice.
//!
//! Phase 2a deliverable per the Trio plan
//! (`docs/progress/plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md`
//! §"Phasing" item 2): only `NR_WRITE`, `NR_EXIT`, `NR_EXIT_GROUP`,
//! `NR_GETPID` are implemented; everything else returns `-ENOSYS`.
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

use tx_reactor::userspace::SyscallRequest;
use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::process::{step_exit_group, ExitStatus, ProcessIdentity};
use tx_subsystems::thread_runtime::{step_thread_exit, ThreadIdentity};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::wait_carrier;

pub mod numbers;

#[cfg(test)]
mod tests;

pub use numbers::{NR_EXIT, NR_EXIT_GROUP, NR_GETPID, NR_WRITE};

/// Maximum number of input bytes the Phase 2a `write` syscall accepts
/// in a single call. The dispatcher copies `[buf_ptr, buf_ptr+len)` into
/// a kernel-side stack-bounded slice (via `from_raw_parts`); higher-level
/// `copy_from_user` machinery is deferred per the trio plan §"Out of
/// scope". 4 KiB matches a single page; values above that should batch
/// across multiple write calls until the userspace-VA copy lane lands.
pub const TTY_WRITE_MAX_INLINE: usize = 4096;

/// Linux generic ABI errno value for "function not implemented" (`ENOSYS`).
/// Used as the `-ENOSYS` magnitude returned from `dispatch` for every
/// syscall number not handled by Phase 2a.
const ENOSYS_VALUE: i32 = 38;
/// Linux generic ABI errno value for "bad file descriptor" (`EBADF`).
const EBADF_VALUE: i32 = 9;
/// Linux generic ABI errno value for "argument list too long" (`E2BIG`).
/// Used when a syscall argument violates a Phase 2a slice bound (e.g.
/// `write(len > TTY_WRITE_MAX_INLINE)`).
const E2BIG_VALUE: i32 = 7;

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
pub async fn dispatch<'a>(req: SyscallRequest, ctx: &SyscallCtx<'a>) -> SyscallResult {
    match req.nr {
        NR_WRITE => sys_write(req.args, ctx).await,
        NR_EXIT => sys_exit(req.args, ctx),
        NR_EXIT_GROUP => sys_exit_group(req.args, ctx),
        NR_GETPID => sys_getpid(ctx),
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
        Errno::EFAULT => 14,
        Errno::EINVAL => 22,
        Errno::EIO => 5,
        Errno::EISDIR => 21,
        Errno::ENAMETOOLONG => 36,
        Errno::ENODEV => 19,
        Errno::ENOMEM => 12,
        Errno::ENOENT => 2,
        Errno::ENOSYS => ENOSYS_VALUE,
        Errno::ENOTDIR => 20,
        Errno::EPERM => 1,
        Errno::EROFS => 30,
        Errno::ESRCH => 3,
        Errno::ESTALE => 116,
    }
}
