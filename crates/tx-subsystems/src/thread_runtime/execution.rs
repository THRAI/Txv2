//! Thread-runtime execution: thread-exit step, signal-mask updates,
//! and the internal helpers used by the signal shim.

use core::sync::atomic::{AtomicU64, Ordering};

use tx_hal::{UserPtr, UserTrapContext};

use crate::thread_runtime::adapter::step_engine::{
    self, Cap, MailboxEvent, OneShotStepOp, OperationalCapExt, PayloadCap, SignalRouting,
};

use crate::futex::step_futex_lifecycle_wake_in;
use crate::signal::{refresh_deliverable_signal_summary, SignalMask, Signum};
use crate::thread_runtime::structure::{
    drain_pending_syscall_return, ThreadIdentity, ThreadPayload,
};
// UserAccessIf trait not needed — copy_to_user is inherent on AddressSpace

static CLEAR_CHILD_TID_WAKE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static THREAD_EXIT_PHASE_SAMPLE: AtomicU64 = AtomicU64::new(0);

fn clear_child_tid_debug_sample() -> bool {
    let n = CLEAR_CHILD_TID_WAKE_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
    n <= 128 || n % 256 == 0
}

fn emit_clear_child_tid_debug(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

fn thread_exit_debug_sample() -> bool {
    let n = THREAD_EXIT_PHASE_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
    n <= 128 || n % 256 == 0
}

fn emit_thread_exit_debug(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

/// Best-effort `MailboxEvent::SignalDelivered` post to a thread
/// payload's bound mailbox per D9-A.
///
/// Silently no-ops when no mailbox is bound (early bring-up) or
/// when the `Weak::upgrade` fails because the reactor task has
/// already dropped its `TaskMailbox`. Idempotent — duplicate posts
/// queue up but the future's poll consults `InterruptSummary`,
/// which is the truth-bearing path.
pub(crate) fn post_signal_mailbox(payload: &ThreadPayload, signum: Signum, routing: SignalRouting) {
    let Some(weak) = payload.mailbox_handle() else {
        return;
    };
    let Some(mailbox) = weak.upgrade() else {
        return;
    };
    let _ = mailbox.post(MailboxEvent::SignalDelivered {
        signum: signum.raw() as u32,
        routing,
    });
}

/// Mark a thread zombie: set its exit status, drop its payload. Does
/// not touch the parent process's thread list — callers that need
/// parent-side bookkeeping (e.g. `step_thread_exit`) do that
/// themselves; callers that already hold the parent payload (e.g.
/// `process::step_exit_group`) skip it.
///
/// **D9-A.** Before dropping the payload, post a `SignalDelivered`
/// wake-hint with signum `SIGKILL` and routing `ProcessDirected` to
/// the bound mailbox so a parked future re-polls and observes
/// `summary.termination` / the payload-dropped state instead of
/// sitting idle until some unrelated channel fires. Silently no-op
/// when no mailbox is bound (D9-A's early-bring-up invariant). The
/// summary side of "termination" itself is *not* mutated here —
/// `step_exit_group_with_signal` is the canonical site for the
/// termination bit; D9-A only adds the wake-hint side.
pub(crate) fn set_thread_zombie(thread: &Cap<ThreadIdentity>, status: i32) {
    // Post the wake-hint *before* dropping the payload so the
    // `mailbox` slot is still readable. If the payload is already
    // gone (idempotent double-zombify), `payload.lock()` returns
    // `None` and we skip the post — the future has already had its
    // last chance to observe state.
    if let Some(payload) = thread.payload.lock().as_ref() {
        post_signal_mailbox(payload, Signum::SIGKILL, SignalRouting::ProcessDirected);
    }
    *thread.exit_status.lock() = Some(status);
    *thread.payload.lock() = None;
}

/// Test-only: mark a thread as a zombie **without** removing it
/// from its parent's thread list. The D9-B eligibility-scan pin
/// (`crates/tx-subsystems/tests/v3_signal_eligibility.rs`) uses this
/// to construct a process whose `payload.threads` includes a
/// zombie entry the scan must skip; the shipping
/// [`step_thread_exit`] removes the zombie from the list entirely
/// (so the scan can't see it).
///
/// Hidden behind `cfg(any(test, feature = "test-support"))` so it
/// never reaches release builds.
#[cfg(any(test, feature = "test-support"))]
pub fn mark_thread_zombie_for_test(thread: &Cap<ThreadIdentity>, status: i32) {
    set_thread_zombie(thread, status);
}

/// Single-thread exit. Marks the thread zombie, removes it from the
/// owning process's thread list, and zombifies the process if this was
/// the last thread.
pub fn step_thread_exit(thread: Cap<ThreadIdentity>, status: i32) {
    let mut group_exit_completed = false;
    let trace = thread_exit_debug_sample();
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.enter", thread.tid.0 as i64);
    }
    // Snapshot clear_child_tid and robust-list BEFORE
    // set_thread_zombie drops the thread payload.
    let ctid = thread
        .payload
        .lock()
        .as_ref()
        .and_then(|p| *p.clear_child_tid.lock());
    let robust = thread.payload.lock().as_ref().and_then(|p| {
        let head = *p.robust_list_head.lock();
        let len = *p.robust_list_len.lock();
        head.map(|h| (h, len))
    });
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.snapshot.after", thread.tid.0 as i64);
    }

    // observe
    // upgrade
    // reserve
    // commit
    // publish
    set_thread_zombie(&thread, status);
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.zombie.after", thread.tid.0 as i64);
    }

    let guard = crate::thread_runtime::adapter::step_engine::guard();
    let Some(parent) = thread.owner_proc.upgrade(&guard) else {
        return;
    };
    drop(guard);
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.parent.after", thread.tid.0 as i64);
    }

    let payload_guard = parent.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return;
    };

    let _removed = payload.threads.detach(&thread);
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.retain.after", thread.tid.0 as i64);
    }
    let prev = payload
        .thread_count
        .fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
    let new_count = prev.saturating_sub(1);
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.count.after", thread.tid.0 as i64);
    }

    // GroupExit coordination (PROCESS_v1 §5): if the owning process
    // has an active group-exit episode (exit_group or multi-threaded
    // execve), decrement the remaining_threads counter. The
    // initiating thread is not counted here.
    if let Some(ref ge) = *payload.group_exit.lock() {
        let prev_remaining = ge
            .remaining_threads
            .fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
        if prev_remaining == 1 {
            group_exit_completed = true;
        }
    }

    let was_last = new_count == 0;
    drop(payload_guard);

    if was_last {
        // Thread side carries `i32` per `THREAD_RUNTIME_v1` §7.2;
        // the cascade promotes that to `ExitStatus::Exited` because
        // signal-driven termination doesn't reach this path (it goes
        // through `step_exit_group_with_signal` which records
        // `ExitStatus::Signaled` directly before zombifying threads).
        crate::process::execution::step_process_exit(
            &parent,
            crate::process::structure::ExitStatus::Exited(status),
        );
    }

    if group_exit_completed {
        if let Some(payload) = parent.payload.lock().as_ref() {
            *payload.group_exit.lock() = None;
        }
    }

    // clear_child_tid futex protocol (CLONE_CHILD_CLEARTID).
    // Linux semantics: atomically write 0 to *ctid, then
    // FUTEX_WAKE on the same address. We do both best-effort —
    // if the userspace page is unmapped, skip the write but
    // still fire the wake (hash-bucket wake is unconditional).
    if let Some(ctid_ptr) = ctid {
        let guard = crate::thread_runtime::adapter::step_engine::guard();
        // Zero the word at *ctid_ptr in userspace.
        if let Some(proc) = thread.owner_proc.upgrade(&guard) {
            if let Some(payload) = proc.payload.lock().as_ref() {
                let aspace = payload.aspace_cap();
                clear_and_wake_child_tid(&aspace, ctid_ptr, &guard);
            }
        }
        drop(guard);
    }
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.ctid.after", thread.tid.0 as i64);
    }

    // robust-list walk: mark each robust futex as FUTEX_OWNER_DIED
    // and issue FUTEX_WAKE. Best-effort — if the userspace pages
    // are unmapped or the list is malformed, skip the entry.
    if let Some((head, _len)) = robust {
        walk_robust_list(&thread, head, 16);
    }
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.robust.after", thread.tid.0 as i64);
    }

    if thread
        .owner_proc
        .upgrade(&crate::thread_runtime::adapter::step_engine::guard())
        .map(|proc| proc.pid.0 != thread.tid.0)
        .unwrap_or(true)
    {
        crate::process::numbers::unregister_tid_number(thread.tid.0 as u64);
    }
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.end", thread.tid.0 as i64);
    }
}

fn clear_and_wake_child_tid(
    aspace: &crate::vm::AddressSpace,
    tid_ptr: u64,
    guard: &step_engine::Guard<'_>,
) {
    let _ = aspace.copy_to_user(UserPtr::<u8>::new(tid_ptr as usize), &[0u8; 4], guard);
    let trace = clear_child_tid_debug_sample();
    if trace {
        emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.uaddr", tid_ptr as i64);
    }
    match step_futex_lifecycle_wake_in(aspace, tid_ptr, 1, guard) {
        step_engine::StepOutcome::Done(woken) => {
            if trace {
                emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.woken", i64::from(woken));
            }
        }
        step_engine::StepOutcome::Err(errno) => {
            if trace {
                emit_clear_child_tid_debug(
                    b"debug.futex.clear_child_tid.err",
                    i64::from(errno as i32),
                );
            }
        }
        step_engine::StepOutcome::Yield { .. } | step_engine::StepOutcome::Continue { .. } => {
            if trace {
                emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.pending", 1);
            }
        }
    }
}

/// Best-effort robust-list walk on thread exit.
///
/// Linux's `robust_list_head` layout:
///   head+0:  `list` (pointer to first robust_list entry)
///   head+8:  `futex_offset` (long)
///   head+16: `list_op_pending` (pointer to in-progress entry)
///
/// Each `robust_list` entry:
///   entry+0:      `next` pointer
///   entry+offset: futex word guarded by the mutex
///
/// Walk the list plus `list_op_pending`. Malformed lists are bounded
/// so a corrupt userspace pointer cannot trap the kernel in a loop.
fn walk_robust_list(thread: &Cap<ThreadIdentity>, head: u64, _offset: u64) {
    let guard = step_engine::guard();
    let Some(proc) = thread.owner_proc.upgrade(&guard) else {
        return;
    };
    let proc_guard = proc.payload.lock();
    let Some(payload) = proc_guard.as_ref() else {
        return;
    };
    let aspace = payload.aspace_cap();

    let Some(first) = read_user_u64(&aspace, head, &guard) else {
        return;
    };
    let Some(futex_offset) = read_user_i64(&aspace, head + 8, &guard) else {
        return;
    };
    let Some(pending) = read_user_u64(&aspace, head + 16, &guard) else {
        return;
    };

    let mut entry = first;
    for _ in 0..2048 {
        if entry == 0 || entry == head {
            break;
        }
        mark_robust_entry_owner_died(&aspace, entry, futex_offset, &guard);
        let Some(next) = read_user_u64(&aspace, entry, &guard) else {
            break;
        };
        if next == entry {
            break;
        }
        entry = next;
    }

    if pending != 0 {
        mark_robust_entry_owner_died(&aspace, pending, futex_offset, &guard);
    }

    drop(guard);
}

fn read_user_u64(
    aspace: &crate::vm::AddressSpace,
    addr: u64,
    guard: &step_engine::Guard<'_>,
) -> Option<u64> {
    let mut buf = [0u8; 8];
    match aspace.copy_from_user(&mut buf, UserPtr::<u8>::new(addr as usize), guard) {
        step_engine::StepOutcome::Done(8) => Some(u64::from_ne_bytes(buf)),
        _ => None,
    }
}

fn read_user_i64(
    aspace: &crate::vm::AddressSpace,
    addr: u64,
    guard: &step_engine::Guard<'_>,
) -> Option<i64> {
    read_user_u64(aspace, addr, guard).map(|v| v as i64)
}

fn read_user_u32(
    aspace: &crate::vm::AddressSpace,
    addr: u64,
    guard: &step_engine::Guard<'_>,
) -> Option<u32> {
    let mut buf = [0u8; 4];
    match aspace.copy_from_user(&mut buf, UserPtr::<u8>::new(addr as usize), guard) {
        step_engine::StepOutcome::Done(4) => Some(u32::from_ne_bytes(buf)),
        _ => None,
    }
}

fn mark_robust_entry_owner_died(
    aspace: &crate::vm::AddressSpace,
    entry: u64,
    futex_offset: i64,
    guard: &step_engine::Guard<'_>,
) {
    const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
    const FUTEX_WAITERS: u32 = 0x8000_0000;

    let futex_addr = (entry as i64).wrapping_add(futex_offset) as u64;
    let old = read_user_u32(aspace, futex_addr, guard).unwrap_or(0);
    let new = (old & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
    let _ = aspace.copy_to_user(
        UserPtr::<u8>::new(futex_addr as usize),
        &new.to_ne_bytes(),
        guard,
    );
    step_futex_lifecycle_wake_in(aspace, futex_addr, 1, guard);
}

/// Outcome of `step_sigprocmask`. `Replaced` is the normal path;
/// `ZombieIgnored` means the target had no payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigprocmaskChange {
    Replaced { prev: SignalMask, new: SignalMask },
    ZombieIgnored,
}

/// How [`step_sigprocmask`] combines `next` with the current mask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigmaskHow {
    /// Replace the mask with `next`.
    SetMask,
    /// Block additional signals: `new = prev | next`.
    Block,
    /// Unblock signals: `new = prev & !next`.
    Unblock,
}

/// Update the per-thread signal mask. Uncatchable signals
/// (`SIGKILL`, `SIGSTOP`) are stripped automatically by [`SignalMask`].
/// Per `THREAD_RUNTIME_v1` §5.2, also recomputes `signal_summary.deliverable_signal`
/// against the new mask.
pub fn step_sigprocmask(
    thread: &Cap<ThreadIdentity>,
    how: SigmaskHow,
    next: SignalMask,
) -> SigprocmaskChange {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Ok(payload) = thread.upgrade_operational() else {
        return SigprocmaskChange::ZombieIgnored;
    };
    let prev_bits = payload.signal_mask.load(Ordering::Acquire);
    let prev = SignalMask::new(prev_bits);
    let new_bits = match how {
        SigmaskHow::SetMask => next.raw_bits(),
        SigmaskHow::Block => prev_bits | next.raw_bits(),
        SigmaskHow::Unblock => prev_bits & !next.raw_bits(),
    };
    let new = SignalMask::new(new_bits);
    if new.raw_bits() == prev.raw_bits() {
        return SigprocmaskChange::Replaced { prev, new };
    }

    payload.signal_mask.store(new.raw_bits(), Ordering::Release);

    refresh_deliverable_signal_summary(thread);

    SigprocmaskChange::Replaced { prev, new }
}

/// Post a single catchable signal to a thread's pending queue.
/// No-op if the thread is a zombie.
///
/// **Precondition**: `sig` is *not* a Gewalt signum
/// (SIGKILL/SIGSTOP/SIGCONT) — those bypass pending queues entirely
/// per `SIGNAL_v1` §1, §2 Consequence 2 and route through
/// `signal::route_gewalt` which updates `signal_summary` directly
/// without enqueueing.
///
/// Sets `signal_summary.deliverable_signal` when the posted signal
/// is unmasked, per `THREAD_RUNTIME_v1` §5.2. SIGTSTP-family
/// (SIGTSTP, SIGTTIN, SIGTTOU) is *catchable* and goes through this
/// path; the default action is `Stop` which `ast_check` recognises,
/// but the bit only sets `summary.stop_requested` once a real
/// stop-state machine consumes it (deferred). For now SIGTSTP-family
/// posts behave exactly like any other catchable signal at this
/// layer — the stop intent is materialised by `ast_check` returning
/// `DefaultStop`, not by a summary bit set here.
pub fn post_signal(
    thread: &Cap<ThreadIdentity>,
    sig: Signum,
    routing: SignalRouting,
    info: Option<crate::signal::SigInfo>,
) {
    debug_assert!(
        !matches!(sig, Signum::SIGKILL | Signum::SIGSTOP | Signum::SIGCONT),
        "post_signal must not be called with Gewalt signums (SIGKILL/SIGSTOP/SIGCONT); \
         use signal::route_gewalt or signal::step_kill_process which dispatches"
    );

    let Ok(payload) = thread.upgrade_operational() else {
        return;
    };
    payload.pending().post(sig);

    // Phase I (SigInfo): store siginfo on the owning process.
    if let Some(info) = info {
        let guard = step_engine::guard();
        if let Some(proc) = thread.owner_proc.upgrade(&guard) {
            drop(guard);
            if let Ok(proc_payload) = proc.upgrade_operational() {
                proc_payload.siginfo_slots.store(sig, info);
            }
        }
    }

    let mask = payload.signal_mask();
    if !mask.is_blocked(sig) {
        payload.update_summary(|s| s.deliverable_signal = true);
    }

    // D9-A: post the wake-hint to the thread's mailbox *after* the
    // summary update so a parked future, on re-poll, observes the
    // same summary bit the post advertises.
    post_signal_mailbox(&payload, sig, routing);
}

// ---------------------------------------------------------------------------
// Userspace-entry shim (Plan B writeback discipline)
// ---------------------------------------------------------------------------

/// Register index of the Linux ABI `a0` argument/return register inside
/// [`UserTrapContext::regs`]. The HAL stores user GPRs at the same
/// indices the architecture uses.
#[cfg(target_arch = "loongarch64")]
pub const USER_CONTEXT_A0_INDEX: usize = 4;
#[cfg(target_arch = "riscv64")]
pub const USER_CONTEXT_A0_INDEX: usize = 10;
#[cfg(not(any(target_arch = "loongarch64", target_arch = "riscv64")))]
pub const USER_CONTEXT_A0_INDEX: usize = 10;

/// Build the merged [`UserTrapContext`] the HAL's
/// `TrapIf::enter_userspace_with_context` consumes on the next userspace
/// re-entry.
///
/// Per Plan B writeback discipline pinned by
/// `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
/// (`docs/design/02_execution/THREAD_RUNTIME_v1.md`) this function is
/// the **only** site that drains `pending_syscall_return` and the
/// **only** site that produces the merged context for userspace
/// re-entry. The trap shell never writes to the trapping frame. The
/// platform's `enter_userspace_with_context` then materialises a fresh
/// trap frame from the returned `UserTrapContext` before `sret`.
///
/// Behaviour:
/// 1. Snapshots `payload.saved_user_context` (set by the trap shell
///    via `view.capture_user_context()` on entry).
/// 2. Drains `pending_syscall_return` exactly once. `Ok(v)` encodes as
///    `v as u64`; `Err(errno)` encodes as `(-errno as i64) as u64`
///    (Linux ABI: negative errno for failure).
/// 3. Overlays the encoded value into the context's `a0`-equivalent
///    register slot.
/// 4. Clears `payload.active_userspace_request` (the wait was resolved
///    on the previous trap; the next iteration re-issues
///    `start_request`).
///
/// **Panics** if no `saved_user_context` is recorded — that means
/// userspace re-entry was attempted before any trap captured a baseline
/// context, which is a thread-future invariant violation.
pub fn prepare_userspace_entry_payload_into(
    payload: &PayloadCap<ThreadPayload>,
    out: &mut UserTrapContext,
) {
    let mut ctx = payload
        .saved_user_context()
        .expect("prepare_userspace_entry_payload: no saved_user_context recorded");

    if let Some(result) = drain_pending_syscall_return(payload) {
        let encoded = match result {
            Ok(v) => v as u64,
            Err(errno) => (-i64::from(errno)) as u64,
        };
        ctx.regs[USER_CONTEXT_A0_INDEX] = encoded as usize;
    }

    payload.set_active_userspace_request(None);

    *out = ctx;
}

pub fn prepare_userspace_entry_payload(payload: &PayloadCap<ThreadPayload>) -> UserTrapContext {
    let mut ctx = UserTrapContext::empty();
    prepare_userspace_entry_payload_into(payload, &mut ctx);
    ctx
}

// -- PR-2 StepOp wraps -------------------------------------------------
//
// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1, PR-2 wraps each free
// `step_*` fn in an `impl StepOp for FooOp` shell. These thread-runtime
// mutators don't take an epoch `Guard`, so no lifetime parameter is
// needed. `step_thread_exit` consumes its `Cap` by value (the wrap
// follows suit and clones internally so `step()`'s `&mut self` can
// re-run if needed); `step_sigprocmask` borrows `&Cap` (the wrap
// stores `Cap` by value per the cred-pilot convention).

/// `StepOp` wrap for [`step_thread_exit`]. PR-2 wave 2.
///
/// `step_thread_exit` returns `()`; the wrap lifts that into
/// `StepOutcome::Done(())`. The `Cap` is stored by value (`Cap` is
/// `Clone`) and the wrap clones into the free fn so the `step` method
/// remains `&mut self`-shaped.
pub struct ThreadExitOp {
    pub thread: Cap<ThreadIdentity>,
    pub status: i32,
}

impl<I: crate::thread_runtime::adapter::step_engine::SubjectIdentity>
    crate::thread_runtime::adapter::step_engine::StepOp<I> for ThreadExitOp
{
    type Output = ();
    type Progress = crate::thread_runtime::adapter::step_engine::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut crate::thread_runtime::adapter::step_engine::ScriptCtx<I>,
    ) -> crate::thread_runtime::adapter::step_engine::StepOutcome<Self::Output, Self::Progress>
    {
        step_thread_exit(self.thread.clone(), self.status);
        crate::thread_runtime::adapter::step_engine::StepOutcome::Done(())
    }
}

impl OneShotStepOp<ProcessIdentity> for ThreadExitOp {}

/// `StepOp` wrap for [`step_sigprocmask`]. PR-2 wave 2.
pub struct SigprocmaskOp {
    pub thread: Cap<ThreadIdentity>,
    pub how: SigmaskHow,
    pub next: SignalMask,
}

impl<I: crate::thread_runtime::adapter::step_engine::SubjectIdentity>
    crate::thread_runtime::adapter::step_engine::StepOp<I> for SigprocmaskOp
{
    type Output = SigprocmaskChange;
    type Progress = crate::thread_runtime::adapter::step_engine::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut crate::thread_runtime::adapter::step_engine::ScriptCtx<I>,
    ) -> crate::thread_runtime::adapter::step_engine::StepOutcome<Self::Output, Self::Progress>
    {
        crate::thread_runtime::adapter::step_engine::StepOutcome::Done(step_sigprocmask(
            &self.thread,
            self.how,
            self.next,
        ))
    }
}

use crate::process::ProcessIdentity;
impl OneShotStepOp<ProcessIdentity> for SigprocmaskOp {}

/// StepOp wrapper for [`post_signal`] — per-thread signal delivery
/// (used by `tkill` and `tgkill`).
pub struct ThreadKillOp {
    pub thread: Cap<ThreadIdentity>,
    pub sig: Signum,
    pub info: Option<crate::signal::SigInfo>,
}

impl<I: crate::thread_runtime::adapter::step_engine::SubjectIdentity>
    crate::thread_runtime::adapter::step_engine::StepOp<I> for ThreadKillOp
{
    type Output = ();
    type Progress = crate::thread_runtime::adapter::step_engine::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut crate::thread_runtime::adapter::step_engine::ScriptCtx<I>,
    ) -> crate::thread_runtime::adapter::step_engine::StepOutcome<
        (),
        crate::thread_runtime::adapter::step_engine::NoProgress,
    > {
        // observe — thread Cap + sig validated by post_signal internally
        // upgrade — N/A
        // reserve — N/A
        // commit — post_signal delivers to thread's pending queue
        // publish — post_signal sends MailboxEvent if mailbox bound
        let routing = SignalRouting::ThreadDirected {
            tid: self.thread.tid.0 as u64,
        };
        post_signal(&self.thread, self.sig, routing, self.info);
        crate::thread_runtime::adapter::step_engine::StepOutcome::Done(())
    }
}

impl OneShotStepOp<ProcessIdentity> for ThreadKillOp {}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 wave-2 `StepOp` wrap tests for the thread-runtime
    //! mutators. Each test exercises one wrap against a
    //! `bootstrap_init_process`-minted cap, confirming the wrap
    //! delegates to the free fn and lifts the result into
    //! `StepOutcome::Done`. The free-fn semantics themselves are
    //! covered by the existing `thread_runtime::tests` and
    //! `signal::tests` modules.
    use super::*;
    use crate::process::bootstrap_init_process;
    use crate::process::structure::reset_pid_counter_for_test;
    use crate::signal::Signum;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::thread_runtime::adapter::step_engine::{Cap, ScriptCtx, StepOp, StepOutcome};
    use crate::thread_runtime::structure::reset_tid_counter_for_test;
    use crate::vm::{AddressSpace, TestPmap};
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        reset_pid_counter_for_test();
        reset_tid_counter_for_test();
        crate::process::execution::reset_init_process_for_test();
        guard
    }

    fn fresh_aspace() -> Cap<AddressSpace> {
        AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
    }

    fn first_thread(
        proc_cap: &Cap<crate::process::structure::ProcessIdentity>,
    ) -> Cap<ThreadIdentity> {
        let payload_guard = proc_cap.payload.lock();
        let payload = payload_guard.as_ref().expect("alive");
        let threads = payload.threads.snapshot();
        threads[0].clone()
    }

    #[test]
    fn thread_exit_op_delegates_to_step_thread_exit() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let leader = first_thread(&proc_cap);
        assert!(!leader.is_zombie());
        let mut op = ThreadExitOp {
            thread: leader.clone(),
            status: 7,
        };
        let mut ctx = ScriptCtx::<
            crate::thread_runtime::adapter::step_engine::PlaceholderProcessSubject,
        >::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
        assert!(leader.is_zombie());
        assert_eq!(leader.exit_status(), Some(7));
    }

    #[test]
    fn sigprocmask_op_delegates_to_step_sigprocmask() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let leader = first_thread(&proc_cap);
        let mut next = SignalMask::EMPTY;
        next.block(Signum::SIGTERM);
        let mut op = SigprocmaskOp {
            thread: leader.clone(),
            how: SigmaskHow::SetMask,
            next,
        };
        let mut ctx = ScriptCtx::<
            crate::thread_runtime::adapter::step_engine::PlaceholderProcessSubject,
        >::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(SigprocmaskChange::Replaced { prev, new }) => {
                assert_eq!(prev, SignalMask::EMPTY);
                assert!(new.is_blocked(Signum::SIGTERM));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }
}
