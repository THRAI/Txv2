//! IPC namespace — IpcNamespace structure and helpers.
//!
//! The canonical `IpcNamespace` lives in `crate::process::nsproxy`.
//! This module provides the namespace-scoped step operations
//! (`step_clone_newipc`, `step_set_limits`) and re-exports the
//! limits type for subsystem-internal use.

pub mod execution;
pub mod structure;

// Re-export from the canonical location.
pub use crate::process::nsproxy::IpcLimits;
