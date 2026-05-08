//! `read(2)`-shaped TTY step.

use core::sync::atomic::Ordering;

use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome, WaitToken};
use crate::tty::checks::{
    background_read_signal, require_fg_pgrp, require_fg_pgrp_for, require_live_tty,
};
use crate::tty::execution::TTY_READABLE;
use crate::tty::structure::termios::{ICANON, VMIN, VTIME};
use crate::tty::structure::TtyIdentity;

/// Drain bytes from a live TTY input queue into `out`.
///
/// Empty queue returns `Blocked(input_readable)`. A canonical VEOF on an empty
/// line returns `Done(0)` once via `eof_pending`.
pub fn step_read(tty: &Cap<TtyIdentity>, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize> {
    if out.is_empty() {
        return StepOutcome::Done(0);
    }

    if let Err(err) = require_fg_pgrp(tty, guard) {
        return StepOutcome::Err(err);
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    if payload.eof_pending.swap(false, Ordering::AcqRel) {
        tty.input_readable.clear(TTY_READABLE);
        return StepOutcome::Done(0);
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
        StepOutcome::Blocked(WaitToken::new(tty.wait_carrier_id(), TTY_READABLE))
    } else if copied == 0 {
        if matches!(vmin_policy, Some(0)) {
            StepOutcome::Done(0)
        } else {
            StepOutcome::Blocked(WaitToken::new(tty.wait_carrier_id(), TTY_READABLE))
        }
    } else {
        StepOutcome::Done(copied)
    }
}

pub fn step_read_for_caller(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    caller: super::IoctlCaller,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    if out.is_empty() {
        return StepOutcome::Done(0);
    }

    if let Err(err) = require_fg_pgrp_for(tty, Some(caller)) {
        let _ = background_read_signal(tty, caller);
        return StepOutcome::Err(err);
    }

    step_read(tty, out, guard)
}

pub fn step_read_for_process(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    let caller_info = match super::IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return StepOutcome::Err(err),
    };

    if out.is_empty() {
        return StepOutcome::Done(0);
    }

    if let Err(err) = require_fg_pgrp_for(tty, Some(caller_info)) {
        let dispatch = background_read_signal(tty, caller_info);
        let _ = super::step_ioctl::deliver_signal_dispatch_for_process_with_guard(
            caller, dispatch, guard,
        );
        return StepOutcome::Err(err);
    }

    step_read(tty, out, guard)
}
