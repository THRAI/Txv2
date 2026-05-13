//! Master-close hangup path for ptys.

use crate::tty::adapter::step_engine::Cap;

use super::step_hangup::{step_hangup, HangupOutcome};
use super::step_ioctl::IoctlSideEffect;
use crate::execution::{Errno, Guard};
#[cfg(test)]
use crate::tty::adapter::step_engine::ByteProgress;
#[cfg(test)]
use crate::tty::adapter::step_engine::{self as step_engine};
use crate::tty::adapter::step_engine::{
    NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use crate::tty::checks::require_live_tty;
use crate::tty::structure::{TtyIdentity, TtyTransport};

pub fn step_master_close_last(
    master: &Cap<TtyIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<(HangupOutcome, IoctlSideEffect), NoProgress> {
    use crate::tty::adapter::step_engine::StepOutcome as V3;

    let payload = match require_live_tty(master, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    let peer = match &payload.transport {
        TtyTransport::Pty { peer } => peer.clone(),
        TtyTransport::Hardware { .. } => return V3::Err(Errno::EINVAL.into()),
    };

    let hangup = match step_hangup(&peer, guard) {
        V3::Done(outcome) => outcome,
        V3::Err(e) => return V3::Err(e),
        V3::Continue { .. } | V3::Yield { .. } => return V3::Err(Errno::EIO.into()),
    };

    let _ = master.take_payload();
    V3::Done((hangup, IoctlSideEffect::default()))
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 3)
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`step_master_close_last`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct MasterCloseLastOp<'a> {
    pub master: &'a Cap<TtyIdentity>,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for MasterCloseLastOp<'a> {
    type Output = (HangupOutcome, IoctlSideEffect);
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_master_close_last(self.master, self.guard)
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
        devt: DevT::new(4, 220),
        name: "tty-master-close-op-test",
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
    fn master_close_op_on_hardware_returns_einval() {
        let _setup = setup();
        // A hardware (non-pty) TTY is rejected with EINVAL since its
        // transport isn't `Pty { .. }`.
        let tty = alloc_hardware_tty(400, "ttyV3-master-close-hw");
        let guard = step_engine::guard();
        let mut op = MasterCloseLastOp {
            master: &tty,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Err(step_engine::Errno::EINVAL) => {}
            other => panic!("expected Err(EINVAL), got {other:?}"),
        }
    }

    #[test]
    fn master_close_op_on_dead_tty_returns_eio() {
        let _setup = setup();
        let tty = alloc_hardware_tty(401, "ttyV3-master-close-dead");
        let _ = tty.take_payload();
        let guard = step_engine::guard();
        let mut op = MasterCloseLastOp {
            master: &tty,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Err(step_engine::Errno::EIO) => {}
            other => panic!("expected Err(EIO), got {other:?}"),
        }
    }
}
