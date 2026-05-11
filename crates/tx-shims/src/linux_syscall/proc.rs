//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;

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
pub(super) fn sys_exit<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let status = args[0] as i32;
    step_thread_exit(ctx.thread.clone(), status);
    SyscallResult::NoReturn
}

/// `exit_group(status)` — per `PROCESS_v1` §7.3.2.
pub(super) fn sys_exit_group<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
pub(super) fn sys_getpid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.pid.0 as i64)
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
// PR-9 phase 3b: not yet StepOp-driven — pending. `sys_execve`
// invokes `exec_script::<P>(...).await` (multi-phase async free
// fn) — no `*Op` wrap exists for the exec orchestration today.
// When `exec_script` gains a StepOp wrap (or is decomposed into a
// pipeline of wraps), thread `&mut KernelScriptCtx` here.
pub static SYS_EXECVE_INVOCATIONS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static SYS_CLONE_INVOCATIONS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static SYS_WAIT4_INVOCATIONS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static SYS_EXECVE_LAST_ERRNO: core::sync::atomic::AtomicI32 =
    core::sync::atomic::AtomicI32::new(0);

pub(super) async fn sys_execve<'a, P: PmapIf + EntropyIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    SYS_EXECVE_INVOCATIONS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
pub static SYS_CLONE_LAST_FLAGS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static SYS_CLONE_LAST_REJECT_FLAGS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub(super) fn sys_clone<'a, P: PmapIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    SYS_CLONE_INVOCATIONS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let flags = args[0];
    let stack = args[1];
    SYS_CLONE_LAST_FLAGS.store(flags, core::sync::atomic::Ordering::Relaxed);

    // Validation: bare-SIGCHLD only. Reject any other flag combo
    // (CLONE_VM, CLONE_VFORK, pthread_create OR-set, zero, etc.) and
    // any non-zero stack.
    if flags != SIGCHLD {
        SYS_CLONE_LAST_REJECT_FLAGS.store(flags, core::sync::atomic::Ordering::Relaxed);
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
    use tx_substrate::step_v3::{StepOp, StepOutcome as V3Fork};
    let mut script_ctx = build_subject_script_ctx(ctx);
    let fork_result = {
        let mut op = tx_subsystems::process::execution::ForkOp::<P> {
            parent: &ctx.process,
            _pmap: core::marker::PhantomData,
        };
        match op.step(&mut script_ctx) {
            V3Fork::Done(r) => r,
            V3Fork::Err(_) | V3Fork::Continue { .. } | V3Fork::Yield { .. } => {
                return SyscallResult::Error(EAGAIN_VALUE);
            }
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
/// from `ctx.process.exit_source_wait_token()` and `wait_source::wait_on_token`
/// it. Post-wake, loop and re-poll: a third party may have reaped the
/// same zombie (e.g. another wait4 caller in the same process; or the
/// shared `INIT_PROCESS` reaper if init wakes first), so the second
/// poll may still return `NoneReady` — re-park.
///
/// Cites: `txdoc:PROCESS-WAIT-FAMILY-1`
/// (`docs/design/04_process-signals/PROCESS_v1.md` §7.4).
pub(super) async fn sys_wait4<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    SYS_WAIT4_INVOCATIONS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
                // Build the WaitToken from the parent's exit_source
                // carrier id (registered at payload-sign time, Wave 1).
                // `None` means the calling process is itself a zombie
                // — race against our own exit; surface as -ECHILD per
                // POSIX (no children to wait for from a dead process).
                let Some(token) = ctx.process.exit_source_wait_token() else {
                    return SyscallResult::Error(ECHILD_VALUE);
                };
                if let Some(future) = wait_source::wait_on_token(token) {
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
pub(super) fn sys_set_tid_address<'a>(_args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
pub(super) fn sys_set_robust_list(_args: [u64; 6]) -> SyscallResult {
    SyscallResult::Return(0)
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
pub(super) fn sys_getpgrp<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.process.pgrp_cap().pgid.0 as i64)
}
