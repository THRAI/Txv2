//! TTY hangup execution steps.

use crate::tty::adapter::step_engine::Cap;

use super::step_ioctl::{JobControlSignal, SessionCtlEvent, SignalDispatch, SignalTarget};
use crate::execution::{Errno, Guard};
#[cfg(test)]
use crate::tty::adapter::step_engine::ByteProgress;
#[cfg(test)]
use crate::tty::adapter::step_engine::{self as step_engine};
use crate::tty::adapter::step_engine::{
    NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use crate::tty::structure::TtyIdentity;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HangupOutcome {
    pub had_payload: bool,
    pub hangup_fired: bool,
    pub session_ctl_fired: bool,
    pub hup_signal: Option<SignalDispatch>,
    pub cont_signal: Option<SignalDispatch>,
}

pub fn step_hangup(
    tty: &Cap<TtyIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<HangupOutcome, NoProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::tty::adapter::step_engine::StepOutcome as V3;

    let binding = tty.session_pgrp();
    let session_leader_pgrp = binding
        .and_then(|binding| {
            binding
                .session
                .as_ref()
                .and_then(|weak| weak.upgrade(guard))
        })
        .and_then(|session| session.leader_pgrp_cap_with_guard(guard));
    if !tty.is_live() {
        return V3::Err(Errno::EIO.into());
    }

    if binding.is_some() {
        tty.clear_session_pgrp();
    }
    let had_payload = tty.take_payload().is_some();
    tty.hangup_port.fire(1);
    tty.session_ctl_port
        .fire(SessionCtlEvent::LostControllingTty as u64);

    V3::Done(HangupOutcome {
        had_payload,
        hangup_fired: true,
        session_ctl_fired: true,
        hup_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::SessionLeaderProcessGroup {
                pgid: binding.session_leader_pgid,
                pgrp: session_leader_pgrp.map(|pgrp| pgrp.downgrade()),
            },
            signal: JobControlSignal::Hup,
        }),
        cont_signal: binding.map(|binding| SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup {
                pgid: binding.foreground_pgid,
                pgrp: binding.foreground_pgrp,
            },
            signal: JobControlSignal::Cont,
        }),
    })
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 cleanup)
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`step_hangup`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct HangupOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for HangupOp<'a> {
    type Output = HangupOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_hangup(self.tty, self.guard)
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
        devt: DevT::new(4, 230),
        name: "tty-hangup-op-test",
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
    fn hangup_op_on_live_tty_returns_done() {
        let _setup = setup();
        let tty = alloc_hardware_tty(500, "ttyV3-hangup-op-live");
        let guard = step_engine::guard();
        let mut op = HangupOp {
            tty: &tty,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Done(o) => {
                assert!(o.had_payload);
                assert!(o.hangup_fired);
                assert!(o.session_ctl_fired);
            }
            other => panic!("expected Done(_), got {other:?}"),
        }
    }

    #[test]
    fn hangup_op_on_dead_tty_returns_eio() {
        let _setup = setup();
        let tty = alloc_hardware_tty(501, "ttyV3-hangup-op-dead");
        let _ = tty.take_payload();
        let guard = step_engine::guard();
        let mut op = HangupOp {
            tty: &tty,
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
