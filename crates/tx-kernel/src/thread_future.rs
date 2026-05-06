//! Pre-ELF Phase 2 Part 1: production thread future + per-hart slot
//! adapter.
//!
//! `run_thread` is the per-thread reactor task body — the
//! "thread future" of `txdoc:THREAD-4-1-SHAPE`. It owns the userspace
//! round-trip:
//!
//! 1. Open a `UserspaceRunSlot::start_request` and record the token on
//!    the thread payload (`set_active_userspace_request`). The trap
//!    shell consults this token to resolve the wait via
//!    `complete_interesting_trap` per
//!    `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`.
//! 2. `.await` the wait. The future yields `Pending` until a userspace
//!    trap fires and the trap shell resolves the slot.
//! 3. Match on the resolved `UserspaceTrapInfo`:
//!    - `Syscall(req)` → drive `tx_shims::linux_syscall::dispatch`,
//!      stash the outcome in `pending_syscall_return`.
//!    - `PageFault(info)` → drive `aspace.fault_script(VmFault).await`
//!      per `txdoc:VM-5-1-FAULT-HANDLER` /
//!      `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`. On `Ok` the
//!      mapping was published; the loop body falls through to AST
//!      drain + userspace-entry without writing
//!      `pending_syscall_return` (the merged context's `a0` comes
//!      from `saved_user_context`, matching Plan B writeback
//!      discipline for fault returns). On `Err` route SIGSEGV per
//!      `SIGNAL_v1` §15.1 default action.
//!    - other variants → unreachable in the slice; panic.
//! 4. Drain ASTs **before** preparing the entry payload (Plan B
//!    Cross-cutting risk #3 — AST drain ordering). Per Open Q #2 only
//!    `EnterUserspace` is valid in this slice.
//! 5. Call `prepare_userspace_entry_payload` to merge the drained
//!    `pending_syscall_return` into the saved user context, then
//!    transfer to the platform's `enter_userspace_with_context`. The
//!    call diverges; the next userspace trap re-resolves the wait via
//!    a fresh poll of this future.
//!
//! ## Sequencing
//!
//! Shape used: **divergent-call inside the future** (the brief's
//! "Shape B" framing, but tempered against the actual slot API). The
//! future first issues `start_request` so the trap shell has a token
//! to resolve, then `await`s the wait — that yields `Pending`, the
//! adapter clears the per-hart slot, the reactor returns control. The
//! trap arrives, the shell calls `complete_interesting_trap`, the
//! reactor re-polls, the future runs the syscall dispatch and then
//! calls `enter_userspace_with_context(ctx)` which diverges into the
//! trap vector. Control never returns to this future call site; the
//! next iteration is a fresh poll triggered by the next trap.
//!
//! `PerHartSlotted<F>` is the task wrapper: it sets
//! `set_current_thread_payload(hart, payload)` synchronously inside
//! `Future::poll`, delegates to the inner future, and clears the slot
//! on poll exit (both `Ready` and `Pending`). The per-hart slot is
//! never held across an `.await` — the wrapper's own poll never
//! `.await`s itself; only the inner future does, and the wrapper's
//! poll is what brackets each polling step.
//!
//! ## Test discipline
//!
//! `enter_userspace_with_context` is `-> !` and the host platform's
//! default impl panics. Host tests therefore drive
//! `prepare_userspace_entry_payload` and `linux_syscall::dispatch`
//! separately rather than reaching the divergent call site. The
//! `PerHartSlotted` adapter is testable directly because its poll body
//! only manipulates the slot.
//!
//! Anchors:
//! - `txdoc:THREAD-4-1-SHAPE`, `txdoc:THREAD-4-2-OWNERSHIP`,
//!   `txdoc:THREAD-5-1-STATE-PLACEMENT`,
//!   `txdoc:THREAD-5-3-THE-INTERRUPT-PREDICATE-CONTRACT`,
//!   `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
//!   (`docs/design/02_execution/THREAD_RUNTIME_v1.md`).
//! - `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`,
//!   `txdoc:REACTOR-PREEMPTION-TRANSPARENCY`
//!   (`docs/design/02_execution/REACTOR_v0.md`).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use tx_hal::{PercpuIf, TrapIf, TxPlatform};
use tx_reactor::ast::AstBatch;
use tx_reactor::userspace::{
    PageFaultAccess, PageFaultInfo as ReactorPageFaultInfo, UserspaceEntryDecision,
    UserspaceTrapInfo,
};
use tx_substrate::zone::PayloadCap;
use tx_subsystems::process::execution::step_exit_group_with_signal;
use tx_subsystems::signal::Signum;
use tx_subsystems::thread_runtime::execution::prepare_userspace_entry_payload;
use tx_subsystems::thread_runtime::{
    clear_current_thread_payload, set_current_thread_payload, ThreadIdentity, ThreadPayload,
};
use tx_subsystems::vm::{AccessMode, UserVirtAddr, VmFault};

/// Translate the reactor's `PageFaultAccess` into the VM subsystem's
/// `AccessMode`, which is what `VmFault` consumes. The two enums do
/// not converge today: the reactor's `PageFaultAccess::Unknown`
/// variant has no VM analogue (the VM layer expects a definite
/// access class for protection / CoW decisions). Phase 3 maps
/// `Unknown` to `Read` defensively — the canonical fault script
/// (`txdoc:VM-5-1-FAULT-HANDLER`) re-derives the protection
/// requirement from the recipe rather than trusting the trap, so a
/// conservative `Read` lookup will still catch genuine no-recipe and
/// protection-violation cases without falsely upgrading a load to a
/// store.
pub(crate) const fn pf_access_to_vm_access(access: PageFaultAccess) -> AccessMode {
    match access {
        PageFaultAccess::Read | PageFaultAccess::Unknown => AccessMode::Read,
        PageFaultAccess::Write => AccessMode::Write,
        PageFaultAccess::Execute => AccessMode::Execute,
    }
}

/// Sanity hook for the from-user invariant: the reactor's
/// `PageFaultInfo` has no `from_user` field because the trap shell
/// only resolves a userspace-run wait with `PageFault(...)` for
/// from-user faults (`crate::trap_handoff::hand_off_user_pf` carries
/// the explicit `debug_assert!`). Reaching this match arm therefore
/// implies the original trap was from-user. The function exists so
/// the assertion site stays close to the dispatch and so any future
/// reactor-side `from_user` field can be checked here without
/// re-plumbing.
const fn pf_info_implies_from_user(_info: ReactorPageFaultInfo) -> bool {
    true
}

/// Per-hart slot adapter for the production thread future.
///
/// Wraps an inner future; on every poll, installs `payload` into the
/// per-hart slot before delegating, and clears it after the inner
/// poll returns. The trap shell reads the slot via
/// `current_thread_payload(hart)` to find the active payload while a
/// userspace trap is in flight.
///
/// The slot is cleared unconditionally on poll exit — `Ready` *and*
/// `Pending`. On `Pending` the slot is briefly empty; the next trap
/// shell read will see `None` and fall through to the `Terminate`
/// policy. That window is small in practice (a trap arriving between
/// the wrapper's poll exit and the reactor's next decision is the
/// race the future architecture is built around) and deliberate: the
/// alternative — leaving the slot set across yields — would deny
/// other futures the slot when SMP scheduling lands.
pub struct PerHartSlotted<P: TxPlatform, F: Future> {
    payload: PayloadCap<ThreadPayload>,
    inner: F,
    _platform: core::marker::PhantomData<fn() -> P>,
}

impl<P: TxPlatform, F: Future> PerHartSlotted<P, F> {
    pub fn new(payload: PayloadCap<ThreadPayload>, inner: F) -> Self {
        Self {
            payload,
            inner,
            _platform: core::marker::PhantomData,
        }
    }
}

impl<P: TxPlatform, F: Future> Future for PerHartSlotted<P, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pinning — we never move `inner` out of
        // `self` after pinning. The other fields are `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        let hart = <P as PercpuIf>::current_cpu_id().0;

        let _prev = set_current_thread_payload(hart, this.payload.clone());

        // SAFETY: `this.inner` is structurally pinned via the
        // `get_unchecked_mut` above; we never move out of it.
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        let out = inner.poll(cx);

        let _ = clear_current_thread_payload(hart);

        out
    }
}

/// Production per-thread reactor future.
///
/// Drives the userspace round-trip described in the module docs.
/// Returns when the thread terminates (`exit_group`, `exit`, or a
/// page-fault that the canonical fault script could not satisfy and
/// is routed to default-action SIGSEGV per `SIGNAL_v1` §15.1).
///
/// The `Cap<ThreadIdentity>` is held across `.await` per
/// `txdoc:THREAD-4-2-OWNERSHIP`; epoch-managed `Cap` is safe across
/// yields. No `IdentRef<'g, _>` ever crosses an await.
pub async fn run_thread<P: TxPlatform>(
    thread: tx_substrate::zone::Cap<ThreadIdentity>,
    payload: PayloadCap<ThreadPayload>,
) {
    loop {
        // (1) Open a userspace-run wait so the trap shell has a token
        // to resolve. On exit_group the previous iteration set
        // `active_userspace_request = None`; on the very first
        // iteration the bootstrap left it `None`.
        let wait = match payload.userspace_slot().start_request() {
            Ok(w) => w,
            Err(_) => {
                // Slot busy or exhausted — log via panic in the slice
                // (no other in-flight request should exist for a single-
                // threaded init in this Phase 2 cut).
                panic!("run_thread: userspace_slot::start_request failed");
            }
        };
        let req_token = wait.request();
        payload.set_active_userspace_request(Some(req_token));

        // (2) Await the wait. Yields Pending; trap shell resolves via
        // `complete_interesting_trap` on the next userspace trap.
        let trap = wait.await;

        // (3) Dispatch the resolved trap.
        match trap {
            UserspaceTrapInfo::Syscall(req) => {
                // Resolve the syscall context from the payload.
                let Some(process) = thread.upgrade_owner_proc() else {
                    // Owning process gone; thread is detached. Stop.
                    return;
                };
                let Some(aspace) = process.aspace_cap() else {
                    // Process zombified concurrently; stop.
                    return;
                };
                let ctx = tx_shims::linux_syscall::SyscallCtx::new(
                    process.clone(),
                    thread.clone(),
                    aspace,
                );
                let result = tx_shims::linux_syscall::dispatch::<P>(req, &ctx).await;

                match result {
                    tx_shims::linux_syscall::SyscallResult::Return(v) => {
                        payload.store_pending_syscall_return(Some(Ok(v)));
                    }
                    tx_shims::linux_syscall::SyscallResult::Error(e) => {
                        payload.store_pending_syscall_return(Some(Err(e)));
                    }
                    tx_shims::linux_syscall::SyscallResult::NoReturn => {
                        // Thread/process exited inside dispatch; do not
                        // re-enter userspace.
                        return;
                    }
                    tx_shims::linux_syscall::SyscallResult::ExecCommitted => {
                        // Wave 4 / Phase 6 of the ELF-loader plan:
                        // `execve` replaced the process's
                        // `AddressSpace` and seeded the thread's
                        // `saved_user_context` with the new image's
                        // entry pc / initial sp. Per Plan B
                        // writeback discipline (and
                        // `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`)
                        // we MUST NOT drain
                        // `pending_syscall_return` for this
                        // iteration — the new image's `_start`
                        // expects a fresh stack and zero-initialised
                        // gprs (System V psABI). The previous trap
                        // frame's `a0` is effectively discarded.
                        //
                        // No early return: fall through to AST drain
                        // + `prepare_userspace_entry_payload` +
                        // `enter_userspace_with_context`. Since the
                        // dispatcher did not write the pending-return
                        // slot, `drain_pending_syscall_return` reads
                        // `None` and the merged context's `a0`
                        // defaults to whatever
                        // `saved_user_context.regs[10]` already
                        // carries (zero from the script's fresh
                        // `UserTrapContext`, per
                        // `make_initial_user_trap_context`).
                    }
                }
            }
            UserspaceTrapInfo::PageFault(info) => {
                // Per `txdoc:VM-5-1-FAULT-HANDLER` and
                // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`: drive
                // the canonical async fault script. On Ok the recipe
                // is materialised + published; loop body falls
                // through to the AST-drain + userspace-entry tail
                // (no `pending_syscall_return` write — the merged
                // context's `a0` comes from `saved_user_context`,
                // matching Plan B writeback discipline for
                // fault returns). On Err route SIGSEGV per
                // `SIGNAL_v1` §15.1 default action.
                debug_assert!(
                    pf_info_implies_from_user(info),
                    "thread future only sees PageFault for from-user faults; \
                     trap shell terminates non-user faults at the shell"
                );
                let Some(process) = thread.upgrade_owner_proc() else {
                    // Owning process gone; thread is detached. Stop.
                    return;
                };
                let Some(aspace) = process.aspace_cap() else {
                    // Process zombified concurrently; stop.
                    return;
                };
                let fault = VmFault::new(
                    UserVirtAddr::new(info.addr.raw() as usize),
                    pf_access_to_vm_access(info.access),
                );
                match aspace.fault_script(fault).await {
                    Ok(_) => {
                        // Mapping was published; fall through to AST
                        // drain + entry-prep tail (same path as a
                        // successful syscall, minus the
                        // `pending_syscall_return` write).
                    }
                    Err(_e) => {
                        step_exit_group_with_signal(&process, Signum::SIGSEGV);
                        return;
                    }
                }
            }
            UserspaceTrapInfo::Fatal(_info) => {
                // Out-of-scope for Phase 2; terminate the future
                // rather than entering a divergent state we can't
                // express yet.
                if let Some(process) = thread.upgrade_owner_proc() {
                    step_exit_group_with_signal(&process, Signum::SIGSEGV);
                }
                return;
            }
        }

        // (4) AST drain ordering — Cross-cutting risk #3. The AST
        // checkpoint must run before `prepare_userspace_entry_payload`
        // so a signal posted between syscall resolution and userspace
        // re-entry traverses the checkpoint.
        //
        // **Two-phase request lifecycle.** The wait `req_token` from
        // step (1) is the *resolution* request — it carries the
        // trap-shell hand-off for the just-completed userspace
        // round-trip. Once `wait.await` returned, that request was
        // consumed and the slot returned to idle (`UserspaceRunWait::poll`
        // clears `state.active` on the Resolved arm). The
        // userspace-entry checkpoint runs on a *fresh* request — the
        // one that will be resolved by the **next** userspace trap.
        // This gives the AST policy hook a non-resolved request to
        // gate against (`active_status` rejects `Resolved`-phase
        // requests with `AlreadyResolved`).
        //
        // For Phase 2 the only valid `UserspaceEntryDecision` is
        // `EnterUserspace`; the reactor task crate does not yet
        // expose a per-task `AstSlot` accessor, so we drive the
        // empty-batch variant. When the per-task AST plumbing lands
        // the `AstBatch::default()` here is replaced by the drained
        // slot.
        let entry_wait = match payload.userspace_slot().start_request() {
            Ok(w) => w,
            Err(_) => panic!(
                "run_thread: userspace_slot::start_request failed before \
                 userspace re-entry"
            ),
        };
        let entry_token = entry_wait.request();
        payload.set_active_userspace_request(Some(entry_token));
        let decision = payload.userspace_slot().checkpoint_userspace_entry_batch(
            entry_token,
            AstBatch::default(),
            |_ckpt| UserspaceEntryDecision::EnterUserspace,
        );
        debug_assert!(
            decision.is_ok(),
            "checkpoint_userspace_entry_batch must succeed with the freshly-started \
             entry-side request"
        );

        // (5) Build the merged context and dive into userspace. The
        // call is `-> !`; control returns through the trap vector,
        // not through this call site. The next userspace trap will
        // resolve `entry_wait` (its `request()` is what
        // `set_active_userspace_request` made visible to the trap
        // shell). On the next poll of this future, the loop top
        // calls `start_request` again expecting an idle slot — the
        // trap shell's `complete_interesting_trap` + this future's
        // `wait.await` (in step (2)) consumes `entry_wait` and
        // returns the slot to idle, so the next iteration's
        // `start_request` succeeds.
        //
        // Note: `prepare_userspace_entry_payload` clears
        // `active_userspace_request` after consuming the saved
        // context. We re-set it above (right after the entry-side
        // `start_request`); the clear inside `prepare_*` then
        // restores the trap shell's view to "no active request"
        // briefly, but the entry wait is still alive and will be
        // dispatched to the trap shell on the very next user trap
        // because `prepare_*` runs synchronously between the
        // checkpoint and the divergent call.
        let ctx = prepare_userspace_entry_payload(&payload);
        // Re-publish the entry token — `prepare_userspace_entry_payload`
        // cleared it as part of the Plan B writeback discipline. The
        // trap shell needs an active request to hand off the next
        // userspace trap into.
        payload.set_active_userspace_request(Some(entry_token));
        // Keep `entry_wait` alive across the divergent call. The
        // future's pinned state retains the `UserspaceRunWait` so
        // the slot's `active` entry persists for the trap shell to
        // resolve. `core::mem::forget` would also work but we use
        // a held binding to keep ownership obvious.
        let _entry_wait_guard = entry_wait;
        <P as TrapIf>::enter_userspace_with_context(ctx);
    }
}

#[cfg(test)]
mod tests;
