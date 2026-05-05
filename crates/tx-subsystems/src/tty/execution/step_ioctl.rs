//! TTY ioctl-style execution steps.

use core::sync::atomic::Ordering;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::tty::checks::{require_live_tty, require_session_leader};
use crate::tty::execution::{TTY_READABLE, TTY_WRITABLE};
use crate::tty::ldisc::SignalKind;
use crate::tty::structure::{SessionPgrp, Termios, TtyIdentity, Winsize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoctlCaller {
    pub session_id: u32,
    pub pgrp_id: u32,
    pub is_session_leader: bool,
    pub has_controlling_tty: bool,
    pub in_foreground: bool,
    pub sigttin_ignored: bool,
    pub sigttou_ignored: bool,
}

impl IoctlCaller {
    pub const fn new(session_id: u32, pgrp_id: u32) -> Self {
        Self {
            session_id,
            pgrp_id,
            is_session_leader: false,
            has_controlling_tty: false,
            in_foreground: true,
            sigttin_ignored: false,
            sigttou_ignored: false,
        }
    }

    pub const fn as_session_leader(mut self) -> Self {
        self.is_session_leader = true;
        self
    }

    pub const fn with_controlling_tty(mut self) -> Self {
        self.has_controlling_tty = true;
        self
    }

    pub const fn background(mut self) -> Self {
        self.in_foreground = false;
        self
    }

    pub const fn ignore_sigttin(mut self) -> Self {
        self.sigttin_ignored = true;
        self
    }

    pub const fn ignore_sigttou(mut self) -> Self {
        self.sigttou_ignored = true;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobControlSignal {
    Int,
    Quit,
    Tstp,
    Ttin,
    Ttou,
    Hup,
    Cont,
    Winch,
}

impl From<SignalKind> for JobControlSignal {
    fn from(value: SignalKind) -> Self {
        match value {
            SignalKind::Int => Self::Int,
            SignalKind::Quit => Self::Quit,
            SignalKind::Tstp => Self::Tstp,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalTarget {
    ForegroundProcessGroup(u32),
    SessionLeaderProcessGroup(u32),
    CallerProcessGroup(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalDispatch {
    pub target: SignalTarget,
    pub signal: JobControlSignal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCtlEvent {
    Bound,
    ForegroundChanged,
    Detached,
    LostControllingTty,
    WinsizeChanged,
    DeferredSignal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IoctlSideEffect {
    pub session_ctl_fired: bool,
    pub signal: Option<SignalDispatch>,
}

pub fn step_ioctl_tiocsctty(
    tty: &Cap<TtyIdentity>,
    caller: IoctlCaller,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    if let Err(err) = require_session_leader(caller) {
        return StepOutcome::Err(err);
    }
    if tty.session_pgrp().is_some() {
        return StepOutcome::Err(Errno::EBUSY);
    }

    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(
        caller.session_id,
        caller.pgrp_id,
        caller.pgrp_id,
    ));
    tty.session_ctl_port.fire(SessionCtlEvent::Bound as u64);

    StepOutcome::Done(IoctlSideEffect {
        session_ctl_fired: true,
        signal: None,
    })
}

pub fn step_ioctl_tiocnotty(
    tty: &Cap<TtyIdentity>,
    caller: IoctlCaller,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let Some(binding) = tty.session_pgrp() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if binding.session_id != caller.session_id {
        return StepOutcome::Err(Errno::EINVAL);
    }

    tty.clear_session_pgrp();
    tty.session_ctl_port.fire(SessionCtlEvent::Detached as u64);
    StepOutcome::Done(IoctlSideEffect {
        session_ctl_fired: true,
        signal: None,
    })
}

pub fn step_ioctl_tiocspgrp(
    tty: &Cap<TtyIdentity>,
    caller: IoctlCaller,
    new_pgrp: u32,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let Some(mut binding) = tty.session_pgrp() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if binding.session_id != caller.session_id {
        return StepOutcome::Err(Errno::EINVAL);
    }

    binding.foreground_pgid = new_pgrp;
    tty.bind_session_pgrp(binding);
    tty.session_ctl_port
        .fire(SessionCtlEvent::ForegroundChanged as u64);
    StepOutcome::Done(IoctlSideEffect {
        session_ctl_fired: true,
        signal: None,
    })
}

pub fn step_ioctl_tiocgpgrp(tty: &Cap<TtyIdentity>, guard: &Guard<'_>) -> StepOutcome<u32> {
    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    match tty.session_pgrp() {
        Some(binding) => StepOutcome::Done(binding.foreground_pgid),
        None => StepOutcome::Err(Errno::EINVAL),
    }
}

pub fn step_ioctl_tiocgwinsz(tty: &Cap<TtyIdentity>, guard: &Guard<'_>) -> StepOutcome<Winsize> {
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };
    StepOutcome::Done(Winsize::from_u64(
        payload.window_size.load(Ordering::Acquire),
    ))
}

pub fn step_ioctl_tiocswinsz(
    tty: &Cap<TtyIdentity>,
    winsize: Winsize,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    payload
        .window_size
        .store(winsize.to_u64(), Ordering::Release);
    tty.session_ctl_port
        .fire(SessionCtlEvent::WinsizeChanged as u64);
    StepOutcome::Done(IoctlSideEffect {
        session_ctl_fired: true,
        signal: tty.session_pgrp().map(|binding| SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup(binding.foreground_pgid),
            signal: JobControlSignal::Winch,
        }),
    })
}

pub fn step_ioctl_tcgets(tty: &Cap<TtyIdentity>, guard: &Guard<'_>) -> StepOutcome<Termios> {
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };
    StepOutcome::Done(payload.with_termios(|termios| *termios))
}

pub fn step_ioctl_tcsets(
    tty: &Cap<TtyIdentity>,
    new_termios: Termios,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let old_termios = payload.publish_termios(new_termios);
    payload.queue_termios_change(old_termios, new_termios);

    // This slice does not yet have a reactor-scheduled synthetic ingest event,
    // so `tcsets` drives a zero-byte linearizer pass inline after publication.
    let linearized = payload.apply_ingest_linearizer();
    if linearized.readable_fired {
        tty.input_readable.fire(TTY_READABLE);
    }
    if linearized.writable_fired {
        tty.output_writable.fire(TTY_WRITABLE);
    }
    StepOutcome::Done(IoctlSideEffect::default())
}

pub fn deferred_signal_for_tty(
    tty: &Cap<TtyIdentity>,
    signal: SignalKind,
) -> Option<SignalDispatch> {
    tty.session_pgrp().map(|binding| SignalDispatch {
        target: SignalTarget::ForegroundProcessGroup(binding.foreground_pgid),
        signal: signal.into(),
    })
}
