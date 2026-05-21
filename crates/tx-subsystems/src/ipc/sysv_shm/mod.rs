//! SysV shared memory — `shmget(2)`, `shmat(2)`, `shmdt(2)`, `shmctl(2)`.
//!
//! Canonical spec: `docs/Txv3/08_SYSV_IPC_v1.md` §5.
//!
//! Phase IPC-1 (lowest difficulty). Zero new `WaitSource` — the only
//! blocking operation is page-fault-on-attach, already handled by the
//! existing VM fault path.

pub mod checks;
pub mod execution;
mod projection;
pub mod structure;

#[cfg(test)]
mod tests;
