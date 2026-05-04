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

pub use execution::step_thread_exit;
pub use structure::{allocate_tid, ThreadIdentity, ThreadPayload, Tid};
