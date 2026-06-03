//! Userfaultfd — fd-table, registration, fault, and reply scaffold.
//!
//! Spec: `docs/Txv3/05_DELEGATE_v1.md` §8.1 (userfaultfd worked
//! example), `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//! (phase plan).
//!
//! # Scope of this module
//!
//! This module gives a userfaultfd-kind fd a place to live in a
//! process's fd table and carries the phase 2–5 machinery:
//!
//! 1. The zone-allocated `UserfaultFd` payload.
//! 2. A stable `ufd_id` minted at construction — used in later phases
//!    as the `endpoint_marker` for `DelegateRegistry::install_request`
//!    and `mark_endpoint_died` (see D7 §3.3).
//! 3. The `register_zones()` hook called from
//!    [`crate::zones::register_all`].
//!
//! `UFFDIO_API` handshake (P-10.2), `UFFDIO_REGISTER` per-VMA
//! attachment (P-10.3), fault-path interception (P-10.4),
//! `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` / `UFFDIO_CONTINUE` reply ioctls
//! (P-10.5), and the pending-fault read queue.
//!
//! # Drop semantics
//!
//! Drop releases the ufd read wait-source registration. Endpoint-death
//! fanout for parked faulting threads remains a follow-up for the
//! production close-the-agent path.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub mod adapter;
pub mod notification;

use adapter::step_engine::{
    sign, ByteProgress, Cap, DelegateRegistry, DelegateTokenId, SpinMutex, StepOutcome,
    TaskMailbox, V3Errno, WaitSource, Zone, ZoneAllocated, ZoneError,
};
use adapter::wait_routing::Channel;

// === ufd_id minting ===================================================

/// Monotonic counter for `UserfaultFd::ufd_id`. The id is the
/// `endpoint_marker` later phases pass to
/// `DelegateRegistry::install_request` / `mark_endpoint_died`. Starts
/// at 1 so 0 can serve as "no endpoint" if a future caller ever needs
/// a sentinel.
static NEXT_UFD_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_ufd_id() -> u64 {
    NEXT_UFD_ID.fetch_add(1, Ordering::AcqRel)
}

// === payload ==========================================================

/// One pending fault message queued on this ufd. Conceptually mirrors
/// Linux's `struct uffd_msg { __u8 event; __u8 reserved1; __u16 reserved2;
/// __u32 reserved3; union { struct { __u64 flags; __u64 address; union
/// { __u32 ptid; __u32 reserved; } feat; } pagefault; ... }; }` — the
/// substrate carries the few fields phase 5 actually serializes plus
/// the per-fault `DelegateTokenId` that links the message back to the
/// pending agent slot so an `UFFDIO_COPY` / `_ZEROPAGE` / `_CONTINUE`
/// reply can `mark_replied` against the right token.
///
/// **Token tracking decision.** Linux's `struct uffd_msg` does not
/// carry a token id natively — the agent infers "which fault is this
/// reply for?" purely from the faulting address. Phase 5 keeps the
/// substrate-side message in lockstep with Linux's wire format on the
/// agent-visible field set (`event`, `address`), and *additionally*
/// stamps the substrate-internal `token_id` so the ioctl arms can
/// resolve "which DelegateTokenId is pending for `dst`?" in O(1)
/// without scanning the registry. The token_id is **not** copied out
/// to userspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdMsg {
    /// Linux `struct uffd_msg.event` byte — phase 5 only emits
    /// `UFFD_EVENT_PAGEFAULT` (0x12). Future events (FORK, REMAP,
    /// REMOVE, UNMAP) would add discriminator values here.
    pub event: u8,
    /// Faulting user-virtual address (page-aligned). Becomes the
    /// `dst` field on the agent's eventual `UFFDIO_COPY` / `_ZEROPAGE`
    /// / `_CONTINUE`.
    pub fault_addr: u64,
    /// Faulting thread id, if recorded. Zero means "not recorded."
    /// Pinned for symmetry with `UfdRequest::PageFault.faulting_tid`.
    pub ufd_thread_id: u64,
    /// Per-fault substrate token. Phase-5 ioctl handlers resolve
    /// "which DelegateTokenId is pending for `dst_uaddr`?" by
    /// popping the queue front and validating that the front
    /// message's `fault_addr` matches the agent-supplied `dst`.
    /// Not serialized to userspace.
    pub token_id: DelegateTokenId,
}

/// A range previously installed against this ufd by `UFFDIO_REGISTER`
/// (PR-10 phase 3).
///
/// Stored on the `UserfaultFd` payload so phase-4's fault path can
/// confirm a faulting page lies inside a still-registered range
/// (`UFFDIO_UNREGISTER` — not yet wired — would remove entries here),
/// and so a follow-up `UFFDIO_API` / `UFFDIO_REGISTER` round can
/// observe what is already registered.
///
/// Phase 3 keeps the structure minimal: a flat `Vec` under a
/// `SpinMutex`. Linux registers ufd ranges relatively rarely (once
/// per VMA at agent setup) so the linear scan is fine; an
/// `AtomicSlot<Cap<UfdRegistrations>>` upgrade is a follow-up if
/// hot-path access (fault interception, page-fault delivery) needs
/// a lock-free read shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UfdRange {
    /// Range start (user-VA, page-aligned).
    pub start: u64,
    /// Range length in bytes (page-multiple).
    pub len: u64,
    /// `struct uffdio_register { mode }` bits the agent requested.
    /// Phase 3 only accepts `UFFDIO_REGISTER_MODE_MISSING`; later
    /// phases may admit `WP` / `MINOR`. The value is stashed verbatim
    /// so callers can inspect what the agent supplied.
    pub mode: u64,
}

/// Userfaultfd payload — zone-allocated per `Cap<UserfaultFd>`.
///
/// **Phases 0–3 surface.** Phase 0 (W-Q) established the zone +
/// `ufd_id` minting. Phase 2 (W-T) extended with the open-time
/// `flags` argument (`O_CLOEXEC`, etc.) and a `UFFDIO_API`
/// handshake flag. Phase 3 (this) adds a registered-ranges list:
/// each `UFFDIO_REGISTER` ioctl appends one [`UfdRange`] entry, and
/// later phases (fault interception, `UFFDIO_UNREGISTER`) consult
/// it.
pub struct UserfaultFd {
    /// Monotonic per-ufd id. Doubles as the `endpoint_marker` later
    /// phases pass to `DelegateRegistry::install_request` (per D7
    /// §3.3). Stable for the lifetime of the `Cap<UserfaultFd>` —
    /// minted once at construction and never reassigned.
    ufd_id: u64,
    /// Open-time flags captured from `sys_userfaultfd(flags)` (PR-10
    /// phase 2). Today's recognised set is just `O_CLOEXEC`; later
    /// phases may admit `O_NONBLOCK` once the ufd has a read-fault
    /// queue. The substrate keeps the raw `u32` so future bits do
    /// not require a payload shape change.
    open_flags: u32,
    /// `UFFDIO_API` handshake bit (per `man userfaultfd(2)` / PR-10
    /// phase 2). Flipped to `true` once an agent successfully drives
    /// the `UFFDIO_API` ioctl; later ioctls
    /// (`UFFDIO_REGISTER` / `COPY` / …) reject with `EINVAL` if the
    /// handshake has not yet been performed. Atomic so the read
    /// side does not need to lock; the write side is already
    /// serialised by the agent — the ioctl is single-shot in Linux's
    /// model.
    api_handshake_done: AtomicBool,
    /// Registered ranges (PR-10 phase 3). Each successful
    /// `UFFDIO_REGISTER` appends one entry. Phase 3 stores the list
    /// under a `SpinMutex<Vec<_>>`; a follow-up may upgrade to an
    /// `AtomicSlot<Cap<UfdRegistrations>>` if a hot path requires
    /// lock-free read access. Re-registering an already-covered
    /// range is **idempotent** at this layer — the appended entry is
    /// the latest registration, but no earlier entry is removed (the
    /// shim layer surfaces duplicate ranges with success per the
    /// docs in `step_uffdio_register`).
    registrations: SpinMutex<Vec<UfdRange>>,
    /// Per-ufd [`DelegateRegistry`] (PR-10 phase 4).
    ///
    /// Decision (per D7 §3.3 + phase 4 worker prompt "where does it
    /// live"): **per-ufd**. Linux's userfaultfd routes faults from any
    /// thread in the registering process to a handler thread bound to a
    /// specific ufd via the `read(uffd_fd, &mut uffd_msg)` arm; one
    /// registry per ufd matches that 1:1 grouping. The `ufd_id` doubles
    /// as the `endpoint_marker` so [`DelegateRegistry::mark_endpoint_died`]
    /// can walk all in-flight faults on this ufd when the cap is
    /// dropped (the close-the-fd-on-handler-exit path).
    ///
    /// Phase 4 wires `fault_script` to install its `OnAgent` request
    /// against this registry and `await_agent_reply` against the
    /// faulting thread's mailbox; phase 5 wires the `UFFDIO_COPY` /
    /// `UFFDIO_ZEROPAGE` ioctls to drive
    /// [`DelegateRegistry::mark_replied`] against the same registry.
    delegate_registry: DelegateRegistry,
    /// PR-10 phase 5: per-ufd pending-fault queue. Each successful
    /// `fault_script` interception pushes one [`UffdMsg`] here; the
    /// agent thread drains via `step_ufd_read` against this fd.
    ///
    /// FIFO ordering — Linux's userfaultfd delivers fault messages in
    /// arrival order, and the phase-5 ioctl arms match against the
    /// front message's `fault_addr` to identify the right pending
    /// token. A `VecDeque` under a `SpinMutex` matches the
    /// pipe.rs::RingBuffer pattern and stays consistent with the
    /// hot-path-cold registration model.
    pending_faults: SpinMutex<VecDeque<UffdMsg>>,
    /// PR-10 phase 5: per-ufd read-readiness wait source. Fired
    /// whenever a fault is pushed onto [`Self::pending_faults`]. The
    /// agent thread's `step_ufd_read` parks on this source when the
    /// queue is empty (blocking case); pipe.rs's `Arc<WaitSource>`
    /// pattern is the model (D2/D4 coexistence: the legacy `Channel`
    /// and the new `WaitSource` fire on every transition).
    ///
    /// `WaitSource::id()` matches [`Self::wait_source_id`] so a
    /// `YieldShape::OnWaitSource { source }` consumer's `source.raw()`
    /// round-trips through the legacy `wait_source` resolver.
    wait_source: Arc<WaitSource>,
    /// Legacy `Channel` companion (D2 coexistence with pipe.rs). The
    /// `wait_source` registry's id is the same `u64` carried on the
    /// new path so a `WaitToken::source_id()` lookup lands here.
    wait_channel: Channel,
    /// Carrier id paired with [`Self::wait_channel`]. Stable for the
    /// lifetime of the `Cap<UserfaultFd>` — minted at construction
    /// and released in `Drop`.
    wait_source_id: u64,
}

impl core::fmt::Debug for UserfaultFd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserfaultFd")
            .field("ufd_id", &self.ufd_id)
            .field("open_flags", &self.open_flags)
            .field("api_handshake_done", &self.api_handshake_done())
            .field("registration_count", &self.registration_count())
            .finish()
    }
}

impl UserfaultFd {
    /// Construct a fresh ufd payload without zone-signing it. Tests
    /// and the `sys_userfaultfd(2)` arm should prefer
    /// [`Self::new_with_flags_cap`]; this lower-level constructor
    /// exists so the zone-allocation step can be deferred.
    pub fn new() -> Self {
        Self::with_flags(0)
    }

    /// Construct a fresh ufd payload with the given `open_flags`
    /// (PR-10 phase 2). The `flags` argument mirrors the `flags`
    /// parameter to `sys_userfaultfd(2)`; today only `O_CLOEXEC` is
    /// observed at the syscall layer, but the raw value is stashed
    /// here so future bits do not break the substrate API.
    pub fn with_flags(open_flags: u32) -> Self {
        let wait_point = notification::new_wait_point();
        Self {
            ufd_id: allocate_ufd_id(),
            open_flags,
            api_handshake_done: AtomicBool::new(false),
            registrations: SpinMutex::new(Vec::new()),
            delegate_registry: DelegateRegistry::new(),
            pending_faults: SpinMutex::new(VecDeque::new()),
            wait_source: wait_point.source,
            wait_channel: wait_point.channel,
            wait_source_id: wait_point.source_id,
        }
    }

    /// Zone-sign a fresh ufd payload (default flags = 0). Returns the
    /// `Cap<UserfaultFd>` the caller installs into a process's fd
    /// table (wrapped in an `OpenFile` whose backing is
    /// `OpenFileBacking::Ufd`). On success the cap's `ufd_id()` is
    /// the stable id later phases use as the `endpoint_marker`.
    pub fn new_cap() -> Result<Cap<Self>, ZoneError> {
        Self::new_with_flags_cap(0)
    }

    /// Zone-sign a fresh ufd payload with the given `open_flags`
    /// (PR-10 phase 2). Companion to [`Self::with_flags`] for the
    /// `sys_userfaultfd(2)` arm.
    pub fn new_with_flags_cap(open_flags: u32) -> Result<Cap<Self>, ZoneError> {
        sign(Self::with_flags(open_flags))
    }

    /// Snapshot the stable per-ufd id. Phase 0 callers use this only
    /// for identity assertions in tests; later phases wire it as the
    /// `endpoint_marker` argument to
    /// `DelegateRegistry::install_request` and
    /// `mark_endpoint_died`.
    pub const fn ufd_id(&self) -> u64 {
        self.ufd_id
    }

    /// Snapshot the open-time `flags` argument captured at
    /// `sys_userfaultfd(2)` time (PR-10 phase 2).
    pub const fn open_flags(&self) -> u32 {
        self.open_flags
    }

    /// `true` once the agent has driven a successful `UFFDIO_API`
    /// handshake against this ufd (PR-10 phase 2). Sticky: a second
    /// handshake call returns `EPERM` per Linux's "API already set"
    /// rule.
    pub fn api_handshake_done(&self) -> bool {
        self.api_handshake_done.load(Ordering::Acquire)
    }

    /// Mark the `UFFDIO_API` handshake as completed. Returns `true`
    /// on the first successful transition, `false` if the handshake
    /// was already performed (the agent must not call `UFFDIO_API`
    /// twice — Linux returns `EPERM`).
    pub fn mark_api_handshake_done(&self) -> bool {
        self.api_handshake_done
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// PR-10 phase 3: append a successful `UFFDIO_REGISTER` to the
    /// per-ufd registered-ranges list. The shim's
    /// `step_uffdio_register` calls this **after** it has tagged the
    /// covering VMAs via [`crate::vm::AddressSpace::tag_ufd_registration`],
    /// so a successful return here pins the ufd-side bookkeeping.
    ///
    /// Phase 3 stores duplicates rather than deduping — callers that
    /// re-register an already-registered range observe both entries
    /// in [`Self::registrations_snapshot`]. The shim layer documents
    /// the user-visible "duplicate register returns success" rule;
    /// dedup is a substrate-side follow-up (cheap, but not load-bearing
    /// for the canary).
    pub fn record_registration(&self, range: UfdRange) {
        self.registrations.lock().push(range);
    }

    /// Snapshot of the registered ranges (PR-10 phase 3). The returned
    /// `Vec` is independent of the locked storage; the caller may
    /// hold it across `.await` points without keeping the spinlock.
    pub fn registrations_snapshot(&self) -> Vec<UfdRange> {
        self.registrations.lock().clone()
    }

    /// Convenience accessor — count of currently-registered ranges.
    /// Tests use this to pin idempotency / append-once semantics
    /// without copying the full list.
    pub fn registration_count(&self) -> usize {
        self.registrations.lock().len()
    }

    /// Per-ufd [`DelegateRegistry`] (PR-10 phase 4). The fault path
    /// installs requests against this registry; the agent's reply
    /// ioctls (phase 5) drive `mark_replied` against it.
    ///
    /// Each `Cap<UserfaultFd>` owns its own registry — Linux's ufd
    /// routes a fault to exactly one handler-bound ufd, so the
    /// per-ufd grouping matches the kernel model (D7 §3.3). The
    /// `ufd_id` is the `endpoint_marker` consumers pass to
    /// `install_request` so a future `mark_endpoint_died` walk can
    /// abort every in-flight fault when the ufd cap drops.
    pub fn delegate_registry(&self) -> &DelegateRegistry {
        &self.delegate_registry
    }

    /// PR-10 phase 5: per-fd read-readiness wait source. Returned as
    /// `&Arc<WaitSource>` so a consumer (`step_ufd_read`'s
    /// `YieldShape::OnWaitSource` producer) can clone and hold the
    /// source across the wait window without keeping the ufd cap
    /// alive longer than necessary.
    pub fn wait_source(&self) -> &Arc<WaitSource> {
        &self.wait_source
    }

    /// PR-10 phase 5: legacy-resolver-side carrier id. The same `u64`
    /// is stamped on the new `WaitSource::id()` so a `WaitToken`
    /// constructed from this id resolves via `wait_source::wait_on_token`
    /// against the [`Self::wait_channel`].
    pub fn wait_source_id(&self) -> u64 {
        self.wait_source_id
    }

    /// PR-10 phase 5: push a fault message onto the pending queue and
    /// fire both wake paths (legacy `Channel` + new `WaitSource`).
    /// Called by the phase-4 `fault_script` OnAgent branch immediately
    /// after `install_request` returns the `DelegateTokenId`.
    pub fn push_fault_msg(&self, msg: UffdMsg) {
        self.pending_faults.lock().push_back(msg);
        notification::notify_readable(&self.wait_channel, &self.wait_source);
    }

    /// PR-10 phase 5: snapshot the pending-fault queue depth. Tests
    /// pin "push N → depth N → pop drains to 0"; not used on the hot
    /// path.
    pub fn pending_fault_count(&self) -> usize {
        self.pending_faults.lock().len()
    }

    /// PR-10 phase 5: peek at the front message *without* popping.
    /// Phase-5 ioctl arms call this to look up the `DelegateTokenId`
    /// associated with an agent-supplied `dst` address before popping;
    /// keeping peek + pop split lets the ioctl reject a mismatch
    /// without dropping the message.
    pub fn front_fault_msg(&self) -> Option<UffdMsg> {
        self.pending_faults.lock().front().copied()
    }

    /// PR-10 phase 5: pop the front fault message. Returns `None` if
    /// the queue is empty. Used by the agent's `read(uffd_fd, ...)`
    /// arm (`step_ufd_read`) and by the ioctl arms after a
    /// `front_fault_msg` match.
    pub fn pop_fault_msg(&self) -> Option<UffdMsg> {
        self.pending_faults.lock().pop_front()
    }
}

/// PR-10 phase 5: per-ufd `read(2)` arm.
///
/// Mirrors pipe.rs's `step_read` shape:
/// - queue non-empty → `Done(serialized_bytes)` after serializing one
///   `struct uffd_msg`-shaped record into `out`. Returns 0 if `out` is
///   smaller than [`UFFD_MSG_WIRE_SIZE`] (the agent must supply a
///   buffer at least 32 bytes wide to receive a fault message).
/// - queue empty + O_NONBLOCK → `Err(EAGAIN)`.
/// - queue empty + blocking → `Yield { OnWaitSource }` on the per-ufd
///   wait source; the dispatcher parks on `wait_source::wait_on_token`
///   and re-polls when [`UserfaultFd::push_fault_msg`] fires.
///
/// **Wire format (32 bytes).** Linux's `struct uffd_msg` is 32 bytes
/// on RV64. The serialized layout phase 5 emits is the minimum the
/// agent needs to identify the fault:
///
/// ```text
///   off  size  field
///   0    1     event (UFFD_EVENT_PAGEFAULT)
///   1    7     reserved (zero)
///   8    8     pagefault.flags (zero — Missing only in phase 5)
///   16   8     pagefault.address (faulting_addr)
///   24   8     pagefault.feat (low 32 bits = ufd_thread_id, high 32 = 0)
/// ```
///
/// The `DelegateTokenId` is **not** written into the wire — it stays
/// substrate-internal. The ioctl arms identify the right token by
/// looking up the front pending message's `fault_addr` against the
/// agent-supplied `dst`.
pub fn step_ufd_read(
    ufd: &UserfaultFd,
    out: &mut [u8],
    nonblocking: bool,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if out.is_empty() {
        return StepOutcome::done(0);
    }
    // Need at least one full `uffd_msg` worth of buffer space — Linux
    // returns -EINVAL if the agent passes a buffer smaller than
    // `sizeof(struct uffd_msg)`. Match that.
    if out.len() < UFFD_MSG_WIRE_SIZE {
        return StepOutcome::err(V3Errno::EINVAL);
    }

    // Try to pop a message under the lock.
    let popped = ufd.pop_fault_msg();
    if let Some(msg) = popped {
        let bytes = serialize_uffd_msg(&msg);
        out[..UFFD_MSG_WIRE_SIZE].copy_from_slice(&bytes);
        return StepOutcome::done(UFFD_MSG_WIRE_SIZE);
    }

    // Queue empty.
    if nonblocking {
        return StepOutcome::err(V3Errno::EAGAIN);
    }
    // Park on the per-ufd wait source. The caller (sys_read in the
    // shim) drives `wait_source::wait_on_token` against the carrier
    // id; `push_fault_msg` fires the channel on the next install.
    notification::wait_until_readable(ufd.wait_source_id())
}

/// Wire-format size of a serialized `struct uffd_msg` record per
/// PR-10 phase 5. Matches Linux's `sizeof(struct uffd_msg)` (32
/// bytes) so an unmodified userspace agent built against the Linux
/// uapi header can `read(uffd_fd, &mut uffd_msg, 32)` and parse the
/// returned bytes directly.
pub const UFFD_MSG_WIRE_SIZE: usize = 32;

/// Pack a substrate [`UffdMsg`] into Linux's `struct uffd_msg` wire
/// format. See `step_ufd_read` docs for the field offsets.
fn serialize_uffd_msg(msg: &UffdMsg) -> [u8; UFFD_MSG_WIRE_SIZE] {
    let mut out = [0u8; UFFD_MSG_WIRE_SIZE];
    out[0] = msg.event;
    // bytes [1..8] reserved, left zero.
    // bytes [8..16] = pagefault.flags — phase 5 only emits Missing,
    //   which Linux represents with `flags = 0`.
    // bytes [16..24] = pagefault.address.
    out[16..24].copy_from_slice(&msg.fault_addr.to_le_bytes());
    // bytes [24..32] = pagefault.feat. The low 32 bits hold ptid;
    //   high 32 bits stay zero (Linux uses a union; the substrate
    //   carries a u64 tid that we truncate to u32 for compatibility).
    let ptid = msg.ufd_thread_id as u32;
    out[24..28].copy_from_slice(&ptid.to_le_bytes());
    out
}

impl Default for UserfaultFd {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for UserfaultFd {
    fn drop(&mut self) {
        // Release the legacy `wait_source` registry's clone so a
        // dropped ufd cap doesn't leak the carrier id. The
        // `Arc<WaitSource>` drops alongside the payload (no external
        // consumers may outlive it because they all upgrade through
        // the cap's `wait_source()` accessor).
        notification::release_wait_point(self.wait_source_id);
    }
}

// === ProcessUfdDispatch ==============================================
//
// PR-10 phase 6: production [`crate::vm::UfdDispatch`] implementation.
//
// Resolves a `ufd_id` to a `Cap<UserfaultFd>` by walking the calling
// process's fd-table looking for an `OpenFile` whose
// `OpenFileBacking::Ufd(cap)` carries the matching `cap.ufd_id()`.
// Replaces `NullUfdDispatch` in the production fault-script call path
// (`thread_future` → `aspace.fault_script` → `fault_script_with_ufd_dispatch`).
//
// Walk is a linear scan — Linux's userfaultfd is rare in shape (a
// process opens 1–2 ufds at most in the common case), and the
// per-fault overhead is paid once per page-fault delegation. A hash
// map shortcut is a follow-up if production profiling shows it.

/// Production [`crate::vm::UfdDispatch`] keyed on a calling process's
/// fd-table.
///
/// Constructed at fault-handler-entry (in `thread_future` once the
/// `Cap<ProcessIdentity>` and faulting-thread's `Weak<TaskMailbox>`
/// are both known) and consumed by
/// `AddressSpace::fault_script_with_ufd_dispatch`. The borrow lifetime
/// `'a` keeps the process cap and mailbox alive for the duration of
/// the dispatch resolution.
///
/// Dispatch flow:
/// 1. `resolve(ufd_id)` walks `process.payload.fds` searching for an
///    [`crate::vfs::OpenFile`] whose `OpenFileBacking::Ufd(cap)`
///    carries the matching `cap.ufd_id()`.
/// 2. On match: returns an [`crate::vm::UfdDispatchTarget`] pointing
///    at the cap's per-ufd `DelegateRegistry`, the faulting thread's
///    `Weak<TaskMailbox>`, and the cap itself as the `fault_pusher`
///    so the OnAgent branch can `push_fault_msg`.
/// 3. On miss (the cap was closed between `UFFDIO_REGISTER` and the
///    fault, or the registration tag is stale): returns `None`. The
///    fault-script falls through to the normal materialize path —
///    graceful degradation, no panic.
///
/// **Lifetime decision.** The struct holds `&'a Cap<ProcessIdentity>`
/// rather than cloning the cap because the fault-handler-entry stack
/// frame already owns a clone (from `thread.upgrade_owner_proc()`); a
/// second clone would just delay drop without gaining anything. The
/// dispatcher does not outlive the fault future, so the borrow shape
/// is the right one.
pub struct ProcessUfdDispatch<'a> {
    /// Calling process — its fd-table is the search domain.
    process: &'a Cap<crate::process::ProcessIdentity>,
    /// Faulting thread's mailbox (held `Weak` so the script frame's
    /// teardown does not pin a dead mailbox alive — see PR-7B
    /// invariant on `mark_replied`'s `Weak::upgrade` semantics).
    mailbox: alloc::sync::Weak<TaskMailbox>,
    /// Cached cap clone produced by `resolve`. The cap is held inside
    /// the dispatcher across the `await_agent_reply` window so the
    /// returned `UfdDispatchTarget`'s `&UserfaultFd` borrow remains
    /// valid for the duration of the dispatch call.
    ///
    /// Stored behind an `UnsafeCell` so `resolve(&self, ...)` can
    /// mutate it once per dispatcher lifecycle. The contract is
    /// **set-once, never-cleared**: `resolve` writes `Some(cap)` on
    /// the first match and never clears it; subsequent reads see
    /// the same stable cap. The fault-script calls `resolve` exactly
    /// once per fault (see `dispatch_ufd_fault`), so the set-once
    /// discipline is structurally guaranteed — there is no other
    /// caller in production. The borrow returned via
    /// [`UfdDispatchTarget::fault_pusher`] / `target.registry` is
    /// tied to the dispatcher's `&self` lifetime, which the fault
    /// future holds across the `await_agent_reply` await.
    resolved_cap: core::cell::UnsafeCell<Option<Cap<UserfaultFd>>>,
}

// SAFETY: `ProcessUfdDispatch` may travel with the fault future
// across awaits. The future may be polled on any reactor thread, so
// the dispatcher must be `Send` for the future to be `Send`. The
// inner `UnsafeCell` is accessed only through `resolve(&self, ...)`
// under the set-once discipline; no `&mut` is ever taken on the
// dispatcher across a yield point in the fault script (the script
// borrows the dispatcher only inside `dispatch_ufd_fault`, which
// finishes its single `resolve` call before the `await`). `Sync` is
// not required by the `UfdDispatch` trait, but is asserted here so a
// future that captures `&ProcessUfdDispatch` can be `Send`.
unsafe impl<'a> Sync for ProcessUfdDispatch<'a> {}

impl<'a> ProcessUfdDispatch<'a> {
    /// Construct a fresh dispatcher. Caller supplies the process cap
    /// (typically `thread.upgrade_owner_proc()` from the trap
    /// handler) and the faulting thread's mailbox (`Weak` to avoid
    /// pinning a dead mailbox alive — `thread.task_mailbox_weak()`
    /// or equivalent).
    pub fn new(
        process: &'a Cap<crate::process::ProcessIdentity>,
        mailbox: alloc::sync::Weak<TaskMailbox>,
    ) -> Self {
        Self {
            process,
            mailbox,
            resolved_cap: core::cell::UnsafeCell::new(None),
        }
    }
}

impl<'a> crate::vm::UfdDispatch for ProcessUfdDispatch<'a> {
    fn resolve(&self, ufd_id: u64) -> Option<crate::vm::UfdDispatchTarget<'_>> {
        // Walk the process's fd-table. Zombies have no payload; an
        // already-dead process's fault-resolution would race in
        // production but the trap handler upstream has already
        // resolved a live aspace, so a zombie window is structurally
        // narrow. `payload.lock()` returns `None` for zombies, which
        // we treat as "no ufd found" → fall-through to normal
        // materialize.
        let payload_guard = self.process.payload.lock();
        let payload = payload_guard.as_ref()?;
        // Snapshot the fds map clone so we can release the payload
        // lock before touching the `&UserfaultFd` borrow (the
        // `UfdDispatchTarget` we return must live across the
        // fault-script's `await_agent_reply` call, which we cannot
        // hold the payload spinlock through).
        let fds = payload.snapshot_fds();
        drop(payload_guard);
        // Linear scan — find the first `OpenFile` whose ufd id
        // matches. Linear is fine for the canary; Linux processes
        // typically open at most a handful of userfaultfds, and the
        // per-fault overhead is paid once per page-fault delegation.
        // An indexed map / hash shortcut is a follow-up if production
        // profiling shows it.
        let matched_cap = fds.values().find_map(|open_file| {
            let cap = open_file.ufd()?;
            if cap.ufd_id() == ufd_id {
                Some(cap.clone())
            } else {
                None
            }
        })?;
        // Stash the cap clone in the dispatcher's set-once slot. The
        // cap pins the zone slot for the dispatcher's lifetime.
        //
        // SAFETY: `resolve(&self, ...)` is called at most once per
        // dispatcher lifetime by the fault-script (see
        // `dispatch_ufd_fault`'s `let Some(target) =
        // dispatch.resolve(ufd_id) else { ... };` — single call site,
        // no loop). No other `&mut` to `resolved_cap` exists. The
        // write happens-before the read because we have `&self` here
        // and the trait is consumed synchronously by
        // `dispatch_ufd_fault` before the `await`.
        let cap_ref: &Cap<UserfaultFd> = unsafe {
            let slot = &mut *self.resolved_cap.get();
            *slot = Some(matched_cap);
            slot.as_ref().expect("just stored")
        };
        // `Cap<UserfaultFd>` derefs to `&UserfaultFd` pointing at the
        // pinned zone slot; the borrow lives as long as `cap_ref`,
        // which is borrowed from `self.resolved_cap` — i.e. `&self`'s
        // lifetime. The borrow is valid for the entire
        // `dispatch_ufd_fault` body including across the
        // `await_agent_reply` await (the dispatcher is `&D` in
        // `dispatch_ufd_fault`'s signature, so the borrow survives
        // the await).
        let ufd_ref: &UserfaultFd = cap_ref;
        Some(crate::vm::UfdDispatchTarget {
            registry: ufd_ref.delegate_registry(),
            mailbox: self.mailbox.clone(),
            fault_pusher: Some(ufd_ref),
        })
    }
}

// === zone wiring ======================================================

static USERFAULT_FD_ZONE: Zone<UserfaultFd> = Zone::const_new();

unsafe impl ZoneAllocated for UserfaultFd {
    fn zone() -> &'static Zone<Self> {
        &USERFAULT_FD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    adapter::step_engine::register_zone_for::<UserfaultFd>()?;
    Ok(())
}

// === test-only counter reset =========================================
//
// Tests that pin the monotonic `ufd_id` sequence reset the counter
// between runs. Production callers must not call this.

#[cfg(any(test, feature = "test-support"))]
pub fn reset_ufd_id_counter_for_test() {
    NEXT_UFD_ID.store(1, Ordering::Release);
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
    fn new_cap_returns_zone_allocated_userfaultfd() {
        let _g = setup();
        let ufd = UserfaultFd::new_cap().expect("ufd cap");
        // The cap must resolve to a live ufd payload with a non-zero id.
        assert!(ufd.ufd_id() > 0, "ufd_id must be a positive monotonic id");
    }

    #[test]
    fn distinct_caps_have_distinct_ufd_ids() {
        let _g = setup();
        let a = UserfaultFd::new_cap().expect("ufd a");
        let b = UserfaultFd::new_cap().expect("ufd b");
        assert_ne!(
            a.ufd_id(),
            b.ufd_id(),
            "each UserfaultFd must mint a fresh ufd_id"
        );
    }
}
