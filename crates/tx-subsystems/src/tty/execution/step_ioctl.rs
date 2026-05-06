//! TTY ioctl-style execution steps.

use core::sync::atomic::Ordering;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::signal::{self, DispatchOutcome, SigDisposition, Signum};
use crate::tty::checks::{require_live_tty, require_session_leader};
use crate::tty::execution::{TTY_READABLE, TTY_WRITABLE};
use crate::tty::ldisc::SignalKind;
use crate::tty::structure::{SessionPgrp, Termios, TtyIdentity, Winsize};

#[derive(Clone, Copy, Debug)]
pub struct IoctlCaller {
    pub session_id: u32,
    pub pgrp_id: u32,
    /// Typed `Weak<ProcessGroup>` for the caller's pgrp. `None` for
    /// legacy raw-id callers; populated by [`Self::with_pgrp_weak`]
    /// once the syscall driver carries a real `Cap<ProcessIdentity>`.
    pub pgrp: Option<tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>>,
    pub is_session_leader: bool,
    pub has_controlling_tty: bool,
    pub in_foreground: bool,
    pub sigttin_ignored: bool,
    pub sigttou_ignored: bool,
}

impl PartialEq for IoctlCaller {
    fn eq(&self, other: &Self) -> bool {
        // Typed pgrp Weak intentionally not compared: tests construct
        // callers via `IoctlCaller::new(...)` (no typed ref) and assert
        // equality on the legacy field set. Two callers with the same
        // ids but different (or missing) Weak refs are operationally
        // equivalent for the day-1 surface — same authority, same
        // dispatch shape.
        self.session_id == other.session_id
            && self.pgrp_id == other.pgrp_id
            && self.is_session_leader == other.is_session_leader
            && self.has_controlling_tty == other.has_controlling_tty
            && self.in_foreground == other.in_foreground
            && self.sigttin_ignored == other.sigttin_ignored
            && self.sigttou_ignored == other.sigttou_ignored
    }
}

impl Eq for IoctlCaller {}

impl IoctlCaller {
    pub const fn new(session_id: u32, pgrp_id: u32) -> Self {
        Self {
            session_id,
            pgrp_id,
            pgrp: None,
            is_session_leader: false,
            has_controlling_tty: false,
            in_foreground: true,
            sigttin_ignored: false,
            sigttou_ignored: false,
        }
    }

    /// Build a caller snapshot from a live process identity.
    ///
    /// This is the process-aware bridge for TTY helpers: it lifts the
    /// canonical process/session/pgrp topology into the existing TTY-side
    /// caller value, including typed `Weak<ProcessGroup>` retention for
    /// signal fanout.
    pub fn from_process(
        process: &Cap<crate::process::structure::ProcessIdentity>,
    ) -> Result<Self, Errno> {
        let guard = tx_substrate::epoch::guard();
        Self::from_process_with_guard(process, &guard)
    }

    pub(crate) fn from_process_with_guard(
        process: &Cap<crate::process::structure::ProcessIdentity>,
        guard: &tx_substrate::epoch::Guard<'_>,
    ) -> Result<Self, Errno> {
        let payload_guard = process.payload.lock();
        let payload = payload_guard.as_ref().ok_or(Errno::ESRCH)?;

        let pgrp = process.pgrp_cap();
        let session = pgrp.session_cap();
        let in_foreground = (*session.controlling_tty.lock())
            .and_then(|weak| weak.upgrade(guard))
            .and_then(|tty| tty.session_pgrp())
            .map(|binding| binding.foreground_pgid == pgrp.pgid.0)
            .unwrap_or(true);

        Ok(Self {
            session_id: session.sid.0,
            pgrp_id: pgrp.pgid.0,
            pgrp: Some(pgrp.downgrade()),
            is_session_leader: process.pid.0 == session.sid.0,
            has_controlling_tty: session.has_controlling_tty(),
            in_foreground,
            sigttin_ignored: matches!(
                payload.sig_actions().get(Signum::SIGTTIN),
                SigDisposition::Ignore
            ),
            sigttou_ignored: matches!(
                payload.sig_actions().get(Signum::SIGTTOU),
                SigDisposition::Ignore
            ),
        })
    }

    /// Attach a typed `Weak<ProcessGroup>` to the caller. Used by the
    /// (forthcoming) syscall driver that already holds the caller's
    /// `Cap<ProcessIdentity>`. Lets dispatched signals route through
    /// `signal::script_kill_pgrp` against a real pgrp instead of
    /// resolving from the raw `pgrp_id` later.
    pub fn with_pgrp_weak(
        mut self,
        pgrp: tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>,
    ) -> Self {
        self.pgrp = Some(pgrp);
        self
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

/// Where a TTY-emitted signal should be delivered. Each variant carries
/// the legacy raw `pgid: u32` and a typed `pgrp: Option<Weak<ProcessGroup>>`
/// — populated when the upstream binding (TTY's `SessionPgrp` or the
/// `IoctlCaller`) was constructed from real process-subsystem caps.
/// Typed refs let `signal::deliver_tty_dispatch` upgrade and route the
/// post through `signal::script_kill_pgrp` directly.
#[derive(Clone, Copy, Debug)]
pub enum SignalTarget {
    /// TTY's bound foreground pgrp (TIOCSCTTY-installed, ioctl-emitted).
    ForegroundProcessGroup {
        pgid: u32,
        pgrp: Option<tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>>,
    },
    /// Session leader's pgrp (used by hangup → SIGHUP fanout).
    SessionLeaderProcessGroup {
        pgid: u32,
        pgrp: Option<tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>>,
    },
    /// Caller's own pgrp (used by background-IO SIGTTIN/SIGTTOU).
    CallerProcessGroup {
        pgid: u32,
        pgrp: Option<tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>>,
    },
}

impl PartialEq for SignalTarget {
    fn eq(&self, other: &Self) -> bool {
        // Typed Weak refs intentionally not compared (same rationale as
        // `IoctlCaller`/`SessionPgrp`).
        match (self, other) {
            (
                SignalTarget::ForegroundProcessGroup { pgid: a, .. },
                SignalTarget::ForegroundProcessGroup { pgid: b, .. },
            ) => a == b,
            (
                SignalTarget::SessionLeaderProcessGroup { pgid: a, .. },
                SignalTarget::SessionLeaderProcessGroup { pgid: b, .. },
            ) => a == b,
            (
                SignalTarget::CallerProcessGroup { pgid: a, .. },
                SignalTarget::CallerProcessGroup { pgid: b, .. },
            ) => a == b,
            _ => false,
        }
    }
}

impl Eq for SignalTarget {}

impl SignalTarget {
    /// Pgid of the targeted pgrp, regardless of typed-ref population.
    pub const fn pgid(&self) -> u32 {
        match *self {
            SignalTarget::ForegroundProcessGroup { pgid, .. }
            | SignalTarget::SessionLeaderProcessGroup { pgid, .. }
            | SignalTarget::CallerProcessGroup { pgid, .. } => pgid,
        }
    }

    /// Borrow the optional typed pgrp Weak (consumed by
    /// `signal::deliver_tty_dispatch` for upgrade-and-route).
    pub fn pgrp_weak(
        &self,
    ) -> Option<&tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>> {
        match self {
            SignalTarget::ForegroundProcessGroup { pgrp, .. }
            | SignalTarget::SessionLeaderProcessGroup { pgrp, .. }
            | SignalTarget::CallerProcessGroup { pgrp, .. } => pgrp.as_ref(),
        }
    }
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

/// Deliver an optional TTY-emitted signal dispatch on behalf of `source`.
///
/// This is the small glue layer between TTY producers and the signal shim's
/// typed pgrp bridge. `None` means "no signal side effect happened".
pub fn deliver_signal_dispatch_for_process(
    source: &Cap<crate::process::structure::ProcessIdentity>,
    dispatch: Option<SignalDispatch>,
) -> Result<Option<DispatchOutcome>, Errno> {
    let guard = tx_substrate::epoch::guard();
    match dispatch {
        Some(dispatch) => {
            signal::deliver_tty_dispatch_with_guard(source, dispatch, &guard).map(Some)
        }
        None => Ok(None),
    }
}

pub(crate) fn deliver_signal_dispatch_for_process_with_guard(
    source: &Cap<crate::process::structure::ProcessIdentity>,
    dispatch: Option<SignalDispatch>,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Result<Option<DispatchOutcome>, Errno> {
    match dispatch {
        Some(dispatch) => {
            signal::deliver_tty_dispatch_with_guard(source, dispatch, guard).map(Some)
        }
        None => Ok(None),
    }
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

/// Process-aware TIOCSCTTY helper. Binds the tty to the caller's canonical
/// session/pgrp and installs the process-side controlling-tty mirror.
pub fn step_ioctl_tiocsctty_for_process(
    tty: &Cap<TtyIdentity>,
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let caller_info = match IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return StepOutcome::Err(err),
    };

    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    if let Err(err) = require_session_leader(caller_info) {
        return StepOutcome::Err(err);
    }
    if tty.session_pgrp().is_some() {
        return StepOutcome::Err(Errno::EBUSY);
    }

    let pgrp = caller.pgrp_cap();
    let session = pgrp.session_cap();
    tty.bind_session_pgrp_typed(&session, &pgrp);
    *session.controlling_tty.lock() = Some(tty.downgrade());
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

/// Process-aware TIOCNOTTY helper. Clears both the tty's authoritative
/// session/pgrp slot and the session-side controlling-tty mirror.
pub fn step_ioctl_tiocnotty_for_process(
    tty: &Cap<TtyIdentity>,
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let caller_info = match IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return StepOutcome::Err(err),
    };

    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let pgrp = caller.pgrp_cap();
    let session = pgrp.session_cap();
    let Some(binding) = tty.session_pgrp() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if binding.session_id != caller_info.session_id {
        return StepOutcome::Err(Errno::EINVAL);
    }

    tty.clear_session_pgrp();
    *session.controlling_tty.lock() = None;
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

/// Process-aware `tcsetpgrp` / `TIOCSPGRP` helper for callers that already
/// resolved the target process group to a canonical `Cap<ProcessGroup>`.
pub fn step_ioctl_tiocspgrp_for_process(
    tty: &Cap<TtyIdentity>,
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    new_pgrp: &Cap<crate::process::structure::ProcessGroup>,
    guard: &Guard<'_>,
) -> StepOutcome<IoctlSideEffect> {
    let caller_info = match IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return StepOutcome::Err(err),
    };

    let _payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let caller_session = caller.pgrp_cap().session_cap();
    if new_pgrp.session_cap().key() != caller_session.key() {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let Some(binding) = tty.session_pgrp() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if binding.session_id != caller_info.session_id {
        return StepOutcome::Err(Errno::EINVAL);
    }

    tty.bind_session_pgrp_typed(&caller_session, new_pgrp);
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
            target: SignalTarget::ForegroundProcessGroup {
                pgid: binding.foreground_pgid,
                pgrp: binding.foreground_pgrp,
            },
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
        target: SignalTarget::ForegroundProcessGroup {
            pgid: binding.foreground_pgid,
            pgrp: binding.foreground_pgrp,
        },
        signal: signal.into(),
    })
}
