//! Process subsystem: process / thread-group / session topology.
//!
//! Day-1 surface: identity-payload split, zone-allocated entities,
//! `step_fork` / group-exit transition / `step_setpgid` / `step_setsid`,
//! `step_process_exit` (internal last-thread cascade). Signal state, credentials, rlimits, and
//! fd-table land in follow-up passes.

pub mod adapter;
#[cfg(all(tx_ds_metrics, tx_ds_metrics_process))]
pub mod ds_metrics;
pub mod exec_prep;
pub mod execution;
mod lock_metrics;
pub mod notification;
pub mod nsproxy;
pub mod numbers;
pub mod structure;
pub mod topology;

#[cfg(test)]
mod tests;

pub use exec_prep::{ExecPrepError, PreparedCloexecClose, ProcessExecPrep};
pub use execution::{
    all_pids, bootstrap_init_process, finalize_detached_open_files, flush_page_backed_open_file,
    fork_with_options_wait, init_process, process_by_pid, process_group_by_pgid,
    seed_child_leader_context, step_chdir, step_chdir_with_mount, step_exit_group_with_posts,
    step_fork, step_fork_with_options, step_getcwd, step_set_mount_namespace, step_setpgid,
    step_setsid, step_waitpid_nohang, waitpid_would_block, ChdirOp, ChdirOutcome, CloneThreadOp,
    CloseOp, Dup3Op, DupOp, FcntlDupFdOp, FcntlFdOp, FinalizeDetachedOpenFileOp, ForkError,
    ForkOptions, GetcwdOp, InstallBrkForExecOp, ProcessExitOutcome,
    ResetSignalDispositionsForExecOp, SetpgidError, SetpgidOp, SetsidError, SetsidOp, WaitError,
    WaitTarget,
};
pub use structure::{
    CwdBinding, ExitStatus, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session, Sid,
    EXIT_SOURCE_CHILD_ZOMBIFIED,
};
pub use topology::{ProcessChildren, ProcessGroupMembers, ProcessThreads, SessionMembers};
