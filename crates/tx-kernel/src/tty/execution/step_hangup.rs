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
        hup_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::SessionLeaderProcessGroup(binding.session_leader_pgid),
            signal: JobControlSignal::Hup,
        }),
        cont_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup(binding.foreground_pgid),
            signal: JobControlSignal::Cont,
        }),
    })
}
