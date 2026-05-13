//! TTY structure types: termios, winsize, ring buffer, and entity types.

pub mod identity;
pub mod payload;
pub mod registry;
pub mod ring;
pub mod termios;
pub mod winsize;

pub use crate::tty::adapter::step_engine::AtomicSlot;
pub use identity::{FixedName, SessionPgrp, TtyIdentity, TtyKind};
pub use payload::{TtyPayload, TtyTransport, INPUT_CAP, OUTPUT_CAP};
pub use ring::{RingFull, TtyRing};
pub use termios::{Termios, MAX_CANON, NCCS, POSIX_VDISABLE};
pub use winsize::Winsize;
