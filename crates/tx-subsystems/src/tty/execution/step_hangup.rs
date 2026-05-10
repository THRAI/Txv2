//! TTY hangup execution steps.

use tx_substrate::zone::Cap;

use super::step_ioctl::{JobControlSignal, SessionCtlEvent, SignalDispatch, SignalTarget};
use crate::execution::{Errno, Guard};
use crate::tty::structure::TtyIdentity;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HangupOutcome {
    pub had_payload: bool,
    pub hangup_fired: bool,
    pub session_ctl_fired: bool,
    pub hup_signal: Option<SignalDispatch>,
    pub cont_signal: Option<SignalDispatch>,
}

pub fn step_hangup(
    tty: &Cap<TtyIdentity>,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<HangupOutcome, tx_substrate::step_v3::NoProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;

    let binding = tty.session_pgrp();
    let session_leader_pgrp = binding
        .and_then(|binding| {
            binding
                .session
                .as_ref()
                .and_then(|weak| weak.upgrade(guard))
        })
        .and_then(|session| session.leader_pgrp_cap_with_guard(guard));
    if !tty.is_live() {
        return V3::Err(Errno::EIO.into());
    }

    if binding.is_some() {
        tty.clear_session_pgrp();
    }
    let had_payload = tty.take_payload().is_some();
    tty.hangup_port.fire(1);
    tty.session_ctl_port
        .fire(SessionCtlEvent::LostControllingTty as u64);

    V3::Done(HangupOutcome {
        had_payload,
        hangup_fired: true,
        session_ctl_fired: true,
        hup_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::SessionLeaderProcessGroup {
                pgid: binding.session_leader_pgid,
                pgrp: session_leader_pgrp.map(|pgrp| pgrp.downgrade()),
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
