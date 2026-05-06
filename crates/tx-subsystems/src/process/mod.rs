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
    bootstrap_init_process, step_chdir, step_close_cloexec_fds, step_exit_group, step_fork,
    step_getcwd, step_install_brk_for_exec, step_setpgid, step_setsid, step_waitpid_nohang,
    ChdirOutcome, ForkError, SetpgidError, SetsidError, WaitError, WaitTarget,
};
pub use structure::{
    allocate_pid, ExitStatus, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session,
    Sid, FD_TABLE_SIZE,
};
