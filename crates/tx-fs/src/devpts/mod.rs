//! devpts — pseudoterminal filesystem.
//!
//! Re-exports the `DevptsInstance` FsOps + FsPageBacking backend
//! from the TTY subsystem (`crates/tx-subsystems/src/tty/project.rs`).
//!
//! `DevptsInstance` is a projection-shaped mount backend: `/dev/pts/`
//! holds a single `ptmx` entry (opens a fresh pty pair on access)
//! plus dynamically-registered slave entries (`0`, `1`, …) that
//! appear as ptys are created and disappear when the last reference
//! to a slave falls.
//!
//! Active-doc anchors:
//! - `txdoc:TTY-DEVPTS-PROJECTION-1` → TTY.md §7
//! - `txdoc:TTY-LOOKUP-1` → TTY.md §6.2
//! - `txdoc:TTY-RNODE-MATERIALIZATION-1` → TTY.md §6.3

pub use tx_subsystems::tty::project::DevptsInstance;
pub use tx_subsystems::tty::project::DEVPTS_PTMX_OBJECT_ID;
pub use tx_subsystems::tty::project::DEVPTS_ROOT_OBJECT_ID;
