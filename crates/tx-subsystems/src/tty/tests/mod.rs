//! TTY test root.
//!
//! `legacy_phase_a` carries the original Phase-A behavioural test
//! suite (line discipline, ioctl matrix, devpts, hangup). Newer tests
//! are grouped by the subsystem responsibility they exercise.

mod execution_ioctl;
mod execution_poll_hardware;
mod execution_read;
mod legacy_phase_a;
mod support;
mod typed_session_pgrp;
