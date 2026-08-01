//! Exec script (the `EXEC_v1` subsystem-script realisation).
//!
//! Module placement per `txdoc:EXEC-2-1-MODULE-PLACEMENT`. v1 ships
//! the stack-image builder (Phase 3 of the ELF-loader plan), the ELF
//! parser binding (`loader.rs`, Phase 4), and the orchestrator
//! (`script.rs::exec_script`, Phase 5) that wires them with the
//! kernel-side VM / process / VFS / signal seams.

mod image_reader;
pub mod loader;
pub mod script;
pub mod stack;

pub use script::{exec_script, ExecError, ExecScriptOp, EXEC_LAST_OPEN_ERRNO};
