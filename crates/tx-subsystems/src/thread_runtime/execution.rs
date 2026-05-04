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
        crate::process::execution::step_zombie(&parent, status);
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
    SigprocmaskChange::Replaced { prev, new }
}

/// Post a single signal to a thread's pending queue. No-op if the
/// thread is a zombie. Used by the kill shim and any future
/// thread-targeted enqueue path.
pub fn post_signal(thread: &Cap<ThreadIdentity>, sig: Signum) {
    let payload_guard = thread.payload.lock();
    if let Some(payload) = payload_guard.as_ref() {
        payload.pending().post(sig);
    }
}
