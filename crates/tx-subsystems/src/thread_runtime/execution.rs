//! Thread-runtime execution: thread-exit step, signal-mask updates,
//! and the internal helpers used by the signal shim.

use alloc::sync::Weak as ArcWeak;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_hal::{UserPtr, UserTrapContext};

use crate::thread_runtime::adapter::step_engine::{
    self, Cap, MailboxEvent, OneShotStepOp, OperationalCapExt, PayloadCap, SignalRouting,
    TaskMailbox,
};
use tx_substrate::wake::MailboxSchedulerHint;

use crate::futex::step_futex_lifecycle_wake_in;
use crate::signal::{refresh_deliverable_signal_summary_with_payload, SignalMask, Signum};
use crate::thread_runtime::structure::{
    clear_thread_slots_for, drain_pending_syscall_return, ThreadIdentity, ThreadPayload,
};
// UserAccessIf trait not needed — copy_to_user is inherent on AddressSpace

static CLEAR_CHILD_TID_WAKE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static THREAD_EXIT_PHASE_SAMPLE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct ThreadExitUserCleanup {
    ctid: Option<u64>,
    ctid_cleared: bool,
    robust: Option<(u64, usize)>,
}

fn snapshot_thread_exit_user_cleanup(thread: &Cap<ThreadIdentity>) -> ThreadExitUserCleanup {
    let payload_guard = thread.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return ThreadExitUserCleanup {
            ctid: None,
            ctid_cleared: false,
            robust: None,
        };
    };
    let ctid = *payload.clear_child_tid.lock();
    let robust = {
        let head = *payload.robust_list_head.lock();
        let len = *payload.robust_list_len.lock();
        head.map(|h| (h, len))
    };
    ThreadExitUserCleanup {
        ctid,
        ctid_cleared: false,
        robust,
    }
}

fn step_thread_exit_user_cleanup(
    aspace: &crate::vm::AddressSpace,
    cleanup: &mut ThreadExitUserCleanup,
) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
    // This cleanup can run from a context that already holds an epoch guard
    // (e.g. a fatal signal that tears the process down mid-syscall reaches
    // the group-exit transition while the delivering path's guard is still active).
    // Creating a fresh `guard()` there trips the EBR no-nesting assertion, so
    // borrow the active guard when one exists and only open a new one otherwise.
    let guard = crate::thread_runtime::adapter::step_engine::borrow_current_guard()
        .unwrap_or_else(crate::thread_runtime::adapter::step_engine::guard);
    if let Some(ctid_ptr) = cleanup.ctid {
        match step_clear_and_wake_child_tid(aspace, ctid_ptr, &mut cleanup.ctid_cleared, &guard) {
            step_engine::StepOutcome::Done(()) => cleanup.ctid = None,
            step_engine::StepOutcome::Continue { progress } => {
                return step_engine::StepOutcome::Continue { progress };
            }
            step_engine::StepOutcome::Yield { progress, shape } => {
                return step_engine::StepOutcome::Yield { progress, shape };
            }
            step_engine::StepOutcome::Err(errno) => {
                return step_engine::StepOutcome::Err(errno);
            }
        }
    }
    if let Some((head, len)) = cleanup.robust.take() {
        walk_robust_list_in_aspace(aspace, head, len, &guard);
    }
    step_engine::StepOutcome::Done(())
}

pub(crate) const THREAD_RUNTIME_LOCK_SERVICE_TRACE_NAMES: &[&[u8]] = &[
    b"debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns",
    b"debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns",
    b"debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns",
    b"debug.lock_service.thread.payload.sigprocmask.payload_missing",
    b"debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns",
    b"debug.lock_service.thread.payload.sigprocmask.mask_noop",
    b"debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns",
    b"debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns",
];

#[inline(always)]
fn emit_thread_lock_service_trace(name: &'static [u8], value: i64) {
    let known_names = THREAD_RUNTIME_LOCK_SERVICE_TRACE_NAMES;
    debug_assert!(known_names.contains(&name));
    #[cfg(tx_sigprocmask_phase_metrics)]
    {
        if let Some(observer) = tx_observe::current() {
            observer.debug_counter(name, value);
        }
    }
    #[cfg(not(tx_sigprocmask_phase_metrics))]
    {
        let _ = value;
    }
}

#[inline(always)]
fn measure_thread_lock_service<R>(name: &'static [u8], f: impl FnOnce() -> R) -> R {
    let known_names = THREAD_RUNTIME_LOCK_SERVICE_TRACE_NAMES;
    debug_assert!(known_names.contains(&name));
    #[cfg(tx_sigprocmask_phase_metrics)]
    {
        let start = tx_observe::clock_now_ns();
        let result = f();
        let duration = tx_observe::clock_now_ns().saturating_sub(start);
        emit_thread_lock_service_trace(name, duration.min(i64::MAX as u64) as i64);
        result
    }
    #[cfg(not(tx_sigprocmask_phase_metrics))]
    {
        f()
    }
}

fn clear_child_tid_debug_sample() -> bool {
    let n = CLEAR_CHILD_TID_WAKE_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
    n <= 128 || n % 256 == 0
}

fn emit_clear_child_tid_debug(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

fn thread_exit_debug_sample() -> bool {
    let n = THREAD_EXIT_PHASE_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
    n <= 128 || n % 256 == 0
}

fn emit_thread_exit_debug(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

/// Build and publish a `MailboxEvent::SignalDelivered` wake hint
/// through a caller-provided mailbox post operation.
///
/// Signal code owns the semantic decision that a target thread should
/// observe a signal. The caller owns the wake-delivery mechanism: plain
/// subsystem paths pass a direct mailbox post, while reactor contexts
/// can inject owner-aware posting so scheduler placement is resolved
/// from the current task owner.
///
/// Returns `false` when no mailbox is bound. A bound but already-dropped
/// mailbox is handed to `post`; the injected operation decides whether
/// `Weak::upgrade` failure is observable or silently skipped.
pub fn post_signal_mailbox_with_post<F>(
    payload: &ThreadPayload,
    signum: Signum,
    routing: SignalRouting,
    mut post: F,
) -> bool
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    let Some(weak) = payload.mailbox_handle() else {
        return false;
    };
    post(
        weak,
        MailboxEvent::SignalDelivered {
            signum: signum.raw() as u32,
            routing,
        },
    );
    true
}

/// Mark a thread zombie: set its exit status, drop its payload. Does
/// not touch the parent process's thread list — callers that need
/// parent-side bookkeeping (e.g. `step_thread_exit`) do that
/// themselves; callers that already hold the parent payload (e.g.
/// group-exit transition) skip it.
///
/// **D9-A.** Before dropping the payload, post a `SignalDelivered`
/// wake-hint with signum `SIGKILL` and routing `ProcessDirected` to
/// the bound mailbox so a parked future re-polls and observes
/// `summary.termination` / the payload-dropped state instead of
/// sitting idle until some unrelated channel fires. Silently no-op
/// when no mailbox is bound (D9-A's early-bring-up invariant). The
/// summary side of "termination" itself is *not* mutated here —
/// the fatal group-exit transition is the canonical site for the
/// termination bit; D9-A only adds the wake-hint side.
pub(crate) fn set_thread_zombie_with_post<F>(thread: &Cap<ThreadIdentity>, status: i32, mut post: F)
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    // Post the wake-hint *before* dropping the payload so the
    // `mailbox` slot is still readable. If the payload is already
    // gone (idempotent double-zombify), `payload.lock()` returns
    // `None` and we skip the post — the future has already had its
    // last chance to observe state.
    let mut payload_guard = thread.payload.lock();
    if let Some(payload) = payload_guard.as_ref() {
        // Lifecycle termination must wake the outer thread task even when a
        // nested futex/I/O wait has replaced or cleared the mailbox waker.
        payload.wake_lifecycle_task();
        let _ = post_signal_mailbox_with_post(
            payload,
            Signum::SIGKILL,
            SignalRouting::ProcessDirected,
            &mut post,
        );
        clear_thread_slots_for(thread, payload);
    }
    *thread.exit_status.lock() = Some(status);
    *payload_guard = None;
}

pub(crate) fn set_thread_zombie(thread: &Cap<ThreadIdentity>, status: i32) {
    set_thread_zombie_with_post(thread, status, |weak, event| {
        let Some(mailbox) = weak.upgrade() else {
            return;
        };
        let _ = mailbox.post(event);
    });
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadExitOutcome {
    Completed,
    Retry,
}

pub type ThreadExitSignalPost = fn(ArcWeak<TaskMailbox>, MailboxEvent);
pub type ThreadExitWakePost = fn(&TaskMailbox, MailboxEvent) -> bool;
pub type ThreadExitWakePostWithHint = fn(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool;

/// Owner-aware wake routes retained by a resumable thread-exit operation.
///
/// The last-thread cascade can wake a parent parked on `wait4(2)`. Keeping
/// these callbacks with the exit state ensures that publication still goes
/// through the reactor's cross-hart route after the originating syscall
/// future has returned its terminal intent.
#[derive(Clone, Copy, Default)]
pub struct ThreadExitPosts {
    signal: Option<ThreadExitSignalPost>,
    wake: Option<ThreadExitWakePost>,
    wake_with_hint: Option<ThreadExitWakePostWithHint>,
}

impl ThreadExitPosts {
    pub const fn new(
        signal: Option<ThreadExitSignalPost>,
        wake: Option<ThreadExitWakePost>,
        wake_with_hint: Option<ThreadExitWakePostWithHint>,
    ) -> Self {
        Self {
            signal,
            wake,
            wake_with_hint,
        }
    }

    fn post_signal(self, mailbox: ArcWeak<TaskMailbox>, event: MailboxEvent) {
        if let Some(post) = self.signal {
            post(mailbox, event);
            return;
        }
        let Some(mailbox) = mailbox.upgrade() else {
            return;
        };
        let _ = mailbox.post(event);
    }

    fn post_wake(self, mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
        if let Some(post) = self.wake_with_hint {
            // A thread-exit publication releases lifecycle waiters such as a
            // parent blocked in wait4/pclose.  Treating this as ordinary work
            // lets the SMP scheduler defer the parent indefinitely behind
            // CPU-heavy runnable tasks, even though the child has already
            // reached its final-thread cascade.
            return post(mailbox, event, MailboxSchedulerHint::LifecycleWake);
        }
        if let Some(post) = self.wake {
            return post(mailbox, event);
        }
        mailbox.post(event)
    }
}

struct PreparedThreadExit {
    parent: Option<Cap<crate::process::ProcessIdentity>>,
    process_payload: Option<PayloadCap<crate::process::ProcessPayload>>,
    aspace: Option<Cap<crate::vm::AddressSpace>>,
    exit_permit: Option<crate::process::structure::ThreadExitPermit>,
    cleanup: ThreadExitUserCleanup,
    thread_status: i32,
    process_status: crate::process::structure::ExitStatus,
    trace: bool,
}

enum PrepareThreadExitOutcome {
    Completed,
    Retry,
    Prepared(PreparedThreadExit),
}

fn prepare_thread_exit_state<H>(
    thread: &Cap<ThreadIdentity>,
    requested_thread_status: i32,
    requested_process_status: crate::process::structure::ExitStatus,
    after_lane_check: H,
) -> PrepareThreadExitOutcome
where
    H: FnOnce(),
{
    if thread.is_zombie() {
        return PrepareThreadExitOutcome::Completed;
    }
    let parent = {
        let guard = crate::thread_runtime::adapter::step_engine::guard();
        let parent = thread.owner_proc.upgrade(&guard);
        drop(guard);
        parent
    };
    // Pin the process payload and its current address space before claiming
    // the lifecycle lane. Both must remain stable while cleanup is suspended.
    let process_payload = parent
        .as_ref()
        .and_then(|parent| parent.payload.lock().as_ref().cloned());
    let exit_permit = if let Some(payload) = process_payload.as_ref() {
        match payload.prepare_thread_exit(thread.tid.0, requested_process_status) {
            Some(permit) => Some(permit),
            None => return PrepareThreadExitOutcome::Retry,
        }
    } else {
        None
    };
    let (thread_status, process_status) = exit_permit
        .as_ref()
        .map(|permit| {
            (
                permit.thread_status(requested_thread_status),
                permit.status(),
            )
        })
        .unwrap_or((requested_thread_status, requested_process_status));
    after_lane_check();
    let trace = thread_exit_debug_sample();
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.enter", thread.tid.0 as i64);
    }
    let (aspace, cleanup) = if let Some(payload) = process_payload.as_ref() {
        (
            Some(payload.aspace_cap()),
            snapshot_thread_exit_user_cleanup(thread),
        )
    } else {
        (
            None,
            ThreadExitUserCleanup {
                ctid: None,
                ctid_cleared: false,
                robust: None,
            },
        )
    };
    PrepareThreadExitOutcome::Prepared(PreparedThreadExit {
        parent,
        process_payload,
        aspace,
        exit_permit,
        cleanup,
        thread_status,
        process_status,
        trace,
    })
}

fn finish_prepared_thread_exit_with_posts<Z, F, G>(
    thread: &Cap<ThreadIdentity>,
    prepared: PreparedThreadExit,
    after_zombify: Z,
    signal_post: &mut F,
    wake_post: &mut G,
) where
    Z: FnOnce(),
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    let PreparedThreadExit {
        parent,
        process_payload,
        exit_permit,
        thread_status,
        process_status,
        trace,
        ..
    } = prepared;
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.snapshot.after", thread.tid.0 as i64);
    }

    set_thread_zombie(thread, thread_status);
    after_zombify();
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.zombie.after", thread.tid.0 as i64);
    }

    let Some(parent) = parent else {
        return;
    };
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.parent.after", thread.tid.0 as i64);
    }
    let Some(payload) = process_payload else {
        return;
    };

    let removed = crate::process::execution::measure_process_lock_service(
        b"debug.lock_service.process.payload.thread_exit.threads_detach.duration_ns",
        || payload.threads.detach(thread),
    );
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.retain.after", thread.tid.0 as i64);
    }
    let new_count = removed.as_ref().and_then(|_| {
        crate::process::execution::measure_process_lock_service(
            b"debug.lock_service.process.payload.thread_exit.thread_count.duration_ns",
            || {
                payload
                    .thread_count
                    .fetch_update(
                        core::sync::atomic::Ordering::AcqRel,
                        core::sync::atomic::Ordering::Acquire,
                        |count| count.checked_sub(1),
                    )
                    .map(|previous| previous - 1)
                    .ok()
            },
        )
    });
    debug_assert!(removed.is_none() || new_count.is_some());
    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.count.after", thread.tid.0 as i64);
    }

    let was_last = new_count == Some(0);
    if let Some(permit) = exit_permit {
        let finished = crate::process::execution::measure_process_lock_service(
            b"debug.lock_service.process.payload.thread_exit.group_exit.duration_ns",
            || payload.finish_thread_exit(permit, was_last),
        );
        debug_assert!(
            finished,
            "thread-exit lifecycle permit remains generation-valid"
        );
    }
    if was_last {
        crate::process::execution::step_process_exit_with_posts(
            &parent,
            process_status,
            signal_post,
            wake_post,
        );
    }

    if trace {
        emit_thread_exit_debug(b"debug.thread_exit.ctid.after", thread.tid.0 as i64);
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

pub fn step_thread_exit(thread: Cap<ThreadIdentity>, status: i32) -> ThreadExitOutcome {
    let mut op = ThreadExitOp::new(thread, status);
    match op.step_exit() {
        step_engine::StepOutcome::Done(()) => ThreadExitOutcome::Completed,
        step_engine::StepOutcome::Continue { .. }
        | step_engine::StepOutcome::Yield { .. }
        | step_engine::StepOutcome::Err(_) => ThreadExitOutcome::Retry,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn step_thread_exit_with_status_and_posts<F, G>(
    thread: Cap<ThreadIdentity>,
    status: crate::process::structure::ExitStatus,
    signal_post: &mut F,
    wake_post: &mut G,
) -> ThreadExitOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    let mut prepared =
        match prepare_thread_exit_state(&thread, status.wait_status_word(), status, || {}) {
            PrepareThreadExitOutcome::Completed => return ThreadExitOutcome::Completed,
            PrepareThreadExitOutcome::Retry => return ThreadExitOutcome::Retry,
            PrepareThreadExitOutcome::Prepared(prepared) => prepared,
        };
    if let Some(aspace) = prepared.aspace.as_ref() {
        loop {
            match step_thread_exit_user_cleanup(aspace, &mut prepared.cleanup) {
                step_engine::StepOutcome::Done(()) | step_engine::StepOutcome::Err(_) => break,
                step_engine::StepOutcome::Continue { .. }
                | step_engine::StepOutcome::Yield { .. } => core::hint::spin_loop(),
            }
        }
    }
    finish_prepared_thread_exit_with_posts(&thread, prepared, || {}, signal_post, wake_post);
    ThreadExitOutcome::Completed
}

pub fn step_thread_exit_with_status(
    thread: Cap<ThreadIdentity>,
    status: crate::process::structure::ExitStatus,
) -> ThreadExitOutcome {
    let mut op = ThreadExitOp::with_status(thread, status);
    match op.step_exit() {
        step_engine::StepOutcome::Done(()) => ThreadExitOutcome::Completed,
        step_engine::StepOutcome::Continue { .. }
        | step_engine::StepOutcome::Yield { .. }
        | step_engine::StepOutcome::Err(_) => ThreadExitOutcome::Retry,
    }
}

#[cfg(test)]
fn step_thread_exit_inner<H, Z>(
    thread: Cap<ThreadIdentity>,
    status: i32,
    process_status: crate::process::structure::ExitStatus,
    after_lane_check: H,
    after_zombify: Z,
) -> ThreadExitOutcome
where
    H: FnOnce(),
    Z: FnOnce(),
{
    let mut prepared =
        match prepare_thread_exit_state(&thread, status, process_status, after_lane_check) {
            PrepareThreadExitOutcome::Completed => return ThreadExitOutcome::Completed,
            PrepareThreadExitOutcome::Retry => return ThreadExitOutcome::Retry,
            PrepareThreadExitOutcome::Prepared(prepared) => prepared,
        };
    if let Some(aspace) = prepared.aspace.as_ref() {
        loop {
            match step_thread_exit_user_cleanup(aspace, &mut prepared.cleanup) {
                step_engine::StepOutcome::Done(()) | step_engine::StepOutcome::Err(_) => break,
                step_engine::StepOutcome::Continue { .. }
                | step_engine::StepOutcome::Yield { .. } => core::hint::spin_loop(),
            }
        }
    }
    let posts = ThreadExitPosts::default();
    let mut signal_post = |mailbox, event| posts.post_signal(mailbox, event);
    let mut wake_post = |mailbox: &TaskMailbox, event| posts.post_wake(mailbox, event);
    finish_prepared_thread_exit_with_posts(
        &thread,
        prepared,
        after_zombify,
        &mut signal_post,
        &mut wake_post,
    );
    ThreadExitOutcome::Completed
}

#[cfg(test)]
pub(crate) fn step_thread_exit_after_lane_check_for_test<H>(
    thread: Cap<ThreadIdentity>,
    status: i32,
    after_lane_check: H,
) -> ThreadExitOutcome
where
    H: FnOnce(),
{
    step_thread_exit_inner(
        thread,
        status,
        crate::process::structure::ExitStatus::Exited(status),
        after_lane_check,
        || {},
    )
}

#[cfg(test)]
pub(crate) fn step_thread_exit_after_zombify_for_test<Z>(
    thread: Cap<ThreadIdentity>,
    status: i32,
    after_zombify: Z,
) -> ThreadExitOutcome
where
    Z: FnOnce(),
{
    step_thread_exit_inner(
        thread,
        status,
        crate::process::structure::ExitStatus::Exited(status),
        || {},
        after_zombify,
    )
}

fn step_clear_and_wake_child_tid(
    aspace: &crate::vm::AddressSpace,
    tid_ptr: u64,
    cleared: &mut bool,
    guard: &step_engine::Guard<'_>,
) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
    let trace = clear_child_tid_debug_sample();
    if trace {
        emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.uaddr", tid_ptr as i64);
    }

    if !*cleared {
        match aspace.copy_to_user(
            UserPtr::<u8>::new(tid_ptr as usize),
            &[0u8; core::mem::size_of::<u32>()],
            guard,
        ) {
            step_engine::StepOutcome::Done(written) if written == core::mem::size_of::<u32>() => {
                *cleared = true;
            }
            step_engine::StepOutcome::Yield { shape, .. } => {
                return step_engine::StepOutcome::Yield {
                    progress: step_engine::NoProgress,
                    shape,
                };
            }
            step_engine::StepOutcome::Continue { .. } => {
                return step_engine::StepOutcome::Continue {
                    progress: step_engine::NoProgress,
                };
            }
            step_engine::StepOutcome::Done(_) | step_engine::StepOutcome::Err(_) => {
                if trace {
                    emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.err", -1);
                }
                return step_engine::StepOutcome::Done(());
            }
        }
    }

    match step_futex_lifecycle_wake_in(aspace, tid_ptr, 1, guard) {
        step_engine::StepOutcome::Done(woken) => {
            if trace {
                emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.woken", i64::from(woken));
            }
            step_engine::StepOutcome::Done(())
        }
        step_engine::StepOutcome::Err(errno) => {
            if trace {
                emit_clear_child_tid_debug(
                    b"debug.futex.clear_child_tid.err",
                    i64::from(errno as i32),
                );
            }
            step_engine::StepOutcome::Done(())
        }
        step_engine::StepOutcome::Yield { shape, .. } => {
            if trace {
                emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.pending", 1);
            }
            step_engine::StepOutcome::Yield {
                progress: step_engine::NoProgress,
                shape,
            }
        }
        step_engine::StepOutcome::Continue { .. } => {
            if trace {
                emit_clear_child_tid_debug(b"debug.futex.clear_child_tid.pending", 1);
            }
            step_engine::StepOutcome::Continue {
                progress: step_engine::NoProgress,
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
fn walk_robust_list_in_aspace(
    aspace: &crate::vm::AddressSpace,
    head: u64,
    _len: usize,
    guard: &step_engine::Guard<'_>,
) {
    let Some(first) = read_user_u64(aspace, head, guard) else {
        return;
    };
    let Some(futex_offset) = read_user_i64(aspace, head + 8, guard) else {
        return;
    };
    let Some(pending) = read_user_u64(aspace, head + 16, guard) else {
        return;
    };

    let mut entry = first;
    for _ in 0..2048 {
        if entry == 0 || entry == head {
            break;
        }
        mark_robust_entry_owner_died(aspace, entry, futex_offset, guard);
        let Some(next) = read_user_u64(aspace, entry, guard) else {
            break;
        };
        if next == entry {
            break;
        }
        entry = next;
    }

    if pending != 0 {
        mark_robust_entry_owner_died(aspace, pending, futex_offset, guard);
    }
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
    #[cfg(tx_sigprocmask_phase_metrics)]
    let payload_result = {
        let lock_start = tx_observe::clock_now_ns();
        let payload_guard = thread.payload.lock();
        let acquired = tx_observe::clock_now_ns();
        emit_thread_lock_service_trace(
            b"debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns",
            acquired.saturating_sub(lock_start).min(i64::MAX as u64) as i64,
        );
        let clone_start = tx_observe::clock_now_ns();
        let payload = payload_guard.as_ref().cloned();
        let clone_done = tx_observe::clock_now_ns();
        emit_thread_lock_service_trace(
            b"debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns",
            clone_done.saturating_sub(clone_start).min(i64::MAX as u64) as i64,
        );
        drop(payload_guard);
        let released = tx_observe::clock_now_ns();
        emit_thread_lock_service_trace(
            b"debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns",
            released.saturating_sub(acquired).min(i64::MAX as u64) as i64,
        );
        payload.ok_or(crate::thread_runtime::adapter::step_engine::Dead)
    };
    #[cfg(not(tx_sigprocmask_phase_metrics))]
    let payload_result = thread.upgrade_operational();

    let Ok(payload) = payload_result else {
        emit_thread_lock_service_trace(
            b"debug.lock_service.thread.payload.sigprocmask.payload_missing",
            1,
        );
        return SigprocmaskChange::ZombieIgnored;
    };

    step_sigprocmask_with_payload(thread, &payload, how, next)
}

/// Update the per-thread signal mask using an already-resolved thread payload.
///
/// This is the hot syscall fast path: the thread future has already installed
/// the current payload in a per-hart slot, so reopening `thread.payload` would
/// only clone the same cap under the thread-payload lock.
pub fn step_sigprocmask_with_payload(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    how: SigmaskHow,
    next: SignalMask,
) -> SigprocmaskChange {
    let (prev, new_bits) = measure_thread_lock_service(
        b"debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns",
        || {
            let prev_bits = payload.signal_mask.load(Ordering::Acquire);
            let prev = SignalMask::new(prev_bits);
            let new_bits = match how {
                SigmaskHow::SetMask => next.raw_bits(),
                SigmaskHow::Block => prev_bits | next.raw_bits(),
                SigmaskHow::Unblock => prev_bits & !next.raw_bits(),
            };
            (prev, new_bits)
        },
    );
    let new = SignalMask::new(new_bits);
    if new.raw_bits() == prev.raw_bits() {
        emit_thread_lock_service_trace(
            b"debug.lock_service.thread.payload.sigprocmask.mask_noop",
            1,
        );
        return SigprocmaskChange::Replaced { prev, new };
    }

    measure_thread_lock_service(
        b"debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns",
        || payload.signal_mask.store(new.raw_bits(), Ordering::Release),
    );

    measure_thread_lock_service(
        b"debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns",
        || refresh_deliverable_signal_summary_with_payload(thread, &payload),
    );

    SigprocmaskChange::Replaced { prev, new }
}

/// Post a catchable signal and publish the signal wake hint through a
/// caller-provided mailbox post operation.
///
/// The signal state transition is independent from the wake route: pending
/// bits, optional siginfo storage, signal-mask check, and interrupt summary
/// update are committed before the caller-provided mailbox publication step.
/// No-context tests can pass an explicit direct mailbox-post closure; callers
/// with reactor/scheduler context should inject an owner-aware post.
///
/// No-op if the thread is a zombie.
///
/// **Precondition**: `sig` is *not* a Gewalt signum
/// (SIGKILL/SIGSTOP/SIGCONT) — those bypass pending queues entirely
/// per `SIGNAL_v1` §1, §2 Consequence 2 and route through
/// `signal::route_gewalt_with_post` which updates `signal_summary` directly
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
pub fn post_signal_with_post<F>(
    thread: &Cap<ThreadIdentity>,
    sig: Signum,
    routing: SignalRouting,
    info: Option<crate::signal::SigInfo>,
    mut post: F,
) where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    debug_assert!(
        !matches!(sig, Signum::SIGKILL | Signum::SIGSTOP | Signum::SIGCONT),
        "post_signal_with_post must not be called with Gewalt signums (SIGKILL/SIGSTOP/SIGCONT); \
         use signal::route_gewalt_with_post or signal::step_kill_process_with_post which dispatches"
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
    let _ = post_signal_mailbox_with_post(&payload, sig, routing, &mut post);
}

/// Direct-publication counterpart retained for final-smp call sites.  The
/// state transition remains implemented once, in `post_signal_with_post`.
pub fn post_signal(
    thread: &Cap<ThreadIdentity>,
    sig: Signum,
    routing: SignalRouting,
    info: Option<crate::signal::SigInfo>,
) {
    post_signal_with_post(thread, sig, routing, info, |weak, event| {
        let Some(mailbox) = weak.upgrade() else {
            return;
        };
        let _ = mailbox.post(event);
    });
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

/// Resumable current-thread exit operation.
///
/// The operation owns the lifecycle permit, old address space and userspace
/// cleanup snapshot across a `clear_child_tid` wait. No zombie/detach state is
/// published until that cleanup has completed.
pub struct ThreadExitOp {
    thread: Cap<ThreadIdentity>,
    thread_status: i32,
    process_status: crate::process::structure::ExitStatus,
    posts: ThreadExitPosts,
    prepared: Option<PreparedThreadExit>,
    completed: bool,
}

impl ThreadExitOp {
    pub fn new(thread: Cap<ThreadIdentity>, status: i32) -> Self {
        Self {
            thread,
            thread_status: status,
            process_status: crate::process::structure::ExitStatus::Exited(status),
            posts: ThreadExitPosts::default(),
            prepared: None,
            completed: false,
        }
    }

    pub fn with_status(
        thread: Cap<ThreadIdentity>,
        process_status: crate::process::structure::ExitStatus,
    ) -> Self {
        Self {
            thread,
            thread_status: process_status.wait_status_word(),
            process_status,
            posts: ThreadExitPosts::default(),
            prepared: None,
            completed: false,
        }
    }

    pub fn with_posts(mut self, posts: ThreadExitPosts) -> Self {
        self.posts = posts;
        self
    }

    pub fn step_exit(&mut self) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        self.step_exit_with_cleanup(&mut step_thread_exit_user_cleanup)
    }

    fn step_exit_with_cleanup<F>(
        &mut self,
        cleanup_step: &mut F,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress>
    where
        F: FnMut(
            &crate::vm::AddressSpace,
            &mut ThreadExitUserCleanup,
        ) -> step_engine::StepOutcome<(), step_engine::NoProgress>,
    {
        if self.completed {
            return step_engine::StepOutcome::Done(());
        }
        if self.prepared.is_none() {
            match prepare_thread_exit_state(
                &self.thread,
                self.thread_status,
                self.process_status,
                || {},
            ) {
                PrepareThreadExitOutcome::Completed => {
                    self.completed = true;
                    return step_engine::StepOutcome::Done(());
                }
                PrepareThreadExitOutcome::Retry => {
                    return step_engine::StepOutcome::Continue {
                        progress: step_engine::NoProgress,
                    };
                }
                PrepareThreadExitOutcome::Prepared(prepared) => {
                    self.prepared = Some(prepared);
                }
            }
        }

        let prepared = self.prepared.as_mut().expect("exit state prepared");
        if let Some(aspace) = prepared.aspace.as_ref() {
            match cleanup_step(aspace, &mut prepared.cleanup) {
                step_engine::StepOutcome::Done(()) => {}
                other => return other,
            }
        }

        let prepared = self.prepared.take().expect("exit state prepared");
        let posts = self.posts;
        let mut signal_post = |mailbox, event| posts.post_signal(mailbox, event);
        let mut wake_post = |mailbox: &TaskMailbox, event| posts.post_wake(mailbox, event);
        finish_prepared_thread_exit_with_posts(
            &self.thread,
            prepared,
            || {},
            &mut signal_post,
            &mut wake_post,
        );
        self.completed = true;
        step_engine::StepOutcome::Done(())
    }

    #[cfg(test)]
    fn step_exit_with_cleanup_for_test<F>(
        &mut self,
        cleanup_step: &mut F,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress>
    where
        F: FnMut(
            &crate::vm::AddressSpace,
            &mut ThreadExitUserCleanup,
        ) -> step_engine::StepOutcome<(), step_engine::NoProgress>,
    {
        self.step_exit_with_cleanup(cleanup_step)
    }
}

impl Drop for ThreadExitOp {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.as_mut() else {
            return;
        };
        let (Some(payload), Some(permit)) = (
            prepared.process_payload.as_ref(),
            prepared.exit_permit.take(),
        ) else {
            return;
        };
        let aborted = payload.abort_thread_exit(self.thread.tid.0, permit);
        debug_assert!(aborted, "prepared thread-exit permit remains abortable");
    }
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
        self.step_exit()
    }
}

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

/// StepOp wrapper for per-thread signal delivery with caller-injected
/// mailbox publication.
pub struct ThreadKillWithPostOp<F> {
    pub thread: Cap<ThreadIdentity>,
    pub sig: Signum,
    pub info: Option<crate::signal::SigInfo>,
    pub post: F,
}

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
        let routing = SignalRouting::ThreadDirected {
            tid: self.thread.tid.0 as u64,
        };
        post_signal(&self.thread, self.sig, routing, self.info);
        crate::thread_runtime::adapter::step_engine::StepOutcome::Done(())
    }
}

impl<I: crate::thread_runtime::adapter::step_engine::SubjectIdentity> OneShotStepOp<I>
    for ThreadKillOp
{
}

impl<I, F> crate::thread_runtime::adapter::step_engine::StepOp<I> for ThreadKillWithPostOp<F>
where
    I: crate::thread_runtime::adapter::step_engine::SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
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
        let routing = SignalRouting::ThreadDirected {
            tid: self.thread.tid.0 as u64,
        };
        post_signal_with_post(&self.thread, self.sig, routing, self.info, &mut self.post);
        crate::thread_runtime::adapter::step_engine::StepOutcome::Done(())
    }
}

impl<F> OneShotStepOp<ProcessIdentity> for ThreadKillWithPostOp<F> where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent)
{
}

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

    fn assert_lifecycle_wake_post(
        mailbox: &TaskMailbox,
        event: MailboxEvent,
        hint: MailboxSchedulerHint,
    ) -> bool {
        assert_eq!(hint, MailboxSchedulerHint::LifecycleWake);
        mailbox.post_with_scheduler_hint(event, hint)
    }

    #[test]
    fn thread_exit_posts_classify_parent_wake_as_lifecycle() {
        let mailbox = TaskMailbox::new();
        let event = MailboxEvent::SignalDelivered {
            signum: Signum::SIGCHLD.raw() as u32,
            routing: SignalRouting::ProcessDirected,
        };
        let posts = ThreadExitPosts::new(None, None, Some(assert_lifecycle_wake_post));

        assert!(posts.post_wake(&mailbox, event));
        assert_eq!(mailbox.poll(), Some(event));
    }

    #[test]
    fn thread_exit_op_delegates_to_step_thread_exit() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let leader = first_thread(&proc_cap);
        assert!(!leader.is_zombie());
        let mut op = ThreadExitOp::new(leader.clone(), 7);
        let mut ctx = ScriptCtx::<
            crate::thread_runtime::adapter::step_engine::PlaceholderProcessSubject,
        >::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
        assert!(leader.is_zombie());
        assert_eq!(leader.exit_status(), Some(7));
    }

    #[test]
    fn thread_exit_op_retains_permit_across_cleanup_yield() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let ctid = 0x8000_1000;
        let sibling = crate::process::execution::step_clone_thread(
            &proc_cap,
            &tx_hal::UserTrapContext::empty(),
            SignalMask::EMPTY,
            0,
            0,
            ctid,
        )
        .expect("sibling");
        assert_eq!(proc_cap.live_thread_count(), 2);

        let mut op = ThreadExitOp::new(sibling.clone(), 9);
        let mut first = true;
        let mut cleanup_step = |_aspace: &AddressSpace, cleanup: &mut ThreadExitUserCleanup| {
            assert_eq!(cleanup.ctid, Some(ctid));
            if core::mem::replace(&mut first, false) {
                StepOutcome::yield_on_wait_source(step_engine::NoProgress, 0xfeed, 1)
            } else {
                StepOutcome::Done(())
            }
        };

        assert_eq!(
            op.step_exit_with_cleanup_for_test(&mut cleanup_step),
            StepOutcome::yield_on_wait_source(step_engine::NoProgress, 0xfeed, 1)
        );
        assert!(!sibling.is_zombie());
        assert_eq!(sibling.exit_status(), None);
        assert_eq!(proc_cap.live_thread_count(), 2);
        assert!(proc_cap.thread_by_tid(sibling.tid.0).is_some());

        assert_eq!(
            op.step_exit_with_cleanup_for_test(&mut cleanup_step),
            StepOutcome::Done(())
        );
        assert!(sibling.is_zombie());
        assert_eq!(sibling.exit_status(), Some(9));
        assert_eq!(proc_cap.live_thread_count(), 1);
        assert!(proc_cap.thread_by_tid(sibling.tid.0).is_none());
    }

    #[test]
    fn dropping_suspended_thread_exit_releases_lifecycle_claim() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let sibling = crate::process::execution::step_clone_thread(
            &proc_cap,
            &tx_hal::UserTrapContext::empty(),
            SignalMask::EMPTY,
            0,
            0,
            0x8000_2000,
        )
        .expect("sibling");

        let mut suspended = ThreadExitOp::new(sibling.clone(), 11);
        let mut yield_cleanup = |_aspace: &AddressSpace, _cleanup: &mut ThreadExitUserCleanup| {
            StepOutcome::yield_on_wait_source(step_engine::NoProgress, 0xbeef, 1)
        };
        assert!(matches!(
            suspended.step_exit_with_cleanup_for_test(&mut yield_cleanup),
            StepOutcome::Yield { .. }
        ));
        drop(suspended);

        let mut retry = ThreadExitOp::new(sibling.clone(), 11);
        let mut complete_cleanup =
            |_aspace: &AddressSpace, _cleanup: &mut ThreadExitUserCleanup| StepOutcome::Done(());
        assert_eq!(
            retry.step_exit_with_cleanup_for_test(&mut complete_cleanup),
            StepOutcome::Done(())
        );
        assert!(sibling.is_zombie());
        assert_eq!(proc_cap.live_thread_count(), 1);
    }

    #[test]
    fn group_exit_status_overrides_concurrent_local_thread_exit() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let sibling = crate::process::execution::step_clone_thread(
            &proc_cap,
            &tx_hal::UserTrapContext::empty(),
            SignalMask::EMPTY,
            0,
            0,
            0,
        )
        .expect("sibling");
        let payload = proc_cap
            .payload
            .lock()
            .as_ref()
            .cloned()
            .expect("process alive");
        assert!(payload.reserve_group_exit(crate::process::structure::ExitStatus::Exited(7)));

        let mut op = ThreadExitOp::new(sibling.clone(), 99);
        let mut complete_cleanup =
            |_aspace: &AddressSpace, _cleanup: &mut ThreadExitUserCleanup| StepOutcome::Done(());
        assert_eq!(
            op.step_exit_with_cleanup_for_test(&mut complete_cleanup),
            StepOutcome::Done(())
        );

        assert_eq!(
            sibling.exit_status(),
            Some(crate::process::structure::ExitStatus::Exited(7).wait_status_word())
        );
        assert_eq!(proc_cap.live_thread_count(), 1);
        assert_eq!(
            crate::process::execution::group_exit_status(&proc_cap),
            Some(crate::process::structure::ExitStatus::Exited(7))
        );
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
