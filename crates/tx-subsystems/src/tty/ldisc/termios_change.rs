//! Line discipline reaction to termios mutations.
//!
//! Called at the commit sub-phase of `tty::step_ioctl_tcsetattr_commit`
//! (Phase C+) after the new `Termios` has been atomically published. Handles
//! ldisc-state side effects that a flag change requires.
//!
//! In the production path this function runs through the ingest linearizer
//! (TTY.md §3.5). For Phase A/B it may be called directly since there is
//! only one writer in tests.

use crate::tty::ldisc::state::LdiscState;
use crate::tty::structure::ring::TtyRing;
use crate::tty::structure::termios::{Termios, ICANON};

/// React to a `Termios` change at commit time.
///
/// `old` is the previous termios; `new` is the just-published replacement.
/// Both queue references must be the same queues that the ingest step uses.
pub fn on_termios_changed<const IC: usize, const OC: usize>(
    state: &mut LdiscState,
    old: &Termios,
    new: &Termios,
    input_queue: &mut TtyRing<IC>,
    _output_queue: &mut TtyRing<OC>,
) {
    // ICANON → off: flush any pending canonical buffer to input_queue.
    //
    // POSIX: "If ICANON is cleared, bytes in the canonical buffer shall be
    // placed in the raw-mode input queue as if they had been received in the
    // new mode."
    let icanon_turned_off = old.c_lflag & ICANON != 0 && new.c_lflag & ICANON == 0;
    if icanon_turned_off && !state.cooked_buf.is_empty() {
        state.cooked_buf.drain_into(input_queue);
    }

    // ECHO → off: precise echo-region deletion is not implemented in v1.
    // The output_queue may contain echoed bytes generated while ECHO was on;
    // leaving them in place is conservative and correct for POSIX.
    //
    // TODO(Phase G): track the echo region boundary in LdiscState and excise
    // it from output_queue here when ECHO transitions off mid-line.
}
