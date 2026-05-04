//! TTY observe/authorization checks.
//!
//! Phase C only has live-payload checks plus staging job-control helpers.
//! Caller-aware foreground-pgrp checks are not yet wired into the real VFS I/O
//! path; they exist so tty-side behavior can be exercised before PROCESS owns
//! canonical Session/ProcessGroup identities.

mod require_fg_pgrp;
mod require_live_tty;
mod require_session_leader;

pub use require_fg_pgrp::{
    background_read_signal, background_write_signal, require_fg_pgrp, require_fg_pgrp_for,
};
pub use require_live_tty::require_live_tty;
pub use require_session_leader::require_session_leader;
