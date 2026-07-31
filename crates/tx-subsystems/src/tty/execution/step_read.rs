//! `read(2)`-shaped TTY step.

use core::sync::atomic::Ordering;

use crate::tty::adapter::step_engine::Cap;

use crate::execution::Guard;
// `step_engine` alias is used both at runtime (by the StepOp wraps
// that acquire their own epoch guard per STEP_MODEL_v2 §1) and by
// tests, so it must not be cfg-test gated.
use crate::tty::adapter::step_engine::{self as step_engine};
use crate::tty::adapter::step_engine::{
    ByteProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use crate::tty::checks::{
    background_read_signal, require_fg_pgrp, require_fg_pgrp_for, require_live_tty,
};
use crate::tty::execution::TTY_READABLE;
use crate::tty::structure::termios::{ICANON, VMIN, VTIME};
use crate::tty::structure::TtyIdentity;

/// Blocking plan for a non-canonical `read(2)` on a TTY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TtyReadWaitPlan {
    /// A read can complete immediately.
    Ready,
    /// The caller must wait for more input; no termios timer is active yet.
    WaitIndefinite,
    /// The caller should wait for input or for `VTIME` deciseconds to expire.
    WaitVtime { deciseconds: u8 },
}

/// Return whether a process-side `read(2)` on this TTY can complete now.
///
/// This is the TTY-owned readiness predicate used by `poll`/`select` callers.
/// It intentionally follows the same staged non-canonical threshold policy as
/// [`step_read`]: `VMIN` is honored even when `VTIME` is non-zero, while the
/// actual timer expiry remains driven by the syscall wait path.
pub fn tty_read_would_complete(tty: &Cap<TtyIdentity>, guard: &Guard<'_>) -> bool {
    matches!(
        tty_read_wait_plan(tty, usize::MAX, guard),
        TtyReadWaitPlan::Ready
    )
}

/// Return the blocking plan for a `read(2)` of `out_len` bytes.
pub fn tty_read_wait_plan(
    tty: &Cap<TtyIdentity>,
    out_len: usize,
    guard: &Guard<'_>,
) -> TtyReadWaitPlan {
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(_) => return TtyReadWaitPlan::Ready,
    };

    if payload.eof_pending.load(Ordering::Acquire) {
        return TtyReadWaitPlan::Ready;
    }

    payload.with_termios(|termios| {
        payload.with_input_queue(|queue| {
            if termios.c_lflag & ICANON != 0 {
                return if queue.is_empty() {
                    TtyReadWaitPlan::WaitIndefinite
                } else {
                    TtyReadWaitPlan::Ready
                };
            }

            let queued = queue.len();
            let vmin = termios.c_cc[VMIN] as usize;
            let vtime = termios.c_cc[VTIME];

            match (vmin, vtime) {
                (0, 0) => TtyReadWaitPlan::Ready,
                (0, _) if queued > 0 => TtyReadWaitPlan::Ready,
                (0, deciseconds) => TtyReadWaitPlan::WaitVtime { deciseconds },
                (_, 0) if queued >= vmin.min(out_len) => TtyReadWaitPlan::Ready,
                (_, 0) => TtyReadWaitPlan::WaitIndefinite,
                (_, _) if queued >= vmin.min(out_len) => TtyReadWaitPlan::Ready,
                (_, deciseconds) if queued > 0 => TtyReadWaitPlan::WaitVtime { deciseconds },
                (_, _) => TtyReadWaitPlan::WaitIndefinite,
            }
        })
    })
}

/// Drain bytes from a live TTY input queue into `out`.
///
/// Empty queue returns a `Yield` on the TTY's wait source. A canonical
/// VEOF on an empty line returns `Done(0)` once via `eof_pending`.
pub fn step_read(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    step_read_inner(tty, out, guard, false)
}

pub fn step_read_after_vtime(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    step_read_inner(tty, out, guard, true)
}

fn step_read_inner(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    guard: &Guard<'_>,
    vtime_expired: bool,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::tty::adapter::step_engine::StepOutcome as V3;

    if out.is_empty() {
        return V3::Done(0);
    }

    if let Err(err) = require_fg_pgrp(tty, guard) {
        return V3::Err(err.into());
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => {
            return V3::Err(err.into());
        }
    };

    if payload.eof_pending.swap(false, Ordering::AcqRel) {
        tty.input_readable.clear(TTY_READABLE);
        return V3::Done(0);
    }

    let vmin_policy = payload.with_termios(|termios| {
        if termios.c_lflag & ICANON != 0 {
            None
        } else {
            Some((termios.c_cc[VMIN] as usize, termios.c_cc[VTIME]))
        }
    });

    let mut threshold_unmet = false;
    let copied = payload.with_input_queue(|queue| {
        if let Some((vmin, vtime)) = vmin_policy {
            let threshold = match (vmin, vtime, vtime_expired) {
                (0, 0, _) | (0, _, true) => 0,
                (0, _, false) => 1,
                (_, 0, _) => vmin.min(out.len()),
                (_, _, true) => 1,
                (_, _, false) => vmin.min(out.len()),
            };
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

    // Pre-ELF Phase 5 (item 9): the readable endpoint is the TTY identity's
    // registered `WaitSource`. `step_ingest` notifies it after any byte
    // ingest so `sys_read` parks until UART RX bytes arrive.
    // Threshold / VMIN logic comes from main's
    // 2026-05-06 tty work.
    if threshold_unmet {
        crate::tty::notification::yield_readable_for_tty(tty.read_endpoint())
    } else if copied == 0 {
        if matches!(vmin_policy, Some((0, _))) {
            V3::Done(0)
        } else {
            crate::tty::notification::yield_readable_for_tty(tty.read_endpoint())
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
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::tty::adapter::step_engine::StepOutcome as V3;

    if out.is_empty() {
        return V3::Done(0);
    }

    if let Err(err) = require_fg_pgrp_for(tty, Some(caller)) {
        let _ = background_read_signal(tty, caller);
        return V3::Err(err.into());
    }

    step_read(tty, out, guard)
}

pub fn step_read_after_vtime_for_process(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    use crate::tty::adapter::step_engine::StepOutcome as V3;

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

    step_read_after_vtime(tty, out, guard)
}

pub fn step_read_for_process(
    tty: &Cap<TtyIdentity>,
    out: &mut [u8],
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::tty::adapter::step_engine::StepOutcome as V3;

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
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct ReadOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        // Foreground-pgrp authority is explicit in `caller`; this step still
        // runs under the enclosing script subject.
        let _ = ctx.subject();
        let __guard = step_engine::guard();
        step_read(self.tty, self.out, &__guard)
    }
}

/// `StepOp` wrap of [`step_read_for_caller`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct ReadForCallerOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
    pub caller: super::IoctlCaller,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadForCallerOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_read_for_caller(self.tty, self.out, self.caller, &__guard)
    }
}

/// `StepOp` wrap of [`step_read_for_process`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct ReadForProcessOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
    pub caller: &'a Cap<crate::process::structure::ProcessIdentity>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadForProcessOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_read_for_process(self.tty, self.out, self.caller, &__guard)
    }
}

/// `StepOp` wrap of [`step_read_after_vtime_for_process`].
#[allow(dead_code)]
pub struct ReadForProcessAfterVtimeOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub out: &'a mut [u8],
    pub caller: &'a Cap<crate::process::structure::ProcessIdentity>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadForProcessAfterVtimeOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_read_after_vtime_for_process(self.tty, self.out, self.caller, &__guard)
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
        devt: DevT::new(4, 199),
        name: "tty-read-op-test",
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
    fn read_op_empty_out_returns_done_zero() {
        let _setup = setup();
        let tty = alloc_tty(200, "ttyV3-read-op-empty");
        let mut buf: [u8; 0] = [];
        let mut op = ReadOp {
            tty: &tty,
            out: &mut buf,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            V3::Done(0) => {}
            other => panic!("expected Done(0), got {other:?}"),
        }
    }

    #[test]
    fn read_for_caller_op_empty_out_returns_done_zero() {
        let _setup = setup();
        let tty = alloc_tty(201, "ttyV3-read-caller-op-empty");
        let mut buf: [u8; 0] = [];
        let caller = super::super::IoctlCaller::new(1, 1);
        let mut op = ReadForCallerOp {
            tty: &tty,
            out: &mut buf,
            caller,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
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
        let mut buf = [0u8; 4];
        let mut op = ReadOp {
            tty: &tty,
            out: &mut buf,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            V3::Err(step_engine::Errno::EIO) => {}
            other => panic!("expected Err(EIO), got {other:?}"),
        }
    }
}
