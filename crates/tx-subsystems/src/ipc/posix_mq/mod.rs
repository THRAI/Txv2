//! POSIX message queues — `mq_open(2)`, `mq_send(2)`, `mq_receive(2)`,
//! `mq_notify(2)`, `mq_unlink(2)`, plus timed variants.
//!
//! Canonical spec: `docs/Txv3/08_SYSV_IPC_v1.md` §10.
//!
//! Phase IPC-4. Thin fd-shaped wrapper (`PosixMqInstance`) over the
//! SysV `MsgQueuePayload`. `mq_notify` is a one-shot signal attachment
//! per `SIGNAL_ATTACHMENTS_v1.md`.

pub mod checks;
pub mod execution;
mod projection;
pub mod structure;
