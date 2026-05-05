//! Thread-runtime execution: thread-exit step, signal-mask updates,
//! and the internal helpers used by the signal shim.

use core::sync::atomic::Ordering;

use tx_substrate::zone::Cap;

use crate::signal::{SignalMask, Signum};
use crate::thread_runtime::structure::ThreadIdentity;

/// Mark a thread zombie: set its exit status, drop its payload. Does
/// not touch the parent process's thread list — callers that need
/// parent-side bookkeeping (e.g. `step_thread_exit`) do that
/// themselves; callers that already hold the parent payload (e.g.
/// `process::step_exit_group`) skip it.
pub(crate) fn set_thread_zombie(thread: &Cap<ThreadIdentity>, status: i32) {
    *thread.exit_status.lock() = Some(status);
    *thread.payload.lock() = None;
}

/// Single-thread exit. Marks the thread zombie, removes it from the
/// owning process's thread list, and zombifies the process if this was
/// the last thread.
pub fn step_thread_exit(thread: Cap<ThreadIdentity>, status: i32) {
    set_thread_zombie(&thread, status);

    let guard = tx_substrate::epoch::guard();
    let Some(parent) = thread.owner_proc.upgrade(&guard) else {
        return;
    };
    drop(guard);

    let payload_guard = parent.payload.lock();
    let was_last = match payload_guard.as_ref() {
        Some(payload) => {
            let mut threads = payload.threads.lock();
            threads.retain(|t| t.key() != thread.key());
            threads.is_empty()
        }
        None => return,
    };
    drop(payload_guard);

    if was_last {
        // Thread side carries `i32` per `THREAD_RUNTIME_v1` §7.2;
        // the cascade promotes that to `ExitStatus::Exited` because
        // signal-driven termination doesn't reach this path (it goes
        // through `step_exit_group_with_signal` which records
        // `ExitStatus::Signaled` directly before zombifying threads).
        crate::process::execution::step_process_exit(
            &parent,
            crate::process::structure::ExitStatus::Exited(status),
        );
    }
}

/// Outcome of `step_sigprocmask`. `Replaced` is the normal path;
/// `ZombieIgnored` means the target had no payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigprocmaskChange {
    Replaced { prev: SignalMask, new: SignalMask },
    ZombieIgnored,
}

/// How [`step_sigprocmask`] combines `next` with the current mask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigmaskHow {
    /// Replace the mask with `next`.
    SetMask,
    /// Block additional signals: `new = prev | next`.
    Block,
    /// Unblock signals: `new = prev & !next`.
    Unblock,
}

/// Update the per-thread signal mask. Uncatchable signals
/// (`SIGKILL`, `SIGSTOP`) are stripped automatically by [`SignalMask`].
/// Per `THREAD_RUNTIME_v1` §5.2, also recomputes `signal_summary.deliverable_signal`
/// against the new mask.
pub fn step_sigprocmask(
    thread: &Cap<ThreadIdentity>,
    how: SigmaskHow,
    next: SignalMask,
) -> SigprocmaskChange {
    let payload_guard = thread.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return SigprocmaskChange::ZombieIgnored;
    };
    let prev_bits = payload.signal_mask.load(Ordering::Acquire);
    let prev = SignalMask::new(prev_bits);
    let new_bits = match how {
        SigmaskHow::SetMask => next.raw_bits(),
        SigmaskHow::Block => prev_bits | next.raw_bits(),
        SigmaskHow::Unblock => prev_bits & !next.raw_bits(),
    };
    let new = SignalMask::new(new_bits);
    payload.signal_mask.store(new.raw_bits(), Ordering::Release);

    let any_deliverable = payload.pending().deliverable_bits(new) != 0;
    payload.update_summary(|s| s.deliverable_signal = any_deliverable);

    SigprocmaskChange::Replaced { prev, new }
}

/// Post a single catchable signal to a thread's pending queue.
/// No-op if the thread is a zombie.
///
/// **Precondition**: `sig` is *not* a Gewalt signum
/// (SIGKILL/SIGSTOP/SIGCONT) — those bypass pending queues entirely
/// per `SIGNAL_v1` §1, §2 Consequence 2 and route through
/// `signal::route_gewalt` which updates `signal_summary` directly
/// without enqueueing.
///
/// Sets `signal_summary.deliverable_signal` when the posted signal
/// is unmasked, per `THREAD_RUNTIME_v1` §5.2. SIGTSTP-family
/// (SIGTSTP, SIGTTIN, SIGTTOU) is *catchable* and goes through this
/// path; the default action is `Stop` which `ast_check` recognises,
/// but the bit only sets `summary.stop_requested` once a real
/// stop-state machine consumes it (deferred). For now SIGTSTP-family
/// posts behave exactly like any other catchable signal at this
/// layer — the stop intent is materialised by `ast_check` returning
/// `DefaultStop`, not by a summary bit set here.
pub fn post_signal(thread: &Cap<ThreadIdentity>, sig: Signum) {
    debug_assert!(
        !matches!(sig, Signum::SIGKILL | Signum::SIGSTOP | Signum::SIGCONT),
        "post_signal must not be called with Gewalt signums (SIGKILL/SIGSTOP/SIGCONT); \
         use signal::route_gewalt or signal::step_kill_process which dispatches"
    );

    let payload_guard = thread.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return;
    };
    payload.pending().post(sig);

    let mask = payload.signal_mask();
    if !mask.is_blocked(sig) {
        payload.update_summary(|s| s.deliverable_signal = true);
    }
}
