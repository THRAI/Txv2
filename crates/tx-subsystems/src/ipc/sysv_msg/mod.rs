//! SysV message queues — `msgget(2)`, `msgsnd(2)`, `msgrcv(2)`, `msgctl(2)`.
//!
//! Canonical spec: `docs/Txv3/08_SYSV_IPC_v1.md` §6.
//!
//! Phase IPC-2 (medium difficulty). Introduces per-type buckets,
//! `queue_seq` counter, `send_source` / `recv_source` WaitSources,
//! and `Sequenced` predicate for `msgrcv` with `msgtyp < 0`.

pub mod checks;
pub mod execution;
mod projection;
pub mod structure;
