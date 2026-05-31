//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine};
use crate::linux_syscall::numbers::{CLONE_NEWIPC, NR_CLONE};

/// Linux raw `wait4`/`getrusage` rusage image for musl LP64:
/// two `timeval`s plus fourteen `long` counters. musl passes the
/// syscall a pointer adjusted to this 144-byte prefix and keeps the
/// public `struct rusage` reserved tail in libc-owned memory.
const RUSAGE_BYTES: usize = 144;
const SCHED_OTHER: i32 = 0;
const PRIO_PROCESS: i32 = 0;
const NICE_MIN: i32 = -20;
const NICE_MAX: i32 = 19;
const NICE_ZERO_RAW: i64 = 20;
const IOPRIO_WHO_PROCESS: i32 = 1;
const IOPRIO_CLASS_NONE: i32 = 0;
const IOPRIO_CLASS_BE: i32 = 2;
const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_DEFAULT_BE: i32 = IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT;
const RUSAGE_CHILDREN: i32 = -1;
const RUSAGE_SELF: i32 = 0;
const RUSAGE_THREAD: i32 = 1;
const LINUX_DEFAULT_PERSONALITY: u32 = 0;
const PERSONALITY_QUERY: u32 = u32::MAX;

fn emit_clone_marker(name: &[u8]) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            NR_CLONE as i64,
        );
        tx_observe::dump_registered_if_requested();
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
/// PR-3 migration: `ThreadExitOp` is a `OneShotStepOp` — dispatched
/// via `drive_oneshot` (no reactor, no yield).
pub(super) fn sys_exit<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let status = args[0] as i32;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = ThreadExitOp {
        thread: ctx.thread.clone(),
        status,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(()) => SyscallResult::NoReturn,
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `exit_group(status)` — per `PROCESS_v1` §7.3.2.
/// PR-3 migration: `ExitGroupOp` is a `OneShotStepOp` — dispatched
/// via `drive_oneshot` (no reactor, no yield).
/// `gettid()` — return the callers thread id.
pub(super) fn sys_gettid(ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Return(ctx.thread.tid.0 as i64)
}

pub(super) fn sys_exit_group<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let status = args[0] as i32;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = ExitGroupOp {
        process: &ctx.process,
        status: ExitStatus::Exited(status),
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(()) => SyscallResult::NoReturn,
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `getpid()` — direct read of `process.pid` per `PROCESS_v1`
/// §"Step catalog" / `getpid` row in the trio plan.
///
/// No `.await`, no guard — `Pid` is `Copy` and the `pid` field on
/// `ProcessIdentity` is plainly addressable (it does not change after
/// construction).
pub(super) fn sys_getpid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.pid.0 as i64)
}

/// `getcpu(cpup, nodep, unused)`. Linux RV64 generic ABI `__NR_getcpu = 168`.
///
/// v1 has a fixed single-node test/kernel shape. Write CPU 0 and
/// NUMA node 0 when requested; the cache pointer is obsolete on Linux
/// and ignored.
pub(super) fn sys_getcpu<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let cpu_uaddr = args[0];
    let node_uaddr = args[1];
    if cpu_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, cpu_uaddr, 0) {
            return SyscallResult::error_from(errno);
        }
    }
    if node_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, node_uaddr, 0) {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

/// `personality(persona)`. Linux RV64 generic ABI `__NR_personality = 92`.
///
/// txKernel has no personality-dependent execution policy. Support
/// Linux's query sentinel and a no-op set of the default personality;
/// reject all other changes so tests see the unsupported policy
/// boundary explicitly.
pub(super) fn sys_personality(args: [u64; 6]) -> SyscallResult {
    let persona = args[0] as u32;
    match persona {
        PERSONALITY_QUERY | LINUX_DEFAULT_PERSONALITY => {
            SyscallResult::Return(LINUX_DEFAULT_PERSONALITY as i64)
        }
        _ => SyscallResult::Error(EINVAL_VALUE),
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
//
// PR-9 phase 3b: StepOp-driven via ExecOp (10-phase state
// machine).  Async operations yield; the drive loop parks on I/O.
// Remaining synchronous phases return Continue.
pub(super) async fn sys_execve<'a, P: PmapIf + EntropyIf + AuxvIf>(
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

    // (Debug execve-marker observe-reset hook removed once
    // `basename` was traced — the wedge was the TimerId/TimerToken
    // mismatch in `tx_scripts::drive::resolve_on_timer`. See that
    // function for the fix.)

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

    // `exec_script` is the canonical multi-phase async free fn that
    // realises the EXEC_v1 protocol. The StepOp-shaped `ExecOp` wrap
    // in `super::exec_op` is an unfinished refactor; the syscall arm
    // drives `exec_script` directly until that lands.
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
        Ok(()) => {
            ctx.process.notify_vfork_done();
            SyscallResult::ExecCommitted
        }
        Err(e) => SyscallResult::Error(execve_errno_magnitude(e)),
    }
}

/// Translate `ExecError` to the dispatched `-errno` magnitude the
/// Phase 6 syscall arm hands back through `SyscallResult::Error`.
///
/// `ExecError::to_errno_i32` returns the *signed* `-errno`
/// (`-2` for `ENOENT`); `SyscallResult::Error` carries the *positive*
/// magnitude (the userspace-entry shim negates before writing). We
/// flip the sign here so the existing `Error(i32)` discipline is
/// unchanged.
pub(super) fn execve_errno_magnitude(e: ExecError) -> i32 {
    -e.to_errno_i32()
}

// =====================================================================
// Wave 2 of the fork/clone/wait4 slice — Part 2 (NR_CLONE) +
// Part 4 (process-tree introspection arms) + Part 5 (musl-startup
// stubs).
//
// NR_WAIT4 is intentionally absent — it lives in Wave 3 with the
// blocking-wait scaffolding (`exit_source` `WaitToken` await loop). See
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
/// - `args[1]` (`stack`) may be non-zero. libc's `clone(fn, arg,
///   stack, stack_size, SIGCHLD)` wrapper places `fn` and `arg` at
///   that stack pointer before issuing the raw syscall; the child must
///   resume with `sp = stack` so the wrapper can tail-call `fn(arg)`.
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
pub(super) fn sys_clone_oneshot<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> Option<SyscallResult> {
    emit_clone_marker(b"debug.clone.enter");
    let flags = args[0];
    let stack = args[1];

    if (flags & CLONE_VFORK) != 0 && (flags & CLONE_THREAD) == 0 {
        return None;
    }

    // Validation: the lower byte specifies the exit signal.
    // CLONE_THREAD threads don't generate an exit signal (the
    // thread-group leader's exit signal governs process-wide
    // SIGCHLD).  For fork-like clones we require SIGCHLD; for
    // thread clones we accept any signal (including zero — musl
    // sets the lower byte to zero when CLONE_THREAD is set).
    let clone_thread = (flags & CLONE_THREAD) != 0;
    if !clone_thread && flags & SIGCHLD == 0 {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    let clone_vm = (flags & CLONE_VM) != 0;
    let clone_sighand = (flags & CLONE_SIGHAND) != 0;
    let clone_settls = (flags & CLONE_SETTLS) != 0;
    let clone_child_cleartid = (flags & CLONE_CHILD_CLEARTID) != 0;
    let clone_parent_settid = (flags & CLONE_PARENT_SETTID) != 0;
    let clone_newipc = (flags & CLONE_NEWIPC) != 0;

    let allowed_mask = if clone_thread {
        // CLONE_THREAD requires CLONE_SIGHAND per Linux semantics.
        if flags & CLONE_SIGHAND == 0 {
            return Some(SyscallResult::Error(EINVAL_VALUE));
        }
        if clone_newipc {
            return Some(SyscallResult::Error(EINVAL_VALUE));
        }
        SIGCHLD
            | CLONE_THREAD
            | CLONE_VM
            | CLONE_SIGHAND
            | CLONE_SETTLS
            | CLONE_CHILD_CLEARTID
            | CLONE_PARENT_SETTID
            | CLONE_FILES
            | CLONE_FS
            | CLONE_DETACHED
            | CLONE_SYSVSEM
            | CLONE_NEWCGROUP
            | CLONE_NEWUTS
    } else {
        SIGCHLD
            | CLONE_SETTLS
            | CLONE_VM
            | CLONE_VFORK
            | CLONE_SIGHAND
            | CLONE_FILES
            | CLONE_FS
            | CLONE_NEWIPC
    };
    if flags & !allowed_mask != 0 {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    // `stack` (newsp) — Linux semantic: zero means the child shares the
    // parent's sp (bare fork). Non-zero means the libc clone wrapper has
    // staged the child stack (typically with `fn`/`arg` pushed by
    // `__clone`) and wants the child to enter userspace with sp = stack.
    // Seeded into `regs[STACK_REG_INDEX]` by `seed_child_leader_context`
    // below.

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
    emit_clone_marker(b"debug.clone.parent_ctx.after");

    // Txv2's Linux syscall shim receives clone arguments in the
    // asm-generic order used by the current userspace test images:
    //   clone(flags, stack, ptid, tls, ctid)
    // Treating LoongArch64 as ctid/tls here seeds the child thread
    // pointer with the clear_child_tid address; pthread children then
    // spin or fault after the first futex wake.
    let (tls_arg, ctid_arg) = (args[3], args[4]);

    let tls = if clone_settls { tls_arg } else { 0 };

    // ── CLONE_THREAD fast path ─────────────────────────────────
    // Create a new thread within the calling process — no new
    // ProcessIdentity is created.
    if clone_thread {
        let ctid_ptr = if clone_child_cleartid { ctid_arg } else { 0 };
        let mut script_ctx = crate::KernelScriptCtx::new();
        let mut op = tx_subsystems::process::CloneThreadOp {
            process: &ctx.process,
            parent_user_ctx: &parent_user_ctx,
            stack: stack as usize,
            tls: tls as usize,
            ctid_ptr,
        };
        let child_thread = match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(result) => result,
            Err(v3errno) => return Some(SyscallResult::error_from(Errno::from(v3errno))),
        };
        emit_clone_marker(b"debug.clone.step_thread.after");
        let child_thread = match child_thread {
            Ok(t) => t,
            Err(_) => return Some(SyscallResult::Error(ENOMEM_VALUE)),
        };

        // CLONE_PARENT_SETTID: write child tid to *ptid in parent's
        // userspace. Linux semantics: write `child_tid` (as i32)
        // before the child is scheduled.
        if clone_parent_settid {
            let ptid_ptr = args[2];
            if ptid_ptr != 0 {
                let _ = super::user_copy::bootstrap_write_user(
                    &ctx.aspace,
                    ptid_ptr,
                    child_thread.tid.0 as i32,
                );
            }
        }
        emit_clone_marker(b"debug.clone.parent_settid.after");

        // Hand the child thread to the reactor.
        emit_clone_marker(b"debug.clone.reactor_submit.before");
        let child_submit =
            reactor_submit::submit_child_thread(ctx.process.clone(), child_thread.clone());
        emit_clone_marker(b"debug.clone.reactor_submit.after");

        emit_clone_marker(b"debug.clone.return");
        return Some(SyscallResult::CloneReturn {
            value: child_thread.tid.0 as i64,
            child_submit,
        });
    }

    // ── Non-CLONE_THREAD (fork) path ────────────────────────────
    // PR-9 phase 3b: drive `step_fork::<P>` via the `ForkOp::<P>`
    // StepOp wrap, threading a `&mut KernelScriptCtx`. The wrap lifts
    // the `Result<Cap<...>, ForkError>` into `StepOutcome::Done(inner_result)`
    // per its `Output` shape, so the existing match-on-`ForkError`
    // arm is preserved post-unwrap.
    //
    // PR-9 phase 5 (D5 Path A): populate `SubjectContext` from
    // `SyscallCtx`. The subject identifies the calling (parent)
    // process+thread and carries the `Cap<Cred>` snapshot loaded at
    // syscall entry. `step_fork` reads the parent cred internally
    // (via `payload.cred()`) to seed the child — the subject's role
    // here is SUBJ-1 hygiene, not driving the fork-time cred copy.
    let mut script_ctx = build_subject_script_ctx(ctx);
    let fork_result = {
        let mut op = tx_subsystems::process::execution::ForkOp::<P> {
            parent: &ctx.process,
            clone_vm,
            clone_sighand,
            clone_newipc,
            _pmap: core::marker::PhantomData,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(r) => r,
            Err(v3errno) => return Some(SyscallResult::error_from(Errno::from(v3errno))),
        }
    };
    // step_fork: mint a child ProcessIdentity + leader ThreadIdentity
    // + payload + parent.children/pgrp wiring.
    let child = match fork_result {
        Ok(c) => c,
        Err(tx_subsystems::process::ForkError::ParentZombie) => {
            // Impossible by construction — the calling process is the
            // parent and is alive (we're servicing its syscall). Map
            // to ESRCH defensively.
            return Some(SyscallResult::Error(ESRCH_VALUE));
        }
        Err(tx_subsystems::process::ForkError::Vm(_)) => {
            // VmMapError (e.g. a transient WouldBlock or OOM during
            // fork_aspace). Map to EAGAIN — Linux's canonical
            // transient-fork-failure errno.
            return Some(SyscallResult::Error(EAGAIN_VALUE));
        }
        Err(tx_subsystems::process::ForkError::Zone(_)) => {
            return Some(SyscallResult::Error(ENOMEM_VALUE));
        }
        Err(tx_subsystems::process::ForkError::Busy) => {
            return Some(SyscallResult::Error(EAGAIN_VALUE));
        }
        Err(tx_subsystems::process::ForkError::PidNamespace) => {
            return Some(SyscallResult::Error(ENOMEM_VALUE));
        }
    };

    // Resolve the child's leader thread (always at slot 0 by
    // `step_fork`'s post-condition).
    let child_thread = child
        .nth_thread(0)
        .expect(":clone:no-leader: kernel-invariant violation, fresh child has no leader thread");

    // Seed the child's leader trap context with the parent's GPRs
    // (a0 := 0, tp := tls when CLONE_SETTLS, sp := stack when non-zero,
    // pc := pc + 4). Infallible.
    seed_child_leader_context(
        &child_thread,
        &parent_user_ctx,
        tls as usize,
        stack as usize,
    );

    // Hand the child's leader thread to the reactor. Panics with
    // `:clone:no-reactor-seam` if the boot path didn't install the
    // seam — that's a boot-time invariant violation.
    let child_submit = reactor_submit::submit_child_thread(child.clone(), child_thread.clone());

    Some(SyscallResult::CloneReturn {
        value: child.pid.0 as i64,
        child_submit,
    })
}

pub(super) async fn sys_clone<'a, P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if let Some(result) = sys_clone_oneshot::<P>(args, ctx) {
        return result;
    }

    emit_clone_marker(b"debug.clone.enter");
    let flags = args[0];
    let stack = args[1];

    // Validation: the lower byte specifies the exit signal.
    // CLONE_THREAD threads don't generate an exit signal (the
    // thread-group leader's exit signal governs process-wide
    // SIGCHLD).  For fork-like clones we require SIGCHLD; for
    // thread clones we accept any signal (including zero — musl
    // sets the lower byte to zero when CLONE_THREAD is set).
    let clone_thread = (flags & CLONE_THREAD) != 0;
    if !clone_thread && flags & SIGCHLD == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let clone_vm = (flags & CLONE_VM) != 0;
    let clone_sighand = (flags & CLONE_SIGHAND) != 0;
    let clone_vfork = (flags & CLONE_VFORK) != 0;
    let clone_settls = (flags & CLONE_SETTLS) != 0;
    let clone_newipc = (flags & CLONE_NEWIPC) != 0;

    let allowed_mask = SIGCHLD
        | CLONE_SETTLS
        | CLONE_VM
        | CLONE_VFORK
        | CLONE_SIGHAND
        | CLONE_FILES
        | CLONE_FS
        | CLONE_NEWIPC;
    if flags & !allowed_mask != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Snapshot parent's saved trap context. Plan B discipline: the
    // trap shell stored this at trap entry. `None` here means the
    // shell never stored it — a kernel-invariant violation.
    let parent_user_ctx = ctx
        .thread
        .payload_cap()
        .expect(":clone:no-payload: kernel-invariant violation, calling thread had no payload")
        .saved_user_context()
        .expect(":clone:no-context: kernel-invariant violation, parent thread had no saved_user_context");
    emit_clone_marker(b"debug.clone.parent_ctx.after");

    // Txv2's Linux syscall shim receives clone arguments in the
    // asm-generic order used by the current userspace test images:
    //   clone(flags, stack, ptid, tls, ctid)
    let tls = if clone_settls { args[3] } else { 0 };

    // ── Non-CLONE_THREAD (fork) path with CLONE_VFORK ─────────────
    let mut script_ctx = build_subject_script_ctx(ctx);
    let fork_result = {
        let mut op = tx_subsystems::process::execution::ForkOp::<P> {
            parent: &ctx.process,
            clone_vm,
            clone_sighand,
            clone_newipc,
            _pmap: core::marker::PhantomData,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(r) => r,
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };
    let child = match fork_result {
        Ok(c) => c,
        Err(tx_subsystems::process::ForkError::ParentZombie) => {
            return SyscallResult::Error(ESRCH_VALUE);
        }
        Err(tx_subsystems::process::ForkError::Vm(_)) => {
            return SyscallResult::Error(EAGAIN_VALUE);
        }
        Err(tx_subsystems::process::ForkError::Zone(_)) => {
            return SyscallResult::Error(ENOMEM_VALUE);
        }
        Err(tx_subsystems::process::ForkError::Busy) => {
            return SyscallResult::Error(EAGAIN_VALUE);
        }
        Err(tx_subsystems::process::ForkError::PidNamespace) => {
            return SyscallResult::Error(ENOMEM_VALUE);
        }
    };

    let child_thread = child
        .nth_thread(0)
        .expect(":clone:no-leader: kernel-invariant violation, fresh child has no leader thread");

    seed_child_leader_context(
        &child_thread,
        &parent_user_ctx,
        tls as usize,
        stack as usize,
    );
    let child_submit = reactor_submit::submit_child_thread(child.clone(), child_thread.clone());

    if clone_vfork {
        use core::future::Future;
        use core::pin::Pin;
        use core::task::{Context, Poll};

        struct VforkWait<'a> {
            child: &'a Cap<ProcessIdentity>,
            stored: bool,
        }

        impl Future for VforkWait<'_> {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if !self.stored {
                    // Store the parent's waker on the child's
                    // ProcessPayload so the child's exec/exit paths
                    // can wake us.
                    if let Some(payload) = self.child.payload_slot().lock().as_ref() {
                        payload.store_vfork_waiter(cx.waker().clone());
                    }
                    self.stored = true;
                }
                if self.child.is_vfork_done() {
                    return Poll::Ready(());
                }
                Poll::Pending
            }
        }

        VforkWait {
            child: &child,
            stored: false,
        }
        .await;
    }

    // Parent observes the child's pid.
    SyscallResult::CloneReturn {
        value: child.pid.0 as i64,
        child_submit,
    }
}

/// `wait4(pid, status, options, rusage)` — Wave 3 of the fork/clone/wait4
/// slice. The blocking variant: when no zombie matches and `WNOHANG`
/// is unset, the arm parks on the caller's per-process `exit_source`
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
/// A non-NULL `rusage` pointer receives a zero-filled Linux raw LP64
/// `rusage` prefix: two `timeval`s plus fourteen `long` counters
/// (144 bytes). musl keeps the public `struct rusage` reserved tail in
/// libc-owned memory. txKernel doesn't track per-process resource usage
/// today; `TODO(phase-rusage)` populates the counters once accounting
/// state lands.
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
/// The blocking path drives `WaitpidNohangOp` through the v3 StepOp
/// loop. On `Err(WaitError::NoneReady)` the op yields on the parent's
/// exit source; child exit fires that source, and the loop re-runs the
/// synchronous walker. Post-wake re-polling is still required because
/// another waiter may have consumed the same zombie.
///
/// Cites: `txdoc:PROCESS-WAIT-FAMILY-1`
/// (`docs/design/04_process-signals/PROCESS_v1.md` §7.4).
pub(super) async fn sys_wait4<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0] as i64 as i32;
    let wstatus_uaddr = args[1];
    let options = args[2] as i32;
    let rusage_uaddr = args[3];

    // pid → WaitTarget. i32::MIN's negate overflows; reject upfront.
    let target = match pid {
        i32::MIN => return SyscallResult::Error(EINVAL_VALUE),
        -1 => WaitTarget::Any,
        0 => WaitTarget::CallerPgrp,
        p if p > 0 => WaitTarget::Pid(Pid(p as u32)),
        p => {
            // p < -1: any child whose pgid matches `-p`.
            let pgid = (-p) as u32;
            WaitTarget::Pgrp(Pgid(pgid))
        }
    };

    let wnohang = (options & WNOHANG) != 0;

    if wnohang {
        // WNOHANG: one-shot poll, no waiting.
        match step_waitpid_nohang(&ctx.process, target) {
            Ok((child_pid, status)) => {
                if let Err(result) = write_wait4_rusage_if_requested(ctx, rusage_uaddr) {
                    return result;
                }
                if wstatus_uaddr != 0 {
                    let word = status.wait_status_word();
                    if let Err(errno) =
                        bootstrap_write_user::<i32>(&ctx.aspace, wstatus_uaddr, word)
                    {
                        return SyscallResult::error_from(errno);
                    }
                }
                return SyscallResult::Return(child_pid.0 as i64);
            }
            Err(WaitError::NoChildren) => return SyscallResult::Error(ECHILD_VALUE),
            Err(WaitError::NoneReady) => return SyscallResult::Return(0),
        }
    }

    // Blocking wait: drive WaitpidNohangOp through the v3 StepOp loop.
    // The op yields on NoneReady via YieldShape::OnWaitSource; the
    // drive loop parks the parent task, child exit fires the source,
    // and step() is re-called on wake.
    use step_engine::{StepOp, StepOutcome as WaitOutcome, YieldShape};
    use tx_subsystems::process::execution::WaitpidNohangOp;
    let mut op = WaitpidNohangOp {
        parent: &ctx.process,
        target,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    loop {
        match op.step(&mut script_ctx) {
            WaitOutcome::Done(Ok((child_pid, status))) => {
                if let Err(result) = write_wait4_rusage_if_requested(ctx, rusage_uaddr) {
                    return result;
                }
                if wstatus_uaddr != 0 {
                    let word = status.wait_status_word();
                    if let Err(errno) =
                        bootstrap_write_user::<i32>(&ctx.aspace, wstatus_uaddr, word)
                    {
                        return SyscallResult::error_from(errno);
                    }
                }
                return SyscallResult::Return(child_pid.0 as i64);
            }
            WaitOutcome::Done(Err(WaitError::NoChildren)) => {
                return SyscallResult::Error(ECHILD_VALUE);
            }
            WaitOutcome::Done(Err(WaitError::NoneReady)) => {
                // Should not reach here — op yields on NoneReady.
                // Fall through to the exit_source wait.
            }
            WaitOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, interests },
                ..
            } => {
                await_wait_source(ctx, source, interests).await;
            }
            WaitOutcome::Err(e) => return SyscallResult::error_from(e.into()),
            _ => {}
        }
    }
}

fn write_wait4_rusage_if_requested(
    ctx: &SyscallCtx<'_>,
    rusage_uaddr: u64,
) -> Result<(), SyscallResult> {
    if rusage_uaddr == 0 {
        return Ok(());
    }
    let zeros = [0u8; RUSAGE_BYTES];
    bootstrap_copy_to_user(&ctx.aspace, rusage_uaddr, &zeros).map_err(SyscallResult::error_from)
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
pub(super) fn sys_getppid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
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
pub(super) fn sys_setpgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetpgidOp {
        target: &ctx.process,
        new_pgid: Pgid(new_pgid_raw),
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
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
pub(super) fn sys_getpgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
pub(super) fn sys_getsid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
pub(super) fn sys_setsid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetsidOp {
        target: &ctx.process,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(sid) => SyscallResult::Return(sid.0 as i64),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `set_tid_address(tidptr)`.
///
/// Stores the `clear_child_tid` pointer on the calling thread's
/// payload. On thread exit, the kernel atomically writes 0 to *tidptr
/// and issues `FUTEX_WAKE` so pthread_join can observe the transition.
pub(super) fn sys_set_tid_address<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let tidptr = args[0];
    // Store the clear_child_tid pointer on the thread payload.
    // NULL clears the slot; otherwise record the userspace address
    // where a futex wake + zero-write should occur on thread exit.
    if let Some(payload) = ctx.thread.payload_cap() {
        let mut slot = payload.clear_child_tid.lock();
        if tidptr == 0 {
            *slot = None;
        } else {
            *slot = Some(tidptr);
        }
    }
    SyscallResult::Return(ctx.thread.tid.0 as i64)
}

/// `set_robust_list(head, len)`.
///
/// Stores the robust-list head pointer and byte length on the
/// calling thread's payload. The thread-exit path walks the list
/// and marks each futex word as `FUTEX_OWNER_DIED` + wakes waiters.
///
/// Returns `0` unconditionally (Linux returns `0` on success; the
/// only failure is `-EINVAL` for `len % size_of::<usize>() != 0`
/// which we skip — txKernel ignores `len`).
pub(super) fn sys_set_robust_list<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let head = args[0];
    let len = args[1] as usize;
    if let Some(payload) = ctx.thread.payload_cap() {
        let mut slot = payload.robust_list_head.lock();
        *slot = if head == 0 { None } else { Some(head) };
        *payload.robust_list_len.lock() = len;
    }
    SyscallResult::Return(0)
}

/// `get_robust_list(pid, head, len)`.
///
/// musl probes this before enabling robust mutexes. Returning the
/// registered head/len for the current thread is enough for both
/// private and process-shared robust mutex tests; non-current tids are
/// resolved through the global pid/tid namespace when available.
pub(super) fn sys_get_robust_list<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let pid = args[0];
    let head_out = args[1];
    let len_out = args[2];

    let thread = if pid == 0 || pid == ctx.thread.tid.0 as u64 {
        ctx.thread.clone()
    } else {
        match tx_subsystems::process::numbers::resolve_pid_number(pid) {
            Some(tx_subsystems::process::numbers::PidName::Thread(thread)) => thread,
            _ => return SyscallResult::Error(ESRCH_VALUE),
        }
    };

    let Some(payload) = thread.payload_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let head = payload.robust_list_head.lock().unwrap_or(0);
    let len = *payload.robust_list_len.lock() as u64;

    if let Err(errno) = bootstrap_write_user::<u64>(&ctx.aspace, head_out, head) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    if let Err(errno) = bootstrap_write_user::<u64>(&ctx.aspace, len_out, len) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

fn affinity_tid_arg(pid_arg: u64, ctx: &SyscallCtx<'_>) -> Result<u32, SyscallResult> {
    if pid_arg == 0 {
        return Ok(ctx.thread.tid.0);
    }
    u32::try_from(pid_arg).map_err(|_| SyscallResult::Error(ESRCH_VALUE))
}

fn affinity_error_to_syscall(
    err: tx_subsystems::reactor_affinity::ReactorAffinityError,
) -> SyscallResult {
    use tx_subsystems::reactor_affinity::ReactorAffinityError as E;
    match err {
        E::InvalidMask => SyscallResult::Error(EINVAL_VALUE),
        E::NoSuchThread => SyscallResult::Error(ESRCH_VALUE),
        E::NotInstalled => SyscallResult::Error(ENOSYS_VALUE),
    }
}

/// `sched_setaffinity(pid, cpusetsize, mask)`.
///
/// v1 supports the single-`u64` CPU mask shape used by txKernel's HAL
/// `CpuMask`. `pid == 0` targets the calling thread; otherwise the numeric
/// pid is interpreted as a tid, matching Linux's per-thread affinity ABI.
pub(super) fn sys_sched_setaffinity<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let tid = match affinity_tid_arg(args[0], ctx) {
        Ok(tid) => tid,
        Err(err) => return err,
    };
    let cpusetsize = args[1] as usize;
    let mask_ptr = args[2];
    if cpusetsize < core::mem::size_of::<u64>() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if mask_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let mask = match bootstrap_read_user::<u64>(&ctx.aspace, mask_ptr) {
        Ok(mask) => mask,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    match tx_subsystems::reactor_affinity::set_thread_affinity(tid, mask) {
        Ok(()) => SyscallResult::Return(0),
        Err(err) => affinity_error_to_syscall(err),
    }
}

/// `sched_getaffinity(pid, cpusetsize, mask)`.
pub(super) fn sys_sched_getaffinity<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let tid = match affinity_tid_arg(args[0], ctx) {
        Ok(tid) => tid,
        Err(err) => return err,
    };
    let cpusetsize = args[1] as usize;
    let mask_ptr = args[2];
    if cpusetsize < core::mem::size_of::<u64>() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if mask_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let mask = match tx_subsystems::reactor_affinity::get_thread_affinity(tid) {
        Ok(mask) => mask,
        Err(err) => return affinity_error_to_syscall(err),
    };
    match bootstrap_write_user::<u64>(&ctx.aspace, mask_ptr, mask) {
        Ok(()) => SyscallResult::Return(core::mem::size_of::<u64>() as i64),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_getrusage<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let who = args[0] as i32;
    let usage = args[1];
    if !matches!(who, RUSAGE_SELF | RUSAGE_CHILDREN | RUSAGE_THREAD) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if usage == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let raw = [0u8; RUSAGE_BYTES];
    match bootstrap_copy_to_user(&ctx.aspace, usage, &raw) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

fn validate_sched_pid(pid: u64, ctx: &SyscallCtx<'_>) -> Result<(), SyscallResult> {
    if pid == 0 || pid == ctx.process.pid.0 as u64 || pid == ctx.thread.tid.0 as u64 {
        Ok(())
    } else {
        Err(SyscallResult::Error(ESRCH_VALUE))
    }
}

fn validate_sched_policy(policy: i32) -> bool {
    policy == SCHED_OTHER
}

fn validate_self_process_target(
    which: i32,
    who: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<(), SyscallResult> {
    if which != PRIO_PROCESS {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    if who == 0 || who == ctx.process.pid.0 as u64 {
        Ok(())
    } else {
        Err(SyscallResult::Error(ESRCH_VALUE))
    }
}

pub(super) fn sys_sched_getscheduler<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_sched_pid(args[0], ctx) {
        return err;
    }
    SyscallResult::Return(SCHED_OTHER as i64)
}

pub(super) fn sys_sched_setparam<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_sched_pid(args[0], ctx) {
        return err;
    }
    let param = args[1];
    if param == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let sched_priority = match bootstrap_read_user::<i32>(&ctx.aspace, param) {
        Ok(priority) => priority,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if sched_priority != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_sched_getparam<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_sched_pid(args[0], ctx) {
        return err;
    }
    let param = args[1];
    if param == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    match bootstrap_copy_to_user(&ctx.aspace, param, &0i32.to_le_bytes()) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_sched_yield() -> SyscallResult {
    SyscallResult::Return(0)
}

pub(super) fn sys_sched_get_priority_max(args: [u64; 6]) -> SyscallResult {
    if validate_sched_policy(args[0] as i32) {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(EINVAL_VALUE)
    }
}

pub(super) fn sys_sched_get_priority_min(args: [u64; 6]) -> SyscallResult {
    if validate_sched_policy(args[0] as i32) {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(EINVAL_VALUE)
    }
}

pub(super) fn sys_sched_rr_get_interval<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_sched_pid(args[0], ctx) {
        return err;
    }
    let interval = args[1];
    if interval == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let raw = [0u8; 16];
    match bootstrap_copy_to_user(&ctx.aspace, interval, &raw) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_getpriority<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_self_process_target(args[0] as i32, args[1], ctx) {
        return err;
    }
    SyscallResult::Return(NICE_ZERO_RAW)
}

pub(super) fn sys_setpriority<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_self_process_target(args[0] as i32, args[1], ctx) {
        return err;
    }
    let nice = args[2] as i32;
    if !(NICE_MIN..=NICE_MAX).contains(&nice) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_ioprio_get<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_ioprio_target(args[0] as i32, args[1], ctx) {
        return err;
    }
    SyscallResult::Return(IOPRIO_DEFAULT_BE as i64)
}

pub(super) fn sys_ioprio_set<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(err) = validate_ioprio_target(args[0] as i32, args[1], ctx) {
        return err;
    }
    let ioprio = args[2] as i32;
    if ioprio == IOPRIO_CLASS_NONE || ioprio == IOPRIO_DEFAULT_BE {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(EINVAL_VALUE)
    }
}

fn validate_ioprio_target(which: i32, who: u64, ctx: &SyscallCtx<'_>) -> Result<(), SyscallResult> {
    if which != IOPRIO_WHO_PROCESS {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    if who == 0 || who == ctx.process.pid.0 as u64 {
        Ok(())
    } else {
        Err(SyscallResult::Error(ESRCH_VALUE))
    }
}

// =====================================================================
// Slice 7 of the shell-prompt roadmap — fcntl extension + day-1 misc
// syscalls (`kill` / `tkill` / `tgkill` / `getrandom` / `uname` /
// `prlimit64` / `rt_sigreturn`). Each is a small, isolated
// arm that unblocks a specific shell-startup path. F_DUPFD /
// F_DUPFD_CLOEXEC / F_GETFL extensions to fcntl live inside `sys_fcntl`
// itself (see above). See
// `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 7.
// =====================================================================
