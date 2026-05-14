//! Path-resolution state machine per `txdoc:VFS-CHECKS-MODULE-LAYOUT-1` (§3).
//!
//! The `resolution/` module tree provides the shared kernel
//! (`kernel_step`), mode-specific terminal builders
//! (`accepts` / `build_witness`), error classification, and the
//! driver loop that threads them together.
//!
//! ## Current status
//!
//! The synchronous walker (`vfs::walker::step_walk`) predates the
//! state-machine vocabulary defined here.  `state.rs` provides the
//! forward-looking type definitions; future phases will implement
//! `step.rs`, `terminal.rs`, `error.rs`, and `driver.rs`.

pub mod state;
