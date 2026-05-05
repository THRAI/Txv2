//! TTY hangup execution steps.

use tx_substrate::zone::Cap;

use super::step_ioctl::{JobControlSignal, SessionCtlEvent, SignalDispatch, SignalTarget};
use crate::execution::{Errno, Guard, StepOutcome};
use crate::tty::structure::TtyIdentity;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HangupOutcome {
    pub had_payload: bool,
    pub hangup_fired: bool,
    pub session_ctl_fired: bool,
    pub hup_signal: Option<SignalDispatch>,
    pub cont_signal: Option<SignalDispatch>,
}

pub fn step_hangup(tty: &Cap<TtyIdentity>, _guard: &Guard<'_>) -> StepOutcome<HangupOutcome> {
    let binding = tty.session_pgrp();
    if !tty.is_live() {
        return StepOutcome::Err(Errno::EIO);
    }

    if binding.is_some() {
        tty.clear_session_pgrp();
    }
    let had_payload = tty.take_payload().is_some();
    tty.hangup_port.fire(1);
    tty.session_ctl_port
        .fire(SessionCtlEvent::LostControllingTty as u64);

    StepOutcome::Done(HangupOutcome {
        had_payload,
        hangup_fired: true,
        session_ctl_fired: true,
        // SessionLeaderProcessGroup's typed pgrp is left None: the
        // SessionPgrp binding carries only a Weak<Session> and
        // Weak<ProcessGroup> for the *foreground* pgrp. The session-
        // leader's pgrp would require walking session.groups for
        // pgid == session_leader_pgid; that lookup lands when the
        // session→leader-pgrp index is wired.
        hup_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::SessionLeaderProcessGroup {
                pgid: binding.session_leader_pgid,
                pgrp: None,
            },
            signal: JobControlSignal::Hup,
        }),
        cont_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup {
                pgid: binding.foreground_pgid,
                pgrp: binding.foreground_pgrp,
            },
            signal: JobControlSignal::Cont,
        }),
    })
}
