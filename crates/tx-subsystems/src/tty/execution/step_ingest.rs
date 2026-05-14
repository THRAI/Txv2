//! Transport-to-TTY ingest step.

use core::sync::atomic::Ordering;

use crate::tty::adapter::step_engine::Cap;
use crate::tty::adapter::wait_routing::Mask;

use crate::execution::Guard;
#[cfg(test)]
use crate::tty::adapter::step_engine::ByteProgress;
#[cfg(test)]
use crate::tty::adapter::step_engine::{self as step_engine};
use crate::tty::adapter::step_engine::{
    InterestMask, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
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
) -> StepOutcome<IngestOutcome, NoProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::tty::adapter::step_engine::StepOutcome as V3;
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
        // PR-3D-4 (D2 coexistence): fire the new `WaitSource`
        // alongside the legacy `Channel`. Same `WaitSourceId`
        // namespace, same `TTY_READABLE` interest bit. Subscribers
        // installed via `WaitSource::prepare(..).install_if(..)`
        // receive a `MailboxEvent::SourceFired` posted under the same
        // payload-observation arm as the Channel fire above.
        tty.wait_source()
            .notify_emit(InterestMask::new(TTY_READABLE));
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
                            // PR-3D-4 (D2 coexistence): paired notify
                            // on the new `WaitSource`. See the
                            // companion site above for the rationale.
                            tty.wait_source()
                                .notify_emit(InterestMask::new(TTY_READABLE));
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

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 cleanup)
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`step_ingest`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct IngestOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub bytes: &'a [u8],
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for IngestOp<'a> {
    type Output = IngestOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_ingest(self.tty, self.bytes, self.guard)
    }
}

#[cfg(test)]
mod step_op_wraps {
    use super::*;
    use crate::tty::adapter::step_engine::{
        reserve_for, sign_for, PayloadCap, PlaceholderProcessSubject, ScriptCtx, StepOp,
        StepOutcome as V3,
    };

    use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::tty::structure::{TtyKind, TtyPayload};

    struct NoopOps;

    impl CharDeviceOps for NoopOps {
        fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
            StepOutcome::Done(0)
        }

        fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
            StepOutcome::Done(bytes.len())
        }
    }

    static NOOP_OPS: NoopOps = NoopOps;
    static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(4, 240),
        name: "tty-ingest-op-test",
        ops: &NOOP_OPS,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = crate::zones::register_all();
        crate::tty::structure::registry::reset_for_tests();
        guard
    }

    fn alloc_hardware_tty(index: u32, name: &str) -> Cap<TtyIdentity> {
        let id_res = reserve_for::<TtyIdentity>().expect("tty identity reservation");
        let payload_res = reserve_for::<TtyPayload>().expect("tty payload reservation");
        let payload_cap = PayloadCap::from_cap(sign_for(
            payload_res,
            TtyPayload::new_hardware(&NOOP_BINDING),
        ));
        let identity = sign_for(
            id_res,
            TtyIdentity::new(TtyKind::SerialHardware, index, name),
        );
        identity.install_payload(payload_cap);
        identity
    }

    #[test]
    fn ingest_op_consumes_bytes() {
        let _setup = setup();
        let tty = alloc_hardware_tty(600, "ttyV3-ingest-op-live");
        let guard = step_engine::guard();
        let bytes: &[u8] = b"hi";
        let mut op = IngestOp {
            tty: &tty,
            bytes,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Done(o) => assert_eq!(o.consumed, bytes.len()),
            other => panic!("expected Done(_), got {other:?}"),
        }
    }

    #[test]
    fn ingest_op_on_dead_tty_errors() {
        let _setup = setup();
        let tty = alloc_hardware_tty(601, "ttyV3-ingest-op-dead");
        let _ = tty.take_payload();
        let guard = step_engine::guard();
        let mut op = IngestOp {
            tty: &tty,
            bytes: b"x",
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Err(_) => {}
            other => panic!("expected Err(_), got {other:?}"),
        }
    }
}
