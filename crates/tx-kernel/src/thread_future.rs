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

use alloc::sync::Arc;

use crate::adapter::boot_runtime;
use crate::adapter::step_engine::{Cap, PayloadCap};
use boot_runtime::ast::AstBatch;
use boot_runtime::userspace::{
    PageFaultAccess, PageFaultInfo as ReactorPageFaultInfo, UserspaceEntryDecision,
    UserspaceTrapInfo,
};
use tx_hal::{PercpuIf, TrapIf, TxPlatform};
use tx_subsystems::signal::deliver_synchronous_fault;
use tx_subsystems::signal::Signum;
use tx_subsystems::signal::{ast_dispatch, refresh_deliverable_signal_summary, AstOutcome};
use tx_subsystems::thread_runtime::execution::prepare_userspace_entry_payload_into;
use tx_subsystems::thread_runtime::{
    clear_current_thread_payload, clear_current_userspace_payload, set_current_thread_payload,
    set_current_userspace_payload, ThreadIdentity, ThreadPayload,
};
use tx_subsystems::vm::{
    AccessMode, AddressSpace, Prot, UserAccessKind, UserRange, UserVirtAddr, VmBacking, VmEntry,
    VmFault, USER_PAGE_SIZE,
};

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

fn restore_sigreturn_frame<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    aspace: &Cap<AddressSpace>,
    payload: &PayloadCap<ThreadPayload>,
    sigreturn_ctx: &tx_hal::UserTrapContext,
) -> Result<(), ()> {
    let frame_size = <P as tx_hal::SignalFrameIf>::signal_frame_size();
    if frame_size == 0 || frame_size > tx_hal::SignalFrameBytes::CAPACITY {
        return Err(());
    }
    let frame_sp = tx_hal::UserPtr::<u8>::new(user_sp_from_context::<P>(sigreturn_ctx));
    let mut bytes = [0u8; tx_hal::SignalFrameBytes::CAPACITY];
    let guard = crate::adapter::step_engine::guard();
    let copied = aspace.copy_from_user(&mut bytes[..frame_size], frame_sp, &guard);
    match copied {
        crate::adapter::step_engine::StepOutcome::Done(n) if n == frame_size => {}
        _ => return Err(()),
    }
    drop(guard);

    let frame =
        <P as tx_hal::SignalFrameIf>::decode_signal_frame_bytes(frame_sp, &bytes[..frame_size])
            .map_err(|_| ())?;
    payload.store_signal_mask(tx_subsystems::signal::SignalMask::new(
        frame.saved_mask.bits,
    ));
    refresh_deliverable_signal_summary(thread);
    payload.store_saved_user_context(Some(frame.user_context));
    Ok(())
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

// SAFETY: `PerHartSlotted` owns a `PayloadCap<ThreadPayload>` (Send)
// and an `F: Future`. The EBR `Guard` held transiently inside `F`'s
// async state machine is created and dropped within a single poll
// boundary — it never crosses an `.await` point. The `Send` bound on
// `F` is the caller's responsibility; `submit_task_with_meta` already
// requires `F: Send`.
unsafe impl<P: TxPlatform, F: Future> Send for PerHartSlotted<P, F> where F: Send {}
unsafe impl<P: TxPlatform, F: Future> Sync for PerHartSlotted<P, F> where F: Sync {}

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
        if let Some(mailbox) = crate::adapter::boot_runtime::current_task_mailbox(hart) {
            this.payload.bind_mailbox(Arc::downgrade(&mailbox));
        }

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
    thread: Cap<ThreadIdentity>,
    payload: PayloadCap<ThreadPayload>,
) {
    loop {
        // ----------------------------------------------------------------
        // (1) ENTRY-SIDE WAIT + AST CHECKPOINT.
        //
        // Open the userspace-run wait *before* diving into userspace.
        // This is the resolution token the trap shell will fill in
        // when the next userspace trap fires
        // (`txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`).
        //
        // First iteration: `saved_user_context` was seeded by
        // `exec_script` (entry pc + initial sp), `pending_syscall_return`
        // is empty, so the merged ctx below is the fresh user state.
        // Subsequent iterations: the previous round-trip's trap-shell
        // hand-off stashed `saved_user_context`; the previous arm of
        // this loop's match wrote `pending_syscall_return` (Syscall
        // arm) or left it empty (PageFault Ok / ExecCommitted).
        //
        // The AST-checkpoint's empty-batch variant is the Phase-2
        // shape; per-task AST plumbing fills the batch in a later
        // slice.
        // ----------------------------------------------------------------
        let entry_wait = match payload.userspace_slot().start_request() {
            Ok(w) => w,
            Err(_) => {
                // Slot busy or exhausted — log via panic in the slice
                // (no other in-flight request should exist for a single-
                // threaded init in this Phase 2 cut).
                panic!("run_thread: userspace_slot::start_request failed");
            }
        };
        let entry_token = entry_wait.request();
        payload.set_active_userspace_request(Some(entry_token));

        // Phase E (stop-state): before entering userspace, check
        // whether the thread is stopped (SIGSTOP / default-Stop
        // disposition). A stopped thread must park until SIGCONT
        // clears the flag.
        while payload.is_stopped() {
            // Busy-wait placeholder.  On real hardware this spins
            // until route_gewalt(SIGCONT) clears the flag.  TODO:
            // replace with a reactor-managed wait-source woken by
            // SIGCONT's mailbox post.
            core::hint::spin_loop();
        }

        // Phase D (AST checkpoint): run ast_dispatch *before* entering
        // userspace. This is the entry-side AST checkpoint — it drains
        // pending signals and, for handler-disposition signals, modifies
        // `saved_user_context` so the thread enters the handler instead
        // of its original code on next `enter_userspace_with_context`.
        //
        // Per SIGNAL_v1 §15.1: AST checkpoint runs on every kernel→user
        // transition. Pending signals are selected by signum priority
        // (lowest first), disposition is consulted, and outcomes that
        // need materialisation (DefaultTerminate, DeliverHandler) are
        // handled inline.
        if let Some(process) = thread.upgrade_owner_proc() {
            let posix_deadline = tx_shims::linux_syscall::poll_due_posix_timers::<P>(&process);
            let itimer_deadline = tx_shims::linux_syscall::poll_due_itimers::<P>(&process);
            match (posix_deadline, itimer_deadline) {
                (Some(left), Some(right)) => P::set_deadline_ns(left.min(right)),
                (Some(deadline_ns), None) | (None, Some(deadline_ns)) => {
                    P::set_deadline_ns(deadline_ns);
                }
                (None, None) => {}
            }
            if thread.payload_cap().is_none() || process.aspace_cap().is_none() {
                return;
            }
        }

        let ast_outcome = ast_dispatch(&thread);
        match ast_outcome {
            AstOutcome::DeliverHandler { sig, action } => {
                // Phase D: full signal-frame delivery via
                // SignalFrameIf::prepare_signal_frame (added in the
                // HAL for this purpose).  The HAL builds the
                // platform-specific frame bytes + handler-entry
                // UserTrapContext; we write the bytes to the user
                // stack via the process AddressSpace, then store
                // the modified context for the next userspace entry.
                //
                // See: `txdoc:SIGNAL-V1-S15-HANDLER-DELIVERY`.
                if let Some(mut orig_ctx) = payload.saved_user_context() {
                    // Resolve owning process for aspace + fallback exit.
                    let Some(process) = thread.upgrade_owner_proc() else {
                        return;
                    };
                    let Some(aspace) = process.aspace_cap() else {
                        return;
                    };

                    // If a syscall return is pending (e.g. wait4 just
                    // resolved, child exit raised SIGCHLD, and the AST
                    // is now delivering the handler), apply that return
                    // value to the parked pre-signal context's `a0` and
                    // clear the pending slot.
                    //
                    // Without this, `prepare_userspace_entry_payload`
                    // below would overlay `pending_syscall_return` onto
                    // the freshly-built `handler_ctx.a0`, clobbering the
                    // POSIX-required `sig_no` argument. Apply-and-clear
                    // moves the syscall return into the place it should
                    // surface — the post-`rt_sigreturn` userspace context
                    // — while keeping the handler's `a0` equal to `sig_no`.
                    //
                    // The canonical `a0` register index lives in
                    // `tx_subsystems::thread_runtime::execution::USER_CONTEXT_A0_INDEX`
                    // and is the same index `prepare_userspace_entry_payload`
                    // uses for the overlay we're pre-empting here. Source it
                    // through that re-export so tx-kernel doesn't carry an
                    // arch cfg (CI gate `CI-GATE-ARCH-LINT`).
                    if let Some(result) =
                        tx_subsystems::thread_runtime::structure::drain_pending_syscall_return(
                            &payload,
                        )
                    {
                        let encoded = match result {
                            Ok(v) => v as u64,
                            Err(errno) => (-i64::from(errno)) as u64,
                        };
                        orig_ctx.regs
                            [tx_subsystems::thread_runtime::execution::USER_CONTEXT_A0_INDEX] =
                            encoded as usize;
                    }

                    // Save pre-handler context for sigreturn (now
                    // carrying the applied syscall return in a0).
                    payload.store_saved_signal_context(Some(orig_ctx));

                    let handler = match action.disposition {
                        tx_subsystems::signal::SigDisposition::Handler(addr) => addr,
                        _ => continue,
                    };

                    // Read current mask to pass to the handler, and
                    // compute the handler-entry mask per sigaction(2).
                    let old_mask = payload.signal_mask();
                    payload.store_saved_signal_mask(Some(old_mask));
                    let mut new_mask = old_mask.union(action.sa_mask);
                    if !action
                        .flags
                        .contains(tx_subsystems::signal::SaFlags::NODEFER)
                    {
                        new_mask.block(sig);
                    }
                    let siginfo = process
                        .siginfo_take(sig)
                        .map(siginfo_to_user_abi)
                        .unwrap_or(tx_hal::UserSigInfoAbi::ZERO);

                    // Build the signal frame write descriptor.
                    let stack_top = if action
                        .flags
                        .contains(tx_subsystems::signal::SaFlags::ONSTACK)
                    {
                        payload
                            .alt_stack()
                            .map(|(base, size)| tx_hal::UserPtr::<u8>::new(base + size))
                            .unwrap_or_else(|| {
                                tx_hal::UserPtr::<u8>::new(user_sp_from_context::<P>(&orig_ctx))
                            })
                    } else {
                        tx_hal::UserPtr::<u8>::new(user_sp_from_context::<P>(&orig_ctx))
                    };
                    let setup = tx_hal::SignalFrameWrite {
                        stack_top,
                        sig_no: sig.raw() as u32,
                        siginfo,
                        old_mask: tx_hal::UserSignalMaskAbi {
                            bits: old_mask.raw_bits(),
                        },
                        flags: tx_hal::UserSaFlagsAbi {
                            bits: action.flags.bits(),
                        },
                        handler_pc: tx_hal::UserPtr::<()>::new(handler),
                        restorer_pc: tx_hal::UserPtr::<()>::new(action.restorer),
                    };

                    // Guard is scoped inside this block so it does not
                    // straddle the next `.await` further down in
                    // `run_thread` (the spawned future must be `Send`).
                    let prepared =
                        <P as tx_hal::SignalFrameIf>::prepare_signal_frame(&orig_ctx, &setup);
                    match prepared {
                        Ok((handler_ctx, frame_bytes)) => {
                            let frame_addr = user_sp_from_context::<P>(&handler_ctx);
                            if !reserve_signal_frame_storage(
                                &aspace,
                                frame_addr,
                                frame_bytes.as_slice().len(),
                            ) {
                                tx_subsystems::process::execution::step_exit_group_with_signal(
                                    &process, sig,
                                );
                                return;
                            }
                            // Check that the full frame (including the
                            // sigreturn trampoline at its tail) actually
                            // landed on the user stack. The previous
                            // `let _ =` ignored every non-Done outcome —
                            // including `Yield`/short copy — which left
                            // the trampoline slot uninitialised, so the
                            // handler returned to garbage / zeros and
                            // the next instruction fetch faulted
                            // (observed end-to-end as the basic-musl
                            // crash with `lPF pc=trampoline_pc`). If the
                            // copy doesn't complete fully, fall back to
                            // the default action (terminate by sig) per
                            // SIGNAL_v1 §15.1 — handler delivery cannot
                            // proceed without a valid trampoline.
                            use crate::adapter::step_engine::StepOutcome as V3;
                            let copy_outcome = {
                                let guard = crate::adapter::step_engine::guard();
                                aspace.copy_to_user(
                                    tx_hal::UserPtr::<u8>::new(frame_addr),
                                    frame_bytes.as_slice(),
                                    &guard,
                                )
                            };
                            let fully_written = match copy_outcome {
                                V3::Done(n) => n == frame_bytes.as_slice().len(),
                                _ => false,
                            };
                            if !fully_written {
                                tx_subsystems::process::execution::step_exit_group_with_signal(
                                    &process, sig,
                                );
                                return;
                            }
                            make_signal_frame_executable(
                                &aspace,
                                frame_addr,
                                frame_bytes.as_slice().len(),
                            );
                            <P as tx_hal::CacheIf>::flush_icache_range(
                                tx_hal::VirtAddr(frame_addr),
                                frame_bytes.as_slice().len(),
                            );
                            payload.store_signal_mask(new_mask);
                            if action
                                .flags
                                .contains(tx_subsystems::signal::SaFlags::RESETHAND)
                            {
                                let _ = tx_subsystems::signal::step_sigaction(
                                    &process,
                                    sig,
                                    tx_subsystems::signal::SigDisposition::Default,
                                );
                            }
                            payload.store_saved_user_context(Some(handler_ctx));
                        }
                        Err(_) => {
                            // Signal frame write failed (bad stack).
                            // Take default action: terminate.
                            tx_subsystems::process::execution::step_exit_group_with_signal(
                                &process, sig,
                            );
                            return;
                        }
                    }
                }
            }
            AstOutcome::DefaultTerminate { .. } => return,
            AstOutcome::InitiateTermination => return,
            _ => {}
        }

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

        // ----------------------------------------------------------------
        // (2) BUILD MERGED CONTEXT AND DIVE INTO USERSPACE.
        //
        // `prepare_userspace_entry_payload` is the single Plan B
        // writeback site (`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`):
        // drains `pending_syscall_return` and overlays it into the
        // ctx's `a0` slot, then clears `active_userspace_request`.
        //
        // We re-set `active_userspace_request` after `prepare_*` to
        // keep the trap-shell hand-off pointing at our `entry_wait`
        // token: `prepare_*` cleared it as part of its writeback
        // discipline; the upcoming user trap needs it published.
        //
        // `enter_userspace_with_context` is `()`-typed but its
        // platform impl on a real board diverges via `sret` and
        // returns only when the trap handler chooses
        // `TrapAction::Reschedule` (the platform's trap-shell
        // longjmps back to this call site, restoring the kernel sp /
        // ra / s-regs the platform's userspace-entry shim stashed
        // before `sret`). On host platforms the default `panic!`
        // body remains divergent — host tests override it with a
        // recording no-op + Pending fallthrough so `run_thread` can
        // be exercised end-to-end.
        // ----------------------------------------------------------------
        // Resolve the user process's pmap right before userspace
        // entry. The HAL copies `ctx` into an architecture trapframe
        // and activates the root at the final machine handoff point.
        //
        // We re-resolve `process` and `aspace` per iteration rather
        // than caching across the await: the owning process may
        // exit_group between userspace round-trips, in which case
        // bailing out cleanly here is safer than dereferencing a
        // stale Cap.
        if let Some(process) = thread.upgrade_owner_proc() {
            if let Some(aspace) = process.aspace_cap() {
                let mut ctx = tx_hal::UserTrapContext::empty();
                prepare_userspace_entry_payload_into(&payload, &mut ctx);
                payload.set_active_userspace_request(Some(entry_token));
                let entry_hart = <P as tx_hal::SmpIf>::current_cpu_id().0;
                let _prev_userspace = set_current_userspace_payload(entry_hart, payload.clone());
                ctx = tx_shims::linux_syscall::maybe_deliver_itimer_signal::<P>(
                    ctx, &process, &thread, &aspace,
                );
                let root = aspace.pmap().root_handle();
                <P as TrapIf>::enter_userspace_with_context(&ctx, root);
            } else {
                return;
            }
        } else {
            return;
        }

        // ----------------------------------------------------------------
        // (3) AWAIT THE RESOLVED WAIT.
        //
        // On a real platform with the reschedule longjmp wired up,
        // the wait was resolved by the trap shell *before*
        // `enter_userspace_with_context` returned, so this `.await`
        // is a fast Poll::Ready pop. On host platforms with a
        // recording-no-op override, the wait is Pending here and
        // the test driver resolves it with the scripted trap before
        // the next poll.
        // ----------------------------------------------------------------
        let trap = entry_wait.await;
        let entry_hart = <P as tx_hal::SmpIf>::current_cpu_id().0;
        if !matches!(trap, UserspaceTrapInfo::TimerPreempt) {
            let _ = clear_current_userspace_payload(entry_hart);
        }
        payload.set_active_userspace_request(None);

        // ----------------------------------------------------------------
        // (4) DISPATCH THE RESOLVED TRAP.
        // ----------------------------------------------------------------
        match trap {
            UserspaceTrapInfo::TimerPreempt => {
                crate::adapter::boot_runtime::yield_now().await;
            }
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
                let mut ctx = tx_shims::linux_syscall::SyscallCtx::new(
                    process.clone(),
                    thread.clone(),
                    aspace.clone(),
                );
                // drive-taskmb: inject the current task's mailbox so
                // drive() can park on it for yield resolution.
                let hart = <P as tx_hal::SmpIf>::current_cpu_id().0;
                if let Some(mailbox) = crate::adapter::boot_runtime::current_task_mailbox(hart) {
                    ctx = ctx.with_mailbox(mailbox);
                }
                // drive-taskmb: inject the reactor's timer wheel for
                // OnTimer yield resolution.
                if let Some(tw) = crate::adapter::boot_runtime::current_timer_wheel(hart) {
                    ctx = ctx.with_timer_wheel(tw);
                }
                // drive-taskmb: inject the reactor's delegate registry
                // for OnAgent yield resolution.
                if let Some(dr) = crate::adapter::boot_runtime::current_delegate_registry(hart) {
                    ctx = ctx.with_delegate_registry(dr);
                }
                let sigreturn_ctx = payload.saved_user_context();
                let result = tx_shims::linux_syscall::dispatch::<P>(req, &ctx).await;

                // Threshold-based observation dump. If the boot path
                // installed a non-zero `OBSERVE_DUMP_THRESHOLD` (see
                // `tx_observe::set_dump_threshold`), every emit ticks
                // a global counter; once it crosses the threshold, this
                // syscall return dumps the ring over the console and
                // powers off the platform. Captures a bounded trace from
                // workloads where init never naturally exits (e.g.
                // oscomp's continuous test-group sequence).
                if tx_observe::should_dump_now() {
                    tx_observe::dump_console_hex::<P>(<P as tx_hal::SmpIf>::current_cpu_id());
                    tx_hal::console_write_str::<P>(":observe:dump:threshold\n");
                    <P as tx_hal::PowerIf>::system_off();
                }

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
                    tx_shims::linux_syscall::SyscallResult::SigreturnRestored => {
                        // `rt_sigreturn` is special: musl cancellation
                        // handlers may edit the on-stack ucontext (notably
                        // MC_PC) before returning through the trampoline.
                        // Read the user frame with the selected platform's
                        // ABI decoder and replace the parked syscall-layer
                        // fallback context with the user-edited one.
                        let Some(sigreturn_ctx) = sigreturn_ctx else {
                            tx_subsystems::process::execution::step_exit_group_with_signal(
                                &process,
                                Signum::SIGSEGV,
                            );
                            return;
                        };
                        if restore_sigreturn_frame::<P>(&thread, &aspace, &payload, &sigreturn_ctx)
                            .is_err()
                        {
                            tx_subsystems::process::execution::step_exit_group_with_signal(
                                &process,
                                Signum::SIGSEGV,
                            );
                            return;
                        }
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
                    Err(e) => {
                        log_user_segv::<P>(&payload, info.addr.raw(), info.access, "pf", e);
                        log_nearby_recipes::<P>(&aspace, info.addr.raw() as usize);
                        deliver_synchronous_fault(&thread, Signum::SIGSEGV);
                        return;
                    }
                }
            }
            UserspaceTrapInfo::Fatal(_info) => {
                // Phase B: route fatal trap through canonical
                // synchronous-fault entry per SIGNAL_v1 §20.
                log_user_segv::<P>(
                    &payload,
                    0,
                    PageFaultAccess::Unknown,
                    "fatal",
                    tx_subsystems::vm::VmFaultError::NoRecipe,
                );
                deliver_synchronous_fault(&thread, Signum::SIGSEGV);
                return;
            }
        }
        // Fall through to the top of the loop — next iteration
        // re-opens the entry-side wait, re-runs the AST checkpoint,
        // and re-dives into userspace with the merged context.
    }
}

fn log_user_segv<P: TxPlatform>(
    payload: &ThreadPayload,
    fault_addr: u64,
    access: PageFaultAccess,
    kind: &str,
    error: tx_subsystems::vm::VmFaultError,
) {
    let pc = payload
        .saved_user_context()
        .map(|ctx| ctx.pc as u64)
        .unwrap_or(0);
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":user-segv:");
    tx_hal::console_write_str::<P>(kind);
    tx_hal::console_write_str::<P>(":access=");
    tx_hal::console_write_str::<P>(match access {
        PageFaultAccess::Read => "read",
        PageFaultAccess::Write => "write",
        PageFaultAccess::Execute => "exec",
        PageFaultAccess::Unknown => "unknown",
    });
    tx_hal::console_write_str::<P>(":err=");
    tx_hal::console_write_str::<P>(vm_fault_error_label(error));
    tx_hal::console_write_str::<P>(":pc=0x");
    write_hex_u64::<P>(pc);
    tx_hal::console_write_str::<P>(":addr=0x");
    write_hex_u64::<P>(fault_addr);
    tx_hal::console_write_str::<P>("\n");
}

fn log_nearby_recipes<P: TxPlatform>(aspace: &AddressSpace, fault_addr: usize) {
    let recipes = aspace.recipes_snapshot();
    let mut containing: Option<VmEntry> = None;
    let mut lower: Option<VmEntry> = None;
    let mut upper: Option<VmEntry> = None;

    for entry in recipes.iter().cloned() {
        let start = entry.range.start().as_usize();
        let end = entry.range.end().as_usize();
        if start <= fault_addr && fault_addr < end {
            containing = Some(entry);
            break;
        }
        if end <= fault_addr
            && lower
                .as_ref()
                .is_none_or(|old| old.range.end().as_usize() < end)
        {
            lower = Some(entry.clone());
        }
        if fault_addr < start
            && upper
                .as_ref()
                .is_none_or(|old| start < old.range.start().as_usize())
        {
            upper = Some(entry);
        }
    }

    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":user-segv:recipes:count=0x");
    write_hex_u64::<P>(recipes.len() as u64);
    tx_hal::console_write_str::<P>(":fault=0x");
    write_hex_u64::<P>(fault_addr as u64);
    tx_hal::console_write_str::<P>("\n");

    if let Some(entry) = containing {
        log_recipe::<P>("hit", &entry);
    } else {
        if let Some(entry) = lower {
            log_recipe::<P>("lower", &entry);
        }
        if let Some(entry) = upper {
            log_recipe::<P>("upper", &entry);
        }
    }
}

fn log_recipe<P: TxPlatform>(label: &str, entry: &VmEntry) {
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":user-segv:recipe:");
    tx_hal::console_write_str::<P>(label);
    tx_hal::console_write_str::<P>(":start=0x");
    write_hex_u64::<P>(entry.range.start().as_usize() as u64);
    tx_hal::console_write_str::<P>(":end=0x");
    write_hex_u64::<P>(entry.range.end().as_usize() as u64);
    tx_hal::console_write_str::<P>(":prot=");
    tx_hal::console_write_str::<P>(if entry.prot.read { "r" } else { "-" });
    tx_hal::console_write_str::<P>(if entry.prot.write { "w" } else { "-" });
    tx_hal::console_write_str::<P>(if entry.prot.execute { "x" } else { "-" });
    tx_hal::console_write_str::<P>(":backing=");
    match &entry.backing {
        VmBacking::None => tx_hal::console_write_str::<P>("none"),
        VmBacking::PrivateAnon => tx_hal::console_write_str::<P>("anon"),
        VmBacking::Page { offset, .. } => {
            tx_hal::console_write_str::<P>("page@0x");
            write_hex_u64::<P>(*offset);
        }
    }
    tx_hal::console_write_str::<P>("\n");
}

fn vm_fault_error_label(error: tx_subsystems::vm::VmFaultError) -> &'static str {
    match error {
        tx_subsystems::vm::VmFaultError::Range(_) => "range",
        tx_subsystems::vm::VmFaultError::NoRecipe => "no-recipe",
        tx_subsystems::vm::VmFaultError::ProtectionViolation => "protection",
        tx_subsystems::vm::VmFaultError::WouldBlock => "would-block",
        tx_subsystems::vm::VmFaultError::BackingMismatch => "backing-mismatch",
        tx_subsystems::vm::VmFaultError::BackingOffsetOverflow => "backing-offset-overflow",
        tx_subsystems::vm::VmFaultError::PageBeyondSize => "page-beyond-size",
        tx_subsystems::vm::VmFaultError::PageCache(_) => "page-cache",
        tx_subsystems::vm::VmFaultError::StaleRecipe => "stale-recipe",
        tx_subsystems::vm::VmFaultError::Pmap(error) => match error {
            tx_subsystems::vm::VmPmapError::Pmap(tx_hal::PmapError::InvalidRequest) => {
                "pmap-invalid"
            }
            tx_subsystems::vm::VmPmapError::Pmap(tx_hal::PmapError::Exhausted) => "pmap-exhausted",
            tx_subsystems::vm::VmPmapError::Pmap(tx_hal::PmapError::AlreadyMapped) => {
                "pmap-already-mapped"
            }
            tx_subsystems::vm::VmPmapError::Pmap(tx_hal::PmapError::Unsupported) => {
                "pmap-unsupported"
            }
            tx_subsystems::vm::VmPmapError::Zone(_) => "pmap-zone",
            tx_subsystems::vm::VmPmapError::MissingReservation => "pmap-missing-reservation",
            tx_subsystems::vm::VmPmapError::AlreadyMappedDrift => "pmap-already-mapped-drift",
            tx_subsystems::vm::VmPmapError::MappingMismatch => "pmap-mapping-mismatch",
        },
    }
}

fn write_hex_u64<P: TxPlatform>(value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; 16];
    let mut started = false;
    let mut out = [0u8; 16];
    let mut len = 0;
    for (idx, slot) in digits.iter_mut().enumerate() {
        let shift = (15 - idx) * 4;
        let digit = ((value >> shift) & 0xf) as usize;
        *slot = HEX[digit];
        if digit != 0 || started || idx == 15 {
            started = true;
            out[len] = *slot;
            len += 1;
        }
    }
    let s = core::str::from_utf8(&out[..len]).unwrap_or("0");
    tx_hal::console_write_str::<P>(s);
}

fn siginfo_to_user_abi(info: tx_subsystems::signal::SigInfo) -> tx_hal::UserSigInfoAbi {
    let mut abi = tx_hal::UserSigInfoAbi::ZERO;
    abi.bytes[0..4].copy_from_slice(&info.si_signo.to_ne_bytes());
    abi.bytes[4..8].copy_from_slice(&0i32.to_ne_bytes());
    abi.bytes[8..12].copy_from_slice(&info.si_code.to_ne_bytes());
    abi.bytes[16..20].copy_from_slice(&info.si_pid.to_ne_bytes());
    abi.bytes[20..24].copy_from_slice(&info.si_uid.to_ne_bytes());
    abi
}

fn user_sp_from_context<P: TxPlatform>(ctx: &tx_hal::UserTrapContext) -> usize {
    match P::ARCH {
        tx_hal::Arch::Riscv64 => ctx.regs[2],
        tx_hal::Arch::LoongArch64 => ctx.regs[3],
    }
}

fn make_signal_frame_executable(aspace: &AddressSpace, frame_addr: usize, frame_len: usize) {
    let start = frame_addr & !(USER_PAGE_SIZE - 1);
    let Some(end_unaligned) = frame_addr.checked_add(frame_len) else {
        return;
    };
    let Some(end) = end_unaligned.checked_next_multiple_of(USER_PAGE_SIZE) else {
        return;
    };
    let Some(len) = end.checked_sub(start) else {
        return;
    };
    let Ok(range) = UserRange::new_aligned(UserVirtAddr(start), len) else {
        return;
    };
    let _ = aspace.try_mprotect(range, Prot::new(true, true, true));
}

fn reserve_signal_frame_storage(
    aspace: &AddressSpace,
    frame_addr: usize,
    frame_len: usize,
) -> bool {
    let start = frame_addr & !(USER_PAGE_SIZE - 1);
    let Some(end_unaligned) = frame_addr.checked_add(frame_len) else {
        return false;
    };
    let Some(end) = end_unaligned.checked_next_multiple_of(USER_PAGE_SIZE) else {
        return false;
    };
    let Some(len) = end.checked_sub(start) else {
        return false;
    };
    let Ok(range) = UserRange::new_aligned(UserVirtAddr(start), len) else {
        return false;
    };
    use crate::adapter::step_engine::StepOutcome as V3;
    !matches!(
        aspace.reserve_user_range_for_access(range, UserAccessKind::Write),
        V3::Err(_) | V3::Yield { .. }
    )
}

#[cfg(test)]
mod tests;
