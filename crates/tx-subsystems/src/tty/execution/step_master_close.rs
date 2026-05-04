//! Master-close hangup path for ptys.

use tx_substrate::zone::Cap;

use super::step_hangup::{step_hangup, HangupOutcome};
use super::step_ioctl::IoctlSideEffect;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::tty::checks::require_live_tty;
use crate::tty::structure::{TtyIdentity, TtyTransport};

pub fn step_master_close_last(
    master: &Cap<TtyIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<(HangupOutcome, IoctlSideEffect)> {
    let payload = match require_live_tty(master, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let peer = match &payload.transport {
        TtyTransport::Pty { peer } => peer.clone(),
        TtyTransport::Hardware { .. } => return StepOutcome::Err(Errno::EINVAL),
    };

    let hangup = match step_hangup(&peer, guard) {
        StepOutcome::Done(outcome) => outcome,
        StepOutcome::Err(err) => return StepOutcome::Err(err),
        StepOutcome::Advanced(outcome) => outcome,
        StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
            return StepOutcome::Err(Errno::EIO)
        }
    };

    let _ = master.take_payload();
    StepOutcome::Done((hangup, IoctlSideEffect::default()))
}
