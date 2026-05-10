//! Master-close hangup path for ptys.

use tx_substrate::zone::Cap;

use super::step_hangup::{step_hangup, HangupOutcome};
use super::step_ioctl::IoctlSideEffect;
use crate::execution::{Errno, Guard};
use crate::tty::checks::require_live_tty;
use crate::tty::structure::{TtyIdentity, TtyTransport};

pub fn step_master_close_last(
    master: &Cap<TtyIdentity>,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<
    (HangupOutcome, IoctlSideEffect),
    tx_substrate::step_v3::NoProgress,
> {
    use tx_substrate::step_v3::StepOutcome as V3;

    let payload = match require_live_tty(master, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    let peer = match &payload.transport {
        TtyTransport::Pty { peer } => peer.clone(),
        TtyTransport::Hardware { .. } => return V3::Err(Errno::EINVAL.into()),
    };

    let hangup = match step_hangup(&peer, guard) {
        V3::Done(outcome) => outcome,
        V3::Err(e) => return V3::Err(e),
        V3::Continue { .. } | V3::Yield { .. } => return V3::Err(Errno::EIO.into()),
    };

    let _ = master.take_payload();
    V3::Done((hangup, IoctlSideEffect::default()))
}
