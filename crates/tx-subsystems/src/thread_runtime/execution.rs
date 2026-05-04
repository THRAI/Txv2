//! Thread-runtime execution: thread-exit step + the internal helper
//! that clears thread payload without touching parent bookkeeping.

use tx_substrate::zone::Cap;

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
