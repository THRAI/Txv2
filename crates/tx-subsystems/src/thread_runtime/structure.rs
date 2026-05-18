//! Thread-runtime subsystem structure: identities and payloads.
//!
//! Per `THREAD_RUNTIME_v1`, the thread is the unit of execution: each
//! thread owns its own future + reactor task. This module realizes the
//! identity/payload split, including the per-thread signal mask and
//! pending queue. The realtime per-occurrence queue and `signal_summary`
//! fast-check atomic land alongside the delivery pass.

use alloc::sync::Weak as ArcWeak;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use tx_hal::UserTrapContext;

use crate::thread_runtime::adapter::reactor_entry::{
    TaskKey, UserspaceRunRequest, UserspaceRunSlot,
};
use crate::thread_runtime::adapter::step_engine::{
    Dead, Entity, PayloadCap, PayloadPolicy, SpinMutex, TaskMailbox, Weak, Zone, ZoneAllocated,
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

    /// Snapshot the owning process via `Weak::upgrade` under a fresh
    /// guard. Returns `None` if the process identity has been dropped.
    pub fn upgrade_owner_proc(
        &self,
    ) -> Option<crate::thread_runtime::adapter::step_engine::Cap<ProcessIdentity>> {
        let guard = crate::thread_runtime::adapter::step_engine::guard();
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
    /// `InterruptSummary` packed into 8 bits, kept current by
    /// `post_signal`, `step_sigprocmask`, `step_thread_exit`, and the
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
    /// Saved signal context: the `UserTrapContext` that was active
    /// before the most recent handler delivery.  Written by the AST
    /// checkpoint in `thread_future` when `DeliverHandler` fires;
    /// consumed by `sys_rt_sigreturn` to restore the original
    /// execution state.  `None` when no handler is currently
    /// executing.
    pub(crate) saved_signal_context: SpinMutex<Option<UserTrapContext>>,
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
    /// **D9-A (lost-wake fix for signals).** `post_signal`,
    /// `route_gewalt`, and `set_thread_zombie` post a
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
    /// Thread stop flag.  Set by `route_gewalt(SIGSTOP)` /
    /// `DefaultStop` AST materialisation; cleared by
    /// `route_gewalt(SIGCONT)`.  When `true`, the thread must not
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
    /// a wait-source parked by the reactor, woken by SIGCONT's
    /// `route_gewalt` clearing the flag + posting to the thread's
    /// mailbox.
    ///
    /// Atomic because `route_gewalt` sets/clears it under the
    /// thread-list lock, and the AST checkpoint reads it from the
    /// thread future without acquiring the payload lock.
    /// See: `txdoc:SIGNAL-V1-S12-3-ROUTE-GEWALT-STOP`.
    pub(crate) stopped: core::sync::atomic::AtomicBool,
    /// Alternate signal stack (`sigaltstack(2)`).  `None` means
    /// "no alternate stack" (deliver on the normal stack).
    /// `Some((base, size))` gives the alternate stack range.
    pub(crate) alt_stack: SpinMutex<Option<(usize, usize)>>,
    /// `clear_child_tid` pointer from `set_tid_address`.  Written
    /// atomically to 0 on thread exit when futex wake is supported.
    pub clear_child_tid: SpinMutex<Option<u64>>,
}

impl ThreadPayload {
    /// Build a `ThreadPayload` with a fresh `UserspaceRunSlot` and all
    /// trap-handoff state cleared. Used by `sign_thread` and tests.
    pub fn fresh() -> Self {
        Self {
            task: SpinMutex::new(None),
            signal_mask: AtomicU64::new(0),
            thread_pending: PendingSignalQueue::new(),
            signal_summary: AtomicU8::new(0),
            userspace_slot: UserspaceRunSlot::new(),
            active_request: SpinMutex::new(None),
            saved_user_context: SpinMutex::new(None),
            saved_signal_context: SpinMutex::new(None),
            pending_syscall_return: SpinMutex::new(None),
            mailbox: SpinMutex::new(None),
            stopped: core::sync::atomic::AtomicBool::new(false),
            alt_stack: SpinMutex::new(None),
            clear_child_tid: SpinMutex::new(None),
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

    /// Snapshot the reactor task handle, if one has been bound. Always
    /// `None` until the reactor coupling lands.
    pub fn task(&self) -> Option<TaskKey> {
        *self.task.lock()
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

    /// Replace the saved signal context. Called by signal delivery
    /// to preserve the pre-handler context for `rt_sigreturn`.
    pub fn store_saved_signal_context(&self, ctx: Option<UserTrapContext>) {
        *self.saved_signal_context.lock() = ctx;
    }

    /// Take (consume) the saved signal context. Called by
    /// `rt_sigreturn` to retrieve the pre-handler context for
    /// restoration into `saved_user_context`. Returns `None` if no
    /// signal frame is in flight (stray `rt_sigreturn` call).
    pub fn take_saved_signal_context(&self) -> Option<UserTrapContext> {
        self.saved_signal_context.lock().take()
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

    /// Whether this thread is stopped (SIGSTOP / default-Stop
    /// disposition). The AST checkpoint in thread_future uses this
    /// to decide whether to enter userspace.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Set or clear the stopped flag. Used by
    /// `route_gewalt(SIGSTOP)` (set) and `route_gewalt(SIGCONT)`
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

    /// Snapshot the current interrupt summary.
    pub fn interrupt_summary(&self) -> InterruptSummary {
        InterruptSummary::unpack(self.signal_summary.load(Ordering::Acquire))
    }

    /// Atomic read-modify-write on the packed summary bits. Used by
    /// `post_signal`, `step_sigprocmask`, etc., to keep the summary
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

use crate::signal::adapter::wait_routing::InterruptSource;

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

/// Clear the current thread payload on `hart`. Returns whatever was
/// installed, if any.
pub fn clear_current_thread_payload(hart: usize) -> Option<PayloadCap<ThreadPayload>> {
    if hart >= MAX_THREAD_PAYLOAD_HARTS {
        return None;
    }
    CURRENT_THREAD_PAYLOAD.slots[hart].lock().take()
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

/// Simple atomic TID allocator. TID 1 reserved for the init leader by
/// convention; allocator starts at 2.
static NEXT_TID: AtomicU32 = AtomicU32::new(2);

pub fn allocate_tid() -> Tid {
    Tid(NEXT_TID.fetch_add(1, Ordering::Relaxed))
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn reset_tid_counter_for_test() {
    NEXT_TID.store(2, Ordering::Relaxed);
}
