//! Process subsystem: process / thread-group / session topology.
//!
//! Day-1 surface: identity-payload split, zone-allocated entities,
//! `step_fork` / `step_exit_group` / `step_setpgid` / `step_setsid`,
//! `step_process_exit` (internal last-thread cascade). Signal state, credentials, rlimits, and
//! fd-table land in follow-up passes.

pub mod execution;
pub mod structure;

#[cfg(test)]
mod tests;

pub use execution::{
    bootstrap_init_process, step_chdir, step_exit_group, step_fork, step_getcwd, step_setpgid,
    step_setsid, step_waitpid_nohang, ChdirOutcome, ForkError, SetpgidError, SetsidError,
    WaitError, WaitTarget,
};
pub use structure::{
    allocate_pid, ExitStatus, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session,
    Sid,
};
