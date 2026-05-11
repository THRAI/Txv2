//! `OnBehalfOf<P>` borrow primitive — PR-11 phase 0 framework.
//!
//! Per `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §3, the `with_on_behalf_of`
//! async helper is the borrow primitive that lets a kernel actor run
//! scripts under a borrowed `Cap<ProcessIdentity>`. It is the runtime
//! counterpart to the [`ExecutionScope::OnBehalfOf`] catalog member
//! reshaped in PR-11 phase 0.
//!
//! ## What the helper does
//!
//! 1. **Holds the principal cap.** A clone of `principal: Cap<I>` is
//!    moved into the borrow's `OnBehalfOfBorrow` guard. EBR retains
//!    the principal's identity slot for the borrow's lifetime — the
//!    principal stays addressable even if its payload is torn down
//!    concurrently (the slot stays alive until the last cap drops).
//! 2. **Subscribes the principal's `exit_source`.** Per
//!    `06_EXECUTION_SCOPE_v1.md` §5 the borrow watches the principal's
//!    exit channel as an abandonment source. PR-11 phase 0 records the
//!    `WaitSourceId` reported by `SubjectIdentity::exit_source` so
//!    later phases can wire it to a real `WaitSource` subscription
//!    (PR-3D-3); the framework test pin uses a manual
//!    [`AbortSignal::trip`] surrogate to simulate the fire path
//!    without dragging in the wake substrate yet.
//! 3. **Builds the borrow's `SubjectContext`.** The body sees
//!    `SubjectContext::borrowed(principal_clone,
//!    SubjectAuthority::derived_from(owner))` — process = principal,
//!    thread = None, authority = snapshot of P's cred + restrictions
//!    at borrow time.
//! 4. **Polls the body future, racing it against the abort signal.**
//!    On body complete → return its result. On abort → return
//!    `Err(OnBehalfOfAbort::PrincipalExited)` (or the matching
//!    `OnBehalfOfAbort` variant).
//! 5. **Drops the borrow at end-of-scope.** The principal cap clone
//!    retires through EBR; the (future) exit-source subscription
//!    unregisters. Per `06_EXECUTION_SCOPE_v1.md` §6 any scope-bound
//!    resources still held at this moment drop with the borrow.
//!
//! ## What is stubbed in PR-11 phase 0
//!
//! - **Exit-source subscription.** The actual `WaitSource::register`
//!   call requires a live `TaskMailbox` / wake substrate wiring; that
//!   wiring is owned by PR-3D-3 and lands ahead of (or alongside)
//!   PR-11 phase 1. PR-11 phase 0 stores the `WaitSourceId` and the
//!   abort signal is delivered through an in-borrow
//!   [`AbortSignal`] handle. The framework tests synthesise an abort
//!   by calling [`AbortSignal::trip`] directly; production callers in
//!   later phases bind the signal to the principal's `exit_source`
//!   via the standard wake-substrate path.
//! - **Cooperative cancel of the body future.** PR-11 phase 0 polls
//!   the body in a cooperative loop and observes the abort signal
//!   between polls. The body must yield often enough for the abort
//!   to be observed (per `06_EXECUTION_SCOPE_v1.md` §6 — same
//!   discipline as native syscall scripts under SIGKILL).
//!
//! Doc tags pinned by the framework test:
//! - `txdoc:TXV3-EXECUTION-SCOPE-V1`
//! - `txdoc:SCOPE-V1-PRIMITIVE-1`
//! - `txdoc:SCOPE-V1-SUBJECT-1`
//! - `txdoc:SCOPE-V1-ABANDONMENT-1`

use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};

use crate::step_v3::subject_context::{SubjectAuthority, SubjectContext, SubjectIdentity};
use crate::step_v3::{ScriptCtx, WaitSourceId};
use crate::zone::Cap;

/// Closed catalog of reasons a `with_on_behalf_of` borrow may abort
/// before the body completes. Per `06_EXECUTION_SCOPE_v1.md` §5.
///
/// Catalog extension is ARCH-3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnBehalfOfAbort {
    /// The principal exited (its `exit_source` fired) while the borrow
    /// was live. The body's next yield resolution would observe a
    /// `Killed` outcome on the underlying wait. Per
    /// `06_EXECUTION_SCOPE_v1.md` §5: this is the primary abandonment
    /// path. Resolves to a `Killable`-protocol `Killed` in the
    /// production wiring; the script terminates with
    /// `Err(EOWNERDEAD)` at the syscall surface.
    PrincipalExited,
    /// The principal's restriction stack was revoked while the
    /// borrow was live. Reserved for a future authority-refresh
    /// path; PR-11 phase 0 does not fire this variant (the borrow's
    /// authority is a snapshot per `06_EXECUTION_SCOPE_v1.md` §4 Open
    /// Q 11.1). The variant is present so the catalog is stable
    /// across the future extension.
    PrincipalRestrictionRevoked,
    /// Cooperative cancel from outside the borrow (e.g. `io_destroy`
    /// on an AIO context, `io_uring_unregister`, an explicit
    /// per-iocb `io_cancel`). Per `06_EXECUTION_SCOPE_v1.md` §5: the
    /// borrow drops; in-flight delegation tokens reach
    /// `SENTINEL_DEAD` via the standard endpoint cleanup.
    CooperativeCancel(CancelReason),
}

/// Closed catalog of cooperative-cancel reasons surfaced through
/// [`OnBehalfOfAbort::CooperativeCancel`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelReason {
    /// The owning subsystem (e.g. AIO context, io_uring instance)
    /// requested teardown without a principal-exit fire.
    OwnerRequested,
    /// A specific in-borrow op was cancelled (e.g. Linux `io_cancel`
    /// on an iocb). PR-11 phase 0 reserves the variant; the per-op
    /// cancel routing lands in PR-11 phase 6.
    OpCanceled,
}

/// One-shot abort signal that the borrow scope watches.
///
/// PR-11 phase 0 framework shape: the signal is set either by a real
/// `WaitSource` subscriber callback (production wiring, future PR-11
/// phase 4+) or by the test harness directly via
/// [`AbortSignal::trip`]. The borrow's drive loop checks
/// [`AbortSignal::is_tripped`] before/after each body poll; once
/// tripped the borrow returns `Err(OnBehalfOfAbort::*)` with the
/// stored reason.
///
/// Atomic so the signal is `Sync`; the borrow's body may be polled on
/// any reactor task and the signal may be tripped from any other.
#[derive(Debug)]
pub struct AbortSignal {
    tripped: AtomicBool,
    reason: SpinReason,
}

/// Tiny `SpinMutex<Option<OnBehalfOfAbort>>` analogue specialised to
/// the abort reason. PR-11 phase 0 doesn't need the full `SpinMutex`
/// surface; an atomic byte tag keeps the no-std overhead trivial and
/// avoids pulling the `SpinMutex` into a path that is only ever
/// written-once.
#[derive(Debug)]
struct SpinReason {
    tag: core::sync::atomic::AtomicU8,
}

// Reason-tag encoding for `SpinReason`.
const REASON_NONE: u8 = 0;
const REASON_PRINCIPAL_EXITED: u8 = 1;
const REASON_PRINCIPAL_RESTRICTION_REVOKED: u8 = 2;
const REASON_OWNER_REQUESTED: u8 = 3;
const REASON_OP_CANCELED: u8 = 4;

impl SpinReason {
    const fn new() -> Self {
        Self {
            tag: core::sync::atomic::AtomicU8::new(REASON_NONE),
        }
    }

    fn set(&self, reason: OnBehalfOfAbort) {
        let tag = match reason {
            OnBehalfOfAbort::PrincipalExited => REASON_PRINCIPAL_EXITED,
            OnBehalfOfAbort::PrincipalRestrictionRevoked => REASON_PRINCIPAL_RESTRICTION_REVOKED,
            OnBehalfOfAbort::CooperativeCancel(CancelReason::OwnerRequested) => {
                REASON_OWNER_REQUESTED
            }
            OnBehalfOfAbort::CooperativeCancel(CancelReason::OpCanceled) => REASON_OP_CANCELED,
        };
        // First-writer-wins (DTOK-3-style determinism): only set if
        // currently `None`. Subsequent writes are dropped on the
        // floor; the first abort reason is the canonical one.
        let _ = self
            .tag
            .compare_exchange(REASON_NONE, tag, Ordering::AcqRel, Ordering::Acquire);
    }

    fn get(&self) -> Option<OnBehalfOfAbort> {
        match self.tag.load(Ordering::Acquire) {
            REASON_NONE => None,
            REASON_PRINCIPAL_EXITED => Some(OnBehalfOfAbort::PrincipalExited),
            REASON_PRINCIPAL_RESTRICTION_REVOKED => {
                Some(OnBehalfOfAbort::PrincipalRestrictionRevoked)
            }
            REASON_OWNER_REQUESTED => Some(OnBehalfOfAbort::CooperativeCancel(
                CancelReason::OwnerRequested,
            )),
            REASON_OP_CANCELED => {
                Some(OnBehalfOfAbort::CooperativeCancel(CancelReason::OpCanceled))
            }
            _ => unreachable!("SpinReason tag out of range"),
        }
    }
}

impl AbortSignal {
    /// Build a fresh, untripped abort signal.
    pub const fn new() -> Self {
        Self {
            tripped: AtomicBool::new(false),
            reason: SpinReason::new(),
        }
    }

    /// Trip the signal with `reason`. First-writer-wins: subsequent
    /// trips do not overwrite the stored reason. The borrow body's
    /// next abort check will observe `Some(reason)`.
    pub fn trip(&self, reason: OnBehalfOfAbort) {
        self.reason.set(reason);
        self.tripped.store(true, Ordering::Release);
    }

    /// `true` iff [`Self::trip`] has been called at least once.
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// The stored abort reason if [`Self::is_tripped`] is `true`,
    /// else `None`.
    pub fn reason(&self) -> Option<OnBehalfOfAbort> {
        if self.is_tripped() {
            self.reason.get()
        } else {
            None
        }
    }
}

impl Default for AbortSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard owning the borrow's principal cap + abort signal.
///
/// Built by [`with_on_behalf_of`]; dropped when the helper returns
/// (body complete or aborted). On drop, the principal cap clone
/// retires through EBR (the zone slot stays live until the last cap
/// drops, so any concurrent borrow of the same principal keeps the
/// slot reachable). The future-PR-3D exit-source unregistration also
/// happens here; PR-11 phase 0 stores the source id for that later
/// phase to consume.
pub struct OnBehalfOfBorrow<I: SubjectIdentity> {
    /// Principal cap clone — the EBR retain that keeps the
    /// principal's identity slot live for the borrow's duration.
    /// Dropped on borrow exit; the underlying zone slot stays live
    /// until every clone drops.
    _principal: Cap<I>,
    /// Wait-source id for the principal's exit channel, captured at
    /// borrow entry via [`SubjectIdentity::exit_source`]. `None`
    /// means the principal has no live exit channel (already a
    /// zombie). Stored for the future PR-3D wake-substrate wiring;
    /// PR-11 phase 0 does not subscribe.
    exit_source_id: Option<WaitSourceId>,
    /// Shared abort signal — both the borrow's drive loop and (in
    /// production) the exit-source subscriber callback hold a clone.
    abort: Arc<AbortSignal>,
}

impl<I: SubjectIdentity> OnBehalfOfBorrow<I> {
    /// The wait-source id the borrow watches for principal exit.
    /// Captured at borrow entry; `None` if the principal had no live
    /// exit channel. Used by the production exit-source wiring to
    /// register a `WaitSource` subscriber that calls
    /// [`AbortSignal::trip`] on fire.
    pub fn exit_source_id(&self) -> Option<WaitSourceId> {
        self.exit_source_id
    }

    /// Borrow the shared abort signal. Production wake-substrate
    /// wiring hands a clone of this `Arc` to the exit-source
    /// subscriber callback so a principal-exit fire can trip the
    /// borrow. Framework tests use this to simulate an exit.
    pub fn abort_signal(&self) -> Arc<AbortSignal> {
        self.abort.clone()
    }
}

/// Run `body` inside an `OnBehalfOf<P>` borrow scope.
///
/// Per `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §3 (txdoc:SCOPE-V1-PRIMITIVE-1):
///
/// ```text
/// async fn with_on_behalf_of<F, R>(
///     owner: Cap<ProcessIdentity>,
///     body: impl FnOnce(SubjectContext) -> F,
/// ) -> Result<R, Errno>
/// ```
///
/// PR-11 phase 0 signature differs in two respects from the doc-spec:
///
/// 1. **Generic over `I: SubjectIdentity`.** The doc spec's
///    `Cap<ProcessIdentity>` is `Cap<I>` here; production code
///    resolves the alias against
///    `tx_subsystems::process::ProcessIdentity`.
/// 2. **Body takes `ScriptCtx<I>` by value rather than the bare
///    `SubjectContext`.** The body needs a full `ScriptCtx` so step
///    bodies inside it (e.g. AIO worker's `step_pread`) can be
///    invoked with the same shape as a native syscall script. The
///    helper builds the `ScriptCtx` (populating `subject` from
///    `SubjectContext::borrowed(...)`) and hands it to the body by
///    value; the body retains it for its lifetime and the ctx
///    drops with the body's frame. The body never sees a
///    worker-thread `SubjectContext`; all authority lookups inside
///    resolve against the principal's identity (SUBJ-1 +
///    SUBJ-2(b)).
///
///    By-value (rather than `&mut`) avoids the closure-returning-
///    a-future lifetime headache: a body of the shape
///    `|&mut ctx| async move { ... ctx.subject() ... }` would
///    require the closure's `&mut` to outlive the returned future,
///    which Rust cannot express through `FnOnce` today. By-value
///    keeps the body shape ergonomic and matches the doc-spec's
///    `SubjectContext`-by-value intent.
///
/// **Owner snapshot.** `owner_subject` is a reference to the
/// principal's existing `SubjectContext` — typically the principal's
/// thread-rooted `SubjectContext::from_thread` snapshot captured at
/// `io_setup` (AIO) / `io_uring_setup` (uring) time. The borrow
/// constructs its own `SubjectContext::borrowed` from clones of the
/// owner's cred / restrictions caps via
/// [`SubjectAuthority::derived_from`]. Per
/// `06_EXECUTION_SCOPE_v1.md` §4 the snapshot is stable for the
/// borrow's duration (Open Q 11.1).
///
/// **Race against abort.** On each poll boundary the helper checks
/// the borrow's [`AbortSignal`]; if tripped, the body is dropped
/// (`core::mem::drop`) and the helper returns
/// `Err(OnBehalfOfAbort::*)`. Cooperative-yield discipline: the body
/// must `.await` often enough for the abort to be observed (same
/// rule as native syscall scripts under SIGKILL). Per
/// `06_EXECUTION_SCOPE_v1.md` §5 + §6.
///
/// **End-of-scope drop.** On body complete or abort, the
/// `OnBehalfOfBorrow` guard drops:
/// 1. Principal cap clone retires through EBR.
/// 2. Future PR-3D: exit-source subscription unregisters.
/// 3. Any scope-bound `OperationalEvidence` held by the body drops
///    with the body's frame (per `06_EXECUTION_SCOPE_v1.md` §6).
pub async fn with_on_behalf_of<I, F, Fut, T>(
    principal: Cap<I>,
    owner_subject: &SubjectContext<I>,
    body: F,
) -> Result<T, OnBehalfOfAbort>
where
    I: SubjectIdentity,
    F: FnOnce(ScriptCtx<I>) -> Fut,
    Fut: Future<Output = Result<T, OnBehalfOfAbort>>,
{
    // Step 1 — capture the principal's exit-source id for the
    // production exit-watcher wiring. Done while the principal is
    // still live so the source id is meaningful; reading through the
    // cap is safe because the borrow holds its own retain.
    let exit_source_id = principal.exit_source();

    // Step 2 — build the borrow guard. The guard owns the principal
    // cap clone (EBR retain) and the abort signal.
    let borrow = OnBehalfOfBorrow {
        _principal: principal.clone(),
        exit_source_id,
        abort: Arc::new(AbortSignal::new()),
    };

    // Step 3 — materialise the borrow's `SubjectContext` per
    // `06_EXECUTION_SCOPE_v1.md` §4 (txdoc:SCOPE-V1-SUBJECT-1):
    // process = principal, thread = None,
    // authority = SubjectAuthority::derived_from(owner).
    let borrow_authority = SubjectAuthority::derived_from(owner_subject);
    let borrow_subject = SubjectContext::borrowed(principal, borrow_authority);

    // Step 4 — hand a ScriptCtx<I> by value to the body. The body
    // owns the ctx for its frame's duration; the ctx drops with the
    // body's frame. The body never sees a worker-thread
    // SubjectContext (SUBJ-1 by absence).
    let script_ctx = ScriptCtx::<I>::new().with_subject(borrow_subject);
    let body_future = body(script_ctx);

    // Step 5 — drive the body, racing it against the abort signal.
    let abort_handle = borrow.abort.clone();
    let racer = BorrowRacer {
        body: body_future,
        abort: abort_handle,
    };
    let result = racer.await;

    // Step 6 — `borrow` drops here. Principal cap retires through
    // EBR; future PR-3D exit-source subscription unregisters.
    drop(borrow);

    result
}

/// Race a body future against an `AbortSignal`. First to fire wins.
///
/// On every poll boundary the racer checks the abort signal before
/// (and after) polling the body. If the signal is tripped at any
/// point, the body is dropped and the racer resolves to
/// `Err(OnBehalfOfAbort::*)`. Otherwise the body's `Poll::Ready`
/// propagates.
///
/// **Drop semantics.** The body is a `Fut` owned by value inside the
/// racer. When the racer drops (either by being polled to `Ready`,
/// or by the caller dropping the racer-returning future), the body
/// drops with it — any scope-bound resources held by the body
/// release at that point. The pinning discipline below ensures the
/// body is never moved after its first poll.
struct BorrowRacer<Fut> {
    body: Fut,
    abort: Arc<AbortSignal>,
}

impl<Fut, T> Future for BorrowRacer<Fut>
where
    Fut: Future<Output = Result<T, OnBehalfOfAbort>>,
{
    type Output = Result<T, OnBehalfOfAbort>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: BorrowRacer is `!Unpin`-equivalent for its `body`
        // field by virtue of being a struct field we project. The
        // outer Pin guarantees we never move `self`; we use
        // `get_unchecked_mut` to project into the body and re-pin it
        // for its own poll. The `abort` field is `Unpin` (Arc) and
        // doesn't need this.
        let this = unsafe { self.get_unchecked_mut() };

        // Pre-check: if the abort signal is already tripped, drop
        // the body and surface the reason.
        if let Some(reason) = this.abort.reason() {
            return Poll::Ready(Err(reason));
        }

        // Poll the body.
        let body = unsafe { Pin::new_unchecked(&mut this.body) };
        match body.poll(cx) {
            Poll::Ready(out) => {
                // Post-check: if the body produced an answer at the
                // same time as the abort tripped, the abort takes
                // priority — the borrow is no longer permitted to
                // observe a value. (Same discipline as native
                // syscall scripts: SIGKILL beats a Ready value at
                // the same poll boundary.)
                if let Some(reason) = this.abort.reason() {
                    return Poll::Ready(Err(reason));
                }
                Poll::Ready(out)
            }
            Poll::Pending => {
                // If abort tripped while body was running, observe
                // it before parking. (Body's `cx.waker()` clone is
                // assumed to be wired into whatever wait the body
                // installed; the abort path independently wakes the
                // task through the production wake-substrate
                // wiring. PR-11 phase 0 framework tests poll the
                // racer directly so the wake-loop coverage is
                // synthetic.)
                if let Some(reason) = this.abort.reason() {
                    return Poll::Ready(Err(reason));
                }
                Poll::Pending
            }
        }
    }
}
