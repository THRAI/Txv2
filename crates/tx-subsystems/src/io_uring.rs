//! io_uring SQPOLL scaffold — second `OnBehalfOf<P>` canary
//! (future PR-12 phase 0).
//!
//! Spec:
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §8.1 (SQPOLL design)
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §13
//!   (future canary section — "io_uring SQPOLL as a second canary")
//!
//! # Goal
//!
//! Linux's `io_uring_setup(IORING_SETUP_SQPOLL, ...)` mints a kernel
//! polling thread (the SQPOLL kthread) that watches an SQE submission
//! ring for new work, executes each SQE, and writes completions into a
//! CQE ring. The kthread runs **on behalf of** the user process that
//! called `io_uring_setup` — exactly the
//! `OnBehalfOf<P>` execution-scope shape PR-11 (AIO) built first.
//!
//! Per W-W's PR-11 phase 0 report (echoed in §13 of the AIO plan):
//!
//! > the OnBehalfOf<P> framework built for AIO directly supports
//! > io_uring SQPOLL as a second canary with zero additional framework
//! > work; the SQPOLL kthread is one reactor task that enters
//! > `with_on_behalf_of` at startup and runs per-SQE sub-scripts inside
//! > the borrow.
//!
//! This module is the structural scaffold that proves the prediction.
//!
//! # Phase 0 surface (scaffold only)
//!
//! 1. The zone-allocated [`IoUring`] payload — SQE ring + CQE ring +
//!    per-ring abort signal + per-ring wait sources + a monotonic
//!    `ring_id` minted at construction (the addressing key the worker /
//!    future syscall arms route by, mirroring W-Z's `context_id`
//!    discipline on `AioContext`).
//! 2. The [`SqeStub`] / [`CqeStub`] placeholder structs. Phase 1 of the
//!    future PR-12 will replace these with the real `struct io_uring_sqe`
//!    / `struct io_uring_cqe` shapes parsed from the user-mmapped ring;
//!    for now the wire-layout parsing is intentionally deferred.
//! 3. The [`spawn_sqpoll_worker`] helper — constructs the SQPOLL
//!    kthread future as `with_on_behalf_of(owner, body)`. The body is a
//!    single long-lived borrow that loops dequeuing SQEs from the ring
//!    and dispatching them; this scaffold's "dispatch" is a counter
//!    increment, matching W-CC's PR-11 phase 2 stub pattern.
//!
//! # Non-goals (deferred to future PR-12 phases)
//!
//! - **No real SQE / CQE wire-layout parsing.** SqeStub / CqeStub are
//!   placeholder kernel-side shapes; the future phase 1 wires the real
//!   UAPI `struct io_uring_sqe` (64 bytes) + `struct io_uring_cqe`
//!   (16 bytes) parsers from the user-mmapped rings.
//! - **No user-mmapped ring.** Phase 0 uses an in-kernel
//!   `VecDeque<SqeStub>` / `VecDeque<CqeStub>` rather than the
//!   user-mmapped ring buffers Linux exposes. Same divergence shape as
//!   W-Z made for AIO's completion ring; the SQPOLL kthread sees the
//!   in-kernel queue.
//! - **No `IORING_SETUP_*` flag handling.** Phase 0 accepts any
//!   `flags` value verbatim; `IORING_SETUP_SQPOLL` is implicit. Future
//!   phases gate the SQPOLL kthread spawn on the flag.
//! - **No `io_uring_enter(2)`.** SQPOLL by definition does not need
//!   `io_uring_enter` for SQE submission (the kthread polls); the
//!   syscall is wired here only as a numeric constant for forward
//!   reference.
//!
//! # Framework reusability claim
//!
//! This module imports `with_on_behalf_of` / `AbortSignal` /
//! `ScriptCtx` / `SubjectContext` / `SubjectIdentity` **as-is** from
//! `tx_substrate::step_v3`. No new framework primitive is introduced.
//! The IoUring is just another principal-P invocation of the same
//! borrow-scope shape W-W shipped for AIO. See the report section at
//! the bottom of `crates/tx-shims/tests/v3_io_uring_sqpoll_scaffold.rs`
//! for the reusability reflection.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};

use tx_substrate::step_v3::{
    with_on_behalf_of, AbortSignal, InterestMask, OnBehalfOfAbort, ScriptCtx, SubjectContext,
    SubjectIdentity, WaitSourceId,
};
use tx_substrate::wake::WaitSource;
use tx_substrate::zone::{self, Cap, Zone, ZoneAllocated, ZoneError};
use tx_substrate::SpinMutex;

use crate::wait_source;

// === SQE / CQE stubs =================================================
//
// Placeholders for `struct io_uring_sqe` and `struct io_uring_cqe`.
// Phase 0 just needs a shape the worker body can pop off the queue and
// "dispatch" (count) — the real UAPI layouts (64-byte SQE, 16-byte CQE)
// land in future PR-12 phase 1 alongside the user-mmapped ring parsers.

/// Phase-0 placeholder for `struct io_uring_sqe`. Production phase 1
/// parses the 64-byte UAPI shape from the user-mmapped SQ ring; phase
/// 0 carries only the opcode + `user_data` cookie so the worker can
/// echo the cookie back into the CQE stub on dispatch.
///
/// Fields will track the Linux `struct io_uring_sqe` layout (opcode,
/// fd, off, addr, len, flags, user_data) once the wire-format parsing
/// lands. For now we keep just `opcode` and `user_data` — the same two
/// fields the AIO PR-11 phase 2 stub kept on `Iocb` before phase 3
/// extended it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqeStub {
    /// Linux `IORING_OP_*` opcode. Phase 0 stubs every value — the
    /// scaffold worker increments the dispatch counter regardless.
    pub opcode: u8,
    /// User cookie echoed verbatim into the CQE on completion (Linux's
    /// `sqe.user_data` → `cqe.user_data` round-trip).
    pub user_data: u64,
}

impl SqeStub {
    /// Construct a minimal SQE stub for the scaffold. Tests use this
    /// to push a single SQE through the test helper
    /// [`IoUring::push_sqe_for_test`].
    pub const fn new(opcode: u8, user_data: u64) -> Self {
        Self { opcode, user_data }
    }
}

/// Phase-0 placeholder for `struct io_uring_cqe`. The SQPOLL kthread
/// writes one of these per SQE it dispatches. Production phase 1 will
/// serialise into the 16-byte UAPI layout (user_data: u64, res: i32,
/// flags: u32) and publish into the user-mmapped CQ ring.
///
/// Fields mirror Linux's UAPI:
/// - `user_data`: echoed from the SQE (correlation cookie).
/// - `res`: primary result — bytes done on success, negative errno on
///   failure. Phase 0 always sets 0 (the dispatch is a counter).
/// - `flags`: CQE flags (`IORING_CQE_F_*`). Phase 0 always 0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CqeStub {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

impl CqeStub {
    /// Construct a CQE stub. Scaffold-only — phase 1 will replace this
    /// with the wire-shape serialiser.
    pub const fn new(user_data: u64, res: i32, flags: u32) -> Self {
        Self {
            user_data,
            res,
            flags,
        }
    }
}

// === ring_id minting =================================================
//
// Mirrors W-Z's `NEXT_CONTEXT_ID` discipline on `AioContext`. Stable
// addressing key for the per-ring worker registry the syscall arm
// uses. Starts at 1 so 0 can serve as a "no ring" sentinel.

static NEXT_RING_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_ring_id() -> u64 {
    NEXT_RING_ID.fetch_add(1, Ordering::AcqRel)
}

// === interest masks ==================================================

/// Interest-mask bit the [`IoUring::sqe_arrived_source`] publishes on
/// push. Single bit because the carrier has a single semantic event
/// ("ring went from empty to non-empty"). Mirrors
/// [`crate::aio::IOCB_ARRIVED_MASK`].
pub const SQE_ARRIVED_MASK: u64 = 0x1;

/// Interest-mask bit the [`IoUring::cqe_available_source`] publishes on
/// completion-push. Single bit; pairs with [`SQE_ARRIVED_MASK`] on the
/// opposite carrier. Mirrors [`crate::aio::EVENTS_AVAILABLE_MASK`].
pub const CQE_AVAILABLE_MASK: u64 = 0x1;

// === IoUring payload =================================================

/// io_uring instance payload — zone-allocated per `Cap<IoUring>`.
///
/// **Scaffold scope.** The struct carries the per-ring SQE queue + CQE
/// queue + per-ring wait sources + worker abort signal + dispatched
/// counter. The fields match `AioContext` row-for-row except for
/// terminology (SQE/CQE instead of iocb/io_event) — that
/// row-for-row mirror is itself the framework-reusability evidence.
pub struct IoUring {
    /// Monotonic per-ring id. Stable for the lifetime of the
    /// `Cap<IoUring>` — minted once at construction. Used by the future
    /// syscall arms (`sys_io_uring_setup` / `sys_io_uring_enter` /
    /// `sys_io_uring_destroy`) as the worker-registry key, the same way
    /// PR-11 used `AioContext::context_id`.
    ring_id: u64,
    /// SQE-ring depth requested at `io_uring_setup(2)` time
    /// (Linux: `params.sq_entries`). Captured verbatim; future phases
    /// will round up to the next power of two and use as the SQ ring
    /// size.
    sq_entries: u32,
    /// CQE-ring depth requested at `io_uring_setup(2)` time
    /// (Linux: `params.cq_entries`). Defaults to `2 * sq_entries` in
    /// Linux; phase 0 captures the user-requested value verbatim.
    cq_entries: u32,
    /// Submission queue. Phase 0 uses a `VecDeque<SqeStub>`; phase 1
    /// will move to the user-mmapped SQ ring. Bounded by `sq_entries`.
    sq_ring: SpinMutex<VecDeque<SqeStub>>,
    /// Completion queue. Phase 0 uses a `VecDeque<CqeStub>`; phase 1
    /// will move to the user-mmapped CQ ring.
    cq_ring: SpinMutex<VecDeque<CqeStub>>,
    /// Wait source notified on every SQE push. The SQPOLL kthread
    /// parks on this between SQEs (when the ring is empty); production
    /// wiring binds the kthread's `TaskMailbox` via
    /// `WaitSource::prepare(...).install_if(...)`.
    sqe_arrived: Arc<WaitSource>,
    /// Cached id for [`Self::sqe_arrived`].
    sqe_arrived_id: u64,
    /// Wait source notified on every CQE push. Future `io_uring_enter`
    /// callers waiting on completion can park on this carrier.
    cqe_available: Arc<WaitSource>,
    /// Cached id for [`Self::cqe_available`].
    cqe_available_id: u64,
    /// Worker abort signal. `io_uring_destroy(2)` (a future phase) and
    /// the `Drop` impl trip this; the kthread's `with_on_behalf_of`
    /// racer observes the trip and returns `Err(...)`.
    worker_abort: Arc<AbortSignal>,
    /// Monotonic count of SQEs the SQPOLL kthread has dequeued and
    /// "dispatched". Phase 0 just increments; phase 1+ couples to a
    /// real CQE push. Tests poll this to confirm the kthread actually
    /// drained the ring under the borrow.
    dispatched: AtomicU64,
}

impl core::fmt::Debug for IoUring {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IoUring")
            .field("ring_id", &self.ring_id)
            .field("sq_entries", &self.sq_entries)
            .field("cq_entries", &self.cq_entries)
            .field("sq_len", &self.sq_len())
            .field("cq_len", &self.cq_len())
            .field("dispatched", &self.dispatched.load(Ordering::Acquire))
            .finish()
    }
}

impl IoUring {
    /// Construct a fresh io_uring payload without zone-signing it.
    /// Tests and the `sys_io_uring_setup(2)` arm should prefer
    /// [`Self::new_with_entries_cap`]; this lower-level constructor
    /// exists so the zone-allocation step can be deferred.
    pub fn new() -> Self {
        Self::with_entries(0, 0)
    }

    /// Construct a fresh io_uring payload with the given SQ / CQ ring
    /// depths. Mirrors `AioContext::with_nr_events`.
    pub fn with_entries(sq_entries: u32, cq_entries: u32) -> Self {
        let sqe_arrived_channel = tx_reactor::wait::Channel::new();
        let sqe_arrived_id = wait_source::register_wait_channel(sqe_arrived_channel);
        let sqe_arrived = Arc::new(WaitSource::new(WaitSourceId::new(sqe_arrived_id)));
        let cqe_available_channel = tx_reactor::wait::Channel::new();
        let cqe_available_id = wait_source::register_wait_channel(cqe_available_channel);
        let cqe_available = Arc::new(WaitSource::new(WaitSourceId::new(cqe_available_id)));
        Self {
            ring_id: allocate_ring_id(),
            sq_entries,
            cq_entries,
            sq_ring: SpinMutex::new(VecDeque::new()),
            cq_ring: SpinMutex::new(VecDeque::new()),
            sqe_arrived,
            sqe_arrived_id,
            cqe_available,
            cqe_available_id,
            worker_abort: Arc::new(AbortSignal::new()),
            dispatched: AtomicU64::new(0),
        }
    }

    /// Zone-sign a fresh io_uring payload (default `sq_entries = 0,
    /// cq_entries = 0`). Returns the `Cap<IoUring>` the caller installs
    /// into a process's fd table (wrapped in an `OpenFile` whose backing
    /// is [`crate::vfs::structure::OpenFileBacking::IoUring`]).
    pub fn new_cap() -> Result<Cap<Self>, ZoneError> {
        Self::new_with_entries_cap(0, 0)
    }

    /// Zone-sign a fresh io_uring with the given ring depths. Companion
    /// to [`Self::with_entries`] for the `sys_io_uring_setup(2)` arm.
    pub fn new_with_entries_cap(sq_entries: u32, cq_entries: u32) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(
            reservation,
            Self::with_entries(sq_entries, cq_entries),
        ))
    }

    /// Snapshot the stable per-ring id. Used as the worker-registry
    /// key by the syscall arm.
    pub const fn ring_id(&self) -> u64 {
        self.ring_id
    }

    /// Snapshot the requested SQ ring depth.
    pub const fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// Snapshot the requested CQ ring depth.
    pub const fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// Current depth of the SQ ring.
    pub fn sq_len(&self) -> usize {
        self.sq_ring.lock().len()
    }

    /// Current depth of the CQ ring.
    pub fn cq_len(&self) -> usize {
        self.cq_ring.lock().len()
    }

    /// Wait-source id paired with [`Self::sqe_arrived`].
    pub const fn sqe_arrived_id(&self) -> u64 {
        self.sqe_arrived_id
    }

    /// Borrow the SQE-arrival wait source.
    pub fn sqe_arrived_source(&self) -> &Arc<WaitSource> {
        &self.sqe_arrived
    }

    /// Wait-source id paired with [`Self::cqe_available`].
    pub const fn cqe_available_id(&self) -> u64 {
        self.cqe_available_id
    }

    /// Borrow the CQE-availability wait source.
    pub fn cqe_available_source(&self) -> &Arc<WaitSource> {
        &self.cqe_available
    }

    /// Number of SQEs the kthread body has dequeued + "dispatched"
    /// (scaffold stub). Tests poll this to pin "the kthread actually
    /// drained the SQ ring under the borrow".
    pub fn dispatched(&self) -> u64 {
        self.dispatched.load(Ordering::Acquire)
    }

    /// Push an SQE onto the SQ ring. Returns the SQE back to the
    /// caller (as `Err(sqe)`) when the ring is at its `sq_entries`
    /// capacity. Mirrors `AioContext::push_iocb`.
    ///
    /// **Phase 0 / test-only.** Production phase 1 will dequeue from
    /// the user-mmapped SQ ring rather than accept pushes here; this
    /// helper exists so the scaffold test can drive a single SQE
    /// through the kthread's drain loop without a wire-layout parser.
    pub fn push_sqe_for_test(&self, sqe: SqeStub) -> Result<(), SqeStub> {
        let mut ring = self.sq_ring.lock();
        if (ring.len() as u32) >= self.sq_entries {
            return Err(sqe);
        }
        ring.push_back(sqe);
        drop(ring);
        // Wake any parked SQPOLL kthread. Mirrors the AIO push path.
        self.sqe_arrived.notify_emit(InterestMask::new(SQE_ARRIVED_MASK));
        Ok(())
    }

    /// Pop the front of the SQ ring. Used by the SQPOLL kthread body.
    pub fn pop_sqe(&self) -> Option<SqeStub> {
        self.sq_ring.lock().pop_front()
    }

    /// Push a CQE onto the CQ ring and notify any waiters parked on
    /// `cqe_available`. Called by the kthread body after each SQE
    /// dispatch (phase 1+; phase 0's body increments the dispatch
    /// counter only).
    pub fn push_cqe(&self, cqe: CqeStub) {
        self.cq_ring.lock().push_back(cqe);
        self.cqe_available
            .notify_emit(InterestMask::new(CQE_AVAILABLE_MASK));
    }

    /// Pop one CQE off the ring, if any. Future `io_uring_enter(2)`
    /// completion-drain paths will consume this.
    pub fn pop_cqe(&self) -> Option<CqeStub> {
        self.cq_ring.lock().pop_front()
    }

    /// Borrow the worker abort signal. The `io_uring_destroy` arm and
    /// the `Drop` impl trip this. Returns the shared `Arc` so external
    /// code can hold a clone across `.await` points.
    pub fn worker_abort_signal(&self) -> Arc<AbortSignal> {
        self.worker_abort.clone()
    }

    /// Trip the worker's abort signal with
    /// `OnBehalfOfAbort::PrincipalExited`. Equivalent to the worker's
    /// principal exiting under the borrow.
    pub fn abort_worker(&self) {
        self.worker_abort.trip(OnBehalfOfAbort::PrincipalExited);
    }

    /// Trip the worker's abort signal with
    /// `OnBehalfOfAbort::CooperativeCancel(OwnerRequested)`. Used by
    /// the future `sys_io_uring_destroy(2)` arm — mirrors
    /// `AioContext::cancel_worker`.
    pub fn cancel_worker(&self) {
        self.worker_abort.trip(OnBehalfOfAbort::CooperativeCancel(
            tx_substrate::step_v3::CancelReason::OwnerRequested,
        ));
    }
}

impl Default for IoUring {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IoUring {
    fn drop(&mut self) {
        // Trip the worker's abort signal so any still-live SQPOLL
        // kthread future observes the abort on its next poll and
        // terminates. Mirrors `AioContext::Drop`.
        self.worker_abort.trip(OnBehalfOfAbort::CooperativeCancel(
            tx_substrate::step_v3::CancelReason::OwnerRequested,
        ));
        wait_source::release_wait_channel(self.sqe_arrived_id);
        wait_source::release_wait_channel(self.cqe_available_id);
    }
}

// === zone wiring ======================================================

static IO_URING_ZONE: Zone<IoUring> = Zone::const_new();

unsafe impl ZoneAllocated for IoUring {
    fn zone() -> &'static Zone<Self> {
        &IO_URING_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    zone::register_zone_for::<IoUring>()?;
    Ok(())
}

// === SQPOLL kthread future ==========================================

/// Construct the SQPOLL kthread future for an io_uring instance.
///
/// Per D8 §13 (echoing `06_EXECUTION_SCOPE_v1.md` §8.1): "the SQPOLL
/// kthread is one reactor task that enters `with_on_behalf_of` at
/// startup and runs per-SQE sub-scripts inside the borrow."
///
/// **Worker body sketch (phase 0):**
/// 1. `with_on_behalf_of(owner, &owner_subject, body)`.
/// 2. `body` is a single `async move` that loops:
///    - Check `ring.worker_abort` — if tripped, return `Err`.
///    - Try `ring.pop_sqe()` — if `Some(sqe)`, increment
///      `dispatched`, continue. Phase 1 invokes a real dispatcher
///      mirroring `AioContext::push_completion` + `IocbDispatcher`.
///    - If `None`, yield `Pending`. Re-poll observes either a wake
///      from `sqe_arrived` or the abort signal.
///
/// **Framework reusability.** This function is a row-for-row clone of
/// [`crate::aio::spawn_worker_for_context`] with `AioContext` →
/// `IoUring`, `pop_iocb` → `pop_sqe`, and the dispatcher arg removed
/// (phase 0 has no dispatcher closure; phase 1 will accept one with
/// the same shape as `IocbDispatcher`). No new framework primitive is
/// introduced — `with_on_behalf_of` is consumed verbatim.
pub fn spawn_sqpoll_worker<I>(
    ring_cap: Cap<IoUring>,
    owner_principal: Cap<I>,
    owner_subject: SubjectContext<I>,
) -> SqpollWorkerFuture
where
    I: SubjectIdentity + Send + Sync,
    I::Credential: Send + Sync,
    I::Restrictions: Send + Sync,
    I::ThreadIdentity: Send + Sync,
{
    let worker_abort = ring_cap.worker_abort.clone();
    let ring_cap_for_body = ring_cap.clone();
    let helper = async move {
        let ring_inner = ring_cap_for_body;
        let helper_result = with_on_behalf_of(
            owner_principal,
            &owner_subject,
            move |_ctx: ScriptCtx<I>| async move {
                // Borrow body — per D8 §13 the body loops draining
                // SQEs under the long-lived `OnBehalfOf<P>` borrow.
                // Phase 0: increment the dispatch counter on each
                // SQE. Phase 1 will invoke a real per-SQE dispatcher
                // mirroring the AIO `IocbDispatcher` shape.
                loop {
                    if let Some(_sqe) = ring_inner.pop_sqe() {
                        ring_inner.dispatched.fetch_add(1, Ordering::AcqRel);
                        continue;
                    }
                    // SQ ring drained — yield so the outer racer
                    // re-checks the abort signal. Phase 1+ replaces
                    // this with a real `WaitSource`-based park bound
                    // to `sqe_arrived`.
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
    SqpollWorkerFuture {
        inner: SqpollWorkerState::Running(alloc::boxed::Box::pin(WorkerOuter {
            helper: alloc::boxed::Box::pin(helper),
            worker_abort,
        })),
        _ring_cap: ring_cap,
    }
}

/// Erased SQPOLL kthread future returned by [`spawn_sqpoll_worker`].
/// Mirrors [`crate::aio::AioWorkerFuture`]; the only structural
/// difference is the cap type it retains.
pub struct SqpollWorkerFuture {
    inner: SqpollWorkerState,
    /// The io_uring cap the kthread drains; held across the future's
    /// lifetime so the ring / wait sources stay live even if userspace
    /// closes the fd in the same poll cycle. Prefixed with `_` because
    /// the clone's job is the EBR retain, not direct use.
    _ring_cap: Cap<IoUring>,
}

enum SqpollWorkerState {
    Running(
        Pin<alloc::boxed::Box<dyn Future<Output = Result<(), OnBehalfOfAbort>> + Send + 'static>>,
    ),
    Finished(Result<(), OnBehalfOfAbort>),
}

impl SqpollWorkerFuture {
    /// Snapshot the most recent terminal result, if the kthread has
    /// finished. Tests use this to assert the body terminated with
    /// `Err(...)` after the abort signal was tripped.
    pub fn finished_result(&self) -> Option<Result<(), OnBehalfOfAbort>> {
        match &self.inner {
            SqpollWorkerState::Finished(r) => Some(*r),
            _ => None,
        }
    }
}

impl Future for SqpollWorkerFuture {
    type Output = Result<(), OnBehalfOfAbort>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we project into our fields without moving them. The
        // `inner` field's `Box<dyn Future>` is itself `Pin`-rooted on
        // the heap; we hold its `Pin<Box<>>` directly.
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            SqpollWorkerState::Running(fut) => match fut.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(out) => {
                    this.inner = SqpollWorkerState::Finished(out);
                    Poll::Ready(out)
                }
            },
            SqpollWorkerState::Finished(out) => Poll::Ready(*out),
        }
    }
}

/// Helper future that races the `with_on_behalf_of` body against the
/// io_uring's worker abort signal. Mirrors `crate::aio::WorkerOuter`.
///
/// `with_on_behalf_of` internally races the body against its own
/// `AbortSignal` (the principal's exit channel). The SQPOLL scaffold
/// needs to surface a second abort source — the ring-scoped
/// `worker_abort` — so the future `io_uring_destroy` arm can terminate
/// the kthread without principal exit. We trip from the outer layer.
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

/// A `Future` that always returns `Pending`. Used inside the kthread
/// body's drain-then-park loop. Phase 1+ replaces this with a real
/// `WaitSource`-based park on `sqe_arrived`. Mirrors
/// `crate::aio::NoopPending`.
struct NoopPending;

impl Future for NoopPending {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}

// === test-only counter reset =========================================

#[cfg(any(test, feature = "test-support"))]
pub fn reset_ring_id_counter_for_test() {
    NEXT_RING_ID.store(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;
    use tx_substrate::testing::init_host_for_test_once;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_host_for_test_once();
        let _ = zones::register_all();
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        guard
    }

    #[test]
    fn new_cap_returns_zone_allocated_io_uring() {
        let _g = setup();
        let ring = IoUring::new_cap().expect("io_uring cap");
        assert!(
            ring.ring_id() > 0,
            "ring_id must be a positive monotonic id"
        );
        assert_eq!(ring.sq_entries(), 0);
        assert_eq!(ring.cq_entries(), 0);
    }

    #[test]
    fn distinct_caps_have_distinct_ring_ids() {
        let _g = setup();
        let a = IoUring::new_cap().expect("ring a");
        let b = IoUring::new_cap().expect("ring b");
        assert_ne!(
            a.ring_id(),
            b.ring_id(),
            "each IoUring must mint a fresh ring_id"
        );
    }

    #[test]
    fn new_with_entries_cap_stashes_capacities() {
        let _g = setup();
        let ring = IoUring::new_with_entries_cap(32, 64).expect("ring cap");
        assert_eq!(ring.sq_entries(), 32);
        assert_eq!(ring.cq_entries(), 64);
    }

    #[test]
    fn push_sqe_admits_within_capacity_and_rejects_beyond() {
        let _g = setup();
        let ring = IoUring::new_with_entries_cap(2, 4).expect("ring cap");
        let make = |ud: u64| SqeStub::new(0, ud);
        assert_eq!(ring.sq_len(), 0);
        ring.push_sqe_for_test(make(1))
            .expect("first push admitted");
        ring.push_sqe_for_test(make(2))
            .expect("second push admitted");
        let rejected = ring
            .push_sqe_for_test(make(3))
            .expect_err("third push beyond capacity must reject");
        assert_eq!(rejected.user_data, 3, "rejection returns the sqe verbatim");
        assert_eq!(ring.sq_len(), 2);
    }

    #[test]
    fn pop_sqe_drains_fifo() {
        let _g = setup();
        let ring = IoUring::new_with_entries_cap(4, 8).expect("ring cap");
        ring.push_sqe_for_test(SqeStub::new(0, 10))
            .expect("push 10");
        ring.push_sqe_for_test(SqeStub::new(0, 20))
            .expect("push 20");
        let first = ring.pop_sqe().expect("first pop");
        let second = ring.pop_sqe().expect("second pop");
        assert_eq!(first.user_data, 10);
        assert_eq!(second.user_data, 20);
        assert!(ring.pop_sqe().is_none(), "ring drained");
    }

    #[test]
    fn push_cqe_then_pop_returns_fifo() {
        let _g = setup();
        let ring = IoUring::new_cap().expect("ring cap");
        ring.push_cqe(CqeStub::new(0xAAA, 0, 0));
        ring.push_cqe(CqeStub::new(0xBBB, 0, 0));
        assert_eq!(ring.cq_len(), 2);
        let a = ring.pop_cqe().expect("first");
        let b = ring.pop_cqe().expect("second");
        assert_eq!(a.user_data, 0xAAA);
        assert_eq!(b.user_data, 0xBBB);
        assert!(ring.pop_cqe().is_none());
    }

    #[test]
    fn cancel_worker_trips_with_owner_requested() {
        let _g = setup();
        let ring = IoUring::new_cap().expect("ring cap");
        ring.cancel_worker();
        let reason = ring.worker_abort.reason().expect("tripped");
        assert!(
            matches!(
                reason,
                OnBehalfOfAbort::CooperativeCancel(
                    tx_substrate::step_v3::CancelReason::OwnerRequested
                )
            ),
            "cancel_worker trips OwnerRequested, got {reason:?}"
        );
    }

    #[test]
    fn abort_worker_trips_with_principal_exited() {
        let _g = setup();
        let ring = IoUring::new_cap().expect("ring cap");
        ring.abort_worker();
        let reason = ring.worker_abort.reason().expect("tripped");
        assert!(
            matches!(reason, OnBehalfOfAbort::PrincipalExited),
            "abort_worker trips PrincipalExited, got {reason:?}"
        );
    }
}
