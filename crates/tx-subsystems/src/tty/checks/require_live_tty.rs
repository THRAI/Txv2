//! Live-payload check for TTY operations.

use tx_substrate::zone::{OperationalCapExt, PayloadCap};

use crate::execution::{Errno, Guard};
use crate::tty::structure::{TtyIdentity, TtyPayload};

/// Upgrade a TTY identity into live payload evidence.
///
/// Hangup is represented by `TtyIdentity::payload == None`; operations that
/// need queues or line-discipline state fail with `EIO`.
pub fn require_live_tty(
    tty: &tx_substrate::zone::Cap<TtyIdentity>,
    _guard: &Guard<'_>,
) -> Result<PayloadCap<TtyPayload>, Errno> {
    tty.upgrade_operational().map_err(|_| Errno::EIO)
}
