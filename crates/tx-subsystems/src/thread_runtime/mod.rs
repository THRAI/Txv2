//! Thread-runtime subsystem: thread identities, payloads, and the
//! `step_thread_exit` execution path.
//!
//! Day-1 surface mirrors `process/`: identity-payload split, zone-
//! allocated entities. Signal mask / summary / pending queue and the
//! reactor TaskKey wiring land in follow-up passes.

pub mod execution;
pub mod structure;

#[cfg(test)]
mod tests;

pub use execution::{prepare_userspace_entry_payload, step_thread_exit};
pub use structure::{
    allocate_tid, clear_current_thread_payload, current_thread_payload,
    drain_pending_syscall_return, set_current_thread_payload, ThreadIdentity, ThreadPayload, Tid,
    MAX_THREAD_PAYLOAD_HARTS,
};
