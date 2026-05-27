//! SysV semaphores — `semget(2)`, `semop(2)`, `semctl(2)` + `SEM_UNDO`.
//!
//! Canonical spec: `docs/Txv3/08_SYSV_IPC_v1.md` §4.
//!
//! Phase IPC-3 (highest difficulty). `SEM_UNDO` exit-step integration,
//! compound op-array atomicity, `sem_changed_source` WaitSource with
//! sequenced predicate.

pub mod checks;
pub mod execution;
pub mod projection;
pub mod structure;

#[cfg(test)]
mod tests;
