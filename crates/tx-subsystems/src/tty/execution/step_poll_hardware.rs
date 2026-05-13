//! Poll-driven hardware RX ingest helper.
//!
//! This is the Phase F bridge before the full reactor/IRQ runtime lands:
//! board or driver code can call into this step after a UART readable event,
//! or from a polling loop, and the bytes will be fed through the tty line
//! discipline exactly like future IRQ-driven ingest.

use alloc::vec;

use crate::tty::adapter::step_engine::Cap;

use crate::execution::{Errno, Guard};
#[cfg(test)]
use crate::tty::adapter::step_engine::ByteProgress;
#[cfg(test)]
use crate::tty::adapter::step_engine::{self as step_engine};
use crate::tty::adapter::step_engine::{
    NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use crate::tty::checks::require_live_tty;
use crate::tty::execution::{step_ingest, IngestOutcome};
use crate::tty::structure::{TtyIdentity, TtyTransport};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HardwarePollOutcome {
    pub bytes_read: usize,
    pub ingest: IngestOutcome,
}

/// Read up to `max_bytes` from a hardware-backed tty transport and feed the
/// result into `step_ingest`.
///
/// v3-shape: a one-shot `NoProgress` outcome. The pre-v3 flavour
/// surfaced `AdvancedThenBlocked(outcome, wait)` when the driver's
/// `read` returned partial bytes followed by a wait token; in v3 the
/// closed catalog only allows `Done | Yield | Err` for `NoProgress`
/// ops, so we collapse partial-then-blocked into `Done(outcome)` —
/// the caller observes the bytes already ingested and re-enters the
/// step the next time the carrier fires.
pub fn step_poll_hardware_input(
    tty: &Cap<TtyIdentity>,
    max_bytes: usize,
    guard: &Guard<'_>,
) -> StepOutcome<HardwarePollOutcome, NoProgress> {
    use crate::tty::adapter::step_engine::{NoProgress, StepOutcome as V3, YieldShape};

    if max_bytes == 0 {
        return V3::Done(HardwarePollOutcome::default());
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    let binding = match &payload.transport {
        TtyTransport::Hardware { binding } => *binding,
        TtyTransport::Pty { .. } => return V3::Err(Errno::EINVAL.into()),
    };

    // Helper: dispatch ingest's v3 outcome up into our v3 outcome.
    let drive_ingest = |bytes: &[u8],
                        read: usize,
                        guard: &Guard<'_>|
     -> StepOutcome<HardwarePollOutcome, NoProgress> {
        match step_ingest(tty, bytes, guard) {
            V3::Done(ingest) => V3::Done(HardwarePollOutcome {
                bytes_read: read,
                ingest,
            }),
            V3::Continue { .. } => V3::Done(HardwarePollOutcome {
                bytes_read: read,
                ingest: IngestOutcome::default(),
            }),
            V3::Yield {
                progress: _,
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
            } => V3::yield_on_wait_source(NoProgress, carrier.raw(), interests.raw()),
            V3::Yield { shape, .. } => V3::Yield {
                progress: NoProgress,
                shape,
            },
            V3::Err(e) => V3::Err(e),
        }
    };

    let mut bytes = vec![0u8; max_bytes];
    // `binding.ops.read` is now a v3 `StepOutcome<usize, ByteProgress>`.
    // Map directly:
    // - `Done(n)` → ingest the bytes (or short-circuit at n=0).
    // - `Continue { progress }` → bytes were consumed but the op asked
    //   to continue without waiting; ingest those bytes (the caller's
    //   next poll re-enters the step).
    // - `Yield { progress, shape: OnWaitSource }` with non-empty progress
    //   → ingest the bytes and surface the inner outcome (matches the
    //   pre-v3 `AdvancedThenBlocked` collapse). With empty progress the
    //   yield-on-wait-source propagates as-is.
    // - `Yield { shape: OnAgent .. }` → unsupported; surface `Err(EIO)`.
    // - `Err(errno)` → propagate.
    match binding.ops.read(&mut bytes, guard) {
        V3::Done(read) => {
            let read = read.min(bytes.len());
            if read == 0 {
                return V3::Done(HardwarePollOutcome::default());
            }
            bytes.truncate(read);
            drive_ingest(&bytes, read, guard)
        }
        V3::Continue { progress } => {
            let read = progress.bytes().min(bytes.len());
            if read == 0 {
                return V3::Done(HardwarePollOutcome::default());
            }
            bytes.truncate(read);
            drive_ingest(&bytes, read, guard)
        }
        V3::Yield {
            progress,
            shape:
                YieldShape::OnWaitSource {
                    source: carrier,
                    interests,
                },
        } => {
            let read = progress.bytes().min(bytes.len());
            if read == 0 {
                return V3::yield_on_wait_source(NoProgress, carrier.raw(), interests.raw());
            }
            bytes.truncate(read);
            drive_ingest(&bytes, read, guard)
        }
        V3::Yield { .. } => V3::Err(Errno::EIO.into()),
        V3::Err(err) => V3::Err(err),
    }
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 2)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapter per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// `max_bytes` is `Copy` scalar; the guard is held by reference under the
// wrap lifetime.

/// `StepOp` wrap of [`step_poll_hardware_input`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct PollHardwareInputOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub max_bytes: usize,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for PollHardwareInputOp<'a> {
    type Output = HardwarePollOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_poll_hardware_input(self.tty, self.max_bytes, self.guard)
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
        devt: DevT::new(4, 209),
        name: "tty-poll-op-test",
        ops: &NOOP_OPS,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = crate::zones::register_all();
        crate::tty::structure::registry::reset_for_tests();
        guard
    }

    fn alloc_tty(index: u32, name: &str) -> Cap<TtyIdentity> {
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
    fn poll_hardware_input_op_zero_max_bytes_returns_done_default() {
        let _setup = setup();
        let tty = alloc_tty(300, "ttyV3-poll-op-zero");
        let guard = step_engine::guard();
        let mut op = PollHardwareInputOp {
            tty: &tty,
            max_bytes: 0,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Done(out) => {
                assert_eq!(out.bytes_read, 0);
            }
            other => panic!("expected Done(default), got {other:?}"),
        }
    }

    #[test]
    fn poll_hardware_input_op_dead_tty_returns_err() {
        let _setup = setup();
        let tty = alloc_tty(301, "ttyV3-poll-op-dead");
        let _ = tty.take_payload();
        let guard = step_engine::guard();
        let mut op = PollHardwareInputOp {
            tty: &tty,
            max_bytes: 8,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Err(_) => {}
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
