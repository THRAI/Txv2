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
/// Empty queue returns a `Yield` on the TTY's wait source. A canonical
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

    // Pre-ELF Phase 5 (item 9): the wait source is the TTY
    // identity's `wait_channel`, registered with the global
    // `wait_source` resolver at construction. `step_ingest` fires
    // it after any byte ingest, so `sys_read`'s
    // `wait_on_token(token).await` actually parks until UART RX
    // bytes arrive. Threshold / VMIN logic comes from main's
    // 2026-05-06 tty work.
    if threshold_unmet {
        V3::yield_on_wait_source(ByteProgress::EMPTY, tty.wait_source_id(), TTY_READABLE)
    } else if copied == 0 {
        if matches!(vmin_policy, Some(0)) {
            V3::Done(0)
        } else {
            V3::yield_on_wait_source(ByteProgress::EMPTY, tty.wait_source_id(), TTY_READABLE)
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

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 2)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs under a single lifetime `'a` and delegates
// from `step()` to the corresponding free fn above — semantics are
// unchanged. The output buffer is held by `&'a mut [u8]`.

/// `StepOp` wrap of [`step_read`].
pub struct ReadOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ReadOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_read(self.tty, self.out, self.guard)
    }
}

/// `StepOp` wrap of [`step_read_for_caller`].
pub struct ReadForCallerOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
    pub caller: super::IoctlCaller,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ReadForCallerOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_read_for_caller(self.tty, self.out, self.caller, self.guard)
    }
}

/// `StepOp` wrap of [`step_read_for_process`].
pub struct ReadForProcessOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
    pub caller: &'a Cap<crate::process::structure::ProcessIdentity>,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ReadForProcessOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_read_for_process(self.tty, self.out, self.caller, self.guard)
    }
}

#[cfg(test)]
mod step_op_wraps {
    use super::*;
    use tx_substrate::step_v3::{ScriptCtx, StepOp, StepOutcome as V3};
    use tx_substrate::zone::{self as zone_mod, PayloadCap};

    use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::tty::structure::{TtyKind, TtyPayload};

    struct NoopOps;

    impl CharDeviceOps for NoopOps {
        fn read(
            &self,
            _out: &mut [u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress>
        {
            tx_substrate::step_v3::StepOutcome::Done(0)
        }

        fn write(
            &self,
            bytes: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress>
        {
            tx_substrate::step_v3::StepOutcome::Done(bytes.len())
        }
    }

    static NOOP_OPS: NoopOps = NoopOps;
    static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(4, 199),
        name: "tty-read-op-test",
        ops: &NOOP_OPS,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_substrate::testing::init_host_for_test_once();
        let _ = crate::zones::register_all();
        crate::tty::structure::registry::reset_for_tests();
        guard
    }

    fn alloc_tty(index: u32, name: &str) -> Cap<TtyIdentity> {
        let id_res = zone_mod::reserve_for::<TtyIdentity>().expect("tty identity reservation");
        let payload_res =
            zone_mod::reserve_for::<TtyPayload>().expect("tty payload reservation");
        let payload_cap = PayloadCap::from_cap(zone_mod::sign_for(
            payload_res,
            TtyPayload::new_hardware(&NOOP_BINDING),
        ));
        let identity = zone_mod::sign_for(
            id_res,
            TtyIdentity::new(TtyKind::SerialHardware, index, name),
        );
        identity.install_payload(payload_cap);
        identity
    }

    #[test]
    fn read_op_empty_out_returns_done_zero() {
        let _setup = setup();
        let tty = alloc_tty(200, "ttyV3-read-op-empty");
        let guard = tx_substrate::epoch::guard();
        let mut buf: [u8; 0] = [];
        let mut op = ReadOp {
            tty: &tty,
            out: &mut buf,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Done(0) => {}
            other => panic!("expected Done(0), got {other:?}"),
        }
    }

    #[test]
    fn read_for_caller_op_empty_out_returns_done_zero() {
        let _setup = setup();
        let tty = alloc_tty(201, "ttyV3-read-caller-op-empty");
        let guard = tx_substrate::epoch::guard();
        let mut buf: [u8; 0] = [];
        let caller = super::super::IoctlCaller::new(1, 1);
        let mut op = ReadForCallerOp {
            tty: &tty,
            out: &mut buf,
            caller,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Done(0) => {}
            other => panic!("expected Done(0), got {other:?}"),
        }
    }

    #[test]
    fn read_op_dead_tty_returns_err_eio() {
        let _setup = setup();
        let tty = alloc_tty(202, "ttyV3-read-op-dead");
        let _ = tty.take_payload();
        let guard = tx_substrate::epoch::guard();
        let mut buf = [0u8; 4];
        let mut op = ReadOp {
            tty: &tty,
            out: &mut buf,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Err(tx_substrate::step_v3::Errno::EIO) => {}
            other => panic!("expected Err(EIO), got {other:?}"),
        }
    }
}
