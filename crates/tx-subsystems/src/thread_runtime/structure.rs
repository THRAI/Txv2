//! Thread-runtime subsystem structure: identities and payloads.
//!
//! Per `THREAD_RUNTIME_v1`, the thread is the unit of execution: each
//! thread owns its own future + reactor task. This module realizes the
//! identity/payload split, including the per-thread signal mask and
//! pending queue. The realtime per-occurrence queue and `signal_summary`
//! fast-check atomic land alongside the delivery pass.

use alloc::sync::Weak as ArcWeak;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use core::task::Waker;

use tx_hal::UserTrapContext;

use crate::thread_runtime::adapter::reactor_entry::{
    TaskKey, UserspaceRunRequest, UserspaceRunSlot,
};
use crate::thread_runtime::adapter::step_engine::{
    Cap, Dead, Entity, PayloadCap, PayloadPolicy, SpinMutex, TaskMailbox, Weak, Zone, ZoneAllocated,
};

use crate::process::ProcessIdentity;
use crate::signal::{InterruptSummary, PendingSignalQueue, SignalMask};

/// Thread identifier. TID 0 is reserved.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Tid(pub u32);

impl Tid {
    pub const RESERVED: Self = Self(0);
}

/// Thread identity. Persists across payload teardown so a zombie thread
/// can be `wait`-ed on without racing the runtime tear-down path.
///
/// `owner_proc` is `Weak` to break the ownership cycle: the parent
/// `ProcessPayload` retains threads via `Cap<ThreadIdentity>`, so the
/// thread cannot strongly reference the parent or the parent would never
/// drop.
pub struct ThreadIdentity {
    pub tid: Tid,
    pub(crate) owner_proc: Weak<ProcessIdentity>,
    pub(crate) exit_status: SpinMutex<Option<i32>>,
    pub(crate) payload: SpinMutex<Option<PayloadCap<ThreadPayload>>>,
}

impl ThreadIdentity {
    /// Read the recorded thread exit status. `Some` once
    /// `step_thread_exit` has run; otherwise `None`.
    pub fn exit_status(&self) -> Option<i32> {
        *self.exit_status.lock()
    }

    /// Whether the thread is a zombie (payload dropped, identity
    /// retained until reaped or the parent process drops).
    pub fn is_zombie(&self) -> bool {
        self.payload.lock().is_none()
    }

    /// Thread state character for `/proc/<pid>/task/<tid>/stat`.
    pub fn proc_state_char(&self) -> u8 {
        let Some(payload) = self.payload.lock().as_ref().cloned() else {
            return b'Z';
        };
        if payload.is_stopped() {
            return b'T';
        }
        if payload.proc_sleeping() {
            return b'S';
        }
        if crate::futex::thread_has_waiter(self.tid.0) {
            return b'S';
        }
        b'R'
    }

    /// Snapshot the owning process via `Weak::upgrade` under a fresh
    /// guard. Returns `None` if the process identity has been dropped.
    pub fn upgrade_owner_proc(
        &self,
    ) -> Option<crate::thread_runtime::adapter::step_engine::Cap<ProcessIdentity>> {
        let guard = crate::thread_runtime::adapter::step_engine::borrow_current_guard()
            .unwrap_or_else(crate::thread_runtime::adapter::step_engine::guard);
        self.owner_proc.upgrade(&guard)
    }

    /// Snapshot the live `PayloadCap<ThreadPayload>` if the thread is
    /// not yet a zombie. Returns `None` once `step_thread_exit` has
    /// dropped the payload.
    ///
    /// Production callers: `tx-kernel`'s `kernel_main` reactor-loop
    /// wiring (Pre-ELF Phase 7) needs the leader's payload to build
    /// `PerHartSlotted::new(payload, run_thread::<P>(thread, payload))`
    /// before submitting it to the reactor. The trap shell still
    /// reaches the payload via the per-hart slot
    /// (`current_thread_payload(hart)`); `step_thread_exit` still
    /// mutates the payload via the crate-private field. This accessor
    /// exists so the bootstrap site can build the future without
    /// reaching into the `pub(crate)` slot directly.
    ///
    /// Test callers: the per-hart slot can be installed manually for
    /// targeted unit tests (see
    /// `crates/tx-kernel/src/thread_future/tests.rs`).
    pub fn payload_cap(&self) -> Option<PayloadCap<ThreadPayload>> {
        self.payload.lock().clone()
    }

    /// Backwards-compatible alias for [`Self::payload_cap`]. Kept so
    /// existing `cfg(test)`-gated call sites compile unchanged while
    /// the production accessor takes over the canonical name.
    #[cfg(any(test, feature = "test-support"))]
    pub fn payload_cap_for_test(&self) -> Option<PayloadCap<ThreadPayload>> {
        self.payload_cap()
    }
}

impl Entity for ThreadIdentity {
    type OperationalEvidence = PayloadCap<ThreadPayload>;

    fn upgrade_operational(
        identity: &crate::thread_runtime::adapter::step_engine::Cap<Self>,
    ) -> Result<Self::OperationalEvidence, Dead> {
        identity.payload.lock().as_ref().cloned().ok_or(Dead)
    }
}

/// Thread payload. Dropped on `step_thread_exit`.
///
/// `task` is the reactor `TaskKey` driving this thread's future. It is
/// `None` until the reactor coupling lands (β4); for now construction
/// paths leave it `None` and tests do not exercise reactor wiring.
///
/// Trap-shell hand-off fields (`userspace_slot`, `active_request`,
/// `saved_user_context`, `pending_syscall_return`) realise the
/// `txdoc:THREAD-5-1-STATE-PLACEMENT` rule that the trap shell
/// snapshots/handoffs through the payload, and the
/// `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE` writeback discipline:
/// the trap shell only resolves the wait, while the userspace-entry
/// shim consults `pending_syscall_return` and `saved_user_context`
/// to produce the next `enter_userspace`.
pub struct ThreadPayload {
    pub(crate) task: SpinMutex<Option<TaskKey>>,
    /// Blocked-signal mask. Stored as an atomic so single-bit
    /// updates from the same thread don't need the spin mutex.
    pub(crate) signal_mask: AtomicU64,
    /// Per-thread pending-signal bitset.
    pub(crate) thread_pending: PendingSignalQueue,
    /// Conservative summary of process-group pending signals known to
    /// affect this thread. A zero value lets signal-mask refresh skip
    /// the owner-process upgrade; nonzero falls back to the
    /// authoritative `ProcessPayload.group_pending`.
    pub(crate) group_pending_summary: AtomicU64,
    /// `InterruptSummary` packed into 8 bits, kept current by
    /// catchable-signal posting, `step_sigprocmask`, `step_thread_exit`, and the
    /// SIGKILL routing path. Read by `select_next_signal` /
    /// `ast_check`. Per `THREAD_RUNTIME_v1` §5.2.
    pub(crate) signal_summary: AtomicU8,
    /// One slot per thread; clone-shared with the task future driving
    /// this thread. Created when the thread's bootstrap helper
    /// installs the userspace-run task wrapper. The trap shell looks
    /// the slot up off the active payload and resolves the wait via
    /// `complete_interesting_trap` per
    /// `txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`.
    pub userspace_slot: UserspaceRunSlot,
    /// Generation token for the in-flight userspace-run wait;
    /// `None` between waits. Set by the thread future when it calls
    /// `start_request`, cleared by the resolved `UserspaceRunWait`
    /// future.
    pub(crate) active_request: SpinMutex<Option<UserspaceRunRequest>>,
    /// Captured on entry to a syscall/fault trap by
    /// `view.capture_user_context()`, restored before next userspace
    /// entry. Per `THREAD-5-1-STATE-PLACEMENT`.
    pub(crate) saved_user_context: SpinMutex<Option<UserTrapContext>>,
    /// Mask to restore after a handler which interrupted
    /// `rt_sigsuspend`.
    ///
    /// This is distinct from `saved_signal_mask`: the handler must start with
    /// the temporary suspend mask, while `rt_sigreturn` must restore the mask
    /// that was active before `rt_sigsuspend`.
    pub(crate) sigsuspend_restore_mask: SpinMutex<Option<SignalMask>>,
    /// Saved signal context: the `UserTrapContext` that was active
    /// before the most recent handler delivery.  Written by the AST
    /// checkpoint in `thread_future` when `DeliverHandler` fires;
    /// consumed by `sys_rt_sigreturn` to restore the original
    /// execution state.  `None` when no handler is currently
    /// executing.
    pub(crate) saved_signal_context: SpinMutex<Option<UserTrapContext>>,
    /// Signal mask active before the most recent handler delivery.
    /// Restored together with `saved_signal_context` by `rt_sigreturn`.
    pub(crate) saved_signal_mask: SpinMutex<Option<SignalMask>>,
    /// Result of the last completed syscall, drained by the
    /// userspace-entry checkpoint and written into the (then-fresh)
    /// trap frame via `set_syscall_return` / `set_syscall_error`
    /// immediately before `enter_userspace`. Per the writeback
    /// discipline pinned in the Trio plan's "Cross-cutting risks #1"
    /// (Plan B, deferred writeback).
    pub(crate) pending_syscall_return: SpinMutex<Option<Result<i64, i32>>>,
    /// Wake mailbox bound to the reactor task that drives this
    /// thread's future, used by D9-A's signal-wake plumbing.
    ///
    /// **D9-A (lost-wake fix for signals).** Catchable-signal posting,
    /// Gewalt routing, and `set_thread_zombie` post a
    /// [`MailboxEvent::SignalDelivered`] event to this mailbox after
    /// updating the per-thread `signal_summary`. The mailbox post
    /// wakes the registered `core::task::Waker`, forcing the parked
    /// future to re-poll and observe the new summary bit; the event
    /// itself is **not** matched by `ActiveWait::matches`
    /// (`MailboxEvent::SignalDelivered` is a wake-hint, the truth
    /// lives in `InterruptSummary`).
    ///
    /// **Lifecycle.** Stored as `Weak<TaskMailbox>` so the mailbox
    /// is task-owned, not thread-owned — the mailbox lives with the
    /// reactor task driving the thread future. Bound by the
    /// reactor's thread-task wiring once that exists; until then
    /// the slot stays `None` and the signal-side hooks treat an
    /// unbound mailbox as no-op (best-effort post). See D9 §8 Phase
    /// D9-A: "no-op when the mailbox is unbound (the invariant
    /// during early bring-up)."
    ///
    /// Holds `alloc::sync::Weak` because PR-3A's `TaskMailbox` is
    /// `Arc`-managed at the substrate layer; the eventual zone
    /// migration (PR-3D+) flips this to `zone::Weak<TaskMailbox>`.
    pub(crate) mailbox: SpinMutex<Option<ArcWeak<TaskMailbox>>>,
    /// Stable scheduler wake route for exec/group-exit lifecycle events.
    /// Unlike the mailbox's transient wait waker, this binding survives a
    /// nested futex/I/O wait clearing its own registration.
    pub(crate) lifecycle_waker: SpinMutex<Option<Waker>>,
    /// Thread stop flag.  Set by Gewalt routing for SIGSTOP /
    /// `DefaultStop` AST materialisation; cleared by
    /// Gewalt routing for SIGCONT.  When `true`, the thread must not
    /// enter userspace — it is parked until `stopped` becomes
    /// `false`.
    ///
    /// Per POSIX, SIGSTOP/SIGTSTP (default Stop disposition) puts
    /// the thread in the `TASK_STOPPED` state.  A subsequent
    /// SIGCONT resumes it.  This flag bridges the entry-side AST
    /// checkpoint in `thread_future`: before entering userspace,
    /// the thread checks `stopped` and yields if set.
    ///
    /// Phase E (first pass): busy-wait placeholder.  The thread
    /// future spins on `stopped` until cleared.  TODO: replace with
    /// a wait-source parked by the reactor, woken by SIGCONT
    /// clearing the flag + posting to the thread's
    /// mailbox.
    ///
    /// Atomic because Gewalt routing sets/clears it under the
    /// thread-list lock, and the AST checkpoint reads it from the
    /// thread future without acquiring the payload lock.
    /// See: `txdoc:SIGNAL-V1-S12-3-ROUTE-GEWALT-STOP`.
    pub(crate) stopped: core::sync::atomic::AtomicBool,
    /// Alternate signal stack (`sigaltstack(2)`).  `None` means
    /// "no alternate stack" (deliver on the normal stack).
    /// `Some((base, size))` gives the alternate stack range.
    pub(crate) alt_stack: SpinMutex<Option<(usize, usize)>>,
    /// Best-effort procfs state hint. Set while the thread future is
    /// awaiting syscall dispatch; blocking syscall futures should
    /// appear as sleeping to procfs observers.
    pub(crate) proc_sleeping: AtomicBool,
    /// Lock-free lifecycle latch set when this thread submits `exit(2)` or is
    /// terminated by exec/group-exit collapse.
    ///
    /// The bit is never cleared: a retained task future must not return to
    /// userspace after the identity payload has been detached. Keeping this
    /// separate from `active_syscall_nr` also avoids restoring the old
    /// per-round payload-slot lock on the syscall hot path.
    exit_intent: core::sync::atomic::AtomicBool,
    active_syscall_nr: AtomicU64,
    active_syscall_arg0: AtomicU64,
    active_syscall_arg1: AtomicU64,
    last_user_entry_pc: AtomicU64,
    last_user_entry_ra: AtomicU64,
    last_user_entry_sp: AtomicU64,
    last_user_entry_tls: AtomicU64,
    last_user_entry_syscall: AtomicU64,
    last_user_entry_hart: AtomicU64,
    /// `clear_child_tid` pointer from `set_tid_address`.  Written
    /// atomically to 0 on thread exit when futex wake is supported.
    pub clear_child_tid: SpinMutex<Option<u64>>,
    /// Robust-list head pointer from `set_robust_list`. Linux's
    /// `robust_list_head` structure: `{ list, futex_offset, pending }`.
    /// `list` is a linked list of `robust_list` entries; each entry
    /// carries the futex word the robust mutex protects.
    pub robust_list_head: SpinMutex<Option<u64>>,
    /// Length of the robust list in bytes (Linux's `len` parameter).
    pub robust_list_len: SpinMutex<usize>,
}

impl ThreadPayload {
    /// Build a `ThreadPayload` with a fresh `UserspaceRunSlot` and all
    /// trap-handoff state cleared. Used by `sign_thread` and tests.
    pub fn fresh() -> Self {
        Self {
            task: SpinMutex::new(None),
            signal_mask: AtomicU64::new(0),
            thread_pending: PendingSignalQueue::new(),
            group_pending_summary: AtomicU64::new(0),
            signal_summary: AtomicU8::new(0),
            userspace_slot: UserspaceRunSlot::new(),
            active_request: SpinMutex::new(None),
            saved_user_context: SpinMutex::new(None),
            sigsuspend_restore_mask: SpinMutex::new(None),
            saved_signal_context: SpinMutex::new(None),
            saved_signal_mask: SpinMutex::new(None),
            pending_syscall_return: SpinMutex::new(None),
            mailbox: SpinMutex::new(None),
            lifecycle_waker: SpinMutex::new(None),
            stopped: core::sync::atomic::AtomicBool::new(false),
            alt_stack: SpinMutex::new(None),
            proc_sleeping: AtomicBool::new(false),
            exit_intent: core::sync::atomic::AtomicBool::new(false),
            active_syscall_nr: AtomicU64::new(u64::MAX),
            active_syscall_arg0: AtomicU64::new(0),
            active_syscall_arg1: AtomicU64::new(0),
            last_user_entry_pc: AtomicU64::new(0),
            last_user_entry_ra: AtomicU64::new(0),
            last_user_entry_sp: AtomicU64::new(0),
            last_user_entry_tls: AtomicU64::new(0),
            last_user_entry_syscall: AtomicU64::new(u64::MAX),
            last_user_entry_hart: AtomicU64::new(u64::MAX),
            clear_child_tid: SpinMutex::new(None),
            robust_list_head: SpinMutex::new(None),
            robust_list_len: SpinMutex::new(0),
        }
    }

    /// Bind a `TaskMailbox` to this thread payload so the D9-A
    /// signal-wake plumbing has a wake destination.
    ///
    /// Stored as a `Weak` handle: the mailbox is task-owned (lives
    /// with the reactor task driving the thread future), and the
    /// payload only needs the wake-hint route. A subsequent
    /// `bind_mailbox` replaces the previously bound handle (the
    /// reactor task wrapper is the unique caller and binds once
    /// per task).
    pub fn bind_mailbox(&self, mailbox: ArcWeak<TaskMailbox>) {
        *self.mailbox.lock() = Some(mailbox);
    }

    /// Snapshot the bound mailbox handle (if any). Returns `None`
    /// when the slot is unset, which is the invariant during early
    /// bring-up (D9 Phase D9-A note: the mailbox post is best-
    /// effort while existing thread-creation paths haven't been
    /// extended to call `bind_mailbox`).
    pub fn mailbox_handle(&self) -> Option<ArcWeak<TaskMailbox>> {
        self.mailbox.lock().clone()
    }

    pub fn bind_lifecycle_waker(&self, waker: Waker) {
        *self.lifecycle_waker.lock() = Some(waker);
    }

    /// Request another poll through the stable task waker installed by the
    /// reactor wrapper.
    ///
    /// Architecture userspace-entry shims may return through a non-local trap
    /// handoff, so callers at that boundary must not retain or dereference the
    /// pre-entry stack-local `Context`.
    pub fn request_task_repoll(&self) {
        let waker = self.lifecycle_waker.lock().clone();
        if let Some(waker) = waker {
            waker.wake_by_ref();
        }
    }

    pub(crate) fn wake_lifecycle_task(&self) {
        self.request_task_repoll();
    }

    /// Snapshot the reactor task handle, if one has been bound. Always
    /// `None` until the reactor coupling lands.
    pub fn task(&self) -> Option<TaskKey> {
        *self.task.lock()
    }

    /// Snapshot whether this thread should be shown as sleeping in procfs.
    pub fn proc_sleeping(&self) -> bool {
        self.proc_sleeping.load(Ordering::Acquire)
    }

    /// Update the procfs sleep-state hint for syscall dispatch.
    pub fn set_proc_sleeping(&self, sleeping: bool) {
        self.proc_sleeping.store(sleeping, Ordering::Release);
    }

    pub fn mark_exit_intent(&self) {
        self.exit_intent.store(true, Ordering::Release);
    }

    pub fn exit_intent(&self) -> bool {
        self.exit_intent.load(Ordering::Acquire)
    }

    pub fn begin_syscall_diagnostic(&self, nr: u64, arg0: u64, arg1: u64) {
        self.active_syscall_arg0.store(arg0, Ordering::Relaxed);
        self.active_syscall_arg1.store(arg1, Ordering::Relaxed);
        self.active_syscall_nr.store(nr, Ordering::Release);
    }

    pub fn end_syscall_diagnostic(&self) {
        self.active_syscall_nr.store(u64::MAX, Ordering::Release);
    }

    pub fn active_syscall_diagnostic(&self) -> Option<(u64, u64, u64)> {
        let nr = self.active_syscall_nr.load(Ordering::Acquire);
        (nr != u64::MAX).then(|| {
            (
                nr,
                self.active_syscall_arg0.load(Ordering::Relaxed),
                self.active_syscall_arg1.load(Ordering::Relaxed),
            )
        })
    }

    pub fn record_user_entry_diagnostic(
        &self,
        pc: u64,
        ra: u64,
        sp: u64,
        tls: u64,
        syscall: u64,
        hart: u64,
    ) {
        self.last_user_entry_pc.store(pc, Ordering::Relaxed);
        self.last_user_entry_ra.store(ra, Ordering::Relaxed);
        self.last_user_entry_sp.store(sp, Ordering::Relaxed);
        self.last_user_entry_tls.store(tls, Ordering::Relaxed);
        self.last_user_entry_syscall
            .store(syscall, Ordering::Relaxed);
        self.last_user_entry_hart.store(hart, Ordering::Release);
    }

    pub fn user_entry_diagnostic(&self) -> (u64, u64, u64, u64, u64, u64) {
        let hart = self.last_user_entry_hart.load(Ordering::Acquire);
        (
            self.last_user_entry_pc.load(Ordering::Relaxed),
            self.last_user_entry_ra.load(Ordering::Relaxed),
            self.last_user_entry_sp.load(Ordering::Relaxed),
            self.last_user_entry_tls.load(Ordering::Relaxed),
            self.last_user_entry_syscall.load(Ordering::Relaxed),
            hart,
        )
    }

    /// Borrow the userspace-run slot owned by this thread. The trap
    /// shell uses this to resolve the active wait via
    /// `complete_interesting_trap`.
    pub fn userspace_slot(&self) -> &UserspaceRunSlot {
        &self.userspace_slot
    }

    /// Snapshot the in-flight userspace-run request token, if any.
    pub fn active_userspace_request(&self) -> Option<UserspaceRunRequest> {
        *self.active_request.lock()
    }

    /// Record the in-flight userspace-run request token; cleared by
    /// the thread future when its `UserspaceRunWait` resolves.
    pub fn set_active_userspace_request(&self, request: Option<UserspaceRunRequest>) {
        *self.active_request.lock() = request;
    }

    /// Snapshot the saved user trap context.
    pub fn saved_user_context(&self) -> Option<UserTrapContext> {
        *self.saved_user_context.lock()
    }

    /// Replace the saved user trap context. Called by the trap shell
    /// from `view.capture_user_context()` immediately before
    /// resolving the userspace-run wait, per Plan B writeback
    /// discipline.
    pub fn store_saved_user_context(&self, ctx: Option<UserTrapContext>) {
        *self.saved_user_context.lock() = ctx;
    }

    /// Store a trap capture while retaining an unchanged lazy FP/vector image.
    ///
    /// Architectures may report an invalid FP payload when hardware says the
    /// register file is Clean: the previously saved image is still
    /// authoritative and copying it out of the trap frame would be redundant.
    /// Exec/clone use `store_saved_user_context` directly when they intend to
    /// replace the complete context.
    pub fn store_captured_user_context(&self, mut ctx: UserTrapContext) {
        let mut saved = self.saved_user_context.lock();
        if !ctx.fp.is_valid() {
            if let Some(previous) = saved.as_ref() {
                if previous.fp.is_valid() {
                    ctx.fp = previous.fp;
                }
            }
        }
        *saved = Some(ctx);
    }

    /// Publish the pre-`rt_sigsuspend` mask for the next handler frame.
    pub fn store_sigsuspend_restore_mask(&self, mask: Option<SignalMask>) {
        *self.sigsuspend_restore_mask.lock() = mask;
    }

    /// Consume the pre-`rt_sigsuspend` mask while building that handler frame.
    pub fn take_sigsuspend_restore_mask(&self) -> Option<SignalMask> {
        self.sigsuspend_restore_mask.lock().take()
    }

    /// Replace the saved signal context. Called by signal delivery
    /// to preserve the pre-handler context for `rt_sigreturn`.
    pub fn store_saved_signal_context(&self, ctx: Option<UserTrapContext>) {
        *self.saved_signal_context.lock() = ctx;
    }

    /// Whether a signal handler frame is currently in flight.
    pub fn has_saved_signal_context(&self) -> bool {
        self.saved_signal_context.lock().is_some()
    }

    /// Take (consume) the saved signal context. Called by
    /// `rt_sigreturn` to retrieve the pre-handler context for
    /// restoration into `saved_user_context`. Returns `None` if no
    /// signal frame is in flight (stray `rt_sigreturn` call).
    pub fn take_saved_signal_context(&self) -> Option<UserTrapContext> {
        self.saved_signal_context.lock().take()
    }

    /// Replace the signal mask saved for the active signal frame.
    pub fn store_saved_signal_mask(&self, mask: Option<SignalMask>) {
        *self.saved_signal_mask.lock() = mask;
    }

    /// Take the signal mask saved for the active signal frame.
    pub fn take_saved_signal_mask(&self) -> Option<SignalMask> {
        self.saved_signal_mask.lock().take()
    }

    /// Push a pending syscall return into the per-thread slot. The
    /// userspace-entry shim drains this and writes it into the fresh
    /// trap frame via `set_syscall_return`/`set_syscall_error` before
    /// `enter_userspace`, per Plan B.
    pub fn store_pending_syscall_return(&self, result: Option<Result<i64, i32>>) {
        *self.pending_syscall_return.lock() = result;
    }

    /// Read the current signal mask.
    pub fn signal_mask(&self) -> SignalMask {
        SignalMask::new(self.signal_mask.load(Ordering::Acquire))
    }

    /// Write the current signal mask.
    pub fn store_signal_mask(&self, mask: SignalMask) {
        self.signal_mask.store(mask.raw_bits(), Ordering::Release);
    }

    /// Whether this thread is stopped (SIGSTOP / default-Stop
    /// disposition). The AST checkpoint in thread_future uses this
    /// to decide whether to enter userspace.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Set or clear the stopped flag. Used by
    /// Gewalt routing for SIGSTOP (set) and SIGCONT
    /// (clear).
    pub(crate) fn set_stopped(&self, val: bool) {
        self.stopped
            .store(val, core::sync::atomic::Ordering::Release);
    }

    /// Snapshot the alternate signal stack (base, size).
    /// `None` means use the normal stack.
    pub fn alt_stack(&self) -> Option<(usize, usize)> {
        *self.alt_stack.lock()
    }

    /// Set or clear the alternate signal stack.
    pub fn set_alt_stack(&self, stack: Option<(usize, usize)>) {
        *self.alt_stack.lock() = stack;
    }

    /// Borrow the per-thread pending-signal queue.
    pub fn pending(&self) -> &PendingSignalQueue {
        &self.thread_pending
    }

    /// Snapshot the conservative process-group pending hint.
    pub(crate) fn group_pending_summary(&self) -> u64 {
        self.group_pending_summary.load(Ordering::Acquire)
    }

    /// Replace the process-group pending hint.
    pub(crate) fn store_group_pending_summary(&self, bits: u64) {
        self.group_pending_summary.store(bits, Ordering::Release);
    }

    /// Snapshot the current interrupt summary.
    pub fn interrupt_summary(&self) -> InterruptSummary {
        InterruptSummary::unpack(self.signal_summary.load(Ordering::Acquire))
    }

    /// Atomic read-modify-write on the packed summary bits. Used by
    /// catchable-signal posting, `step_sigprocmask`, etc., to keep the summary
    /// in sync with the underlying state. The mutator is `Fn` because
    /// the CAS loop may retry on contention.
    pub(crate) fn update_summary(&self, f: impl Fn(&mut InterruptSummary)) {
        let mut cur = self.signal_summary.load(Ordering::Acquire);
        loop {
            let mut summary = InterruptSummary::unpack(cur);
            f(&mut summary);
            let new = summary.pack();
            match self.signal_summary.compare_exchange_weak(
                cur,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }
}

// ------------------------------------------------------------------
// D9-A bridge: ThreadPayload → reactor InterruptSource
// ------------------------------------------------------------------

use tx_reactor::interrupt::InterruptSource;

impl InterruptSource for ThreadPayload {
    fn deliverable_signal_pending(&self) -> bool {
        self.interrupt_summary().deliverable_signal
    }

    fn termination_in_force(&self) -> bool {
        self.interrupt_summary().termination
    }

    fn stop_requested(&self) -> bool {
        self.interrupt_summary().stop_requested
    }
}

impl Default for ThreadPayload {
    fn default() -> Self {
        Self::fresh()
    }
}

/// Drain the `pending_syscall_return` slot from a `ThreadPayload`.
///
/// Returns the previously-stored value (if any) and replaces the slot
/// with `None`. Called by the userspace-entry shim immediately before
/// `enter_userspace` so the platform writes `set_syscall_return` /
/// `set_syscall_error` into the fresh trap frame, per the Plan B
/// writeback discipline pinned in the Trio plan's "Cross-cutting
/// risks #1".
pub fn drain_pending_syscall_return(payload: &ThreadPayload) -> Option<Result<i64, i32>> {
    payload.pending_syscall_return.lock().take()
}

/// Allocate and retire reusable `ThreadPayload` slots before userspace starts.
///
/// Pthread-heavy guests can create enough threads in one timed batch to empty
/// the first per-CPU zone bucket. Without a warm spare slab, the next
/// `clone(CLONE_THREAD)` pays the full frame-backed slab allocation cost inside
/// the benchmark window. This helper intentionally moves that cache growth to
/// boot/init time while leaving the slots reusable for normal clone paths.
pub fn prewarm_thread_payload_slots(count: usize) -> usize {
    use alloc::vec::Vec;

    let mut caps = Vec::new();
    for _ in 0..count {
        match crate::thread_runtime::adapter::step_engine::sign(ThreadPayload::fresh()) {
            Ok(cap) => caps.push(cap),
            Err(_) => break,
        }
    }

    let warmed = caps.len();
    drop(caps);
    let mut quiet = 0u8;
    while quiet < 2 {
        let stats = crate::thread_runtime::adapter::step_engine::drain_with_budget(usize::MAX);
        if stats.bag_reclaimed == 0 && stats.publication_dropped == 0 {
            quiet += 1;
        } else {
            quiet = 0;
        }
    }
    warmed
}

// ---------------------------------------------------------------------------
// Per-hart current-thread-payload registry
// ---------------------------------------------------------------------------

/// Maximum hart count the per-hart slot table addresses. Matches the
/// 64-bit `tx_hal::CpuMask` width used elsewhere in the kernel.
pub const MAX_THREAD_PAYLOAD_HARTS: usize = 64;

/// Per-hart slot of "the `ThreadPayload` whose future the reactor on
/// this hart is currently polling". Set by the reactor task wrapper
/// for a thread future before `Future::poll`, cleared after poll
/// returns. The trap shell consults this to resolve the userspace-run
/// wait owned by the trapping thread.
///
/// Today there is no SMP-generic `PerCpu<T>` primitive in
/// tx-substrate; this slot table is a deliberately narrow facsimile.
/// When a richer per-cpu pattern lands the storage can be replaced
/// without changing the public accessor signatures.
struct ThreadPayloadSlots {
    slots: [SpinMutex<Option<PayloadCap<ThreadPayload>>>; MAX_THREAD_PAYLOAD_HARTS],
}

impl ThreadPayloadSlots {
    const fn new() -> Self {
        // Initialise via a `const fn`-friendly literal: each slot is a
        // `SpinMutex<Option<...>>::new(None)`. The repetition pattern
        // requires `Copy` which `SpinMutex` is not, so spell out 64
        // entries via a macro-shaped helper.
        #[allow(clippy::declare_interior_mutable_const)]
        const NIL: SpinMutex<Option<PayloadCap<ThreadPayload>>> = SpinMutex::new(None);
        Self {
            slots: [NIL; MAX_THREAD_PAYLOAD_HARTS],
        }
    }
}

static CURRENT_THREAD_PAYLOAD: ThreadPayloadSlots = ThreadPayloadSlots::new();
static CURRENT_USERSPACE_PAYLOAD: ThreadPayloadSlots = ThreadPayloadSlots::new();

struct ThreadIdentitySlots {
    slots: [SpinMutex<Option<Cap<ThreadIdentity>>>; MAX_THREAD_PAYLOAD_HARTS],
}

impl ThreadIdentitySlots {
    const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const NIL: SpinMutex<Option<Cap<ThreadIdentity>>> = SpinMutex::new(None);
        Self {
            slots: [NIL; MAX_THREAD_PAYLOAD_HARTS],
        }
    }
}

static CURRENT_THREAD_IDENTITY: ThreadIdentitySlots = ThreadIdentitySlots::new();
static CURRENT_USERSPACE_THREAD_IDENTITY: ThreadIdentitySlots = ThreadIdentitySlots::new();
// These counters are post-mortem breadcrumbs, not part of the userspace-entry
// protocol.  Keeping them live in an optimized kernel makes every hart update
// the same cache lines twice per syscall/trap round trip.  Preserve the probe
// for debug kernels while compiling the contended writes out of benchmark and
// submission builds.
static LAST_USERSPACE_SET_HART: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_USERSPACE_CLEAR_HART: AtomicU64 = AtomicU64::new(u64::MAX);
static USERSPACE_SET_COUNT: AtomicU64 = AtomicU64::new(0);
static USERSPACE_CLEAR_COUNT: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
fn trace_userspace_payload_set(hart: usize) {
    #[cfg(debug_assertions)]
    {
        LAST_USERSPACE_SET_HART.store(hart as u64, Ordering::Relaxed);
        USERSPACE_SET_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(not(debug_assertions))]
    let _ = hart;
}

#[inline(always)]
fn trace_userspace_payload_clear(hart: usize) {
    #[cfg(debug_assertions)]
    {
        LAST_USERSPACE_CLEAR_HART.store(hart as u64, Ordering::Relaxed);
        USERSPACE_CLEAR_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(not(debug_assertions))]
    let _ = hart;
}

/// Return the `PayloadCap<ThreadPayload>` registered for `hart`, or
/// `None` if no thread future is currently driving on that hart.
///
/// Cloning the `PayloadCap` is safe across `.await` per zone Cap
/// invariants — the returned handle does not borrow from a guard.
pub fn current_thread_payload(hart: usize) -> Option<PayloadCap<ThreadPayload>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_THREAD_PAYLOAD.slots[hart].lock().clone()
}

pub fn current_thread_identity(hart: usize) -> Option<Cap<ThreadIdentity>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_THREAD_IDENTITY.slots[hart].lock().clone()
}

/// Return the payload that most recently entered userspace on `hart`.
///
/// Unlike [`current_thread_payload`], this slot spans the machine userspace
/// round-trip rather than only the Rust `Future::poll` call. Timer preemption
/// may unwind the poll boundary before a later user fault is delivered; the
/// trap shell still needs a stable payload anchor to hand that fault back to
/// the owning thread future.
pub fn current_userspace_payload(hart: usize) -> Option<PayloadCap<ThreadPayload>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_USERSPACE_PAYLOAD.slots[hart].lock().clone()
}

pub fn current_userspace_thread_identity(hart: usize) -> Option<Cap<ThreadIdentity>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_USERSPACE_THREAD_IDENTITY.slots[hart].lock().clone()
}

pub fn current_thread_payload_mask() -> u64 {
    payload_slot_mask(&CURRENT_THREAD_PAYLOAD)
}

pub fn current_userspace_payload_mask() -> u64 {
    payload_slot_mask(&CURRENT_USERSPACE_PAYLOAD)
}

/// Install `payload` as the current thread payload on `hart`. The
/// reactor task wrapper that drives a thread future is expected to
/// call this before each poll and pair it with
/// [`clear_current_thread_payload`] after the poll completes.
///
/// Returns the previously installed `PayloadCap`, if any. A `Some`
/// return is a programming error (poll re-entrancy on the same hart);
/// callers should panic or assert in debug builds.
pub fn set_current_thread_payload(
    hart: usize,
    payload: PayloadCap<ThreadPayload>,
) -> Option<PayloadCap<ThreadPayload>> {
    assert!(
        hart < MAX_THREAD_PAYLOAD_HARTS,
        "hart {hart} exceeds MAX_THREAD_PAYLOAD_HARTS",
    );
    let mut slot = CURRENT_THREAD_PAYLOAD.slots[hart].lock();
    let prev = slot.clone();
    *slot = Some(payload);
    prev
}

pub fn set_current_thread_identity(
    hart: usize,
    thread: Cap<ThreadIdentity>,
) -> Option<Cap<ThreadIdentity>> {
    assert!(
        hart < MAX_THREAD_PAYLOAD_HARTS,
        "hart {hart} exceeds MAX_THREAD_PAYLOAD_HARTS",
    );
    let mut slot = CURRENT_THREAD_IDENTITY.slots[hart].lock();
    let prev = slot.clone();
    *slot = Some(thread);
    prev
}

/// Clear the current thread payload on `hart`. Returns whatever was
/// installed, if any.
pub fn clear_current_thread_payload(hart: usize) -> Option<PayloadCap<ThreadPayload>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_THREAD_PAYLOAD.slots[hart].lock().take()
}

pub fn clear_current_thread_identity(hart: usize) -> Option<Cap<ThreadIdentity>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_THREAD_IDENTITY.slots[hart].lock().take()
}

/// Install `payload` as the userspace-running payload on `hart`.
pub fn set_current_userspace_payload(
    hart: usize,
    payload: PayloadCap<ThreadPayload>,
) -> Option<PayloadCap<ThreadPayload>> {
    assert!(
        hart < MAX_THREAD_PAYLOAD_HARTS,
        "hart {hart} exceeds MAX_THREAD_PAYLOAD_HARTS",
    );
    let mut slot = CURRENT_USERSPACE_PAYLOAD.slots[hart].lock();
    let prev = slot.clone();
    *slot = Some(payload);
    trace_userspace_payload_set(hart);
    prev
}

pub fn set_current_userspace_thread_identity(
    hart: usize,
    thread: Cap<ThreadIdentity>,
) -> Option<Cap<ThreadIdentity>> {
    assert!(
        hart < MAX_THREAD_PAYLOAD_HARTS,
        "hart {hart} exceeds MAX_THREAD_PAYLOAD_HARTS",
    );
    let mut slot = CURRENT_USERSPACE_THREAD_IDENTITY.slots[hart].lock();
    let prev = slot.clone();
    *slot = Some(thread);
    prev
}

/// Clear the userspace-running payload on `hart`.
pub fn clear_current_userspace_payload(hart: usize) -> Option<PayloadCap<ThreadPayload>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    let cleared = CURRENT_USERSPACE_PAYLOAD.slots[hart].lock().take();
    trace_userspace_payload_clear(hart);
    cleared
}

pub fn clear_current_userspace_thread_identity(hart: usize) -> Option<Cap<ThreadIdentity>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_USERSPACE_THREAD_IDENTITY.slots[hart].lock().take()
}

pub(crate) fn clear_thread_slots_for(
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
) {
    let thread_key = thread.key();
    let payload_key = payload.key();

    let mut hart = 0;
    while hart < MAX_THREAD_PAYLOAD_HARTS {
        clear_payload_slot_if_matches(&CURRENT_THREAD_PAYLOAD, hart, payload_key);
        clear_identity_slot_if_matches(&CURRENT_THREAD_IDENTITY, hart, thread_key);
        if clear_payload_slot_if_matches(&CURRENT_USERSPACE_PAYLOAD, hart, payload_key) {
            trace_userspace_payload_clear(hart);
        }
        clear_identity_slot_if_matches(&CURRENT_USERSPACE_THREAD_IDENTITY, hart, thread_key);
        hart += 1;
    }
}

pub fn userspace_payload_trace_counters() -> (u64, u64, u64, u64) {
    (
        LAST_USERSPACE_SET_HART.load(Ordering::Relaxed),
        LAST_USERSPACE_CLEAR_HART.load(Ordering::Relaxed),
        USERSPACE_SET_COUNT.load(Ordering::Relaxed),
        USERSPACE_CLEAR_COUNT.load(Ordering::Relaxed),
    )
}

fn payload_slot_mask(slots: &ThreadPayloadSlots) -> u64 {
    let mut mask = 0u64;
    let mut hart = 0;
    while hart < MAX_THREAD_PAYLOAD_HARTS {
        if slots.slots[hart].lock().is_some() {
            mask |= 1u64 << hart;
        }
        hart += 1;
    }
    mask
}

fn clear_payload_slot_if_matches(
    slots: &ThreadPayloadSlots,
    hart: usize,
    key: crate::thread_runtime::adapter::step_engine::SlotKey,
) -> bool {
    let mut slot = slots.slots[hart].lock();
    if slot.as_ref().map(PayloadCap::key) == Some(key) {
        *slot = None;
        true
    } else {
        false
    }
}

fn clear_identity_slot_if_matches(
    slots: &ThreadIdentitySlots,
    hart: usize,
    key: crate::thread_runtime::adapter::step_engine::SlotKey,
) -> bool {
    let mut slot = slots.slots[hart].lock();
    if slot.as_ref().map(Cap::key) == Some(key) {
        *slot = None;
        true
    } else {
        false
    }
}

static THREAD_IDENTITY_ZONE: Zone<ThreadIdentity> = Zone::const_new();
static THREAD_PAYLOAD_ZONE: Zone<ThreadPayload> = Zone::const_new();

unsafe impl ZoneAllocated for ThreadIdentity {
    fn zone() -> &'static Zone<Self> {
        &THREAD_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for ThreadPayload {
    type Policy = PayloadPolicy<Self>;
    fn zone() -> &'static Zone<Self> {
        &THREAD_PAYLOAD_ZONE
    }
}

pub fn allocate_tid() -> Tid {
    crate::process::numbers::allocate_tid()
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn reset_tid_counter_for_test() {
    crate::process::numbers::reset_pid_counter_for_test();
}
