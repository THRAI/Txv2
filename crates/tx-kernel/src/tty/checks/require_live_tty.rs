//! Live-payload check for TTY operations.

use tx_substrate::zone::PayloadCap;

use crate::execution::{Errno, Guard};
use crate::tty::structure::{TtyIdentity, TtyPayload};

/// Upgrade a TTY identity into live payload evidence.
///
/// Hangup is represented by `TtyIdentity::payload == None`; operations that
/// need queues or line-discipline state fail with `EIO`.
pub fn require_live_tty(
    tty: &TtyIdentity,
    _guard: &Guard<'_>,
) -> Result<PayloadCap<TtyPayload>, Errno> {
    tty.live_payload().ok_or(Errno::EIO)
}
