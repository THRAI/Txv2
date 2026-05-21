//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use tx_subsystems::process::numbers::{resolve_pid_number, PidName};
use tx_subsystems::signal::{step_kill_pgrp, SigInfo, SI_USER};
use tx_subsystems::signal::{KillOutcome, SignalTarget};

#[repr(C)]
#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct SigaltstackLayout {
    ss_sp: u64,
    ss_flags: i32,
    _pad: u32,
    ss_size: u64,
}

const _: () = assert!(core::mem::size_of::<SigaltstackLayout>() == 24);
const SS_DISABLE: i32 = 2;
const SS_AUTODISARM: i32 = 1 << 31;

/// Drain `SignalDelivered` events from a thread mailbox.
///
/// The `tx-scripts::drive` inner mailbox-await re-posts any
/// `SignalDelivered` event it consumes so the calling syscall's
/// outer loop can re-observe the signal. For `sys_rt_sigtimedwait`,
/// we consume signals directly from `payload.pending()` — the
/// re-posted mailbox event is stale information that would otherwise
/// trap every subsequent `drive(NanosleepOp)` call into returning
/// immediately, degenerating the 5 ms poll cadence to a no-op spin.
/// This helper drains every queued `SignalDelivered` (preserving any
/// other events like `TimerFired` or `SourceFired` that are still
/// genuinely informative for the next park).
fn drain_stale_signal_events(mailbox: &crate::adapter::reactor_entry::TaskMailbox) {
    use crate::adapter::reactor_entry::MailboxEvent;
    let mut keep = alloc::vec::Vec::new();
    while let Some(event) = mailbox.poll() {
        if !matches!(event, MailboxEvent::SignalDelivered { .. }) {
            keep.push(event);
        }
    }
    for event in keep {
        let _ = mailbox.post(event);
    }
}

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
            Err(errno) => return SyscallResult::error_from(errno),
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
                    return SyscallResult::error_from(Errno::from(v3errno));
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
                    return SyscallResult::error_from(Errno::from(v3errno));
                }
            }
        }
    };

    if oldset_ptr != 0 {
        if let Err(errno) =
            bootstrap_write_user::<u64>(&ctx.aspace, oldset_ptr as u64, prev_mask.raw_bits())
        {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

// --- Stub syscalls (deferred to post-bringup) -------------------------

pub(super) fn sys_rt_sigsuspend(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

/// `sigaltstack(ss, old_ss)` — Linux LP64 `stack_t` query/update.
///
/// This records the registered alternate stack in
/// `ThreadPayload.alt_stack` and reports it back through `old_ss`.
/// Signal-frame delivery still uses the current user stack today, so
/// `SA_ONSTACK` delivery semantics remain deferred; the syscall
/// pointer contract itself is Linux/musl-shaped.
pub(super) fn sys_sigaltstack(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let ss_ptr = args[0];
    let old_ss_ptr = args[1];
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };

    if old_ss_ptr != 0 {
        let (ss_sp, ss_flags, ss_size) = match payload.alt_stack() {
            Some((base, size)) => (base as u64, 0, size as u64),
            None => (0, SS_DISABLE, 0),
        };
        let old = SigaltstackLayout {
            ss_sp,
            ss_flags,
            _pad: 0,
            ss_size,
        };
        if let Err(errno) = bootstrap_write_user::<SigaltstackLayout>(&ctx.aspace, old_ss_ptr, old)
        {
            return SyscallResult::error_from(errno);
        }
    }

    if ss_ptr != 0 {
        let new = match bootstrap_read_user::<SigaltstackLayout>(&ctx.aspace, ss_ptr) {
            Ok(value) => value,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        let allowed = SS_DISABLE | SS_AUTODISARM;
        if new.ss_flags & !allowed != 0 {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if new.ss_flags & SS_DISABLE != 0 {
            payload.set_alt_stack(None);
        } else {
            if new.ss_size < MINSIGSTKSZ {
                return SyscallResult::Error(ENOMEM_VALUE);
            }
            payload.set_alt_stack(Some((new.ss_sp as usize, new.ss_size as usize)));
        }
    }

    SyscallResult::Return(0)
}

pub(super) fn sys_rt_sigqueueinfo(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` — Linux RV64
/// ABI `__NR_rt_sigtimedwait = 137`.
///
/// Polls the calling thread's pending-signal bitset for any signal
/// in `set`, with the given timeout (NULL = block forever).
/// Returns the first matching signum on success, `-EAGAIN` on
/// timeout. Does NOT invoke the signal handler — the signal is
/// consumed from `payload.pending()` instead.
///
/// Implementation: synchronous poll loop. Each iteration reads
/// `payload.pending()`, checks for any bit also set in `set`, and
/// if found, clears that bit and returns its signum. Between
/// polls the calling task awaits a short [`NanosleepOp`] so other
/// reactor tasks (notably any sibling thread that will post the
/// signal — usually `post_sigchld_to_parent` for a child exit)
/// get a chance to run.
///
/// Carved out as the libctest unblock per `SYSCALL_STATUS.md`'s
/// "wire `sys_rt_sigtimedwait`" high-stakes row (libctest's
/// `runtest.c` uses `sigtimedwait(SIGCHLD, …)` to wait for child
/// processes; without a real implementation it returns `-ENOSYS`
/// and every test scores 0/220 with `[signal Killed]`).
pub(super) async fn sys_rt_sigtimedwait<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let set_uaddr = args[0];
    let info_uaddr = args[1];
    let timeout_uaddr = args[2];
    let sigsetsize = args[3] as usize;

    // The slice only supports the canonical 8-byte sigset_t on RV64;
    // mirrors the `sys_rt_sigprocmask` precedent (`SIGSETSIZE_BYTES`).
    if sigsetsize != SIGSETSIZE_BYTES as usize {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if set_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    // Read the requested signal set (64-bit bitset).
    let set_bits: u64 = match bootstrap_read_user::<u64>(&ctx.aspace, set_uaddr) {
        Ok(v) => v,
        Err(_) => return SyscallResult::Error(EFAULT_VALUE),
    };
    if set_bits == 0 {
        // Empty set — no signal can ever match. Block until timeout.
    }

    // Read the timeout. NULL means block forever (we cap at a
    // generous u64::MAX/2 sentinel below since the reactor doesn't
    // actually park us for years — each polling iteration sleeps
    // ~5ms and re-checks).
    let timeout_ns: u64 = if timeout_uaddr == 0 {
        u64::MAX / 2
    } else {
        match read_timespec_at(&ctx.aspace, timeout_uaddr) {
            Some(ns) => ns,
            None => return SyscallResult::Error(EINVAL_VALUE),
        }
    };

    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };

    let start_ns = <P as tx_hal::TimeIf>::read_ns();
    let deadline_ns = start_ns.saturating_add(timeout_ns);

    // `await_mailbox_event` (in `tx-scripts::drive::resolve_on_timer`)
    // RE-POSTS any `SignalDelivered` event it consumes back to the
    // mailbox so the caller's main loop can observe it. For
    // sigtimedwait, the "main loop" IS this loop, and we consume
    // signals directly from `payload.pending()` — so the re-posted
    // event is stale information. If left in the queue it traps every
    // subsequent `drive(NanosleepOp)` call into returning Ready(true)
    // immediately, creating a no-actual-sleep tight loop. Drain any
    // pre-existing `SignalDelivered` events left over from a previous
    // sigtimedwait call on this same thread before the poll loop
    // starts; the per-iteration drain below handles new arrivals.
    let mailbox_for_drain = crate::adapter::reactor_entry::current_task_mailbox();
    if let Some(ref mbox) = mailbox_for_drain {
        drain_stale_signal_events(mbox);
    }

    // Poll-and-yield loop. 5 ms chunks: long enough that we don't
    // spin-burn the reactor, short enough that libctest tests with
    // sub-second test bodies (most of them) react promptly to a
    // child-exit-posted SIGCHLD.
    const CHUNK_NS: u64 = 5_000_000;
    loop {
        // Fast path: consume the first matching pending bit.
        let pending = payload.pending().snapshot() & set_bits;
        if pending != 0 {
            let signum_raw = (pending.trailing_zeros() + 1) as u8;
            if let Some(sig) = tx_subsystems::signal::Signum::new(signum_raw) {
                payload.pending().clear(sig);
                // Optionally write the siginfo struct. We don't
                // synthesise full siginfo — kernel-posted SIGCHLD
                // carries enough state via wait4 — but a non-zero
                // `info_uaddr` deserves at least a zeroed-out buffer
                // so the caller sees a valid struct shape rather
                // than uninitialised stack memory.
                if info_uaddr != 0 {
                    let zeros = [0u8; 128];
                    let _ = bootstrap_copy_to_user(&ctx.aspace, info_uaddr, &zeros);
                }
                return SyscallResult::Return(signum_raw as i64);
            }
        }

        // Timeout check before the next yield.
        let now_ns = <P as tx_hal::TimeIf>::read_ns();
        if now_ns >= deadline_ns {
            return SyscallResult::Error(EAGAIN_VALUE);
        }

        // Async-friendly chunk wait. Reuse the `NanosleepOp`
        // machinery so we yield on the timer wheel; the next reactor
        // poll re-enters this loop. **Critically**, pass the calling
        // task's mailbox to `drive()` — without it,
        // `resolve_on_timer` short-circuits to `Retry` immediately
        // and the "5 ms sleep" becomes a no-op spin (parent then
        // hogs the reactor so the child never runs).
        let chunk = CHUNK_NS.min(deadline_ns.saturating_sub(now_ns));
        use crate::adapter::reactor_entry::current_task_mailbox;
        use crate::adapter::step_engine::DriveMode;
        use tx_scripts::drive;
        let mut script_ctx = build_subject_script_ctx(ctx);
        let timer_wheel_arc = script_ctx.timer_wheel().cloned();
        let mailbox = current_task_mailbox();
        let op = super::NanosleepOp {
            nanos: chunk,
            deadline_ns: now_ns.saturating_add(chunk),
            started: false,
        };
        let _ = drive(
            op,
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox.as_ref(),
            None,
            timer_wheel_arc.as_ref(),
        )
        .await;

        // After drive returns, drain any `SignalDelivered` events
        // the inner mailbox-await re-posted. See the matching
        // comment at the top of this function — without this
        // drain, a single stale SignalDelivered traps every
        // subsequent drive iteration into a tight loop and the
        // 5 ms sleep degenerates to a no-op spin.
        if let Some(ref mbox) = mailbox {
            drain_stale_signal_events(mbox);
        }
    }
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
            return SyscallResult::error_from(errno);
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
                    return SyscallResult::error_from(Errno::from(v3errno));
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
            return SyscallResult::error_from(errno);
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
        // Route through script_kill_pgrp (cred-checked per-member fanout)
        // rather than the primitive step_kill_pgrp, which would deliver
        // without consulting cred::require_signal_send.
        // POSIX: kill(0, sig) returns -EPERM when no member was both
        // live AND permitted; script_kill_pgrp folds zombies and
        // permission denials into the same 0-count return.
        return dispatch_errno(
            tx_subsystems::signal::script_kill_pgrp(&ctx.process, &pgrp, signum),
            |n| {
                if n > 0 {
                    SyscallResult::Return(0)
                } else {
                    SyscallResult::Error(EPERM_VALUE)
                }
            },
        );
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

    // Route through the cred-checked script entry point. Drives
    // `cred::require_signal_send` against the caller's syscall-entry
    // snapshot (per cred_service_v_1 §"In flight" + §"Checks
    // surface") and only then commits the post via `step_kill_process`.
    // Going through `KillProcessOp::drive_oneshot` directly would
    // bypass the cred check, since `KillProcessOp::step` calls the
    // primitive `step_kill_process` without authorization.
    use tx_subsystems::signal::KillScriptOutcome;
    dispatch_errno(
        tx_subsystems::signal::script_kill_process(&ctx.process, &target, signum, siginfo),
        |outcome| match outcome {
            KillScriptOutcome::Delivered | KillScriptOutcome::Probed => SyscallResult::Return(0),
            KillScriptOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
        },
    )
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
        // Route through the cred-checked thread-deliver script.
        // POSIX: tkill(tid, sig) is permission-governed by the same
        // rule as kill(pid, sig) (txKernel has no per-thread cred;
        // require_signal_send resolves against the owning process's
        // cred). The previous DeliverSignalOp drive bypassed this.
        return dispatch_errno(
            tx_subsystems::signal::script_deliver_signal(
                &ctx.process,
                SignalTarget::Thread(thread_cap),
                signum,
            ),
            |outcome| match outcome {
                KillOutcome::Delivered => SyscallResult::Return(0),
                KillOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
            },
        );
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
    // Validate tgid. v1 limitation: tgid must match the caller's pid
    // (caller may only tgkill threads in its own thread group). This
    // is what makes the cred check below trivially self-permitted —
    // when cross-process tgkill lands the dispatch must route through
    // `script_deliver_signal` (cred-checked) the way sys_tkill does.
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
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    }
    SyscallResult::Error(ESRCH_VALUE)
}

/// `rt_sigreturn(...)` — Linux RV64 generic ABI
/// `__NR_rt_sigreturn = 139`.
///
/// Restore the pre-handler trap context that signal delivery parked
/// in `payload.saved_signal_context` before flipping
/// `saved_user_context` to the handler-entry context. After the
/// handler `ret`s through the stack trampoline (`addi a7, 0, 139;
/// ecall`), this syscall fires; we move the parked context back into
/// `saved_user_context` so the thread re-enters userspace exactly
/// where the signal interrupted it.
///
/// `SigreturnRestored` tells the syscall-return path in
/// `thread_future` to skip its normal `pending_syscall_return` drain
/// — the merged context's `a0` and `pc` come from the restored
/// pre-signal snapshot, not the syscall's nominal return value.
///
/// Without this restore, the handler's "post-`ret`" path stays in
/// the trampoline / signal-frame memory and the thread reads garbage
/// off the signal stack — observed end-to-end as the busybox-sh
/// SIGCHLD-handler crash on 2026-05-18 (root cause traced through
/// the trap-trace serial log).
///
/// If no signal frame is in flight, the kernel has no parked
/// context to restore. POSIX leaves this case undefined; we return
/// `-EFAULT` defensively rather than corrupt the current context.
#[cfg(target_arch = "loongarch64")]
pub const USER_CONTEXT_SP_INDEX: usize = 3;
#[cfg(target_arch = "riscv64")]
pub const USER_CONTEXT_SP_INDEX: usize = 2;
#[cfg(not(any(target_arch = "loongarch64", target_arch = "riscv64")))]
pub const USER_CONTEXT_SP_INDEX: usize = 2;

pub(super) fn sys_rt_sigreturn<P: tx_hal::SignalFrameIf>(ctx: &SyscallCtx) -> SyscallResult {
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };

    // Gate: a parked signal context must exist — it was stored by the
    // AST checkpoint in `thread_future.rs` when delivering the handler.
    // Without one, no signal frame is in flight and we refuse.
    let Some(saved_signal) = payload.take_saved_signal_context() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };

    // Try the stack-based restore path.  In production the current
    // `saved_user_context` holds the handler-entry register state
    // whose SP points at the signal frame on the user stack.  The HAL
    // `read_signal_frame` reads the `ucontext_t` from that frame,
    // including any modifications the user-space signal handler made
    // (e.g. musl's cancel handler redirecting PC to `__cancel`).
    //
    // In host tests (or if saved_user_context is absent / the stack is
    // corrupted), we fall back to the parked `saved_signal` directly.
    let stack_frame = payload.saved_user_context().and_then(|saved_user| {
        let user_sp = saved_user.regs[USER_CONTEXT_SP_INDEX];
        <P as tx_hal::SignalFrameIf>::read_signal_frame(tx_hal::UserPtr::new(user_sp)).ok()
    });

    match stack_frame {
        Some(frame) => {
            // Stack frame successfully read — use its (possibly
            // handler-modified) register context.
            let mut restored_ctx = frame.user_context;
            // If the handler did NOT redirect PC (normal signal return),
            // and the PC was rewound during signal delivery,
            // advance past the ecall instruction that was rewound during
            // delivery so the thread resumes at the instruction after
            // the trapping syscall.
            if restored_ctx.pc == saved_signal.pc && saved_signal.regs[0] == 1 {
                restored_ctx.pc = restored_ctx.pc.wrapping_add(4);
            }
            payload.store_signal_mask(SignalMask::new(frame.saved_mask.bits));
            payload.store_saved_user_context(Some(restored_ctx));
        }
        None => {
            // Stack read unavailable or failed — restore the parked
            // pre-signal context as-is.  Without a readable stack frame
            // we cannot tell whether the handler redirected PC, so we
            // restore the exact parked snapshot.
            payload.store_saved_user_context(Some(saved_signal));
        }
    }

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
        return SyscallResult::error_from(errno);
    }

    SyscallResult::Return(0)
}
