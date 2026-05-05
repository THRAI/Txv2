//! `write(2)`-shaped TTY step.

use alloc::vec::Vec;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome, WaitToken};
use crate::tty::checks::{background_write_signal, require_fg_pgrp, require_live_tty};
use crate::tty::execution::{step_ingest, TTY_WRITABLE};
use crate::tty::ldisc::process_output;
use crate::tty::structure::{termios::TOSTOP, TtyIdentity, TtyTransport};

/// Transform user bytes through N_TTY output processing, enqueue them, and
/// kick the underlying transport.
pub fn step_write(tty: &Cap<TtyIdentity>, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize> {
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }

    if let Err(err) = require_fg_pgrp(tty, guard) {
        return StepOutcome::Err(err);
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let consumed = payload.with_termios(|termios| {
        payload.with_output_queue(|output_queue| {
            // SAFETY: Phase C serializes ldisc_state writes through TTY steps.
            // `process_output` owns the output column counter for this write.
            let state = unsafe { &mut *payload.ldisc_state.get() };
            process_output(state, termios, output_queue, bytes)
        })
    });

    if consumed == 0 {
        return StepOutcome::Blocked(WaitToken::new(tty.raw() as u64, TTY_WRITABLE));
    }

    match kick_transport(tty, guard) {
        StepOutcome::Err(err) => StepOutcome::Err(err),
        StepOutcome::Blocked(wait) => StepOutcome::AdvancedThenBlocked(consumed, wait),
        StepOutcome::AdvancedThenBlocked(_, wait) => {
            StepOutcome::AdvancedThenBlocked(consumed, wait)
        }
        StepOutcome::Done(_) | StepOutcome::Advanced(_) => StepOutcome::Done(consumed),
    }
}

pub fn step_write_for_caller(
    tty: &Cap<TtyIdentity>,
    bytes: &[u8],
    caller: super::IoctlCaller,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let background = tty
        .session_pgrp()
        .is_some_and(|binding| !caller.in_foreground && caller.pgrp_id != binding.foreground_pgid);
    if background && payload.with_termios(|termios| termios.c_lflag & TOSTOP != 0) {
        return StepOutcome::Err(Errno::EIO);
    }

    step_write(tty, bytes, guard)
}

pub fn step_write_for_process(
    tty: &Cap<TtyIdentity>,
    bytes: &[u8],
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    let caller_info = match super::IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return StepOutcome::Err(err),
    };

    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let background = tty.session_pgrp().is_some_and(|binding| {
        !caller_info.in_foreground && caller_info.pgrp_id != binding.foreground_pgid
    });
    if background && payload.with_termios(|termios| termios.c_lflag & TOSTOP != 0) {
        let dispatch = background_write_signal(tty, caller_info);
        let _ = super::step_ioctl::deliver_signal_dispatch_for_process_with_guard(
            caller, dispatch, guard,
        );
        return StepOutcome::Err(Errno::EIO);
    }

    step_write(tty, bytes, guard)
}

fn kick_transport(tty: &Cap<TtyIdentity>, guard: &Guard<'_>) -> StepOutcome<usize> {
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    enum Kick {
        Hardware,
        Pty(Cap<TtyIdentity>),
    }

    let kick = match &payload.transport {
        TtyTransport::Hardware { .. } => Kick::Hardware,
        TtyTransport::Pty { peer } => Kick::Pty(peer.clone()),
    };

    let mut chunk = Vec::new();
    payload.with_output_queue(|output_queue| {
        while let Some(byte) = output_queue.pop() {
            chunk.push(byte);
        }
        if output_queue.space() > 0 {
            tty.output_writable.fire(TTY_WRITABLE);
        }
    });

    if chunk.is_empty() {
        return StepOutcome::Done(0);
    }

    match kick {
        Kick::Hardware => match &payload.transport {
            TtyTransport::Hardware { binding } => match binding.ops.write(&chunk, guard) {
                StepOutcome::Done(written) | StepOutcome::Advanced(written) => {
                    if written < chunk.len() {
                        restore_front(tty, &payload, &chunk[written..]);
                    }
                    StepOutcome::Done(written.min(chunk.len()))
                }
                StepOutcome::AdvancedThenBlocked(written, wait) => {
                    if written < chunk.len() {
                        restore_front(tty, &payload, &chunk[written..]);
                    }
                    StepOutcome::AdvancedThenBlocked(written.min(chunk.len()), wait)
                }
                StepOutcome::Blocked(wait) => {
                    restore_front(tty, &payload, &chunk);
                    StepOutcome::Blocked(wait)
                }
                StepOutcome::Err(err) => {
                    restore_front(tty, &payload, &chunk);
                    StepOutcome::Err(err)
                }
            },
            TtyTransport::Pty { .. } => StepOutcome::Err(Errno::EIO),
        },
        Kick::Pty(peer) => match step_ingest(&peer, &chunk, guard) {
            StepOutcome::Done(_) | StepOutcome::Advanced(_) => StepOutcome::Done(chunk.len()),
            StepOutcome::Err(err) => StepOutcome::Err(err),
            StepOutcome::Blocked(wait) => StepOutcome::Blocked(wait),
            StepOutcome::AdvancedThenBlocked(_, wait) => StepOutcome::Blocked(wait),
        },
    }
}

fn restore_front(
    tty: &Cap<TtyIdentity>,
    payload: &tx_substrate::zone::PayloadCap<crate::tty::structure::TtyPayload>,
    bytes: &[u8],
) {
    payload.with_output_queue(|output_queue| {
        for &byte in bytes.iter().rev() {
            let _ = output_queue.push_front(byte);
        }
        if output_queue.space() == 0 {
            tty.output_writable.clear(TTY_WRITABLE);
        }
    });
}
