//! Line discipline state: canonical buffer and flow-control flags.
//!
//! `LdiscState` is the mutable state that the N_TTY line discipline carries
//! across successive input bytes. Under the single-writer discipline described
//! in TTY.md §2.6 and §3.5, exactly one ingest step holds `&mut LdiscState`
//! at a time.

use crate::tty::structure::ring::TtyRing;
use crate::tty::structure::termios::MAX_CANON;

/// Mutable state maintained by the N_TTY line discipline.
pub struct LdiscState {
    /// Canonical-mode (cooked) input accumulation buffer.
    ///
    /// Bytes accumulate here until a line-commit condition: `\n`, `VEOF`,
    /// `VEOL`, or the buffer reaching `MAX_CANON` bytes. On commit the buffer
    /// is flushed into `input_queue`.
    pub(crate) cooked_buf: TtyRing<MAX_CANON>,

    /// Current output column, used for `ONOCR` (suppress CR at column 0).
    ///
    /// Incremented for every non-CR/non-NL output byte; reset to 0 on `\r`
    /// or `\n` (when `ONLRET` applies).
    pub(crate) column: u16,

    /// True when output is stopped by a VSTOP character (`IXON` flow control).
    pub(crate) flow_stopped: bool,

    /// True when the next input byte must be treated as literal, bypassing
    /// all special-character interpretation (VLNEXT / ^V escape).
    pub(crate) lnext: bool,
}

impl LdiscState {
    pub const fn new() -> Self {
        Self {
            cooked_buf: TtyRing::new(),
            column: 0,
            flow_stopped: false,
            lnext: false,
        }
    }
}

impl Default for LdiscState {
    fn default() -> Self {
        Self::new()
    }
}
