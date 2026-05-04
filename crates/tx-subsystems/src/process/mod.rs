//! Process subsystem: process / thread-group / session topology.
//!
//! Day-1 surface: identity-payload split, zone-allocated entities,
//! `step_fork` / `step_exit_group` / `step_setpgid` / `step_setsid`,
//! `step_zombie` (internal). Signal state, credentials, rlimits, and
//! fd-table land in follow-up passes.

pub mod execution;
pub mod structure;

#[cfg(test)]
mod tests;

pub use execution::{
    bootstrap_init_process, step_exit_group, step_fork, step_setpgid, step_setsid, ForkError,
    SetpgidError, SetsidError,
};
pub use structure::{
    allocate_pid, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session, Sid,
};
