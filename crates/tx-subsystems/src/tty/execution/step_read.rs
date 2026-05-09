//! `read(2)`-shaped TTY step.

use core::sync::atomic::Ordering;

use tx_substrate::zone::Cap;

use crate::execution::Guard;
use crate::tty::checks::{
    background_read_signal, require_fg_pgrp, require_fg_pgrp_for, require_live_tty,
};
use crate::tty::execution::TTY_READABLE;
use crate::tty::structure::termios::{ICANON, VMIN, VTIME};
use crate::tty::structure::TtyIdentity;

/// Drain bytes from a live TTY input queue into `out`.
///
/// Empty queue returns a `Yield` on the TTY's wait carrier. A canonical
/// VEOF on an empty line returns `Done(0)` once via `eof_pending`.
pub fn step_read(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::{ByteProgress, StepOutcome as V3};

    if out.is_empty() {
        return V3::Done(0);
    }

    if let Err(err) = require_fg_pgrp(tty, guard) {
        return V3::Err(err.into());
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    if payload.eof_pending.swap(false, Ordering::AcqRel) {
        tty.input_readable.clear(TTY_READABLE);
        return V3::Done(0);
    }

    let vmin_policy = payload.with_termios(|termios| {
        if termios.c_lflag & ICANON != 0 || termios.c_cc[VTIME] != 0 {
            None
        } else {
            Some(termios.c_cc[VMIN] as usize)
        }
    });

    let mut threshold_unmet = false;
    let copied = payload.with_input_queue(|queue| {
        if let Some(vmin) = vmin_policy {
            let threshold = vmin.min(out.len());
            if queue.len() < threshold {
                if queue.is_empty() {
                    tty.input_readable.clear(TTY_READABLE);
                }
                threshold_unmet = true;
                return 0;
            }
        }

        let copied = queue.drain_to_slice(out);
        if queue.is_empty() {
            tty.input_readable.clear(TTY_READABLE);
        }
        copied
    });

    // Pre-ELF Phase 5 (item 9): the wait carrier is the TTY
    // identity's `wait_channel`, registered with the global
    // `wait_carrier` resolver at construction. `step_ingest` fires
    // it after any byte ingest, so `sys_read`'s
    // `wait_on_token(token).await` actually parks until UART RX
    // bytes arrive. Threshold / VMIN logic comes from main's
    // 2026-05-06 tty work.
    if threshold_unmet {
        V3::yield_on_carrier(ByteProgress::EMPTY, tty.wait_carrier_id(), TTY_READABLE)
    } else if copied == 0 {
        if matches!(vmin_policy, Some(0)) {
            V3::Done(0)
        } else {
            V3::yield_on_carrier(ByteProgress::EMPTY, tty.wait_carrier_id(), TTY_READABLE)
        }
    } else {
        V3::Done(copied)
    }
}

pub fn step_read_for_caller(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    caller: super::IoctlCaller,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;

    if out.is_empty() {
        return V3::Done(0);
    }

    if let Err(err) = require_fg_pgrp_for(tty, Some(caller)) {
        let _ = background_read_signal(tty, caller);
        return V3::Err(err.into());
    }

    step_read(tty, out, guard)
}

pub fn step_read_for_process(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;

    let caller_info = match super::IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return V3::Err(err.into()),
    };

    if out.is_empty() {
        return V3::Done(0);
    }

    if let Err(err) = require_fg_pgrp_for(tty, Some(caller_info)) {
        let dispatch = background_read_signal(tty, caller_info);
        let _ = super::step_ioctl::deliver_signal_dispatch_for_process_with_guard(
            caller, dispatch, guard,
        );
        return V3::Err(err.into());
    }

    step_read(tty, out, guard)
}
