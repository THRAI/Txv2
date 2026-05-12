//! `write(2)`-shaped TTY step.

use alloc::vec::Vec;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::tty::checks::{background_write_signal, require_fg_pgrp, require_live_tty};
use crate::tty::execution::{step_ingest, TTY_WRITABLE};
use crate::tty::ldisc::process_output;
use crate::tty::structure::{termios::TOSTOP, TtyIdentity, TtyTransport};

/// Transform user bytes through N_TTY output processing, enqueue them, and
/// kick the underlying transport.
pub fn step_write_for_process(
    tty: &Cap<TtyIdentity>,
    bytes: &[u8],
    caller: &Cap<crate::process::structure::ProcessIdentity>,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    let caller_info = match super::IoctlCaller::from_process_with_guard(caller, guard) {
        Ok(caller_info) => caller_info,
        Err(err) => return V3::Err(err.into()),
    };

    if bytes.is_empty() {
        return V3::Done(0);
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    let background = tty.session_pgrp().is_some_and(|binding| {
        !caller_info.in_foreground && caller_info.pgrp_id != binding.foreground_pgid
    });
    if background && payload.with_termios(|termios| termios.c_lflag & TOSTOP != 0) {
        let dispatch = background_write_signal(tty, caller_info);
        let _ = super::step_ioctl::deliver_signal_dispatch_for_process_with_guard(
            caller, dispatch, guard,
        );
        return V3::Err(tx_substrate::step_v3::Errno::EIO);
    }

    step_write(tty, bytes, guard)
}

fn kick_transport(
    tty: &Cap<TtyIdentity>,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::{ByteProgress, StepOutcome as V3Out, YieldShape};
    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return V3Out::Err(err.into()),
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
        return V3Out::Done(0);
    }

    match kick {
        Kick::Hardware => match &payload.transport {
            TtyTransport::Hardware { binding } => match binding.ops.write(&chunk, guard) {
                V3Out::Done(written) => {
                    if written < chunk.len() {
                        restore_front(tty, &payload, &chunk[written..]);
                    }
                    V3Out::Done(written.min(chunk.len()))
                }
                V3Out::Continue { progress } => {
                    let written = progress.bytes();
                    if written < chunk.len() {
                        restore_front(tty, &payload, &chunk[written..]);
                    }
                    V3Out::Done(written.min(chunk.len()))
                }
                V3Out::Yield {
                    progress,
                    shape:
                        YieldShape::OnWaitSource {
                            source: carrier,
                            interests,
                        },
                } => {
                    let written = progress.bytes();
                    if written < chunk.len() {
                        restore_front(tty, &payload, &chunk[written..]);
                    } else {
                        // Defensive: if the driver claimed it advanced
                        // more than we passed, restore nothing.
                    }
                    if written == 0 {
                        // Pure block: restore the chunk so a later kick
                        // can try again.
                        restore_front(tty, &payload, &chunk);
                        V3Out::yield_on_wait_source(
                            ByteProgress::EMPTY,
                            carrier.raw(),
                            interests.raw(),
                        )
                    } else {
                        V3Out::yield_on_wait_source(
                            ByteProgress::new(written.min(chunk.len())),
                            carrier.raw(),
                            interests.raw(),
                        )
                    }
                }
                V3Out::Yield { .. } => {
                    restore_front(tty, &payload, &chunk);
                    V3Out::Err(tx_substrate::step_v3::Errno::EIO)
                }
                V3Out::Err(err) => {
                    restore_front(tty, &payload, &chunk);
                    V3Out::Err(err)
                }
            },
            TtyTransport::Pty { .. } => V3Out::Err(Errno::EIO.into()),
        },
        Kick::Pty(peer) => match step_ingest(&peer, &chunk, guard) {
            V3Out::Done(_) | V3Out::Continue { .. } => V3Out::Done(chunk.len()),
            V3Out::Err(e) => V3Out::Err(e),
            V3Out::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => V3Out::yield_on_wait_source(
                ByteProgress::new(chunk.len()),
                carrier.raw(),
                interests.raw(),
            ),
            V3Out::Yield { .. } => V3Out::Err(tx_substrate::step_v3::Errno::EIO),
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

// === step_v3-shape sibling fns ==========================================
//
// Sibling `*_v3` fns matching the same logic as the fns above but
// emitting `tx_substrate::step_v3::StepOutcome`. Bodies are run inline
// rather than delegating, to keep the step_v3 path independently
// testable and avoid a conversion-shim layer.
//
// `step_write` is the canonical `AdvancedThenBlocked` test of the
// step_v3 mapping. Unlike pipe (single-shot), the tty write path
// genuinely emits all five `execution::StepOutcome` variants. The
// step_v3 mapping:
//
// - empty bytes → `Done(0)` (pre-progress short-circuit)
// - require_fg_pgrp / require_live_tty err → `Err(e.into())`
// - process_output consumed nothing → `Yield { progress:
//   ByteProgress::EMPTY, shape: OnWaitSource { tty.raw(), TTY_WRITABLE } }`
//   (no progress, no bytes-driven yield — the level wire is the
//   carrier)
// - kick_transport Err → `Err(e.into())` (consumed bytes are left in
//   the output queue's restore-front state)
// - kick_transport Blocked / AdvancedThenBlocked → `Yield {
//   progress: ByteProgress::new(consumed), shape: OnWaitSource {
//   wait.carrier, wait.interest } }` — load-bearing: any
//   `Advanced(consumed)` collapses into the byte-progress accumulator
//   that travels through the yield, per STEP-1's monoid composition.
// - kick_transport Done | Advanced → `Done(consumed)` (terminal; the
//   call site considers the bytes terminal so `Advanced(consumed)` is
//   a no-op).
//
// We deliberately fully-qualify step_v3 types as
// `tx_substrate::step_v3::*` instead of adding a `use` so the
// `StepOutcome` / `Errno` already in scope from `crate::execution`
// keep working without rename gymnastics.

/// `write(2)`-shaped TTY step — step_v3 outcome shape.
///
/// Same body and semantics as [`step_write`], translated to a
/// [`tx_substrate::step_v3::StepOutcome`]:
///
/// - empty bytes → `Done(0)`
/// - foreground/liveness check fails → `Err(e.into())`
/// - line discipline produced no output (queue full upstream) →
///   `Yield { progress: ByteProgress::EMPTY, shape: OnWaitSource { tty.raw(),
///   TTY_WRITABLE } }`
/// - transport kick fails → `Err(e.into())`
/// - transport kick blocks (or advances-then-blocks) →
///   `Yield { progress: ByteProgress::new(consumed), shape: OnWaitSource {
///   wait.carrier, wait.interest } }`
/// - transport kick completes (Done / Advanced) → `Done(consumed)`
pub fn step_write(
    tty: &Cap<TtyIdentity>,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    if bytes.is_empty() {
        return tx_substrate::step_v3::StepOutcome::done(0);
    }

    if let Err(err) = require_fg_pgrp(tty, guard) {
        return tx_substrate::step_v3::StepOutcome::err(err.into());
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
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
        return tx_substrate::step_v3::StepOutcome::yield_on_wait_source(
            tx_substrate::step_v3::ByteProgress::EMPTY,
            tty.raw() as u64,
            TTY_WRITABLE,
        );
    }

    use tx_substrate::step_v3::{ByteProgress, StepOutcome as V3Out, YieldShape};
    match kick_transport(tty, guard) {
        V3Out::Err(err) => V3Out::err(err),
        V3Out::Yield {
            shape:
                YieldShape::OnWaitSource {
                    source: carrier,
                    interests,
                },
            ..
        } => {
            V3Out::yield_on_wait_source(ByteProgress::new(consumed), carrier.raw(), interests.raw())
        }
        V3Out::Yield { .. } => V3Out::err(tx_substrate::step_v3::Errno::EIO),
        V3Out::Done(_) | V3Out::Continue { .. } => V3Out::done(consumed),
    }
}

/// `step_write_for_caller`-shaped TTY step — step_v3 outcome shape.
///
/// Mirrors [`step_write_for_caller`] (background TOSTOP gate then the
/// shared `step_write` body), translated to
/// [`tx_substrate::step_v3::StepOutcome`]. The TOSTOP background-write
/// rejection becomes `Err(EIO.into())`.
pub fn step_write_for_caller(
    tty: &Cap<TtyIdentity>,
    bytes: &[u8],
    caller: super::IoctlCaller,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    if bytes.is_empty() {
        return tx_substrate::step_v3::StepOutcome::done(0);
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
    };

    let background = tty
        .session_pgrp()
        .is_some_and(|binding| !caller.in_foreground && caller.pgrp_id != binding.foreground_pgid);
    if background && payload.with_termios(|termios| termios.c_lflag & TOSTOP != 0) {
        return tx_substrate::step_v3::StepOutcome::err(Errno::EIO.into());
    }

    step_write(tty, bytes, guard)
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 2)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs under a single lifetime `'a` and delegates
// from `step()` to the corresponding free fn above — semantics are
// unchanged. The free fns remain the source of truth; callers can migrate
// to the `*Op` types incrementally.

/// `StepOp` wrap of [`step_write`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct WriteOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub bytes: &'a [u8],
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity> tx_substrate::step_v3::StepOp<I>
    for WriteOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_write(self.tty, self.bytes, self.guard)
    }
}

/// `StepOp` wrap of [`step_write_for_caller`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct WriteForCallerOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub bytes: &'a [u8],
    pub caller: super::IoctlCaller,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity> tx_substrate::step_v3::StepOp<I>
    for WriteForCallerOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_write_for_caller(self.tty, self.bytes, self.caller, self.guard)
    }
}

/// `StepOp` wrap of [`step_write_for_process`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct WriteForProcessOp<'a> {
    pub tty: &'a Cap<TtyIdentity>,
    pub bytes: &'a [u8],
    pub caller: &'a Cap<crate::process::structure::ProcessIdentity>,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity> tx_substrate::step_v3::StepOp<I>
    for WriteForProcessOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_write_for_process(self.tty, self.bytes, self.caller, self.guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_substrate::step_v3::StepProgress;
    use tx_substrate::zone::{self as zone_mod, PayloadCap};

    use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::tty::structure::{TtyKind, TtyPayload};

    // -- ops fixtures --------------------------------------------------

    /// Always-blocking write ops. The hardware kick path turns this into
    /// a `Blocked(wait)` from `kick_transport`, which `step_write` maps
    /// to `AdvancedThenBlocked(consumed, wait)`; `step_write` maps
    /// it to `Yield { progress: ByteProgress::new(consumed), shape:
    /// OnWaitSource { wait.carrier, wait.interest } }`.
    struct BlockingOps {
        carrier: u64,
        interest: u64,
    }

    impl CharDeviceOps for BlockingOps {
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
            _bytes: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress>
        {
            tx_substrate::step_v3::StepOutcome::yield_on_wait_source(
                tx_substrate::step_v3::ByteProgress::EMPTY,
                self.carrier,
                self.interest,
            )
        }
    }

    /// Always-Done(n) write ops: the hardware kick succeeds. Used for the
    /// happy path Done(consumed) test.
    struct CompletingOps;

    impl CharDeviceOps for CompletingOps {
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

    static BLOCKING_OPS: BlockingOps = BlockingOps {
        carrier: 0xABCD_0001,
        interest: 0x42,
    };
    static COMPLETING_OPS: CompletingOps = CompletingOps;

    static BLOCKING_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(4, 65),
        name: "tty-blocking",
        ops: &BLOCKING_OPS,
    };
    static COMPLETING_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(4, 66),
        name: "tty-completing",
        ops: &COMPLETING_OPS,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_substrate::testing::init_host_for_test_once();
        let _ = crate::zones::register_all();
        crate::tty::structure::registry::reset_for_tests();
        guard
    }

    fn alloc_tty_with(
        kind: TtyKind,
        index: u32,
        name: &str,
        payload: TtyPayload,
    ) -> Cap<TtyIdentity> {
        let id_res = zone_mod::reserve_for::<TtyIdentity>().expect("tty identity reservation");
        let payload_res = zone_mod::reserve_for::<crate::tty::structure::TtyPayload>()
            .expect("tty payload reservation");
        let payload_cap = PayloadCap::from_cap(zone_mod::sign_for(payload_res, payload));
        let identity = zone_mod::sign_for(id_res, TtyIdentity::new(kind, index, name));
        identity.install_payload(payload_cap);
        identity
    }

    // -- step_write tests ------------------------------------------

    #[test]
    fn step_write_empty_bytes_returns_done_zero() {
        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            0,
            "ttyV3-empty",
            TtyPayload::new_hardware(&COMPLETING_BINDING),
        );
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&tty, b"", &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0), got {other:?}"),
        }
    }

    #[test]
    fn step_write_dead_tty_returns_err_eio() {
        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            1,
            "ttyV3-dead",
            TtyPayload::new_hardware(&COMPLETING_BINDING),
        );
        let _ = tty.take_payload();
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&tty, b"x", &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EIO) => {}
            other => panic!("expected v3 Err(EIO), got {other:?}"),
        }
    }

    #[test]
    fn step_write_completing_kick_returns_done_consumed() {
        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            2,
            "ttyV3-done",
            TtyPayload::new_hardware(&COMPLETING_BINDING),
        );
        let guard = tx_substrate::epoch::guard();
        // 5 bytes; canonical termios -- process_output should pass them
        // through cleanly, then completing ops drains the queue.
        let outcome = step_write(&tty, b"hello", &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(n) => {
                assert_eq!(n, 5, "v3 Done(consumed) must report 5 bytes");
            }
            other => panic!("expected v3 Done(5), got {other:?}"),
        }
    }

    /// **Load-bearing test.** When the hardware kick blocks after
    /// `process_output` consumed bytes, `step_write` emits
    /// `AdvancedThenBlocked(consumed, wait)`. `step_write` must
    /// surface this as `Yield { progress: ByteProgress::new(consumed),
    /// shape: OnWaitSource { wait.carrier, wait.interest } }`. This pins
    /// the `ByteProgress::new(consumed)` ergonomics inside
    /// `yield_on_wait_source`.
    #[test]
    fn step_write_partial_then_blocked_yields_on_wait_source_with_byte_progress() {
        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            3,
            "ttyV3-partial",
            TtyPayload::new_hardware(&BLOCKING_BINDING),
        );
        let guard = tx_substrate::epoch::guard();
        let outcome = step_write(&tty, b"hello", &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Yield {
                progress,
                shape:
                    tx_substrate::step_v3::YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
            } => {
                assert!(
                    !progress.is_empty(),
                    "advanced-then-blocked yield must carry non-empty ByteProgress",
                );
                assert_eq!(
                    progress.bytes(),
                    5,
                    "ByteProgress must carry the consumed byte count",
                );
                assert_eq!(
                    carrier.raw(),
                    BLOCKING_OPS.carrier,
                    "yield carrier id must be the blocking ops' carrier",
                );
                assert_eq!(
                    interests.raw(),
                    BLOCKING_OPS.interest,
                    "yield interest mask must match the blocking ops' interest",
                );
            }
            other => panic!("expected v3 Yield::OnWaitSource with byte progress, got {other:?}"),
        }
    }

    // -- step_write_for_caller tests --------------------------------

    #[test]
    fn step_write_for_caller_empty_bytes_returns_done_zero() {
        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            10,
            "ttyV3-caller-empty",
            TtyPayload::new_hardware(&COMPLETING_BINDING),
        );
        let guard = tx_substrate::epoch::guard();
        let caller = super::super::IoctlCaller::new(1, 1);
        let outcome = step_write_for_caller(&tty, b"", caller, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0), got {other:?}"),
        }
    }

    #[test]
    fn step_write_for_caller_foreground_caller_completes_to_done() {
        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            11,
            "ttyV3-caller-fg",
            TtyPayload::new_hardware(&COMPLETING_BINDING),
        );
        let guard = tx_substrate::epoch::guard();
        // Default IoctlCaller has in_foreground=true, so TOSTOP gate is
        // skipped and step_write runs to completion.
        let caller = super::super::IoctlCaller::new(1, 1);
        let outcome = step_write_for_caller(&tty, b"hi", caller, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(2) => {}
            other => panic!("expected v3 Done(2), got {other:?}"),
        }
    }

    #[test]
    fn step_write_for_caller_background_caller_with_tostop_returns_eio() {
        use crate::tty::structure::termios::TOSTOP as TOSTOP_FLAG;
        use crate::tty::structure::SessionPgrp;

        let _setup = setup();
        let tty = alloc_tty_with(
            TtyKind::SerialHardware,
            12,
            "ttyV3-caller-bg",
            TtyPayload::new_hardware(&COMPLETING_BINDING),
        );
        // Bind a session/pgrp with a foreground pgid different from the
        // caller's pgid so the background-write check trips.
        tty.bind_session_pgrp(SessionPgrp::from_raw_ids(1, 1, 2));
        // Set TOSTOP on the termios so the background write is rejected.
        let payload = tty.live_payload().expect("payload");
        let mut new_termios = payload.with_termios(|t| *t);
        new_termios.c_lflag |= TOSTOP_FLAG;
        payload.publish_termios(new_termios);
        let guard = tx_substrate::epoch::guard();
        // Caller in pgrp 7 (≠ foreground 2) and not in_foreground.
        let mut caller = super::super::IoctlCaller::new(1, 7);
        caller.in_foreground = false;
        let outcome = step_write_for_caller(&tty, b"x", caller, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EIO) => {}
            other => panic!("expected v3 Err(EIO) for TOSTOP background write, got {other:?}"),
        }
    }

    // -- PR-2 wave 2: StepOp wrap tests -----------------------------------
    //
    // Minimal `op.step(&mut ctx)` smoke tests pinning that each wrap
    // delegates to the matching free fn. Compile-check is the primary
    // value.
    mod step_op_wraps {
        use super::super::{WriteForCallerOp, WriteOp};
        use super::{alloc_tty_with, setup, COMPLETING_BINDING};
        use crate::tty::structure::{TtyKind, TtyPayload};
        use tx_substrate::step_v3::{ScriptCtx, StepOp, StepOutcome as V3};

        #[test]
        fn write_op_empty_bytes_returns_done_zero() {
            let _setup = setup();
            let tty = alloc_tty_with(
                TtyKind::SerialHardware,
                100,
                "ttyV3-op-empty",
                TtyPayload::new_hardware(&COMPLETING_BINDING),
            );
            let guard = tx_substrate::epoch::guard();
            let mut op = WriteOp {
                tty: &tty,
                bytes: b"",
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
        fn write_op_delegates_to_step_write() {
            let _setup = setup();
            let tty = alloc_tty_with(
                TtyKind::SerialHardware,
                101,
                "ttyV3-op-done",
                TtyPayload::new_hardware(&COMPLETING_BINDING),
            );
            let guard = tx_substrate::epoch::guard();
            let mut op = WriteOp {
                tty: &tty,
                bytes: b"hello",
                guard: &guard,
            };
            let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
            let outcome = op.step(&mut ctx);
            drop(guard);
            match outcome {
                V3::Done(5) => {}
                other => panic!("expected Done(5), got {other:?}"),
            }
        }

        #[test]
        fn write_for_caller_op_empty_bytes_returns_done_zero() {
            let _setup = setup();
            let tty = alloc_tty_with(
                TtyKind::SerialHardware,
                102,
                "ttyV3-op-caller-empty",
                TtyPayload::new_hardware(&COMPLETING_BINDING),
            );
            let guard = tx_substrate::epoch::guard();
            let caller = super::super::super::IoctlCaller::new(1, 1);
            let mut op = WriteForCallerOp {
                tty: &tty,
                bytes: b"",
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
        fn write_for_caller_op_matches_free_fn() {
            let _setup = setup();
            let tty = alloc_tty_with(
                TtyKind::SerialHardware,
                103,
                "ttyV3-op-caller-match",
                TtyPayload::new_hardware(&COMPLETING_BINDING),
            );
            let guard = tx_substrate::epoch::guard();
            let caller = super::super::super::IoctlCaller::new(1, 1);
            let mut op = WriteForCallerOp {
                tty: &tty,
                bytes: b"hi",
                caller,
                guard: &guard,
            };
            let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
            let wrap_outcome = op.step(&mut ctx);
            // Free fn parallel call observed independently; cannot run on
            // same tty without re-fixturing, so the wrap outcome is
            // checked against an expected Done(2) (CompletingOps reports
            // the queued bytes).
            drop(guard);
            match wrap_outcome {
                V3::Done(2) => {}
                other => panic!("expected Done(2), got {other:?}"),
            }
        }
    }
}
