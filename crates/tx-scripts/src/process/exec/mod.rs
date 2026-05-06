//! Exec script (the `EXEC_v1` subsystem-script realisation).
//!
//! Module placement per `txdoc:EXEC-2-1-MODULE-PLACEMENT`. v1 ships
//! the stack-image builder (Phase 3 of the ELF-loader plan) and the
//! ELF parser binding (`loader.rs`, Phase 4) — the orchestrator
//! (`mod.rs::exec_script`, Phase 5) lands in a subsequent slice.

pub mod loader;
pub mod stack;
