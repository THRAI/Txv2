//! AIO — `aio_context_t` fd-table scaffold + iocb submission queue +
//! worker dispatch + completion ring (PR-11 phases 1–5).
//!
//! Spec:
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution scope)
//! - `docs/Txv3/07_BLAST_RADIUS.md` §4 row J, §5.2 PR-11
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §7 (phase plan)
//!
//! # Phase 1 surface
//!
//! 1. The zone-allocated [`AioContext`] payload.
//! 2. A stable `context_id` minted at construction — used in later
//!    phases as the addressing key for iocb submission and completion
//!    routing (mirrors W-Q's `ufd_id` discipline on `UserfaultFd`).
//! 3. The [`register_zones`] hook called from
//!    [`crate::zones::register_all`].
//!
//! # Phase 2 additions
//!
//! 4. The [`Iocb`] struct (Linux `struct iocb` shape, narrowed to the
//!    fields the worker dispatch reads).
//! 5. A bounded submission queue on each [`AioContext`]
//!    ([`AioContext::push_iocb`]) protected by a [`SpinMutex`].
//! 6. An [`Arc<WaitSource>`] (`iocb_arrived`) every push fires so the
//!    worker future can park on it between iocbs (mirrors the pipe
//!    pattern in `pipe.rs`).
//! 7. A worker abort signal ([`AbortSignal`]) so `io_destroy` (and the
//!    future `Drop for AioContext` impl) can trip the worker's
//!    `with_on_behalf_of` borrow body so the body's next yield
//!    resolution observes a `Killed`-equivalent abort and terminates.
//! 8. A per-context iocb-dispatch counter ([`AioContext::dispatched`])
//!    — incremented for every iocb the worker body consumes; phase 3+
//!    couples the increment to a real completion-event push.
//! 9. The worker spawn helper [`spawn_worker_for_context`] which
//!    constructs the worker future (the borrow's body wrapped in
//!    `with_on_behalf_of`).
//!
//! # Phase 3+4+5 additions (this revision)
//!
//! 10. The [`IoEvent`] struct mirroring Linux's `struct io_event`
//!     (`{ data, obj, res, res2 }`).
//! 11. A per-context completion queue
//!     ([`AioContext::completion_queue`]) protected by a [`SpinMutex`].
//!     Pushed by the worker on each iocb dispatch, drained by
//!     `sys_io_getevents`.
//! 12. An [`Arc<WaitSource>`] (`events_available`) every completion
//!     push fires so `sys_io_getevents` blocked on `min_nr` can wake
//!     and re-check the queue.
//! 13. An [`IocbDispatcher`] callback parameter on
//!     [`spawn_worker_for_context`] — the syscall arm injects a
//!     concrete closure that resolves `aio_fildes` against P's fd
//!     table and copies bytes through P's address space. The body's
//!     dispatch step is generic over `I` but the closure is concrete
//!     over `ProcessIdentity`, so the AIO subsystem stays generic
//!     while the dispatch path resolves through the principal's
//!     real fd table / aspace.
//! 14. [`AioContext::push_completion`] / [`AioContext::pop_completion`]
//!     for the worker-side push and the `sys_io_getevents` drain.
//!
//! # Linux-divergence note
//!
//! Linux's `io_setup(nr_events, &aio_context_t)` writes back an opaque
//! `u64`-pointer-shape value into the user's `aio_context_t *`
//! out-parameter (the user-virtual address of the ring buffer the
//! kernel maps into the caller's address space). **We diverge
//! intentionally.** Per D8 §4.1, `aio_context_t` is normalized to a
//! real fd via [`crate::vfs::structure::OpenFileBacking::AioContext`]
//! — joining the Rnode/Ufd pattern from W-Q's PR-10 phase 0 landing.
//! Userspace glibc shims that expect the pointer-shape bridge the
//! returned fd into the legacy `aio_context_t` slot (a 5-line shim);
//! the divergence is acknowledged at the userspace boundary, and
//! `io_destroy` becomes structurally identical to `close(2)` on the
//! AIO fd (cap-zone drop drives endpoint abandonment via the same
//! `exit_source` mechanism that phase 4 wires).
//!
//! # Drop semantics
//!
//! `AioContext` carries no externally-registered state today, so the
//! default drop is sufficient for the queue and the wait source. When
//! phase 4 wires the worker task and the completion ring, the drop
//! will additionally:
//! - signal the worker's `exit_source` (so the `with_on_behalf_of`
//!   borrow body observes `Killed` on its next yield and the worker
//!   aborts with `EOWNERDEAD`),
//! - drain any in-flight iocbs with cancellation outcomes,
//! - release the user-mmapped completion ring.
//!
//! Phase 2 already trips the worker `abort_signal` when the context
//! drops (via [`AioContext::abort_worker`]); the iocb queue's
//! `VecDeque` and the `Arc<WaitSource>` clean up through normal
//! `Drop`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};

pub mod adapter;
pub mod notification;
pub use notification::{EVENTS_AVAILABLE_MASK, IOCB_ARRIVED_MASK};

use adapter::step_engine::{
    sign, with_on_behalf_of, AbortSignal, CancelReason, Cap, OnBehalfOfAbort, ScriptCtx, SpinMutex,
    SubjectContext, SubjectIdentity, WaitSource, Zone, ZoneAllocated, ZoneError,
};

// === iocb opcodes ====================================================
//
// Linux's `IOCB_CMD_*` enumeration. Phase 2 admits the four read/write
// variants; later phases (FSYNC, FDSYNC, POLL) layer on without
// changing the queue / worker shape.

/// Linux `IOCB_CMD_PREAD`. Read into a single user buffer at a given
/// offset; phase 2 stubs the dispatch.
pub const IOCB_CMD_PREAD: u16 = 0;
/// Linux `IOCB_CMD_PWRITE`. Write a single user buffer at a given
/// offset; phase 2 stubs the dispatch.
pub const IOCB_CMD_PWRITE: u16 = 1;
/// Linux `IOCB_CMD_FSYNC`. Phase 2 stubs the dispatch.
pub const IOCB_CMD_FSYNC: u16 = 2;
/// Linux `IOCB_CMD_FDSYNC`. Phase 2 stubs the dispatch.
pub const IOCB_CMD_FDSYNC: u16 = 3;
/// Linux `IOCB_CMD_NOOP`. Reserved; kept here so the validation set is
/// the same shape Linux exposes.
pub const IOCB_CMD_NOOP: u16 = 6;
/// Linux `IOCB_CMD_PREADV`. Phase 2 admits but stubs the dispatch.
pub const IOCB_CMD_PREADV: u16 = 7;
/// Linux `IOCB_CMD_PWRITEV`. Phase 2 admits but stubs the dispatch.
pub const IOCB_CMD_PWRITEV: u16 = 8;

/// Returns `true` iff `opcode` is one of the known `IOCB_CMD_*` values.
/// Phase 2's submission validation rejects unknown opcodes with
/// `EINVAL` (the syscall arm surfaces the rejection as a return-count
/// short-circuit per `man 2 io_submit`).
pub fn is_valid_iocb_opcode(opcode: u16) -> bool {
    matches!(
        opcode,
        IOCB_CMD_PREAD
            | IOCB_CMD_PWRITE
            | IOCB_CMD_FSYNC
            | IOCB_CMD_FDSYNC
            | IOCB_CMD_NOOP
            | IOCB_CMD_PREADV
            | IOCB_CMD_PWRITEV
    )
}

// === iocb struct ======================================================

/// Kernel-side view of a Linux `struct iocb`.
///
/// The Linux ABI carries more fields (aio_reqprio, aio_resfd, etc.); we
/// narrow to the fields the phase-2 worker reads. Phase 3+ extends the
/// struct in lockstep with the dispatched ops it teaches the worker
/// body to handle.
///
/// Fields mirror Linux:
/// - `aio_fildes`: target fd in the submitter's fd table (`u32`
///   matches the kernel's per-process fd numbering).
/// - `aio_lio_opcode`: one of the [`IOCB_CMD_*`] constants.
/// - `aio_buf`: user-VA of the buffer (read into / write from). Cast
///   to/from `*mut u8` at the dispatch site under the borrow's
///   `SubjectContext.process`'s address space.
/// - `aio_nbytes`: byte length of the buffer.
/// - `aio_offset`: file offset for PREAD/PWRITE; ignored for FSYNC.
/// - `aio_data`: opaque `u64` cookie userspace stashes in the iocb;
///   the kernel returns it verbatim in the completion event. Phase 2
///   stores it; phase 3 echoes it into the completion-ring slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Iocb {
    pub aio_fildes: u32,
    pub aio_lio_opcode: u16,
    pub aio_buf: u64,
    pub aio_nbytes: u64,
    pub aio_offset: i64,
    pub aio_data: u64,
}

impl Iocb {
    /// Construct an `Iocb` from the wire-shape `(opcode, fd, buf,
    /// nbytes, offset, data)`. Used by the `sys_io_submit` arm after
    /// copy-from-user. Validates only opcode here; fd / aspace
    /// validation happens at dispatch time inside the worker (the
    /// borrow's `SubjectContext` is the right principal then).
    pub fn new(
        aio_lio_opcode: u16,
        aio_fildes: u32,
        aio_buf: u64,
        aio_nbytes: u64,
        aio_offset: i64,
        aio_data: u64,
    ) -> Self {
        Self {
            aio_fildes,
            aio_lio_opcode,
            aio_buf,
            aio_nbytes,
            aio_offset,
            aio_data,
        }
    }
}

// === io_event struct =================================================

/// Kernel-side view of a Linux `struct io_event` — the completion-ring
/// record posted by the worker and drained by `sys_io_getevents(2)`.
///
/// Fields mirror Linux's UAPI layout (`<linux/aio_abi.h>`):
///
/// - `data`: the iocb's `aio_data` cookie, returned verbatim so
///   userspace can correlate the completion back to its submission.
/// - `obj`: kernel iocb address — a placeholder pointer-shaped value
///   for now. Linux exposes the in-kernel iocb pointer here so
///   userspace can match a `struct iocb*` it submitted against the
///   event. We use the iocb's `aio_data` again (or zero) as the
///   placeholder; phase 6 may stash a per-iocb id if Linux conformance
///   needs the real pointer.
/// - `res`: primary result — bytes read/written on success, a negative
///   errno value (Linux convention) on failure. `i64` so positive byte
///   counts up to 2^63-1 and negative errnos both fit.
/// - `res2`: secondary result — Linux uses for `PREADV`/`PWRITEV`
///   sub-counts. We always set 0 in phase 3.
///
/// **Wire layout (32 bytes, RV64):** `data: u64 @ 0`, `obj: u64 @ 8`,
/// `res: i64 @ 16`, `res2: i64 @ 24`. The `sys_io_getevents` arm
/// serialises this layout into user memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoEvent {
    pub data: u64,
    pub obj: u64,
    pub res: i64,
    pub res2: i64,
}

impl IoEvent {
    /// Construct an `IoEvent` for the canary phase. `obj` defaults to
    /// the iocb's `aio_data` (placeholder for the kernel iocb pointer
    /// Linux exposes).
    pub const fn new(data: u64, obj: u64, res: i64, res2: i64) -> Self {
        Self {
            data,
            obj,
            res,
            res2,
        }
    }

    /// Serialise into Linux's UAPI wire layout (32 bytes, LE).
    pub fn to_le_bytes(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[0..8].copy_from_slice(&self.data.to_le_bytes());
        out[8..16].copy_from_slice(&self.obj.to_le_bytes());
        out[16..24].copy_from_slice(&self.res.to_le_bytes());
        out[24..32].copy_from_slice(&self.res2.to_le_bytes());
        out
    }
}

/// Number of bytes one [`IoEvent`] occupies on the wire. Pinned by
/// `IoEvent::to_le_bytes` and consumed by `sys_io_getevents`.
pub const IO_EVENT_BYTES: usize = 32;

// === context_id minting ==============================================

/// Monotonic counter for [`AioContext::context_id`]. The id is the
/// addressing key later phases use to route iocb submissions and
/// completion events. Starts at 1 so 0 can serve as "no context" if a
/// future caller ever needs a sentinel. Mirrors W-Q's `NEXT_UFD_ID`
/// discipline on `UserfaultFd`.
static NEXT_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_context_id() -> u64 {
    NEXT_CONTEXT_ID.fetch_add(1, Ordering::AcqRel)
}

// === payload =========================================================

/// AIO context payload — zone-allocated per `Cap<AioContext>`.
///
/// **Phase 2 surface.** The struct now carries the per-context
/// submission queue + wake source + worker abort signal + dispatched
/// counter. Phase 3 will add the user-mmapped completion ring and the
/// per-context worker task handle; the owning process's
/// `Cap<ProcessIdentity>` is recorded by the syscall arm at
/// `io_setup`-time when the worker is spawned (phase 3+ will move
/// ownership into the struct itself once the seam shape settles).
///
/// Capacity bookkeeping: `nr_events` is captured verbatim from the
/// `io_setup(nr_events, _)` argument. Linux clamps to a per-system
/// max (`/proc/sys/fs/aio-max-nr`); we don't clamp in phase 2 — the
/// queue allocation grows lazily (a `VecDeque` rather than a fixed
/// ring); `push_iocb` enforces the per-context `nr_events` bound.
pub struct AioContext {
    /// Monotonic per-context id. Stable for the lifetime of the
    /// `Cap<AioContext>` — minted once at construction and never
    /// reassigned. Used by the iocb-routing fast paths in phases 3+
    /// (e.g. completion-event addressing).
    context_id: u64,
    /// Number of in-flight events the user requested at `io_setup`
    /// time. Phase 2 uses this as the per-context outstanding bound on
    /// [`Self::push_iocb`]: a submission that would overflow returns
    /// `Err(iocb)` so the syscall arm can short-circuit.
    nr_events: u32,
    /// Reserved for future state (worker task slot, completion ring,
    /// owner cap). Kept as a `u32` so the struct's size matches the
    /// eventual ~48-byte payload without churning the field layout
    /// when phase 3 lands. Production callers do not read this field.
    _pad: u32,
    /// Submission queue. Phase 2 uses a `VecDeque<Iocb>` because the
    /// queue's high-water mark is bounded by `nr_events` (a `u32`),
    /// and the worker drains FIFO. Phase 3+ may swap to a bounded
    /// `MpscQueue` once the contention shape across multiple
    /// submitters is measured.
    submit_queue: SpinMutex<VecDeque<Iocb>>,
    /// Wait source notified on every `push_iocb`. The worker future
    /// parks on this between iocbs; the production wiring binds the
    /// worker's `TaskMailbox` to this source via
    /// `WaitSource::prepare(...).install_if(...)` (same pattern pipe
    /// uses in `pipe.rs`). Phase 2's test pump polls the worker
    /// directly so the wait machinery is exercised through `notify`
    /// without needing a live reactor.
    iocb_arrived: Arc<WaitSource>,
    /// Worker abort signal. The worker body checks this between
    /// iocbs (and the racer inside `with_on_behalf_of` also checks
    /// it). `io_destroy` / `Drop for AioContext` (phase 4) trip the
    /// signal to terminate the worker; phase 2 tests trip it via
    /// [`Self::abort_worker`].
    worker_abort: Arc<AbortSignal>,
    /// Monotonic count of iocbs the worker body has dequeued and
    /// "dispatched" (phase 2: just incremented; phase 3 wires the real
    /// op). Tests poll this to confirm the worker actually drained the
    /// queue under the borrow.
    dispatched: AtomicU64,
    /// Cached id for [`Self::iocb_arrived`] — kept here so callers can
    /// publish the id into a `YieldShape::OnWaitSource` without an
    /// extra `Arc` deref.
    iocb_arrived_id: u64,
    /// Completion queue. Per D8 §4.2: Linux uses a user-mmapped
    /// completion ring; phase 4 lands a kernel-side `VecDeque<IoEvent>`
    /// drained by `sys_io_getevents`. A future PR may swap to a
    /// user-mmapped ring once the address-space wiring for cross-task
    /// shared regions matures; the kernel-side queue is the smallest
    /// viable shape that preserves the io_getevents semantics.
    completion_queue: SpinMutex<VecDeque<IoEvent>>,
    /// Wait source notified on every `push_completion`. Blocked
    /// `sys_io_getevents` waiters park on this carrier; the push wakes
    /// them so they can re-check the queue.
    events_available: Arc<WaitSource>,
    /// Cached id for [`Self::events_available`].
    events_available_id: u64,
}

impl core::fmt::Debug for AioContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AioContext")
            .field("context_id", &self.context_id)
            .field("nr_events", &self.nr_events)
            .field("queue_len", &self.queue_len())
            .field("dispatched", &self.dispatched.load(Ordering::Acquire))
            .finish()
    }
}

impl AioContext {
    /// Construct a fresh AIO context payload without zone-signing it.
    /// Tests and the `sys_io_setup(2)` arm should prefer
    /// [`Self::new_with_nr_events_cap`]; this lower-level constructor
    /// exists so the zone-allocation step can be deferred.
    pub fn new() -> Self {
        Self::with_nr_events(0)
    }

    /// Construct a fresh AIO context payload with the given
    /// `nr_events` capacity. Mirrors `UserfaultFd::with_flags` from
    /// W-Q's phase 0 template.
    pub fn with_nr_events(nr_events: u32) -> Self {
        let wait_points = notification::new_wait_points();
        Self {
            context_id: allocate_context_id(),
            nr_events,
            _pad: 0,
            submit_queue: SpinMutex::new(VecDeque::new()),
            iocb_arrived: wait_points.iocb_arrived,
            iocb_arrived_id: wait_points.iocb_arrived_id,
            worker_abort: Arc::new(AbortSignal::new()),
            dispatched: AtomicU64::new(0),
            completion_queue: SpinMutex::new(VecDeque::new()),
            events_available: wait_points.events_available,
            events_available_id: wait_points.events_available_id,
        }
    }

    /// Zone-sign a fresh AIO context payload (default `nr_events = 0`).
    /// Returns the `Cap<AioContext>` the caller installs into a
    /// process's fd table (wrapped in an `OpenFile` whose backing is
    /// [`crate::vfs::structure::OpenFileBacking::AioContext`]). On
    /// success the cap's [`Self::context_id`] is the stable id later
    /// phases use as the iocb-routing key.
    pub fn new_cap() -> Result<Cap<Self>, ZoneError> {
        Self::new_with_nr_events_cap(0)
    }

    /// Zone-sign a fresh AIO context with the given `nr_events`.
    /// Companion to [`Self::with_nr_events`] for the
    /// `sys_io_setup(2)` arm.
    pub fn new_with_nr_events_cap(nr_events: u32) -> Result<Cap<Self>, ZoneError> {
        sign(Self::with_nr_events(nr_events))
    }

    /// Snapshot the stable per-context id. Used as the iocb-routing
    /// key by completion-event addressing in phase 3+.
    pub const fn context_id(&self) -> u64 {
        self.context_id
    }

    /// Snapshot the user-requested `nr_events` capacity captured at
    /// `sys_io_setup(2)` time. Phase 3 will use this to size the
    /// submission queue and the completion ring.
    pub const fn nr_events(&self) -> u32 {
        self.nr_events
    }

    /// Current depth of the submission queue. Tests inspect this to
    /// confirm `push_iocb` bookkeeping; phase 3+ may expose a public
    /// fast-path snapshot too.
    pub fn queue_len(&self) -> usize {
        self.submit_queue.lock().len()
    }

    /// Wait-source id paired with [`Self::iocb_arrived`]. Stable for
    /// the lifetime of the cap. Used by the worker future's wait-park
    /// step (phase 2 ticks the source directly; phase 3+ wires it
    /// through `YieldShape::OnWaitSource` for a real reactor park).
    pub const fn iocb_arrived_id(&self) -> u64 {
        self.iocb_arrived_id
    }

    /// Borrow the iocb-arrival wait source. Mirrors the pipe pattern
    /// (`reader_wait_source` / `writer_wait_source`). Future-PR-3D
    /// callers register against this source via
    /// `WaitSource::prepare(...).install_if(...)`.
    pub fn iocb_arrived_source(&self) -> &Arc<WaitSource> {
        &self.iocb_arrived
    }

    /// Number of iocbs the worker body has dequeued + "dispatched"
    /// (phase 2 stub). Tests use this to pin "the worker actually
    /// drained the queue under the borrow" without depending on a
    /// completion-ring read (which doesn't exist yet).
    pub fn dispatched(&self) -> u64 {
        self.dispatched.load(Ordering::Acquire)
    }

    /// Push an iocb onto the submission queue. Returns the iocb back
    /// to the caller (as `Err(iocb)`) when the queue is at its
    /// `nr_events` capacity; the syscall arm short-circuits the
    /// remainder of the batch in that case (mirrors Linux's
    /// "io_submit returns the count accepted" behavior).
    ///
    /// On success the iocb-arrived wait source is notified — any
    /// worker future parked on it observes the wake and proceeds to
    /// drain.
    pub fn push_iocb(&self, iocb: Iocb) -> Result<(), Iocb> {
        let mut queue = self.submit_queue.lock();
        if queue.len() >= self.nr_events as usize {
            return Err(iocb);
        }
        queue.push_back(iocb);
        drop(queue);
        notification::notify_iocb_arrived(&self.iocb_arrived);
        Ok(())
    }

    /// Pop the front of the submission queue. Used by the worker
    /// body. Returns `None` when the queue is empty.
    pub fn pop_iocb(&self) -> Option<Iocb> {
        self.submit_queue.lock().pop_front()
    }

    /// Borrow the worker abort signal. The `io_destroy` arm (phase 4)
    /// and the `Drop` impl trip this; phase 2 tests also use it
    /// directly. Returns the shared `Arc` so external code can hold a
    /// clone across `.await` points.
    pub fn worker_abort_signal(&self) -> Arc<AbortSignal> {
        self.worker_abort.clone()
    }

    /// Trip the worker's abort signal with
    /// `OnBehalfOfAbort::PrincipalExited`. Equivalent to the worker's
    /// principal exiting under the borrow; the worker's
    /// `with_on_behalf_of` racer observes the trip and returns
    /// `Err(PrincipalExited)`.
    pub fn abort_worker(&self) {
        self.worker_abort.trip(OnBehalfOfAbort::PrincipalExited);
    }

    /// Trip the worker's abort signal with
    /// `OnBehalfOfAbort::CooperativeCancel(OwnerRequested)`. Used by
    /// `sys_io_destroy(2)` (phase 5) — the worker observes the
    /// cooperative-cancel reason on its next yield and terminates.
    /// Distinct from [`Self::abort_worker`] so the worker can
    /// distinguish a principal exit from a clean `io_destroy`
    /// teardown.
    pub fn cancel_worker(&self) {
        self.worker_abort.trip(OnBehalfOfAbort::CooperativeCancel(
            CancelReason::OwnerRequested,
        ));
    }

    /// Wait-source id paired with [`Self::events_available`]. Stable
    /// for the lifetime of the cap. `sys_io_getevents` parks on this
    /// when `min_nr` is not yet satisfied; the worker's
    /// `push_completion` notify wakes the parked syscall.
    pub const fn events_available_id(&self) -> u64 {
        self.events_available_id
    }

    /// Borrow the events-available wait source.
    pub fn events_available_source(&self) -> &Arc<WaitSource> {
        &self.events_available
    }

    /// Push a completion onto the per-context queue and notify any
    /// `sys_io_getevents` parked on `events_available`. Called by the
    /// worker body after each iocb dispatch.
    pub fn push_completion(&self, event: IoEvent) {
        self.completion_queue.lock().push_back(event);
        notification::notify_events_available(&self.events_available);
    }

    /// Pop one completion event off the queue, if any. Used by
    /// `sys_io_getevents` to drain.
    pub fn pop_completion(&self) -> Option<IoEvent> {
        self.completion_queue.lock().pop_front()
    }

    /// Drain up to `max` completion events into the returned `Vec`.
    /// Returns fewer events than `max` if the queue runs dry. Stable
    /// FIFO order.
    pub fn drain_completions(&self, max: usize) -> alloc::vec::Vec<IoEvent> {
        let mut queue = self.completion_queue.lock();
        let n = core::cmp::min(max, queue.len());
        let mut out = alloc::vec::Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(ev) = queue.pop_front() {
                out.push(ev);
            }
        }
        out
    }

    /// Current depth of the completion queue. Tests use this to assert
    /// the worker actually pushed completions.
    pub fn completion_len(&self) -> usize {
        self.completion_queue.lock().len()
    }
}

impl Default for AioContext {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AioContext {
    fn drop(&mut self) {
        // Trip the worker's abort signal so any still-live worker
        // future observes the abort on its next poll and terminates.
        // Phase 5 wires `sys_io_destroy(2)` and the principal-exit
        // path through the same signal.
        self.worker_abort.trip(OnBehalfOfAbort::CooperativeCancel(
            CancelReason::OwnerRequested,
        ));
        notification::release_wait_points(self.iocb_arrived_id, self.events_available_id);
    }
}

// === zone wiring ======================================================

static AIO_CONTEXT_ZONE: Zone<AioContext> = Zone::const_new();

unsafe impl ZoneAllocated for AioContext {
    fn zone() -> &'static Zone<Self> {
        &AIO_CONTEXT_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    adapter::step_engine::register_zone_for::<AioContext>()?;
    Ok(())
}

// === worker future ====================================================

/// Type alias for the iocb dispatch callback the worker invokes for
/// every iocb it pops off the submission queue.
///
/// The callback is provided by the syscall arm at `io_setup(2)` time
/// and closes over the concrete principal `Cap<ProcessIdentity>` /
/// `Cap<AddressSpace>` needed to resolve `aio_fildes` against P's fd
/// table and copy bytes through P's address space. Keeping the
/// callback boxed lets `spawn_worker_for_context` stay generic over
/// `I: SubjectIdentity` (matching the `with_on_behalf_of` framework
/// surface) while the dispatch site is concrete.
///
/// Returns the [`IoEvent`] to post into the completion queue.
///
/// `Send + Sync + 'static` so the future containing the callback is
/// itself `Send + 'static` (matching `AioWorkerFuture`'s bound).
pub type IocbDispatcher = Arc<dyn Fn(&Iocb) -> IoEvent + Send + Sync + 'static>;

/// Default dispatcher: every iocb completes with `-EINVAL` (i.e. the
/// kernel rejects the op). Used by the framework / unit tests that
/// don't want to wire a real fd-table resolver.
pub fn default_einval_dispatcher() -> IocbDispatcher {
    Arc::new(|iocb: &Iocb| {
        // Linux negative-errno convention: EINVAL = 22.
        IoEvent::new(iocb.aio_data, iocb.aio_data, -22, 0)
    })
}

/// Construct the worker future for an AIO context.
///
/// Per D8 §7 (P-11.4 / P-11.5): "one reactor task per AIO context.
/// Inside that task, iocbs are processed sequentially under a single
/// long-lived borrow." The helper returns the worker's future shape;
/// production wiring submits this future to the reactor via the boot
/// reactor seam.
///
/// **Worker body sketch (phase 3):**
/// 1. `with_on_behalf_of(owner, &owner_subject, body)`.
/// 2. `body` is a single `async move` that loops:
///    - Check `aio.worker_abort` — if tripped, return `Err`.
///    - Try `aio.pop_iocb()` — if `Some(iocb)`, invoke
///      `dispatcher(&iocb)` to get an [`IoEvent`], push the event onto
///      the context's completion queue (firing
///      `events_available`), increment `dispatched`, and continue.
///    - If `None`, yield `Pending`. Re-poll observes either a wake
///      from `iocb_arrived` (push fired the wait source) or the abort
///      signal.
///
/// The dispatcher callback closes over the principal's concrete fd
/// table + aspace so the worker can resolve `aio_fildes` and copy
/// user bytes; see [`IocbDispatcher`].
pub fn spawn_worker_for_context<I>(
    aio_cap: Cap<AioContext>,
    owner_principal: Cap<I>,
    owner_subject: SubjectContext<I>,
    dispatcher: IocbDispatcher,
) -> AioWorkerFuture
where
    I: SubjectIdentity + Send + Sync,
    I::Credential: Send + Sync,
    I::Restrictions: Send + Sync,
    I::ThreadIdentity: Send + Sync,
{
    let worker_abort = aio_cap.worker_abort.clone();
    let aio_cap_for_body = aio_cap.clone();
    // The outer `async move` owns `owner_subject` + `principal` so
    // the by-reference borrow inside `with_on_behalf_of` lives long
    // enough — the by-reference input lifetime is bounded by this
    // outer async block's stack frame.
    let helper = async move {
        let aio_cap_inner = aio_cap_for_body;
        let dispatcher_inner = dispatcher;
        let helper_result = with_on_behalf_of(
            owner_principal,
            &owner_subject,
            move |_ctx: ScriptCtx<I>| async move {
                // Borrow body — per D8 §7 the body loops draining
                // iocbs under the long-lived `OnBehalfOf<P>` borrow.
                // For each iocb, invoke the dispatcher (which runs the
                // real read/write under the borrow's identity) and
                // push the returned event onto the completion queue.
                loop {
                    if let Some(iocb) = aio_cap_inner.pop_iocb() {
                        let event = dispatcher_inner(&iocb);
                        aio_cap_inner.push_completion(event);
                        aio_cap_inner.dispatched.fetch_add(1, Ordering::AcqRel);
                        continue;
                    }
                    // Queue drained — yield so the outer racer
                    // re-checks the abort signal. Phase 6+ replaces
                    // this with a real `WaitSource`-based park
                    // bound to `iocb_arrived`.
                    NoopPending.await;
                }
                // Unreachable from inside the loop above, but
                // satisfies the return-type checker.
                #[allow(unreachable_code)]
                Ok::<(), OnBehalfOfAbort>(())
            },
        )
        .await;
        helper_result
    };
    AioWorkerFuture {
        inner: AioWorkerState::Running(alloc::boxed::Box::pin(WorkerOuter {
            helper: alloc::boxed::Box::pin(helper),
            worker_abort,
        })),
        _aio_cap: aio_cap,
    }
}

/// Erased worker future returned by [`spawn_worker_for_context`]. The
/// concrete shape is `with_on_behalf_of(...) -> impl Future<Output=...>`
/// wrapped in a [`WorkerOuter`] race against the context's
/// `worker_abort` signal — phase 2 returns a hand-rolled future so the
/// syscall arm / test harness can spawn / poll it. Production code
/// (phase 3+) will submit this future to the reactor via the
/// boot-reactor seam.
pub struct AioWorkerFuture {
    /// Inner state machine.
    inner: AioWorkerState,
    /// The AIO context cap the worker drains; held across the future's
    /// lifetime so the queue / wait source stay live even if userspace
    /// closes the AIO fd in the same poll cycle. Prefixed with `_` to
    /// suppress unused-field warnings — the cap clone's job is the
    /// EBR retain, not direct use from outside.
    _aio_cap: Cap<AioContext>,
}

enum AioWorkerState {
    Running(
        Pin<alloc::boxed::Box<dyn Future<Output = Result<(), OnBehalfOfAbort>> + Send + 'static>>,
    ),
    Finished(Result<(), OnBehalfOfAbort>),
}

impl AioWorkerFuture {
    /// Snapshot the most recent terminal result, if the worker has
    /// finished. Tests use this to assert the body terminated with
    /// `Err(PrincipalExited)` after `abort_worker` was tripped.
    pub fn finished_result(&self) -> Option<Result<(), OnBehalfOfAbort>> {
        match &self.inner {
            AioWorkerState::Finished(r) => Some(*r),
            _ => None,
        }
    }
}

impl Future for AioWorkerFuture {
    type Output = Result<(), OnBehalfOfAbort>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we project into our fields without moving them. The
        // `inner` field's `Box<dyn Future>` is itself `Pin`-rooted on
        // the heap; we hold its `Pin<Box<>>` directly.
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            AioWorkerState::Running(fut) => match fut.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(out) => {
                    this.inner = AioWorkerState::Finished(out);
                    Poll::Ready(out)
                }
            },
            AioWorkerState::Finished(out) => Poll::Ready(*out),
        }
    }
}

/// Helper future that races the `with_on_behalf_of` body against the
/// AIO context's worker abort signal.
///
/// `with_on_behalf_of` internally races the body against its *own*
/// `AbortSignal` (the principal's exit channel). Phase 2 needs to
/// surface a *second* abort source — the context-scoped
/// `worker_abort` — so `io_destroy` can terminate the worker without
/// principal exit. We wrap the helper here and trip from the outer
/// layer; the body sees Pending on each parked iteration so this
/// outer racer is given a chance to fire.
struct WorkerOuter {
    helper:
        Pin<alloc::boxed::Box<dyn Future<Output = Result<(), OnBehalfOfAbort>> + Send + 'static>>,
    worker_abort: Arc<AbortSignal>,
}

impl Future for WorkerOuter {
    type Output = Result<(), OnBehalfOfAbort>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(reason) = self.worker_abort.reason() {
            return Poll::Ready(Err(reason));
        }
        match self.helper.as_mut().poll(cx) {
            Poll::Ready(out) => Poll::Ready(out),
            Poll::Pending => {
                if let Some(reason) = self.worker_abort.reason() {
                    Poll::Ready(Err(reason))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// A `Future` that always returns `Pending`. Used inside the worker
/// body's drain-then-park loop: when the queue is empty we return
/// `Pending` once so the outer racer re-checks the abort signal.
/// Phase 3+ replaces this with a real `WaitSource`-based park.
struct NoopPending;

impl Future for NoopPending {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        // Re-arm via cooperative yield. The outer racer will re-check
        // the abort signal on its next poll, and the iocb-arrived
        // wait-source notify wakes whichever waker the production
        // wiring installed. Phase 2 tests poll the worker directly
        // after pushing an iocb so the wake path is the test loop.
        Poll::Pending
    }
}

// === test-only counter reset =========================================
//
// Tests that pin the monotonic `context_id` sequence reset the counter
// between runs. Production callers must not call this.

#[cfg(any(test, feature = "test-support"))]
pub fn reset_context_id_counter_for_test() {
    NEXT_CONTEXT_ID.store(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        guard
    }

    #[test]
    fn new_cap_returns_zone_allocated_aio_context() {
        let _g = setup();
        let ctx = AioContext::new_cap().expect("aio context cap");
        assert!(
            ctx.context_id() > 0,
            "context_id must be a positive monotonic id"
        );
        assert_eq!(ctx.nr_events(), 0, "default nr_events is 0");
    }

    #[test]
    fn distinct_caps_have_distinct_context_ids() {
        let _g = setup();
        let a = AioContext::new_cap().expect("aio context a");
        let b = AioContext::new_cap().expect("aio context b");
        assert_ne!(
            a.context_id(),
            b.context_id(),
            "each AioContext must mint a fresh context_id"
        );
    }

    #[test]
    fn new_with_nr_events_cap_stashes_capacity() {
        let _g = setup();
        let ctx = AioContext::new_with_nr_events_cap(128).expect("aio context cap");
        assert_eq!(ctx.nr_events(), 128);
    }

    #[test]
    fn push_iocb_admits_within_capacity_and_rejects_beyond() {
        let _g = setup();
        let ctx = AioContext::new_with_nr_events_cap(2).expect("aio context cap");
        let make = |data: u64| Iocb::new(IOCB_CMD_PREAD, 0, 0, 0, 0, data);
        assert_eq!(ctx.queue_len(), 0);
        ctx.push_iocb(make(1)).expect("first push admitted");
        ctx.push_iocb(make(2)).expect("second push admitted");
        let rejected = ctx
            .push_iocb(make(3))
            .expect_err("third push beyond capacity must reject");
        assert_eq!(rejected.aio_data, 3, "rejection returns the iocb verbatim");
        assert_eq!(ctx.queue_len(), 2);
    }

    #[test]
    fn pop_iocb_drains_fifo() {
        let _g = setup();
        let ctx = AioContext::new_with_nr_events_cap(4).expect("aio context cap");
        ctx.push_iocb(Iocb::new(IOCB_CMD_PREAD, 0, 0, 0, 0, 10))
            .expect("push 10");
        ctx.push_iocb(Iocb::new(IOCB_CMD_PREAD, 0, 0, 0, 0, 20))
            .expect("push 20");
        let first = ctx.pop_iocb().expect("first pop");
        let second = ctx.pop_iocb().expect("second pop");
        assert_eq!(first.aio_data, 10);
        assert_eq!(second.aio_data, 20);
        assert!(ctx.pop_iocb().is_none(), "queue drained");
    }

    #[test]
    fn is_valid_iocb_opcode_admits_known_set() {
        assert!(is_valid_iocb_opcode(IOCB_CMD_PREAD));
        assert!(is_valid_iocb_opcode(IOCB_CMD_PWRITE));
        assert!(is_valid_iocb_opcode(IOCB_CMD_FSYNC));
        assert!(is_valid_iocb_opcode(IOCB_CMD_FDSYNC));
        assert!(is_valid_iocb_opcode(IOCB_CMD_NOOP));
        assert!(is_valid_iocb_opcode(IOCB_CMD_PREADV));
        assert!(is_valid_iocb_opcode(IOCB_CMD_PWRITEV));
        assert!(!is_valid_iocb_opcode(42));
    }

    #[test]
    fn push_completion_then_pop_returns_fifo() {
        let _g = setup();
        let ctx = AioContext::new_cap().expect("cap");
        ctx.push_completion(IoEvent::new(0xAAA, 0, 4, 0));
        ctx.push_completion(IoEvent::new(0xBBB, 0, 8, 0));
        assert_eq!(ctx.completion_len(), 2);
        let a = ctx.pop_completion().expect("first");
        let b = ctx.pop_completion().expect("second");
        assert_eq!(a.data, 0xAAA);
        assert_eq!(b.data, 0xBBB);
        assert!(ctx.pop_completion().is_none());
    }

    #[test]
    fn drain_completions_caps_at_max() {
        let _g = setup();
        let ctx = AioContext::new_cap().expect("cap");
        for i in 0..5 {
            ctx.push_completion(IoEvent::new(i as u64, 0, i as i64, 0));
        }
        let first_two = ctx.drain_completions(2);
        assert_eq!(first_two.len(), 2);
        assert_eq!(first_two[0].data, 0);
        assert_eq!(first_two[1].data, 1);
        let remaining = ctx.drain_completions(10);
        assert_eq!(remaining.len(), 3, "drain caps at queue depth");
    }

    #[test]
    fn io_event_le_bytes_round_trips_layout() {
        let ev = IoEvent::new(0xDEAD_BEEF_CAFE_BABE, 0x1111, -22, 7);
        let bytes = ev.to_le_bytes();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[0..8], &0xDEAD_BEEF_CAFE_BABEu64.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x1111u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &(-22i64).to_le_bytes());
        assert_eq!(&bytes[24..32], &7i64.to_le_bytes());
    }

    #[test]
    fn cancel_worker_trips_with_owner_requested() {
        let _g = setup();
        let ctx = AioContext::new_cap().expect("cap");
        ctx.cancel_worker();
        let reason = ctx.worker_abort.reason().expect("tripped");
        assert!(
            matches!(
                reason,
                OnBehalfOfAbort::CooperativeCancel(CancelReason::OwnerRequested)
            ),
            "cancel_worker trips OwnerRequested, got {reason:?}"
        );
    }
}
