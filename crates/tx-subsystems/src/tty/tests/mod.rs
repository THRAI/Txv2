//! TTY test root.
//!
//! `legacy_phase_a` carries the original Phase-A behavioural test
//! suite (line discipline, ioctl matrix, devpts, hangup). Newer
//! tests grouped by concern live in sibling files (e.g.
//! `typed_session_pgrp` for the typed `Cap<Session>` /
//! `Cap<ProcessGroup>` binding surface).

mod legacy_phase_a;
mod typed_session_pgrp;
