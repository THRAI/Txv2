//! TTY subsystem.
//!
//! # Phase A (this module)
//!
//! Pure semantic core: termios flags, N_TTY line discipline state machine,
//! and ring buffers. No VFS, no process/session, no hardware wiring.
//!
//! # Implementation order
//!
//! - **Phase A** (current): `structure/` + `ldisc/` — no external deps.
//! - **Phase B**: `structure/identity.rs`, `structure/payload.rs`,
//!   `structure/registry.rs` — zone-backed entity types.
//! - **Phase C**: `execution/step_read.rs`, `step_write.rs`,
//!   `step_ingest.rs` — real I/O dispatch.
//! - **Phase D+**: VFS integration, pty, hardware console, job control.
//!
//! See `docs/ljs/TTY_DESIGN_PLAN.md` for the full staged roadmap.

pub mod checks;
pub mod execution;
pub mod ldisc;
pub mod project;
pub mod structure;

#[cfg(test)]
mod tests;
