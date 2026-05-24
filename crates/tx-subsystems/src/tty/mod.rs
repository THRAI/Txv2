//! TTY subsystem.
//!
//! Current surface:
//!
//! - N_TTY line discipline, termios, winsize, and queue state
//! - zone-backed `TtyIdentity` / `TtyPayload` factoring
//! - tty execution steps for read/write/ioctl/ingest/hangup/openpty
//! - process-aware controlling-tty and foreground-pgrp helpers
//! - VFS-facing dispatch through `OpenFile::{step_read, step_write, step_ioctl}`
//!
//! Still staged:
//!
//! - full syscall/fd-table integration remains outside this module
//! - non-canonical `VTIME` and full Linux job-control blocking semantics
//!   are not implemented yet
//! - reactor/IRQ-native hardware ingest wiring is still bridged by
//!   `step_poll_hardware_input`
//!
//! See `docs/design/06_devices/TTY.md` for the design target and
//! `docs/progress/decisions/` for staged deviations that have landed.

pub mod adapter;
pub mod checks;
pub mod execution;
pub mod ldisc;
pub mod notification;
pub mod project;
pub mod structure;

#[cfg(test)]
mod tests;
