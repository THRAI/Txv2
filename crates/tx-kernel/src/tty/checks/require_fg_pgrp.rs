//! Foreground-process-group authorization check.
//!
//! The real check depends on PROCESS-owned Session/ProcessGroup identities.
//! Phase G uses staging caller/session/pgrp ids so tty-side job-control flow
//! can be exercised before PROCESS lands.

use crate::execution::{Errno, Guard};
use crate::tty::structure::TtyIdentity;

use crate::tty::execution::{IoctlCaller, JobControlSignal, SignalDispatch, SignalTarget};

pub fn require_fg_pgrp(tty: &TtyIdentity, _guard: &Guard<'_>) -> Result<(), Errno> {
    require_fg_pgrp_for(tty, None)
}

pub fn require_fg_pgrp_for(tty: &TtyIdentity, caller: Option<IoctlCaller>) -> Result<(), Errno> {
    let Some(caller) = caller else {
        return Ok(());
    };
    let Some(binding) = tty.session_pgrp() else {
        return Ok(());
    };
    if caller.in_foreground || caller.pgrp_id == binding.foreground_pgid {
        return Ok(());
    }

    Err(Errno::EIO)
}

pub fn background_read_signal(tty: &TtyIdentity, caller: IoctlCaller) -> Option<SignalDispatch> {
    let binding = tty.session_pgrp()?;
    if caller.in_foreground || caller.pgrp_id == binding.foreground_pgid {
        return None;
    }
    if caller.sigttin_ignored {
        return None;
    }
    Some(SignalDispatch {
        target: SignalTarget::CallerProcessGroup(caller.pgrp_id),
        signal: JobControlSignal::Ttin,
    })
}

pub fn background_write_signal(tty: &TtyIdentity, caller: IoctlCaller) -> Option<SignalDispatch> {
    let binding = tty.session_pgrp()?;
    if caller.in_foreground || caller.pgrp_id == binding.foreground_pgid {
        return None;
    }
    if caller.sigttou_ignored {
        return None;
    }
    Some(SignalDispatch {
        target: SignalTarget::CallerProcessGroup(caller.pgrp_id),
        signal: JobControlSignal::Ttou,
    })
}
