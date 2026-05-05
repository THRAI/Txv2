//! Trap-shell hand-off: platform-neutral conversion of a userspace
//! trap into the reactor's `UserspaceTrapInfo` payloads, plus the
//! routing helper that resolves the active `UserspaceRunSlot` for the
//! current hart.
//!
//! Per the Trio plan's "Part 1 — Trap shell" and the Plan B writeback
//! discipline (locked in via "Cross-cutting risks #1"): the trap
//! shell does **not** mutate the trap frame on syscall return. It
//! snapshots `view.capture_user_context()` into the active
//! `ThreadPayload`, resolves the userspace-run wait via
//! `complete_interesting_trap`, and returns
//! `TrapAction::Reschedule`. The userspace-entry shim (a separate
//! lane) drains `pending_syscall_return` and writes it into the
//! fresh trap frame before `enter_userspace`.
//!
//! Anchors:
//! - `txdoc:THREAD-4-1-SHAPE`, `txdoc:THREAD-4-2-OWNERSHIP`,
//!   `txdoc:THREAD-5-1-STATE-PLACEMENT`,
//!   `txdoc:THREAD-5-2-THE-INTERRUPT-SUMMARY`,
//!   `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
//!   (`docs/design/02_execution/THREAD_RUNTIME_v1.md`)
//! - `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`,
//!   `txdoc:REACTOR-PREEMPTION-TRANSPARENCY`,
//!   `txdoc:REACTOR-AST-ASYNCHRONOUS-TRAP`
//!   (`docs/design/02_execution/REACTOR_v0.md`)
//! - HAL trap-frame contract (`TrapFrameMut`, `TrapAction`,
//!   `KernelTrapSink`) in `docs/design/01_substrate/HAL_v1.md`
//! - `txdoc:VM-5-1-FAULT-HANDLER`
//!   (`docs/design/03_memory-vm/VM_v1_2.md`) — page-fault hand-off
//!   shape that the future thread-future will dispatch into via
//!   `aspace.fault_script`. The trap shell itself does not call
//!   `fault_script` (cannot `.await`).

use tx_hal::{FaultInfo, TrapAction, TrapFrameMut, TrapFrameView, TxPlatform};
use tx_reactor::userspace::{
    PageFaultAccess, PageFaultInfo as ReactorPageFaultInfo, SyscallRequest, UserAddr,
    UserspaceRunError, UserspaceRunSlot, UserspaceTrapInfo,
};
use tx_substrate::zone::PayloadCap;
use tx_subsystems::thread_runtime::{current_thread_payload, ThreadPayload};

/// Re-export of the reactor's [`PageFaultAccess`] for the trap-shell
/// surface. Phase 1 uses the reactor enum unchanged; the plan's
/// "AccessKind" naming is honoured by this alias.
pub type AccessKind = PageFaultAccess;

/// Page-fault information captured by the trap shell, augmented with
/// `from_user` so the trap-shell branch (user vs. kernel page-fault)
/// can be expressed in a single value.
///
/// Distinct from `tx_reactor::userspace::PageFaultInfo`: that type is
/// the resolved trap delivered to the userspace-run waiter (always
/// from-user), while this carries the trap-shell decision input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFaultInfo {
    pub addr: UserAddr,
    pub access: AccessKind,
    pub present: bool,
    pub from_user: bool,
}

impl PageFaultInfo {
    /// Project to the reactor's narrower `PageFaultInfo`, used to
    /// resolve a userspace-run wait. Only valid when `from_user` is
    /// true; the trap shell guards that condition before constructing
    /// `UserspaceTrapInfo::PageFault(...)`.
    pub const fn into_reactor(self) -> ReactorPageFaultInfo {
        ReactorPageFaultInfo {
            addr: self.addr,
            access: self.access,
            present: self.present,
        }
    }
}

/// Translate a `TrapFrameView` into a `SyscallRequest` (RV64 ABI:
/// `a7` = nr, `a0..a5` = args). Platform-neutral because
/// `TrapFrameView` already exposes `syscall_number` and
/// `syscall_args[0..6]` in the architecture-portable shape per the
/// HAL contract.
///
/// The `<P>` parameter is reserved for a future generic
/// `TrapFrameView<'_, P>` shape pinned in the plan; today the HAL's
/// `TrapFrameView` is platform-erased so `P` is unused. Kept on the
/// signature so callers can spell the platform once and not
/// retro-fit when the HAL view is parameterised.
pub fn translate_syscall<P: TxPlatform>(view: &TrapFrameView<'_>) -> SyscallRequest {
    let _ = core::marker::PhantomData::<P>;
    SyscallRequest::new(view.syscall_number, view.syscall_args)
}

/// Translate a `TrapFrameView` plus a `FaultInfo` into a
/// `PageFaultInfo`. The trap shell uses this to decide whether to
/// hand off to the active userspace-run wait (when `from_user`) or
/// fall through to the legacy `Terminate` policy.
pub fn translate_user_pf<P: TxPlatform>(
    view: &TrapFrameView<'_>,
    fault: &FaultInfo,
) -> PageFaultInfo {
    let _ = view; // future use: faulting-instruction PC, sp, etc.
    let _ = core::marker::PhantomData::<P>;

    let access = if fault.write {
        AccessKind::Write
    } else if fault.instruction {
        AccessKind::Execute
    } else {
        AccessKind::Read
    };

    PageFaultInfo {
        addr: UserAddr::new(fault.address.0 as u64),
        access,
        // The HAL's `FaultInfo` does not currently surface a
        // present/not-present bit; `false` is the correct
        // conservative default for the userspace path, since the
        // canonical fault script (`txdoc:VM-5-1-FAULT-HANDLER`)
        // re-derives presence from the address-space recipe rather
        // than trusting the trap.
        present: false,
        from_user: fault.from_user,
    }
}

/// Outcome of resolving the trap-shell hand-off to the active
/// per-hart `ThreadPayload`.
#[derive(Debug)]
pub enum HandoffOutcome {
    /// The trap was resolved via the active payload's
    /// `userspace_slot`. Trap shell should return
    /// `TrapAction::Reschedule`.
    Resolved,
    /// No active payload / wait existed for this hart (e.g. trap
    /// from kernel code, or before the reactor task wrapper sets the
    /// per-hart slot). Caller falls back to the legacy `Terminate`
    /// policy.
    NoActivePayload,
    /// An active payload was registered but no in-flight
    /// `UserspaceRunRequest` was recorded. Phase 1 treats this the
    /// same as `NoActivePayload` (terminate). A future lane may
    /// want a richer policy.
    NoActiveRequest,
    /// `complete_interesting_trap` rejected the hand-off (e.g. the
    /// active request was already resolved or stale). Phase 1
    /// terminates; later lanes may want richer policy.
    SlotError(UserspaceRunError),
}

/// Resolve the active per-hart [`PayloadCap<ThreadPayload>`].
///
/// This indirects through the per-hart slot table maintained by the
/// reactor task wrapper that drives a thread future
/// (`tx_subsystems::thread_runtime::set_current_thread_payload` /
/// `clear_current_thread_payload`). When no thread future is
/// currently driving on this hart the slot is `None` and the trap
/// shell falls back to the legacy `Terminate` policy.
pub fn current_payload_for_hart(hart: usize) -> Option<PayloadCap<ThreadPayload>> {
    current_thread_payload(hart)
}

/// Hand the syscall trap off to the active per-hart `ThreadPayload`.
///
/// 1. Looks up the per-hart `PayloadCap<ThreadPayload>` via
///    [`current_payload_for_hart`].
/// 2. Snapshots `view.capture_user_context()` into the payload's
///    `saved_user_context` slot (Plan B writeback discipline).
/// 3. Resolves the in-flight userspace-run wait by calling
///    `slot.complete_interesting_trap(req, UserspaceTrapInfo::Syscall(req))`
///    on the payload's `userspace_slot`.
///
/// Returns a [`HandoffOutcome`] so the caller (`KernelTrapDispatcher`)
/// can decide between `Reschedule` and the fall-back policy.
pub fn hand_off_syscall(
    hart: usize,
    view: &TrapFrameMut<'_>,
    req: SyscallRequest,
) -> HandoffOutcome {
    let Some(payload) = current_payload_for_hart(hart) else {
        return HandoffOutcome::NoActivePayload;
    };

    let Some(active) = payload.active_userspace_request() else {
        return HandoffOutcome::NoActiveRequest;
    };

    payload.store_saved_user_context(Some(view.capture_user_context()));

    let slot: UserspaceRunSlot = payload.userspace_slot().clone();
    match slot.complete_interesting_trap(active, UserspaceTrapInfo::Syscall(req)) {
        Ok(_status) => HandoffOutcome::Resolved,
        Err(err) => HandoffOutcome::SlotError(err),
    }
}

/// Hand the user page-fault trap off to the active per-hart
/// `ThreadPayload`. Same shape as [`hand_off_syscall`] but resolves
/// the wait with `UserspaceTrapInfo::PageFault(...)`.
///
/// Phase 1 does **not** call `aspace.fault_script` here — the trap
/// context cannot `.await`. The thread future, woken by the
/// resolved wait, drives `fault_script` through the canonical async
/// path (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`). The
/// integration of that downstream call is left as a TODO for the
/// follow-up phase that introduces a real thread future.
pub fn hand_off_user_pf(
    hart: usize,
    view: &TrapFrameMut<'_>,
    info: PageFaultInfo,
) -> HandoffOutcome {
    debug_assert!(
        info.from_user,
        "hand_off_user_pf must only be called for user-mode page faults",
    );

    let Some(payload) = current_payload_for_hart(hart) else {
        return HandoffOutcome::NoActivePayload;
    };

    let Some(active) = payload.active_userspace_request() else {
        return HandoffOutcome::NoActiveRequest;
    };

    payload.store_saved_user_context(Some(view.capture_user_context()));

    let slot: UserspaceRunSlot = payload.userspace_slot().clone();
    // TODO(phase-2): The thread future, woken by this resolution,
    // is responsible for calling `aspace.fault_script(...)` per
    // `txdoc:VM-5-1-FAULT-HANDLER`. The trap shell hands off
    // raw fault info; downstream policy (SIGSEGV on Err, retry on
    // Ok) lives in the future, not here.
    match slot.complete_interesting_trap(active, UserspaceTrapInfo::PageFault(info.into_reactor()))
    {
        Ok(_status) => HandoffOutcome::Resolved,
        Err(err) => HandoffOutcome::SlotError(err),
    }
}

/// Convert a [`HandoffOutcome`] into the `TrapAction` the
/// `KernelTrapSink` should return. Phase 1 collapses every
/// non-`Resolved` outcome to `Terminate`; later phases may want to
/// distinguish (e.g. `Resume` for a stale race that the reactor will
/// re-issue).
pub const fn outcome_to_trap_action(outcome: &HandoffOutcome) -> TrapAction {
    match outcome {
        HandoffOutcome::Resolved => TrapAction::Reschedule,
        HandoffOutcome::NoActivePayload
        | HandoffOutcome::NoActiveRequest
        | HandoffOutcome::SlotError(_) => TrapAction::Terminate,
    }
}

#[cfg(test)]
mod tests;
