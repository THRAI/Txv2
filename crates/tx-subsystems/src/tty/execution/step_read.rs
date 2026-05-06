//! `read(2)`-shaped TTY step.

use core::sync::atomic::Ordering;

use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome, WaitToken};
use crate::tty::checks::{
    background_read_signal, require_fg_pgrp, require_fg_pgrp_for, require_live_tty,
};
use crate::tty::execution::TTY_READABLE;
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

    let copied = payload.with_input_queue(|queue| {
        let copied = queue.drain_to_slice(out);
        if queue.is_empty() {
            tty.input_readable.clear(TTY_READABLE);
        }
        copied
    });

    if copied == 0 {
        // Pre-ELF Phase 5 (item 9): the wait carrier is the TTY
        // identity's `wait_channel`, registered with the global
        // `wait_carrier` resolver at construction. `step_ingest`
        // fires it after any byte ingest, so `sys_read`'s
        // `wait_on_token(token).await` actually parks until UART RX
        // bytes arrive.
        StepOutcome::Blocked(WaitToken::new(tty.wait_carrier_id(), TTY_READABLE))
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
