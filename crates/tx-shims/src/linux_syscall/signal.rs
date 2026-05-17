//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use tx_subsystems::process::numbers::{resolve_pid_number, PidName};
use tx_subsystems::signal::{step_kill_pgrp, SigInfo, SI_USER};
use tx_subsystems::signal::{KillOutcome, SignalTarget, deliver_posix_signal};

/// `rt_sigprocmask(how, set, oldset, sigsetsize)` per `SIGNAL_v1` §3.
///
/// `sigsetsize` is rejected with `-EINVAL` for any value other than
/// `8` (the kernel's only supported sigset width on RV64 — a single
/// `u64` bitset). `set_ptr == 0` means "query only"; `oldset_ptr == 0`
/// means "don't return the previous mask".
pub(super) fn sys_rt_sigprocmask<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
        Some(how) => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigprocmaskOp {
                thread: ctx.thread.clone(),
                how,
                next: next_mask,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(SigprocmaskChange::Replaced { prev, .. }) => prev,
                Ok(SigprocmaskChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::Error(errno_to_i32(Errno::from(v3errno)));
                }
            }
        }
        None => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigprocmaskOp {
                thread: ctx.thread.clone(),
                how: SigmaskHow::Block,
                next: SignalMask::EMPTY,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(SigprocmaskChange::Replaced { prev, .. }) => prev,
                Ok(SigprocmaskChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::Error(errno_to_i32(Errno::from(v3errno)));
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

// --- Stub syscalls (deferred to post-bringup) -------------------------

pub(super) fn sys_rt_sigsuspend(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_sigaltstack(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_rt_sigqueueinfo(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_rt_sigtimedwait(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_pidfd_open(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_pidfd_send_signal(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

/// `rt_sigaction(signum, act, oldact, sigsetsize)` per `SIGNAL_v1`
/// §15.1.
///
/// Decodes a 32-byte kernel `struct sigaction` (see `SIGACTION_BYTES`
/// for the layout citation). `act_ptr == 0` queries the current
/// disposition without changing it; `oldact_ptr == 0` discards the
/// previous disposition.
pub(super) fn sys_rt_sigaction<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
        Some(disp) => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigactionOp {
                process: ctx.process.clone(),
                sig,
                disposition: disp,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(SigDispositionChange::Replaced { prev }) => prev,
                Ok(SigDispositionChange::Uncatchable(prev)) => prev,
                Ok(SigDispositionChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::Error(errno_to_i32(Errno::from(v3errno)));
                }
            }
        }
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
pub(super) fn sys_kill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let pid = args[0] as i32;
    let sig = args[1] as u32;

    // Phase F: pid == 0 routes to caller's process group.
    if pid == 0 {
        if sig == 0 {
            return SyscallResult::Return(0); // existence probe
        }
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        let pgrp = ctx.process.pgrp_cap();
        let delivered = step_kill_pgrp(&pgrp, signum);
        return if delivered > 0 {
            SyscallResult::Return(0)
        } else {
            SyscallResult::Error(ESRCH_VALUE)
        };
    }
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

    let siginfo = Some(SigInfo {
        si_signo: signum.raw() as u32,
        si_code: SI_USER,
        si_pid: ctx.process.pid.0,
        si_uid: 0, // TODO: populate from cred when available
    });

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = KillProcessOp {
        target: target.clone(),
        sig: signum,
        info: siginfo,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(KillOutcome::Delivered) => SyscallResult::Return(0),
        Ok(KillOutcome::NoLiveThread) => SyscallResult::Error(ESRCH_VALUE),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `tkill(tid, sig)` — Linux RV64 generic ABI `__NR_tkill = 130`.
///
/// Slice 7 v1 aliases this to [`sys_kill`]: txKernel has no
/// per-thread signal state machine yet, so `tkill(tid, sig)` is
/// treated as `kill(tid, sig)` (the tid is interpreted as a pid).
pub(super) fn sys_tkill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let tid = args[0];
    let sig = args[1] as u32;

    if sig > 64 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Resolve tid → ThreadIdentity via PidName namespace
    if let Some(PidName::Thread(thread_cap)) = resolve_pid_number(tid) {
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = DeliverSignalOp {
            target: SignalTarget::Thread(thread_cap),
            sig: signum,
        };
        return match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(KillOutcome::Delivered) => SyscallResult::Return(0),
            Ok(KillOutcome::NoLiveThread) => SyscallResult::Error(ESRCH_VALUE),
            Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
        };
    }

    // Fall back to process-level kill
    sys_kill(args, ctx)
}

/// `tgkill(tgid, tid, sig)` — Linux RV64 generic ABI
/// `__NR_tgkill = 131`.
///
/// Phase 4: validates tgid matches the caller's pid, then posts
/// directly to the target thread. Falls back to `sys_kill` for
/// single-threaded compatibility.
pub(super) fn sys_tgkill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let tgid = args[0] as u32;
    let tid = args[1] as u32;
    let sig = args[2] as u32;
    // Validate tgid.
    if tgid != ctx.process.pid.0 {
        return SyscallResult::Error(ESRCH_VALUE);
    }
    if sig == 0 {
        return SyscallResult::Return(0);
    }
    let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
        Some(s) => s,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if let Some(thread) = ctx.process.thread_by_tid(tid) {
        let siginfo = Some(SigInfo {
            si_signo: signum.raw() as u32,
            si_code: SI_USER,
            si_pid: ctx.process.pid.0,
            si_uid: 0,
        });
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = ThreadKillOp {
            thread,
            sig: signum,
            info: siginfo,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(()) => return SyscallResult::Return(0),
            Err(v3errno) => return SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
        }
    }
    SyscallResult::Error(ESRCH_VALUE)
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
/// Phase B: returns `SigreturnRestored` to wire the dispatch path
/// without actual frame mechanics (TODO phase-signal-frame / Phase D).
pub(super) fn sys_rt_sigreturn(_ctx: &SyscallCtx) -> SyscallResult {
    // Phase B: dispatch plumbing only — actual SignalFrameIf restore
    // (read_signal_frame + restore_signal_frame) lands in Phase D.
    SyscallResult::SigreturnRestored
}

/// `rt_sigpending(set, sigsetsize)` — Linux RV64 generic ABI
/// `__NR_rt_sigpending = 136`.
///
/// Phase F: reads the calling thread's pending signal bitset
/// (thread_pending merged with the owning process's group_pending)
/// and writes it to `set`.
pub(super) fn sys_rt_sigpending(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let set_ptr = args[0] as usize;
    let sigsetsize = args[1];

    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // TODO(merge-fixup): upgrade_operational removed from Cap<ThreadIdentity>;
    // stub with empty pending set until thread payload accessor lands.
    let pending: u64 = 0;
    if let Err(errno) = bootstrap_write_user::<u64>(&ctx.aspace, set_ptr as u64, pending) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    SyscallResult::Return(0)
}
