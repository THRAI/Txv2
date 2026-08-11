//! Thread-runtime subsystem: thread identities, payloads, and the
//! `step_thread_exit` execution path.
//!
//! Day-1 surface mirrors `process/`: identity-payload split, zone-
//! allocated entities. Signal mask / summary / pending queue and the
//! reactor TaskKey wiring land in follow-up passes.

pub mod adapter;
pub mod execution;
pub mod structure;

#[cfg(test)]
mod tests;

pub use execution::{
    prepare_userspace_entry_payload, step_thread_exit, step_thread_exit_with_posts, SigprocmaskOp,
    ThreadExitOp, ThreadExitOutcome, ThreadKillWithPostOp,
};
pub use structure::{
    allocate_tid, clear_current_thread_identity, clear_current_thread_payload,
    clear_current_userspace_payload, clear_current_userspace_thread_identity,
    current_thread_identity, current_thread_payload, current_thread_payload_mask,
    current_userspace_payload, current_userspace_payload_mask, current_userspace_thread_identity,
    drain_pending_syscall_return, prewarm_thread_payload_slots, set_current_thread_identity,
    set_current_thread_payload, set_current_userspace_payload,
    set_current_userspace_thread_identity, userspace_payload_trace_counters, ThreadIdentity,
    ThreadPayload, Tid, MAX_THREAD_PAYLOAD_HARTS,
};
