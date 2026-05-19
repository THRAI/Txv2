//! IPC limit defaults and accessors.
//!
//! The canonical `IpcLimits` type lives in `crate::process::nsproxy`.
//! This module is a thin re-export shim so IPC subsystem code can
//! reference `crate::ipc::IpcLimits` without coupling to process
//! internals.

pub use crate::process::nsproxy::IpcLimits;
