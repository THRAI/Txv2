//! Transport-to-TTY ingest step.

use core::sync::atomic::Ordering;

use tx_reactor::wait::Mask;
use tx_substrate::zone::Cap;

use crate::execution::Guard;
use crate::tty::checks::require_live_tty;
use crate::tty::execution::{
    deferred_signal_for_tty, SignalDispatch, TTY_DEFERRED_SIGNAL, TTY_READABLE, TTY_WRITABLE,
};
use crate::tty::ldisc::{process_input_byte, FlowCtl, LdiscInputEffect, SignalKind};
use crate::tty::structure::TtyIdentity;

/// Deferred signal observed while ingesting bytes.
///
/// Phase C records this event and publishes `session_ctl_port`; Phase G will
/// consume the same event shape to call the signal subsystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeferredSignalEvent {
    pub signal: SignalKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IngestOutcome {
    pub consumed: usize,
    pub readable_fired: bool,
    pub writable_fired: bool,
    pub deferred_signal: Option<DeferredSignalEvent>,
    pub signal_dispatch: Option<SignalDispatch>,
    pub flow_control: Option<FlowCtl>,
}

/// Ingest transport bytes into a TTY's line discipline.
pub fn step_ingest(
    tty: &Cap<TtyIdentity>,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<IngestOutcome, tx_substrate::step_v3::NoProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    let mut outcome = IngestOutcome::default();
    let mut output_touched = false;
    let linearized = payload.apply_ingest_linearizer();
    if linearized.readable_fired {
        tty.input_readable.fire(TTY_READABLE);
        // Pre-ELF Phase 5 (item 9): the BIF-5 readiness wire is
        // RawQueue-shaped and only wakes RawQueue subscribers; the
        // wait-carrier registry that `sys_read`'s `wait_on_token`
        // loop drives is `Channel`-shaped, so we fire both. The
        // Channel is registered at TTY construction; see
        // `TtyIdentity::new`.
        tty.wait_channel().fire(Mask::from_bits(TTY_READABLE));
        outcome.readable_fired = true;
    }
    if linearized.writable_fired {
        tty.output_writable.fire(TTY_WRITABLE);
        outcome.writable_fired = true;
    }

    payload.with_termios(|termios| {
        payload.with_input_queue(|input_queue| {
            payload.with_output_queue(|output_queue| {
                for &byte in bytes {
                    let input_len_before = input_queue.len();
                    let output_len_before = output_queue.len();
                    let effect = {
                        // SAFETY: Phase C keeps `step_ingest` as the only
                        // mutable accessor to ldisc_state. Later external
                        // mutators route through the documented linearizer.
                        let state = unsafe { &mut *payload.ldisc_state.get() };
                        process_input_byte(state, termios, input_queue, output_queue, byte)
                    };
                    outcome.consumed += 1;
                    if output_queue.len() != output_len_before {
                        output_touched = true;
                    }

                    match effect {
                        LdiscInputEffect::QueuedForRead | LdiscInputEffect::LineCommitted => {
                            if effect == LdiscInputEffect::LineCommitted
                                && input_queue.len() == input_len_before
                            {
                                payload.eof_pending.store(true, Ordering::Release);
                            }
                            tty.input_readable.fire(TTY_READABLE);
                            // Pre-ELF Phase 5 (item 9): see the
                            // companion comment near the linearizer
                            // fire above. The wait-carrier `Channel`
                            // wakes `sys_read`'s blocking-read loop.
                            tty.wait_channel().fire(Mask::from_bits(TTY_READABLE));
                            outcome.readable_fired = true;
                        }
                        LdiscInputEffect::SignalFgPgrp(signal) => {
                            outcome.deferred_signal = Some(DeferredSignalEvent { signal });
                            outcome.signal_dispatch = deferred_signal_for_tty(tty, signal);
                            tty.session_ctl_port.fire(TTY_DEFERRED_SIGNAL);
                        }
                        LdiscInputEffect::FlowControl(flow) => {
                            outcome.flow_control = Some(flow);
                        }
                        LdiscInputEffect::Absorbed => {}
                    }
                }

                if output_touched && output_queue.space() > 0 {
                    tty.output_writable.fire(TTY_WRITABLE);
                    outcome.writable_fired = true;
                }
            });
        });
    });

    V3::Done(outcome)
}
