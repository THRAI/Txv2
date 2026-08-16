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
//!    - `PageFault(info)` → drive the process-aware
//!      `aspace.fault_script_for_process_with_post(VmFault, ...)`
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

use alloc::{
    boxed::Box,
    sync::{Arc, Weak as ArcWeak},
};

use crate::adapter::boot_runtime;
use crate::adapter::step_engine::{Cap, PayloadCap};
use boot_runtime::ast::AstBatch;
use boot_runtime::userspace::{
    PageFaultAccess, PageFaultInfo as ReactorPageFaultInfo, SyscallRequest, UserspaceEntryDecision,
    UserspaceRunRequest, UserspaceTrapInfo,
};
use tx_hal::{PercpuIf, TrapIf, TxPlatform};
use tx_services::time::{DeadlineRegistrar, DeadlineRegistrarHandle};
use tx_shims::linux_syscall::numbers::{
    FUTEX_CMD_MASK, FUTEX_WAKE, FUTEX_WAKE_BITSET, NR_CLONE, NR_EXIT, NR_EXIT_GROUP, NR_FUTEX,
    NR_MMAP, NR_MPROTECT, NR_MUNMAP, NR_READ, NR_READV, NR_RT_SIGPROCMASK, NR_WRITE, NR_WRITEV,
};
use tx_shims::linux_syscall::SyscallResult;
use tx_substrate::wake::MailboxSchedulerHint;
use tx_subsystems::process::{ExitStatus, ProcessIdentity};
use tx_subsystems::signal::deliver_synchronous_fault;
use tx_subsystems::signal::Signum;
use tx_subsystems::signal::{ast_dispatch, refresh_deliverable_signal_summary, AstOutcome};
use tx_subsystems::thread_runtime::execution::prepare_userspace_entry_payload_into;
use tx_subsystems::thread_runtime::{
    clear_current_thread_identity, clear_current_thread_payload, clear_current_userspace_payload,
    clear_current_userspace_thread_identity, set_current_thread_identity,
    set_current_thread_payload, set_current_userspace_payload,
    set_current_userspace_thread_identity, ThreadIdentity, ThreadPayload,
};
use tx_subsystems::vm::{
    AccessMode, AddressSpace, Prot, UserAccessKind, UserRange, UserVirtAddr, VmEntry,
    VmEntryBacking, VmFault, USER_PAGE_SIZE,
};

const HOT_SYSCALL_HANDOFF_BUDGET: u8 = 64;
const POST_TRAP_EBR_BUDGET: usize = 512;

/// Run deferred destructors after the architecture trap shell has longjmped
/// back to the saved reactor stack.
///
/// `epoch::guard()` can be entered on the small per-hart trap stack, so its
/// periodic cold path only publishes a collection request there. Waiting for
/// the outer hart loop is too late for a continuously runnable compiler: one
/// future poll can process many immediately-ready syscall/page-fault
/// round-trips. This seam executes once per resolved userspace trap, on the
/// ordinary kernel stack, and preserves Crossbeam's bounded 512-callback batch.
fn drain_post_trap_epoch_maintenance() {
    let _ = crate::adapter::step_engine::drain_requested_with_budget(POST_TRAP_EBR_BUDGET);
    // RecipeTree's EBR callback intentionally transfers ownership to a second
    // normal-stack queue. Drain it in the same round so old persistent roots do
    // not retain millions of shared treap nodes until the reactor becomes idle.
    let _ = tx_subsystems::vm::drain_deferred_recipe_reclaims(POST_TRAP_EBR_BUDGET);
}

struct ThreadLoopState {
    last_entry_sysno: Option<u64>,
    syscall_handoff_pending: bool,
    hot_syscall_budget: u8,
    thread_exit_status: Option<i32>,
}

impl ThreadLoopState {
    const fn new() -> Self {
        Self {
            last_entry_sysno: None,
            syscall_handoff_pending: false,
            hot_syscall_budget: HOT_SYSCALL_HANDOFF_BUDGET,
            thread_exit_status: None,
        }
    }

    fn take_task_result(&mut self) -> ThreadTaskResult {
        match self.thread_exit_status.take() {
            Some(status) => ThreadTaskResult::ThreadExit(status),
            None => ThreadTaskResult::GroupExit,
        }
    }
}

enum ThreadLoopControl {
    Continue,
    YieldBeforeContinue,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Terminal intent returned by the userspace thread loop to its task wrapper.
pub enum ThreadTaskResult {
    /// The inner task completed without requiring thread teardown.
    Complete,
    /// A direct `exit(2)` carrying the raw Linux exit code.
    ThreadExit(i32),
    /// A process-wide or signal-driven exit using the process episode status.
    GroupExit,
}

/// Conversion into the terminal intent understood by [`PerHartSlotted`].
pub trait ThreadTaskOutput {
    /// Consume the inner task result and select the wrapper action.
    fn into_thread_task_result(self) -> ThreadTaskResult;
}

impl ThreadTaskOutput for ThreadTaskResult {
    fn into_thread_task_result(self) -> ThreadTaskResult {
        self
    }
}

impl ThreadTaskOutput for () {
    fn into_thread_task_result(self) -> ThreadTaskResult {
        ThreadTaskResult::Complete
    }
}

struct EntryTimerPollContext {
    hart: usize,
    mailbox: Option<Arc<boot_runtime::TaskMailbox>>,
    registrar: Option<DeadlineRegistrarHandle>,
}

fn current_entry_timer_poll_context<P: TxPlatform>() -> EntryTimerPollContext {
    let hart = <P as tx_hal::SmpIf>::current_cpu_id().0;
    let mailbox = crate::adapter::boot_runtime::current_task_mailbox(hart);
    let registrar = crate::adapter::boot_runtime::current_deadline_registrar(hart);
    EntryTimerPollContext {
        hart,
        mailbox,
        registrar,
    }
}

fn fatal_signal_teardown_with_posts<F, G>(
    process: &Cap<ProcessIdentity>,
    sig: Signum,
    signal_post: F,
    wake_post: G,
) -> ThreadLoopControl
where
    F: FnMut(ArcWeak<boot_runtime::TaskMailbox>, boot_runtime::MailboxEvent),
    G: FnMut(&boot_runtime::TaskMailbox, boot_runtime::MailboxEvent) -> bool,
{
    match tx_subsystems::process::execution::step_exit_group_with_signal_with_posts(
        process,
        sig,
        signal_post,
        wake_post,
    ) {
        tx_subsystems::process::ProcessExitOutcome::Completed => ThreadLoopControl::Exit,
        tx_subsystems::process::ProcessExitOutcome::Retry => ThreadLoopControl::Continue,
    }
}

fn fatal_signal_teardown_from_current_hart<P: TxPlatform>(
    process: &Cap<ProcessIdentity>,
    _thread: &Cap<ThreadIdentity>,
    sig: Signum,
) -> ThreadLoopControl {
    fatal_signal_teardown_with_posts(
        process,
        sig,
        |mailbox, event| {
            crate::init::post_mailbox_event_from_current_hart::<P>(mailbox, event);
        },
        |mailbox, event| {
            crate::init::post_mailbox_ref_event_with_hint_from_current_hart::<P>(
                mailbox,
                event,
                MailboxSchedulerHint::Normal,
            )
        },
    )
}

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
    thread: Cap<ThreadIdentity>,
    payload: PayloadCap<ThreadPayload>,
    inner: Option<Pin<Box<F>>>,
    exit: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
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
    pub fn new(thread: Cap<ThreadIdentity>, payload: PayloadCap<ThreadPayload>, inner: F) -> Self {
        Self {
            thread,
            payload,
            inner: Some(Box::pin(inner)),
            exit: None,
            _platform: core::marker::PhantomData,
        }
    }
}

fn thread_exit_posts<P: TxPlatform>() -> tx_subsystems::thread_runtime::ThreadExitPosts {
    tx_subsystems::thread_runtime::ThreadExitPosts::new(
        Some(crate::init::post_mailbox_event_from_current_hart::<P>),
        None,
        Some(crate::init::post_mailbox_ref_event_with_hint_from_current_hart::<P>),
    )
}

fn group_exit_future<P: TxPlatform>(
    thread: Cap<ThreadIdentity>,
    status: ExitStatus,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let _ = tx_shims::linux_syscall::drive_thread_exit_with_status_and_posts(
            thread,
            status,
            thread_exit_posts::<P>(),
        )
        .await;
    })
}

fn explicit_thread_exit_future<P: TxPlatform>(
    thread: Cap<ThreadIdentity>,
    status: i32,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let _ = tx_shims::linux_syscall::drive_thread_exit_with_posts(
            thread,
            status,
            thread_exit_posts::<P>(),
        )
        .await;
    })
}

fn canonical_group_exit_status(thread: &Cap<ThreadIdentity>) -> ExitStatus {
    thread
        .upgrade_owner_proc()
        .and_then(|process| tx_subsystems::process::execution::group_exit_status(&process))
        .unwrap_or(ExitStatus::Exited(0))
}

impl<P, F> Future for PerHartSlotted<P, F>
where
    P: TxPlatform,
    F: Future,
    F::Output: ThreadTaskOutput,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pinning — we never move `inner` out of
        // `self` after pinning. The other fields are `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        let hart = <P as PercpuIf>::current_cpu_id().0;

        let _prev_thread = set_current_thread_identity(hart, this.thread.clone());
        let _prev = set_current_thread_payload(hart, this.payload.clone());
        this.payload.bind_lifecycle_waker(cx.waker().clone());
        let task_mailbox = crate::adapter::boot_runtime::current_task_mailbox(hart);
        if let Some(mailbox) = task_mailbox.as_ref() {
            this.payload.bind_mailbox(Arc::downgrade(mailbox));
            // `ThreadPayload::mailbox` is the lifecycle wake route used by
            // exec/group-exit and fatal signals.  It must retain a task waker
            // even when the inner future is parked on a userspace-run wait
            // rather than one of drive.rs' mailbox-backed waits.
            //
            // Local wait futures are allowed to replace/clear the mailbox
            // waker while they are polled, so install it both before and
            // after the inner poll.  The post-poll pending check closes the
            // register-vs-post race: an event published before registration
            // is observed in the queue; an event published afterwards sees
            // the registered waker.
            mailbox.register_waker(cx.waker().clone());
        }

        // Process termination is a task-level AST, not a property of the
        // particular wait currently held inside `run_thread`. Checking it in
        // the outer wrapper lets exec/group-exit cancel a thread parked in
        // *any* nested await (userspace-run, futex, I/O, timer, ...). The old
        // inner-loop-only checkpoint was unreachable until that await happened
        // to resolve, which left exec waiting forever for random siblings.
        if this.payload.interrupt_summary().termination && this.exit.is_none() {
            this.inner.take();
            this.exit = Some(group_exit_future::<P>(
                this.thread.clone(),
                canonical_group_exit_status(&this.thread),
            ));
        }
        if let Some(exit) = this.exit.as_mut() {
            let out = exit.as_mut().poll(cx);
            let _ = clear_current_userspace_payload(hart);
            let _ = clear_current_thread_payload(hart);
            let _ = clear_current_thread_identity(hart);
            return out;
        }

        let inner_out = this
            .inner
            .as_mut()
            .expect("inner future remains until task completion or termination")
            .as_mut()
            .poll(cx);
        let mut out = Poll::Pending;
        let inner_result = match inner_out {
            Poll::Pending => None,
            Poll::Ready(result) => Some(result.into_thread_task_result()),
        };

        let termination = this.payload.interrupt_summary().termination;
        if termination || inner_result.is_some() {
            this.inner.take();
        }
        if termination {
            this.exit = Some(group_exit_future::<P>(
                this.thread.clone(),
                canonical_group_exit_status(&this.thread),
            ));
            out = this
                .exit
                .as_mut()
                .expect("exit future installed")
                .as_mut()
                .poll(cx);
        } else if let Some(result) = inner_result {
            match result {
                ThreadTaskResult::Complete => out = Poll::Ready(()),
                ThreadTaskResult::ThreadExit(status) => {
                    this.exit = Some(explicit_thread_exit_future::<P>(
                        this.thread.clone(),
                        status,
                    ));
                    out = this
                        .exit
                        .as_mut()
                        .expect("exit future installed")
                        .as_mut()
                        .poll(cx);
                }
                ThreadTaskResult::GroupExit => {
                    this.exit = Some(group_exit_future::<P>(
                        this.thread.clone(),
                        canonical_group_exit_status(&this.thread),
                    ));
                    out = this
                        .exit
                        .as_mut()
                        .expect("exit future installed")
                        .as_mut()
                        .poll(cx);
                }
            }
        }

        if out.is_pending() {
            if let Some(mailbox) = task_mailbox.as_ref() {
                mailbox.register_waker(cx.waker().clone());
                if !mailbox.is_empty() || mailbox.overflow() {
                    cx.waker().wake_by_ref();
                }
            }
        }

        let _ = clear_current_userspace_payload(hart);
        let _ = clear_current_thread_payload(hart);
        let _ = clear_current_thread_identity(hart);

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
) -> ThreadTaskResult {
    let mut state = ThreadLoopState::new();
    loop {
        if let Some(sysno) = state.last_entry_sysno {
            emit_syscall_roundtrip_marker(sysno, b"debug.thread.loop.top");
        }
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
        if let Some(sysno) = state.last_entry_sysno {
            emit_syscall_roundtrip_marker(sysno, b"debug.thread.start_request.after");
        }
        let entry_token = entry_wait.request();
        payload.set_active_userspace_request(Some(entry_token));
        if let Some(sysno) = state.last_entry_sysno {
            emit_syscall_roundtrip_marker(sysno, b"debug.thread.active_request.after");
        }

        // Phase E (stop-state): before entering userspace, check
        // whether the thread is stopped (SIGSTOP / default-Stop
        // disposition). A stopped thread must park until SIGCONT
        // clears the flag.
        while payload.is_stopped() {
            // Busy-wait placeholder.  On real hardware this spins
            // until Gewalt routing for SIGCONT clears the flag.  TODO:
            // replace with a reactor-managed wait-source woken by
            // SIGCONT's mailbox post.
            core::hint::spin_loop();
        }

        if !poll_entry_timers_and_liveness::<P>(&thread) {
            return state.take_task_result();
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
        if let Some(sysno) = state.last_entry_sysno {
            emit_syscall_roundtrip_marker(sysno, b"debug.thread.ast.before");
        }
        let ast_outcome = ast_dispatch(&thread);
        if let Some(sysno) = state.last_entry_sysno {
            emit_syscall_roundtrip_marker(sysno, b"debug.thread.ast.after");
        }
        match ast_outcome {
            AstOutcome::DeliverHandler { sig, action } => {
                if matches!(
                    deliver_entry_signal_handler::<P>(&thread, &payload, sig, action).await,
                    ThreadLoopControl::Exit
                ) {
                    return state.take_task_result();
                }
            }
            AstOutcome::InitiateTermination => {
                payload.set_active_userspace_request(None);
                drop(entry_wait);
                return state.take_task_result();
            }
            AstOutcome::DefaultTerminate { .. } => {
                payload.set_active_userspace_request(None);
                drop(entry_wait);
                return state.take_task_result();
            }
            AstOutcome::DefaultTerminateDeferred { .. } => {
                payload.set_active_userspace_request(None);
                drop(entry_wait);
                crate::adapter::boot_runtime::yield_now().await;
                continue;
            }
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
        if let Some(sysno) = state.last_entry_sysno {
            emit_syscall_roundtrip_marker(sysno, b"debug.thread.checkpoint.after");
        }

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
        if matches!(
            enter_userspace_once::<P>(&thread, &payload, entry_token, state.last_entry_sysno),
            ThreadLoopControl::Exit
        ) {
            return state.take_task_result();
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
            let _ = clear_current_userspace_thread_identity(entry_hart);
        }

        payload.set_active_userspace_request(None);
        drain_post_trap_epoch_maintenance();

        if let UserspaceTrapInfo::Syscall(req) = trap {
            emit_syscall_roundtrip_marker(req.nr, b"debug.thread.trap.consumed");
        }
        emit_thread_debug_value(b"debug.thread.trap.kind", trap_kind_code(&trap));

        // ----------------------------------------------------------------
        // (4) DISPATCH THE RESOLVED TRAP.
        // ----------------------------------------------------------------
        match dispatch_userspace_trap::<P>(&thread, &payload, trap, &mut state).await {
            ThreadLoopControl::Continue => {}
            ThreadLoopControl::YieldBeforeContinue => {
                crate::adapter::boot_runtime::yield_now().await;
            }
            ThreadLoopControl::Exit => return state.take_task_result(),
        }
        // Fall through to the top of the loop — next iteration
        // re-opens the entry-side wait, re-runs the AST checkpoint,
        // and re-dives into userspace with the merged context.
    }
}

fn poll_entry_timers_and_liveness<P: TxPlatform>(thread: &Cap<ThreadIdentity>) -> bool {
    let Some(process) = thread.upgrade_owner_proc() else {
        return false;
    };
    let timer_ctx = current_entry_timer_poll_context::<P>();
    if let Some(mailbox) = timer_ctx.mailbox.as_ref() {
        drain_signal_timer_events(mailbox);
    }
    let timer_registrar = timer_ctx
        .registrar
        .as_ref()
        .map(|registrar| registrar as &dyn DeadlineRegistrar);
    let timer_mailbox = timer_ctx.mailbox.as_ref().map(Arc::downgrade);
    let mut post =
        |mailbox, event| crate::init::post_mailbox_event_from_current_hart::<P>(mailbox, event);
    let _ = tx_shims::linux_syscall::poll_due_posix_timers_with_post::<P, _>(
        &process,
        timer_registrar,
        timer_mailbox.clone(),
        &mut post,
    );
    let _ = tx_shims::linux_syscall::poll_due_itimers_with_post::<P, _>(
        &process,
        timer_registrar,
        timer_mailbox,
        &mut post,
    );
    thread.payload_cap().is_some() && process.aspace_cap().is_some()
}

fn drain_signal_timer_events(mailbox: &boot_runtime::TaskMailbox) {
    while mailbox
        .poll_select(|event| match event {
            boot_runtime::MailboxEvent::SignalTimerFired { .. } => {
                boot_runtime::MailboxPollAction::Take
            }
            _ => boot_runtime::MailboxPollAction::Keep,
        })
        .is_some()
    {}
}

async fn deliver_entry_signal_handler<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    sig: Signum,
    action: tx_subsystems::signal::SigActionEntry,
) -> ThreadLoopControl {
    // Phase D: full signal-frame delivery via
    // SignalFrameIf::prepare_signal_frame. The HAL builds the
    // platform-specific frame bytes + handler-entry UserTrapContext; we
    // write the bytes to the user stack via the process AddressSpace,
    // then store the modified context for the next userspace entry.
    //
    // See: `txdoc:SIGNAL-V1-S15-HANDLER-DELIVERY`.
    let Some(orig_ctx) = payload.saved_user_context() else {
        return ThreadLoopControl::Continue;
    };
    let Some(process) = thread.upgrade_owner_proc() else {
        return ThreadLoopControl::Exit;
    };
    let Some(aspace) = process.aspace_cap() else {
        return ThreadLoopControl::Exit;
    };

    // If a syscall return is pending (e.g. wait4 just resolved, child
    // exit raised SIGCHLD, and the AST is now delivering the handler),
    // apply that return value to the parked pre-signal context's `a0`
    // and clear the pending slot. This keeps the handler's `a0` equal
    // to `sig_no`, while the syscall return surfaces after rt_sigreturn.
    let interrupted_errno = interrupted_syscall_signal_errno(payload, sig, action);
    let saved_ctx = signal_saved_context_with_pending_return(payload, orig_ctx, interrupted_errno);
    let frame_ctx = signal_frame_source_context(sig, orig_ctx, saved_ctx);

    payload.store_saved_signal_context(Some(saved_ctx));

    let handler = match action.disposition {
        tx_subsystems::signal::SigDisposition::Handler(addr) => addr,
        _ => return ThreadLoopControl::Continue,
    };
    let siginfo_record = process.siginfo_take(sig);

    let handler_base_mask = payload.signal_mask();
    let return_mask = payload
        .take_sigsuspend_restore_mask()
        .unwrap_or(handler_base_mask);
    let mut new_mask = handler_base_mask.union(action.sa_mask);
    if !action
        .flags
        .contains(tx_subsystems::signal::SaFlags::NODEFER)
    {
        new_mask.block(sig);
    }
    let siginfo = siginfo_record
        .map(siginfo_to_user_abi)
        .unwrap_or(tx_hal::UserSigInfoAbi::ZERO);

    let current_sp = user_sp_from_context::<P>(&saved_ctx);
    let stack_top = if action
        .flags
        .contains(tx_subsystems::signal::SaFlags::ONSTACK)
    {
        payload
            .alt_stack()
            .and_then(|(base, size)| {
                let end = base.checked_add(size)?;
                Some(if (base..end).contains(&current_sp) {
                    tx_hal::UserPtr::<u8>::new(current_sp)
                } else {
                    tx_hal::UserPtr::<u8>::new(end)
                })
            })
            .unwrap_or_else(|| tx_hal::UserPtr::<u8>::new(current_sp))
    } else {
        tx_hal::UserPtr::<u8>::new(current_sp)
    };
    let restorer_pc = tx_subsystems::vm::vdso_rt_sigreturn_addr(&aspace)
        .map(|address| address.as_usize())
        .unwrap_or(0);
    let setup = tx_hal::SignalFrameWrite {
        stack_top,
        sig_no: sig.raw() as u32,
        siginfo,
        old_mask: tx_hal::UserSignalMaskAbi {
            bits: return_mask.raw_bits(),
        },
        flags: tx_hal::UserSaFlagsAbi {
            bits: action.flags.bits(),
        },
        handler_pc: tx_hal::UserPtr::<()>::new(handler),
        restorer_pc: tx_hal::UserPtr::<()>::new(restorer_pc),
    };

    let prepared = <P as tx_hal::SignalFrameIf>::prepare_signal_frame(&frame_ctx, &setup);
    match prepared {
        Ok((handler_ctx, frame_bytes)) => {
            let frame_addr = user_sp_from_context::<P>(&handler_ctx);
            if !reserve_signal_frame_storage(&aspace, frame_addr, frame_bytes.as_slice().len())
                .await
            {
                return fatal_signal_teardown_from_current_hart::<P>(&process, thread, sig);
            }
            if !copy_signal_frame_to_user(&aspace, frame_addr, frame_bytes.as_slice()).await {
                return fatal_signal_teardown_from_current_hart::<P>(&process, thread, sig);
            }
            if restorer_pc == 0 {
                make_signal_frame_executable(&aspace, frame_addr, frame_bytes.as_slice().len());
            }
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
            ThreadLoopControl::Continue
        }
        Err(_) => fatal_signal_teardown_from_current_hart::<P>(&process, thread, sig),
    }
}

async fn copy_signal_frame_to_user(
    aspace: &Cap<AddressSpace>,
    frame_addr: usize,
    bytes: &[u8],
) -> bool {
    use crate::adapter::step_engine::StepOutcome as V3;

    loop {
        let copy_outcome = {
            let guard = crate::adapter::step_engine::guard();
            aspace.copy_to_user(tx_hal::UserPtr::<u8>::new(frame_addr), bytes, &guard)
        };
        match copy_outcome {
            V3::Done(n) => return n == bytes.len(),
            V3::Err(_) => return false,
            V3::Continue { .. } | V3::Yield { .. } => {
                // The immutable frame buffer makes replaying an already-copied
                // prefix safe. The copy guard was scoped to `copy_outcome` and
                // is gone here: yield cooperatively, then re-prefault the whole
                // declared range before replaying. This keeps `Continue` from
                // becoming an in-place busy loop and lets publication complete.
                tx_reactor::yield_now().await;
                if !reserve_signal_frame_storage(aspace, frame_addr, bytes.len()).await {
                    return false;
                }
            }
        }
    }
}

fn enter_userspace_once<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    entry_token: UserspaceRunRequest,
    last_entry_sysno: Option<u64>,
) -> ThreadLoopControl {
    let Some(process) = thread.upgrade_owner_proc() else {
        return ThreadLoopControl::Exit;
    };
    let Some(aspace) = process.aspace_cap() else {
        return ThreadLoopControl::Exit;
    };

    let root = aspace.pmap().root_handle();
    let mut ctx = tx_hal::UserTrapContext::empty();
    if let Some(sysno) = last_entry_sysno {
        emit_syscall_roundtrip_marker(sysno, b"debug.thread.entry.prepare.before");
    }
    prepare_userspace_entry_payload_into(payload, &mut ctx);
    if let Some(sysno) = last_entry_sysno {
        emit_syscall_roundtrip_marker(sysno, b"debug.thread.entry.prepare.after");
    }
    payload.set_active_userspace_request(Some(entry_token));
    let entry_hart = <P as tx_hal::SmpIf>::current_cpu_id().0;
    let _prev_userspace_thread = set_current_userspace_thread_identity(entry_hart, thread.clone());
    let _prev_userspace = set_current_userspace_payload(entry_hart, payload.clone());
    payload.record_user_entry_diagnostic(
        ctx.pc as u64,
        user_ra_from_context(&ctx) as u64,
        user_sp_from_context::<P>(&ctx) as u64,
        user_tls_from_context::<P>(&ctx) as u64,
        user_syscall_from_context::<P>(&ctx) as u64,
        entry_hart as u64,
    );
    // Interval/posix timers are polled before this entry and publish normal
    // pending signals. The shared AST delivery path below is architecture
    // neutral; the older RV-only inline signal-frame injection is deliberately
    // not repeated here.
    if let Some(sysno) = last_entry_sysno {
        emit_syscall_roundtrip_marker(sysno, b"debug.thread.enter.before");
    }
    <P as TrapIf>::enter_userspace_with_context(&ctx, root);
    tx_substrate::slab::enable_vmalloc_after_kernel_pmap_activation();
    ThreadLoopControl::Continue
}

async fn dispatch_userspace_trap<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    trap: UserspaceTrapInfo,
    state: &mut ThreadLoopState,
) -> ThreadLoopControl {
    match trap {
        UserspaceTrapInfo::TimerPreempt => {
            crate::adapter::boot_runtime::yield_now().await;
            ThreadLoopControl::Continue
        }
        UserspaceTrapInfo::Syscall(req) => {
            dispatch_syscall_trap::<P>(thread, payload, req, state).await
        }
        UserspaceTrapInfo::PageFault(info) => {
            handle_page_fault_trap::<P>(thread, payload, info).await
        }
        UserspaceTrapInfo::Fatal(info) => {
            // Report the REAL trap cause/tval. The previous placeholder
            // (addr=0, no-recipe) made every fatal trap read like a
            // NULL-pointer page fault and hid the actual scause
            // (illegal-instruction vs misaligned vs ...).
            tx_hal::console_write_str::<P>("txkernel:");
            tx_hal::console_write_str::<P>(P::BOARD);
            tx_hal::console_write_str::<P>(":user-fatal:cause=0x");
            write_hex_u64::<P>(info.cause);
            tx_hal::console_write_str::<P>(":tval=0x");
            write_hex_u64::<P>(info.value);
            tx_hal::console_write_str::<P>("\n");
            log_user_segv::<P>(
                thread,
                payload,
                info.value,
                PageFaultAccess::Unknown,
                "fatal",
                tx_subsystems::vm::VmFaultError::NoRecipe,
            );
            dump_syscall_history::<P>();
            match deliver_synchronous_fault(thread, Signum::SIGSEGV) {
                tx_subsystems::process::ProcessExitOutcome::Completed => ThreadLoopControl::Exit,
                tx_subsystems::process::ProcessExitOutcome::Retry => ThreadLoopControl::Continue,
            }
        }
    }
}

async fn dispatch_syscall_trap<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    req: SyscallRequest,
    state: &mut ThreadLoopState,
) -> ThreadLoopControl {
    if state.syscall_handoff_pending {
        state.syscall_handoff_pending = false;
        emit_thread_debug_value(b"debug.thread.clone_handoff.yield", req.nr as i64);
        crate::adapter::boot_runtime::yield_now().await;
    }
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.await.ready");
    let Some(process) = thread.upgrade_owner_proc() else {
        return ThreadLoopControl::Exit;
    };
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.process.after");

    let sigreturn_ctx = payload.saved_user_context();
    payload.begin_syscall_diagnostic(req.nr, req.args[0], req.args[1]);
    payload.set_proc_sleeping(true);
    let result = match run_syscall_dispatch::<P>(thread, payload, &process, req).await {
        Some(result) => result,
        None => return ThreadLoopControl::Exit,
    };
    payload.set_proc_sleeping(false);
    payload.end_syscall_diagnostic();

    if matches!(result, SyscallResult::Error(5)) {
        log_syscall_eio::<P>(&req, &process, thread, payload);
    }
    dump_observe_threshold_if_ready::<P>();

    if req.nr == NR_EXIT && matches!(result, SyscallResult::NoReturn) {
        state.thread_exit_status = Some(req.args[0] as i32);
    }

    if matches!(
        store_syscall_result::<P>(thread, payload, &process, &sigreturn_ctx, &result),
        ThreadLoopControl::Exit
    ) {
        return ThreadLoopControl::Exit;
    }
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.return.stored");
    state.last_entry_sysno = Some(req.nr);
    update_syscall_handoff_state(&req, &result, state);
    dump_observe_threshold_if_ready::<P>();
    ThreadLoopControl::Continue
}

async fn run_syscall_dispatch<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    process: &Cap<ProcessIdentity>,
    req: SyscallRequest,
) -> Option<SyscallResult> {
    if req.nr == NR_EXIT {
        // The outer PerHartSlotted wrapper owns the resumable exit operation.
        // Returning only the intent here prevents a simultaneous group-exit
        // publication from creating a second operation for the same tid.
        return Some(SyscallResult::NoReturn);
    }
    if let Some(result) = tx_shims::linux_syscall::dispatch_cap_only_immediate(&req, process) {
        emit_syscall_roundtrip_marker(req.nr, b"debug.thread.immediate.after");
        return Some(result);
    }

    let Some(aspace) = process.aspace_cap() else {
        return None;
    };
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.aspace.after");
    if let Some(result) = tx_shims::linux_syscall::dispatch_thread_payload_aspace_oneshot(
        &req, thread, payload, &aspace,
    ) {
        emit_syscall_roundtrip_marker(req.nr, b"debug.thread.oneshot.after");
        return Some(result);
    }
    if let Some(result) = tx_shims::linux_syscall::dispatch_vm_try_oneshot(&req, &aspace) {
        emit_syscall_roundtrip_marker(req.nr, b"debug.thread.oneshot.after");
        return Some(result);
    }
    if let Some(result) =
        tx_shims::linux_syscall::dispatch_clone_oneshot::<P>(&req, process, thread, &aspace)
    {
        emit_syscall_roundtrip_marker(req.nr, b"debug.thread.oneshot.after");
        return Some(result);
    }
    if let Some(result) =
        tx_shims::linux_syscall::dispatch_process_aspace_immediate(&req, process, &aspace)
    {
        emit_syscall_roundtrip_marker(req.nr, b"debug.thread.immediate.after");
        return Some(result);
    }

    let ctx = build_syscall_ctx::<P>(&req, thread, process, &aspace);
    if req.nr == NR_EXIT_GROUP {
        loop {
            let result = dispatch_full_syscall::<P>(req, &ctx).await;
            if !matches!(result, SyscallResult::Error(11)) {
                return Some(result);
            }
            // This EAGAIN is an internal exec/group-exit serialization
            // signal, not a Linux-visible exit_group result.
            crate::adapter::boot_runtime::yield_now().await;
        }
    }
    Some(dispatch_full_syscall::<P>(req, &ctx).await)
}

fn build_syscall_ctx<P: TxPlatform>(
    req: &SyscallRequest,
    thread: &Cap<ThreadIdentity>,
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
) -> tx_shims::linux_syscall::SyscallCtx<'static> {
    let ctx_process = process.clone();
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.ctx.process_clone.after");
    let ctx_thread = thread.clone();
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.ctx.thread_clone.after");
    let ctx_aspace = aspace.clone();
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.ctx.aspace_clone.after");
    let cred_snapshot = process
        .cred_snapshot()
        .unwrap_or_else(tx_subsystems::cred::CredSnapshot::root);
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.ctx.cred_snapshot.after");
    let mut ctx = tx_shims::linux_syscall::SyscallCtx::from_parts_with_cred_snapshot(
        ctx_process,
        ctx_thread,
        ctx_aspace,
        cred_snapshot,
    )
    .with_mailbox_post(crate::init::post_mailbox_event_from_current_hart::<P>)
    .with_mailbox_ref_post_with_hint(
        crate::init::post_mailbox_ref_event_with_hint_from_current_hart::<P>,
    );
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.ctx.after");

    let timer_ctx = current_entry_timer_poll_context::<P>();
    if let Some(mailbox) = timer_ctx.mailbox {
        ctx = ctx.with_mailbox(mailbox);
    }
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.mailbox.after");
    if let Some(registrar) = timer_ctx.registrar {
        ctx = ctx.with_timer_registrar(registrar);
    }
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.timer.after");
    if let Some(dr) = crate::adapter::boot_runtime::current_delegate_registry(timer_ctx.hart) {
        ctx = ctx.with_delegate_registry(dr);
    }
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.delegate.after");
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.saved_ctx.after");
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.dispatch.before");
    ctx
}

async fn dispatch_full_syscall<P: TxPlatform>(
    req: SyscallRequest,
    ctx: &tx_shims::linux_syscall::SyscallCtx<'_>,
) -> SyscallResult {
    let result =
        if let Some(result) = tx_shims::linux_syscall::dispatch_pthread_hot_oneshot(req, ctx) {
            result
        } else if let Some(result) =
            tx_shims::linux_syscall::dispatch_writev_pagebacked_oneshot(&req, ctx)
        {
            emit_syscall_roundtrip_marker(req.nr, b"debug.thread.dispatch.writev_pagebacked.after");
            result
        } else if req.nr == NR_WRITEV {
            let fut = tx_shims::linux_syscall::dispatch_writev_hot(req, ctx);
            emit_syscall_roundtrip_marker(req.nr, b"debug.thread.dispatch.writev_future.after");
            let fut = Box::pin(fut);
            emit_syscall_roundtrip_marker(req.nr, b"debug.thread.dispatch.writev_box.after");
            fut.await
                .expect("writev hot dispatch prefilter covers writev")
        } else if matches!(req.nr, NR_MMAP | NR_MPROTECT | NR_MUNMAP) {
            let mut vm_future = Box::pin(tx_shims::linux_syscall::dispatch_vm_hot(req, ctx));
            let mut pending_reported = false;
            core::future::poll_fn(|cx| match vm_future.as_mut().poll(cx) {
                Poll::Ready(result) => Poll::Ready(
                    result.expect("VM hot dispatch prefilter covers mmap/mprotect/munmap"),
                ),
                Poll::Pending => {
                    if req.nr == NR_MUNMAP && !pending_reported {
                        pending_reported = true;
                        let range_lock = ctx.aspace.range_lock().diagnostic_snapshot();
                        tx_hal::console_write_str::<P>("txkernel:vm-hot:munmap-pending:a0=0x");
                        write_hex_u64::<P>(req.args[0]);
                        tx_hal::console_write_str::<P>(":a1=0x");
                        write_hex_u64::<P>(req.args[1]);
                        tx_hal::console_write_str::<P>(":active=0x");
                        write_hex_u64::<P>(range_lock.active as u64);
                        tx_hal::console_write_str::<P>(":pending-writers=0x");
                        write_hex_u64::<P>(range_lock.pending_writers as u64);
                        tx_hal::console_write_str::<P>(":release-mask=0x");
                        write_hex_u64::<P>(
                            ctx.aspace
                                .range_lock()
                                .release_endpoint()
                                .pending_mask_snapshot(),
                        );
                        tx_hal::console_write_str::<P>("\n");
                    }
                    Poll::Pending
                }
            })
            .await
        } else {
            Box::pin(tx_shims::linux_syscall::dispatch::<P>(req, ctx)).await
        };
    emit_syscall_roundtrip_marker(req.nr, b"debug.thread.dispatch.after");
    result
}

fn store_syscall_result<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    process: &Cap<ProcessIdentity>,
    sigreturn_ctx: &Option<tx_hal::UserTrapContext>,
    result: &SyscallResult,
) -> ThreadLoopControl {
    match result {
        SyscallResult::Return(v) => {
            payload.store_pending_syscall_return(Some(Ok(*v)));
        }
        SyscallResult::CloneReturn { value, .. } => {
            payload.store_pending_syscall_return(Some(Ok(*value)));
        }
        SyscallResult::Error(e) => {
            payload.store_pending_syscall_return(Some(Err(*e)));
        }
        SyscallResult::NoReturn => {
            return ThreadLoopControl::Exit;
        }
        SyscallResult::ExecCommitted => {}
        SyscallResult::SigreturnContextRestored => {}
        SyscallResult::SigreturnRestored => {
            let Some(sigreturn_ctx) = sigreturn_ctx else {
                return fatal_signal_teardown_from_current_hart::<P>(
                    process,
                    thread,
                    Signum::SIGSEGV,
                );
            };
            let Some(aspace) = process.aspace_cap() else {
                return ThreadLoopControl::Exit;
            };
            if restore_sigreturn_frame::<P>(thread, &aspace, payload, sigreturn_ctx).is_err() {
                return fatal_signal_teardown_from_current_hart::<P>(
                    process,
                    thread,
                    Signum::SIGSEGV,
                );
            }
        }
    }
    ThreadLoopControl::Continue
}

fn update_syscall_handoff_state(
    req: &SyscallRequest,
    result: &SyscallResult,
    state: &mut ThreadLoopState,
) {
    if syscall_return_needs_handoff(req, result) {
        state.syscall_handoff_pending = true;
        state.hot_syscall_budget = HOT_SYSCALL_HANDOFF_BUDGET;
    } else if syscall_return_consumes_hot_budget(req, result, &mut state.hot_syscall_budget) {
        state.syscall_handoff_pending = true;
    }
}

async fn handle_page_fault_trap<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    info: ReactorPageFaultInfo,
) -> ThreadLoopControl {
    emit_thread_debug_value(b"debug.thread.page_fault.before", 1);
    emit_thread_debug_value(b"debug.thread.page_fault.addr", info.addr.raw() as i64);
    emit_thread_debug_value(
        b"debug.thread.page_fault.access",
        page_fault_access_code(info.access),
    );
    debug_assert!(
        pf_info_implies_from_user(info),
        "thread future only sees PageFault for from-user faults; \
         trap shell terminates non-user faults at the shell"
    );
    let Some(process) = thread.upgrade_owner_proc() else {
        return ThreadLoopControl::Exit;
    };
    let Some(aspace) = process.aspace_cap() else {
        return ThreadLoopControl::Exit;
    };
    let fault = VmFault::new(
        UserVirtAddr::new(info.addr.raw() as usize),
        pf_access_to_vm_access(info.access),
    );
    dump_observe_threshold_if_ready::<P>();
    let fault_result = if let Some(mailbox) = payload.mailbox_handle() {
        Box::pin(aspace.fault_script_for_process_with_post(
            fault,
            &process,
            mailbox,
            crate::init::post_mailbox_ref_event_with_hint_from_current_hart::<P>,
        ))
        .await
    } else {
        Box::pin(aspace.fault_script(fault)).await
    };
    match fault_result {
        Ok(_) => {
            emit_thread_debug_value(b"debug.thread.page_fault.ok", 1);
            dump_observe_threshold_if_ready::<P>();
            ThreadLoopControl::Continue
        }
        Err(e) => {
            emit_thread_debug_value(b"debug.thread.page_fault.err", vm_fault_error_code(e));
            let pc = log_user_segv::<P>(thread, payload, info.addr.raw(), info.access, "pf", e);
            log_nearby_recipes::<P>(&aspace, info.addr.raw() as usize, "fault");
            if pc as usize != info.addr.raw() as usize {
                log_nearby_recipes::<P>(&aspace, pc as usize, "pc");
            }
            dump_syscall_history::<P>();
            dump_user_regs::<P>(payload);
            dump_user_mem_windows::<P>(&aspace, payload);
            dump_all_recipes::<P>(&aspace);
            match deliver_synchronous_fault(thread, Signum::SIGSEGV) {
                tx_subsystems::process::ProcessExitOutcome::Completed => ThreadLoopControl::Exit,
                tx_subsystems::process::ProcessExitOutcome::Retry => {
                    ThreadLoopControl::YieldBeforeContinue
                }
            }
        }
    }
}

pub(crate) fn syscall_return_needs_handoff(req: &SyscallRequest, result: &SyscallResult) -> bool {
    match (req.nr, result) {
        (NR_CLONE, SyscallResult::Return(_)) => true,
        (
            NR_CLONE,
            SyscallResult::CloneReturn {
                child_submit: tx_subsystems::reactor_submit::SubmitChildThreadStatus::QueuedFallback,
                ..
            },
        ) => true,
        (
            NR_CLONE,
            SyscallResult::CloneReturn {
                child_submit: tx_subsystems::reactor_submit::SubmitChildThreadStatus::Published,
                ..
            },
        ) => true,
        _ => false,
    }
}

pub(crate) fn syscall_return_consumes_hot_budget(
    req: &SyscallRequest,
    result: &SyscallResult,
    budget: &mut u8,
) -> bool {
    let SyscallResult::Return(value) = result else {
        *budget = HOT_SYSCALL_HANDOFF_BUDGET;
        return false;
    };
    if *value < 0 || !matches!(req.nr, NR_READ | NR_WRITE | NR_READV | NR_WRITEV) {
        *budget = HOT_SYSCALL_HANDOFF_BUDGET;
        return false;
    }
    if *budget > 1 {
        *budget -= 1;
        return false;
    }
    *budget = HOT_SYSCALL_HANDOFF_BUDGET;
    true
}

pub(crate) fn syscall_return_may_publish_wake_handoff(
    req: &SyscallRequest,
    result: &SyscallResult,
) -> bool {
    let SyscallResult::Return(woken) = result else {
        return false;
    };
    if *woken <= 0 || req.nr != NR_FUTEX {
        return false;
    }
    let op = (req.args[1] as u32) & FUTEX_CMD_MASK;
    matches!(op, FUTEX_WAKE | FUTEX_WAKE_BITSET)
}

fn emit_syscall_roundtrip_marker(sysno: u64, name: &[u8]) {
    if !cfg!(tx_thread_roundtrip_metrics) {
        return;
    }
    if !matches!(
        sysno,
        tx_shims::linux_syscall::numbers::NR_GETPPID
            | NR_CLONE
            | NR_RT_SIGPROCMASK
            | NR_MMAP
            | NR_MPROTECT
            | NR_MUNMAP
            | NR_WRITEV
    ) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, sysno as i64);
        tx_observe::dump_registered_if_requested();
    }
}

fn emit_thread_debug_value(name: &[u8], value: i64) {
    if !cfg!(tx_thread_roundtrip_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

fn trap_kind_code(trap: &UserspaceTrapInfo) -> i64 {
    match trap {
        UserspaceTrapInfo::TimerPreempt => 1,
        UserspaceTrapInfo::Syscall(_) => 2,
        UserspaceTrapInfo::PageFault(_) => 3,
        UserspaceTrapInfo::Fatal(_) => 4,
    }
}

const fn page_fault_access_code(access: PageFaultAccess) -> i64 {
    match access {
        PageFaultAccess::Read => 1,
        PageFaultAccess::Write => 2,
        PageFaultAccess::Execute => 3,
        PageFaultAccess::Unknown => 4,
    }
}

const fn vm_fault_error_code(error: tx_subsystems::vm::VmFaultError) -> i64 {
    use tx_subsystems::vm::VmFaultError;

    match error {
        VmFaultError::Range(_) => 1,
        VmFaultError::NoRecipe => 2,
        VmFaultError::ProtectionViolation => 3,
        VmFaultError::WouldBlock => 4,
        VmFaultError::BackingMismatch => 5,
        VmFaultError::BackingOffsetOverflow => 6,
        VmFaultError::PageBeyondSize => 7,
        VmFaultError::PageCache(_) => 8,
        VmFaultError::StaleRecipe => 9,
        VmFaultError::Pmap(_) => 10,
        VmFaultError::SpecialUnavailable => 11,
    }
}

fn dump_observe_threshold_if_ready<P: TxPlatform>() {
    if tx_observe::should_dump_now() {
        tx_observe::dump_console_hex::<P>(<P as tx_hal::SmpIf>::current_cpu_id());
        tx_hal::console_write_str::<P>(":observe:dump:threshold\n");
        <P as tx_hal::PowerIf>::system_off();
    }
}

/// Print one complete record when a userspace syscall returns `EIO`.
///
/// This sits after every dispatch lane (direct, one-shot, hot, and generic), so
/// it identifies the actual failing syscall even when libc reports only that a
/// child process was "never executed". Normal syscall traffic is silent.
fn log_syscall_eio<P: TxPlatform>(
    req: &SyscallRequest,
    process: &Cap<tx_subsystems::process::ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
) {
    let comm = process.comm();
    let comm_len = comm
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(comm.len());
    let comm = core::str::from_utf8(&comm[..comm_len]).unwrap_or("<non-utf8>");
    let saved = payload.saved_user_context();
    let pc = saved.as_ref().map(|ctx| ctx.pc).unwrap_or(0);
    let sp = saved.as_ref().map(user_sp_from_context::<P>).unwrap_or(0);
    let hart = <P as tx_hal::SmpIf>::current_cpu_id().0;

    if let Some(aspace) = process.aspace_cap() {
        let pmap = aspace.pmap().stats();
        let range = aspace.range_lock().diagnostic_snapshot();
        tx_hal::console_write_str::<P>(&alloc::format!(
            "txkernel:syscall-eio:pid={}:tid={}:comm={comm}:hart={hart}:nr={}:a0={:#x}:a1={:#x}:a2={:#x}:a3={:#x}:a4={:#x}:a5={:#x}:pc={pc:#x}:sp={sp:#x}:aspace={}:pmap_mapped={}:pmap_reservations={}:pmap_commits={}:pmap_rollbacks={}:pmap_shootdowns={}:range_active={}:range_pending={}:range_source={:#x}\n",
            process.pid.0,
            thread.tid.0,
            req.nr,
            req.args[0],
            req.args[1],
            req.args[2],
            req.args[3],
            req.args[4],
            req.args[5],
            aspace.futex_identity(),
            pmap.mapped_pages,
            pmap.reservations,
            pmap.commits,
            pmap.rollbacks,
            pmap.shootdowns,
            range.active,
            range.pending_writers,
            range.wait_source_id,
        ));
    } else {
        tx_hal::console_write_str::<P>(&alloc::format!(
            "txkernel:syscall-eio:pid={}:tid={}:comm={comm}:hart={hart}:nr={}:a0={:#x}:a1={:#x}:a2={:#x}:a3={:#x}:a4={:#x}:a5={:#x}:pc={pc:#x}:sp={sp:#x}:aspace=none\n",
            process.pid.0,
            thread.tid.0,
            req.nr,
            req.args[0],
            req.args[1],
            req.args[2],
            req.args[3],
            req.args[4],
            req.args[5],
        ));
    }
}

fn log_user_segv<P: TxPlatform>(
    thread: &Cap<ThreadIdentity>,
    payload: &ThreadPayload,
    fault_addr: u64,
    access: PageFaultAccess,
    kind: &str,
    error: tx_subsystems::vm::VmFaultError,
) -> u64 {
    let ctx = payload.saved_user_context();
    let pc = ctx.as_ref().map(|ctx| ctx.pc as u64).unwrap_or(0);
    let hart = <P as tx_hal::SmpIf>::current_cpu_id().0;
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
    if let tx_subsystems::vm::VmFaultError::PageCache(
        tx_subsystems::page_backed::PageCacheError::Backend(errno),
    ) = error
    {
        tx_hal::console_write_str::<P>(":errno=0x");
        write_hex_u64::<P>(errno.linux_i32() as u64);
    }
    tx_hal::console_write_str::<P>(":pc=0x");
    write_hex_u64::<P>(pc);
    tx_hal::console_write_str::<P>(":addr=0x");
    write_hex_u64::<P>(fault_addr);
    tx_hal::console_write_str::<P>(":tid=0x");
    write_hex_u64::<P>(thread.tid.0 as u64);
    tx_hal::console_write_str::<P>(":hart=0x");
    write_hex_u64::<P>(hart as u64);
    if let Some(process) = thread.upgrade_owner_proc() {
        let comm = process.comm();
        let comm_len = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        let comm = core::str::from_utf8(&comm[..comm_len]).unwrap_or("<non-utf8>");
        tx_hal::console_write_str::<P>(":pid=0x");
        write_hex_u64::<P>(process.pid.0 as u64);
        tx_hal::console_write_str::<P>(":comm=");
        tx_hal::console_write_str::<P>(comm);
    }
    if let Some(ctx) = ctx.as_ref() {
        tx_hal::console_write_str::<P>(":ra=0x");
        write_hex_u64::<P>(user_ra_from_context(ctx) as u64);
        tx_hal::console_write_str::<P>(":sp=0x");
        write_hex_u64::<P>(user_sp_from_context::<P>(ctx) as u64);
        tx_hal::console_write_str::<P>(":tls=0x");
        write_hex_u64::<P>(user_tls_from_context::<P>(ctx) as u64);
        tx_hal::console_write_str::<P>(":syscall=0x");
        write_hex_u64::<P>(user_syscall_from_context::<P>(ctx) as u64);
        tx_hal::console_write_str::<P>(":a0=0x");
        write_hex_u64::<P>(user_arg_from_context::<P>(ctx, 0) as u64);
        tx_hal::console_write_str::<P>(":a1=0x");
        write_hex_u64::<P>(user_arg_from_context::<P>(ctx, 1) as u64);
        tx_hal::console_write_str::<P>(":a2=0x");
        write_hex_u64::<P>(user_arg_from_context::<P>(ctx, 2) as u64);
    }
    let (entry_pc, entry_ra, entry_sp, entry_tls, entry_syscall, entry_hart) =
        payload.user_entry_diagnostic();
    tx_hal::console_write_str::<P>(":last-entry-pc=0x");
    write_hex_u64::<P>(entry_pc);
    tx_hal::console_write_str::<P>(":last-entry-ra=0x");
    write_hex_u64::<P>(entry_ra);
    tx_hal::console_write_str::<P>(":last-entry-sp=0x");
    write_hex_u64::<P>(entry_sp);
    tx_hal::console_write_str::<P>(":last-entry-tls=0x");
    write_hex_u64::<P>(entry_tls);
    tx_hal::console_write_str::<P>(":last-entry-syscall=0x");
    write_hex_u64::<P>(entry_syscall);
    tx_hal::console_write_str::<P>(":last-entry-hart=0x");
    write_hex_u64::<P>(entry_hart);
    tx_hal::console_write_str::<P>("\n");
    pc
}

fn log_nearby_recipes<P: TxPlatform>(
    aspace: &AddressSpace,
    lookup_addr: usize,
    lookup_label: &str,
) {
    let recipes = aspace.recipes_snapshot();
    let mut containing: Option<VmEntry> = None;
    let mut lower: Option<VmEntry> = None;
    let mut upper: Option<VmEntry> = None;

    for entry in recipes.iter().cloned() {
        let start = entry.range.start().as_usize();
        let end = entry.range.end().as_usize();
        if start <= lookup_addr && lookup_addr < end {
            containing = Some(entry);
            break;
        }
        if end <= lookup_addr
            && lower
                .as_ref()
                .is_none_or(|old| old.range.end().as_usize() < end)
        {
            lower = Some(entry.clone());
        }
        if lookup_addr < start
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
    tx_hal::console_write_str::<P>(":");
    tx_hal::console_write_str::<P>(lookup_label);
    tx_hal::console_write_str::<P>("=0x");
    write_hex_u64::<P>(lookup_addr as u64);
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
    match entry.backing_kind() {
        VmEntryBacking::None => tx_hal::console_write_str::<P>("none"),
        VmEntryBacking::PrivateAnon => tx_hal::console_write_str::<P>("anon"),
        VmEntryBacking::Page { offset } => {
            tx_hal::console_write_str::<P>("page@0x");
            write_hex_u64::<P>(offset);
            if let Some((pc, _)) = entry.page_backing() {
                tx_hal::console_write_str::<P>(":pc-size=0x");
                write_hex_u64::<P>(pc.size_bytes());
                tx_hal::console_write_str::<P>(":pc-pages=0x");
                write_hex_u64::<P>(pc.page_count());
                match pc.kind() {
                    tx_subsystems::page_backed::PageContainerKind::File {
                        fs_object_id, ..
                    } => {
                        tx_hal::console_write_str::<P>(":file-id=0x");
                        write_hex_u64::<P>(fs_object_id.as_u64());
                    }
                    tx_subsystems::page_backed::PageContainerKind::Anon { .. } => {
                        tx_hal::console_write_str::<P>(":anon");
                    }
                    tx_subsystems::page_backed::PageContainerKind::Device { .. } => {
                        tx_hal::console_write_str::<P>(":device");
                    }
                }
            }
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
        tx_subsystems::vm::VmFaultError::PageCache(error) => match error {
            tx_subsystems::page_backed::PageCacheError::AlreadyPresent { .. } => {
                "page-cache-already-present"
            }
            tx_subsystems::page_backed::PageCacheError::MissingPage => "page-cache-missing-page",
            tx_subsystems::page_backed::PageCacheError::MismatchedFrame { .. } => {
                "page-cache-mismatched-frame"
            }
            tx_subsystems::page_backed::PageCacheError::OutOfBounds => "page-cache-out-of-bounds",
            tx_subsystems::page_backed::PageCacheError::UnsupportedKind => {
                "page-cache-unsupported-kind"
            }
            tx_subsystems::page_backed::PageCacheError::Backend(errno) => match errno {
                tx_subsystems::execution::Errno::EAGAIN => "page-cache-backend-eagain",
                tx_subsystems::execution::Errno::EBUSY => "page-cache-backend-ebusy",
                tx_subsystems::execution::Errno::EIO => "page-cache-backend-eio",
                tx_subsystems::execution::Errno::ENOMEM => "page-cache-backend-enomem",
                tx_subsystems::execution::Errno::ESTALE => "page-cache-backend-estale",
                _ => "page-cache-backend-other",
            },
            tx_subsystems::page_backed::PageCacheError::Alloc(error) => match error {
                tx_substrate::page_allocator::AllocError::Exhausted => "page-cache-alloc-exhausted",
                tx_substrate::page_allocator::AllocError::InvalidRequest => {
                    "page-cache-alloc-invalid"
                }
                tx_substrate::page_allocator::AllocError::CounterOverflow => {
                    "page-cache-alloc-overflow"
                }
                tx_substrate::page_allocator::AllocError::CounterUnderflow => {
                    "page-cache-alloc-underflow"
                }
                tx_substrate::page_allocator::AllocError::DoubleFree => {
                    "page-cache-alloc-double-free"
                }
                _ => "page-cache-alloc-other",
            },
        },
        tx_subsystems::vm::VmFaultError::StaleRecipe => "stale-recipe",
        tx_subsystems::vm::VmFaultError::SpecialUnavailable => "special-unavailable",
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
            tx_subsystems::vm::VmPmapError::ConcurrentPublication => "pmap-concurrent-publication",
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

fn signal_frame_source_context(
    sig: tx_subsystems::signal::Signum,
    orig_ctx: tx_hal::UserTrapContext,
    mut saved_ctx: tx_hal::UserTrapContext,
) -> tx_hal::UserTrapContext {
    // glibc's SIGCANCEL handler inspects the interrupted PC in the
    // ucontext and only performs deferred cancellation when that PC is
    // inside __syscall_cancel_arch_start..end. Keep only that PC from
    // the pre-return syscall context; the rest of the frame must carry
    // the syscall return value that rt_sigreturn will restore.
    if sig.is_libc_sigcancel() {
        saved_ctx.pc = orig_ctx.pc;
    }
    saved_ctx
}

fn signal_saved_context_with_pending_return(
    payload: &ThreadPayload,
    mut orig_ctx: tx_hal::UserTrapContext,
    interrupted_errno: Option<i32>,
) -> tx_hal::UserTrapContext {
    let result = tx_subsystems::thread_runtime::structure::drain_pending_syscall_return(payload)
        .or_else(|| interrupted_errno.map(Err));
    if let Some(result) = result {
        let encoded = match result {
            Ok(v) => v as u64,
            Err(errno) => (-i64::from(errno)) as u64,
        };
        orig_ctx.regs[tx_subsystems::thread_runtime::execution::USER_CONTEXT_A0_INDEX] =
            encoded as usize;
    }
    orig_ctx
}

fn interrupted_syscall_signal_errno(
    payload: &ThreadPayload,
    sig: Signum,
    action: tx_subsystems::signal::SigActionEntry,
) -> Option<i32> {
    if !payload.proc_sleeping() {
        return None;
    }
    if action
        .flags
        .contains(tx_subsystems::signal::SaFlags::RESTART)
        && sig != Signum::SIGINT
    {
        return None;
    }
    Some(4)
}

fn user_sp_from_context<P: TxPlatform>(ctx: &tx_hal::UserTrapContext) -> usize {
    match P::ARCH {
        tx_hal::Arch::Riscv64 => ctx.regs[2],
        tx_hal::Arch::LoongArch64 => ctx.regs[3],
    }
}

fn user_ra_from_context(ctx: &tx_hal::UserTrapContext) -> usize {
    ctx.regs[1]
}

fn user_tls_from_context<P: TxPlatform>(ctx: &tx_hal::UserTrapContext) -> usize {
    match P::ARCH {
        tx_hal::Arch::Riscv64 => ctx.regs[4],
        tx_hal::Arch::LoongArch64 => ctx.regs[2],
    }
}

fn user_syscall_from_context<P: TxPlatform>(ctx: &tx_hal::UserTrapContext) -> usize {
    match P::ARCH {
        tx_hal::Arch::Riscv64 => ctx.regs[17],
        tx_hal::Arch::LoongArch64 => ctx.regs[11],
    }
}

fn user_arg_from_context<P: TxPlatform>(ctx: &tx_hal::UserTrapContext, index: usize) -> usize {
    let base = match P::ARCH {
        tx_hal::Arch::Riscv64 => 10,
        tx_hal::Arch::LoongArch64 => 4,
    };
    ctx.regs[base + index]
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

async fn reserve_signal_frame_storage(
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
    aspace
        .reserve_user_range_for_access_wait(range, UserAccessKind::Write)
        .await
        .is_ok()
}

/// PROBE(proxy-push segv hunt): dump the last syscalls (nr=ret, hex) recorded
/// by the tx-shims dispatch ring, so the fatal-trap report shows what
/// git-remote-https did right before it faulted. Newest entry printed last.
fn dump_syscall_history<P: TxPlatform>() {
    let (nrs, rets, metas, pos) = tx_shims::linux_syscall::syscall_history_snapshot();
    let len = tx_shims::linux_syscall::SYSCALL_HISTORY_LEN;
    tx_hal::console_write_str::<P>("txkernel:syshist(pid.nr(fd,cnt)=ret hex,newest-last):");
    let show = if len < 36 { len } else { 36 };
    for k in (0..show).rev() {
        let idx = (pos + len - 1 - k) % len;
        tx_hal::console_write_str::<P>(" ");
        write_hex_u64::<P>(metas[idx] >> 48);
        tx_hal::console_write_str::<P>(".");
        write_hex_u64::<P>(nrs[idx]);
        tx_hal::console_write_str::<P>("(");
        write_hex_u64::<P>((metas[idx] >> 32) & 0xffff);
        tx_hal::console_write_str::<P>(",");
        write_hex_u64::<P>(metas[idx] & 0xffff_ffff);
        tx_hal::console_write_str::<P>(")=");
        write_hex_u64::<P>(rets[idx] as u64);
    }
    tx_hal::console_write_str::<P>("\n");
}

/// PROBE(proxy-push segv hunt): dump the full user register file so the
/// mallocng-assert crash site can be reconstructed (which assert fired, what
/// the header/meta values were).
fn dump_user_regs<P: TxPlatform>(payload: &ThreadPayload) {
    let Some(ctx) = payload.saved_user_context() else {
        return;
    };
    tx_hal::console_write_str::<P>("txkernel:user-segv:regs");
    for (i, r) in ctx.regs.iter().enumerate().skip(1) {
        tx_hal::console_write_str::<P>(" x");
        write_hex_u64::<P>(i as u64);
        tx_hal::console_write_str::<P>("=");
        write_hex_u64::<P>(*r as u64);
    }
    tx_hal::console_write_str::<P>("\n");
}

/// PROBE(proxy-push segv hunt): hexdump user memory windows around the
/// registers involved in musl mallocng's get_meta asserts (a0/a5/s0 and the
/// stack), so the corrupted heap bytes are visible in the post-mortem.
fn dump_user_mem_windows<P: TxPlatform>(aspace: &AddressSpace, payload: &ThreadPayload) {
    let Some(ctx) = payload.saved_user_context() else {
        return;
    };
    let a0 = ctx.regs[10] as u64;
    let a5 = ctx.regs[15] as u64;
    let s0 = ctx.regs[8] as u64;
    let sp = ctx.regs[2] as u64;
    let centers: [(u64, u64, &str); 4] = [
        (a0.saturating_sub(0x80), 0x100, "a0"),
        (a5.saturating_sub(0x80), 0x100, "a5"),
        (s0.saturating_sub(0x40), 0x80, "s0"),
        (sp, 0x200, "sp"),
    ];
    let mut done: [u64; 4] = [u64::MAX; 4];
    for (slot, (start, len, tag)) in centers.iter().enumerate() {
        let start = *start & !0xf;
        if done[..slot].contains(&start) {
            continue;
        }
        done[slot] = start;
        dump_user_hex::<P>(aspace, start, *len as usize, tag);
    }
}

fn dump_user_hex<P: TxPlatform>(aspace: &AddressSpace, start: u64, len: usize, tag: &str) {
    let mut off = 0usize;
    while off < len {
        let line_addr = start + off as u64;
        let mut buf = [0u8; 16];
        let ok = tx_shims::linux_syscall::probe_copy_from_user(aspace, line_addr, &mut buf);
        tx_hal::console_write_str::<P>("txkernel:mem:");
        tx_hal::console_write_str::<P>(tag);
        tx_hal::console_write_str::<P>(":0x");
        write_hex_u64::<P>(line_addr);
        tx_hal::console_write_str::<P>(":");
        if ok {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut text = [0u8; 32];
            for (i, b) in buf.iter().enumerate() {
                text[i * 2] = HEX[(b >> 4) as usize];
                text[i * 2 + 1] = HEX[(b & 0xf) as usize];
            }
            tx_hal::console_write_str::<P>(core::str::from_utf8(&text).unwrap_or("?"));
        } else {
            tx_hal::console_write_str::<P>("<unmapped>");
        }
        tx_hal::console_write_str::<P>("\n");
        off += 16;
    }
}

/// PROBE(proxy-push segv hunt): dump the entire recipe table (user mmap map)
/// so stack code pointers can be attributed to their libraries offline.
fn dump_all_recipes<P: TxPlatform>(aspace: &AddressSpace) {
    let recipes = aspace.recipes_snapshot();
    for entry in recipes.iter() {
        log_recipe::<P>("map", entry);
    }
}

#[cfg(test)]
mod tests;
