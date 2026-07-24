//! Path-resolution state machine per `txdoc:VFS-CHECKS-MODULE-LAYOUT-1` (§3).
//!
//! The `resolution/` module tree provides:
//!
//! - `state` — WalkMode, WalkState, WalkCause, KernelStep, ResumeToken,
//!   PathResolution, WalkTrail, and related vocabulary types.
//! - `error` — `classify(WalkCause) → Errno` mapping.
//! - `step` — `kernel_step` (mode-agnostic δ, shared transition kernel).
//! - `terminal` — `accepts` + `build_*_witness` (α + ω per mode).
//! - `driver` — `walk_to_completion`, `run_walker`, `resume_walker`.
//!
//! ## Current status
//!
//! The synchronous walker (`vfs::walker::step_walk`) predates the
//! state-machine vocabulary.  `state.rs` provides the type definitions;
//! `error.rs`, `step.rs`, and `terminal.rs` are operational; `driver.rs`
//! bridges the synchronous `step_walk` into the state-machine shape.
//! When ext4 / disk-backed backends grow real IO yields, the component
//! loop in `walker::walk_inner_v3` will be lifted into `kernel_step`.

pub mod diagnostic;
pub mod driver;
pub mod error;
pub mod state;
pub mod step;
pub mod terminal;

pub use state::PathResolution;

/// Re-export the diagnostic probe for kernel-side consumption.
pub use diagnostic::{DiagCtx, last_ctx, last_diag, record_ctx, record_diag, render_ctx};
