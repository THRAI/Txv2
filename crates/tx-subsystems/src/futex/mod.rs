//! POSIX-style `futex(2)` primitive — fast user-space mutex wakeup.
//!
//! Spec: `man 2 futex`, `Documentation/futex/futex2.rst` (Linux).
//! Roadmap: Slice 3 of the shell-prompt roadmap (2026-05-07).
//!
//! **Bucket model.** A fixed array of 256 `FutexBucket`s indexed by
//! `hash(uaddr) & 0xff`. Each bucket holds a [`Channel`] registered
//! with the global [`crate::wait_source`]. `FUTEX_WAIT` parks on the
//! bucket's channel (after verifying `*uaddr == val`); `FUTEX_WAKE`
//! fires the bucket's channel, waking all waiters in that bucket.
//! Collisions are absorbed by the per-waiter re-check on wakeup —
//! the syscall arm re-loads `*uaddr` and either returns success or
//! re-parks. Linux uses a similar global hash-bucket scheme.
//!
//! **Wake-N is best-effort.** Linux's "wake at most n waiters" is
//! advisory. v1 returns `n` directly (over-waking is permissible —
//! spurious wakees re-park on their next iteration). Future
//! tightening can add per-bucket waiter counts.
//!
//! **Timeout deferred to Slice 4.** v1 ignores the `timeout`
//! argument; `FUTEX_WAIT` parks indefinitely until `FUTEX_WAKE`
//! fires. Slice 4's `nanosleep` lands the timer-wait-source
//! infrastructure futex needs for proper timeout support.
//!
//! **PRIVATE / CLOCK_REALTIME flags.** Recognised but ignored at
//! the syscall arm. Per-process isolation is implicit — each process
//! has its own aspace and the user word at `uaddr` is in that
//! aspace.
//!
//! **User-VA discipline.** v1 reads `*uaddr` via direct kernel
//! pointer deref under the bootstrap kernel-buffer exemption
//! shared with the existing `getresuid` / `pipe2` arms.
//! `TODO(phase-userva)` — replace with `UserAccessIf::read_user::<u32>`
//! once Slice 9 lands.
//!
//! **PR-3D-2 coexistence** (D2/D4 ADRs). Each bucket now carries
//! **two** parallel wake-publication points, mirroring the pipe
//! template from PR-3D-1:
//!
//! 1. The legacy `Channel` (`channel`) — backed by `RawPort`+`Waker`.
//!    Consumed by the existing `wait_source` resolver and any caller
//!    that `lookup_wait_channel`s the bucket's `source_id` and awaits
//!    via `WaitFuture`. **Stays in place** until PR-3D-5 retires the
//!    legacy resolver path.
//! 2. The new `Arc<WaitSource>` (`wait_source`) — backed by
//!    `TaskMailbox`. Consumed by v3 callers that own a `TaskMailbox`
//!    and register via `WaitSource::prepare(...).install_if(...)`.
//!    Tested in `crates/tx-subsystems/tests/v3_futex_waitsource.rs`.
//!
//! Both paths fire on every `step_futex_wake`. The `WaitSourceId`
//! stamped into `step_futex_wait`'s `YieldShape::OnWaitSource` is the
//! same `u64` the legacy `wait_source` resolver returned, so the two
//! paths share an id namespace and a v3 caller's
//! `WaitSourceId.raw()` round-trips cleanly to the right bucket's
//! `WaitSource`.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub mod adapter;
pub mod notification;

use adapter::step_engine::{
    self, Errno, NoProgress, OneShotStepOp, ScriptCtx, SpinMutex, StepOp, StepOutcome,
    SubjectIdentity, YieldShape, ZoneError,
};
use adapter::wait_routing::{Channel, WaitSource};

use crate::execution::Guard;
use crate::reactor_priority::{self, PiLockToken, PriorityKey, TaskKey};
use crate::thread_runtime::{thread_payload_by_tid, thread_task_by_tid, Tid};
use crate::vm::{AddressSpace, UserVirtAddr, VmBacking};
use tx_hal::UserPtr;

/// Number of futex hash buckets. Fixed; no dynamic allocation.
/// Collisions are absorbed by the per-waiter re-check on wakeup.
pub const FUTEX_BUCKET_COUNT: usize = 256;

/// Wake-mask bit fired into a bucket's channel by `FUTEX_WAKE`.
/// Waiters subscribe to this same bit so any wake fires every
/// waiter in the bucket.
pub const FUTEX_WAKE_MASK: u64 = u32::MAX as u64;
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutexWaitvEntry {
    pub uaddr: u64,
    pub val: u32,
    pub interest_mask: u64,
}

/// Per-bucket state. Holds the wait channel + the carrier id
/// registered with [`crate::wait_source`].
///
/// **PR-3D-2 (D2/D4 coexistence)** adds an `Arc<WaitSource>` next to
/// the legacy `Channel`. Both fire on `step_futex_wake`; v3 callers
/// register a `TaskMailbox` against `wait_source` via
/// `WaitSource::prepare(..).install_if(..)` while legacy callers stay
/// on the `Channel`+`wait_source` resolver path. `wait_source.id()`
/// matches `source_id` (same `u64` in both worlds) so the round-trip
/// from `YieldShape::OnWaitSource { source: WaitSourceId(source_id), .. }`
/// lands on the right bucket.
struct FutexBucket {
    channel: Channel,
    source_id: u64,
    /// PR-3D-2 new path. Fired alongside `channel` on every
    /// `step_futex_wake` call routed to this bucket.
    wait_source: Arc<WaitSource>,
}

/// Static table of 256 buckets. Initialised lazily under a single
/// [`SpinMutex<Option<...>>`] on the first call to
/// [`register_zones`]. Subsequent calls (from re-running
/// [`crate::zones::register_all`] in tests) are no-ops — the
/// buckets are kept across re-init because their carrier ids are
/// already published to [`crate::wait_source`] and tearing them
/// down would invalidate any token references held by in-flight
/// futures.
static BUCKETS: SpinMutex<Option<[FutexBucket; FUTEX_BUCKET_COUNT]>> = SpinMutex::new(None);

/// One-shot init flag. The slow lock-acquire-and-check inside
/// [`register_zones`] is fine on the cold path; the steady-state
/// fast path is the in-step `lock_buckets()` call which never
/// re-initialises.
static INITIALISED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FutexKey {
    namespace: FutexKeyNamespace,
    offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FutexKeyNamespace {
    Private { aspace: usize },
    SharedPage { page_container: u32 },
}

struct FutexWaiter {
    channel: Channel,
    source_id: u64,
    wait_source: Arc<WaitSource>,
    interest_mask: u64,
    sequence: u64,
    kind: FutexWaiterKind,
}

enum FutexWaiterKind {
    Classic,
    WaitRequeuePi {
        target_key: FutexKey,
        waiter_tid: u32,
    },
}

struct FutexTable {
    entries: BTreeMap<FutexKey, Vec<FutexWaiter>>,
    next_sequence: u64,
}

impl FutexTable {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_sequence: 1,
        }
    }

    fn alloc_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        sequence
    }
}

static EXACT_WAITERS: SpinMutex<Option<FutexTable>> = SpinMutex::new(None);
static WAITING_TIDS: SpinMutex<BTreeMap<u32, usize>> = SpinMutex::new(BTreeMap::new());

pub(crate) fn register_waiting_tid(tid: Option<u32>) {
    let Some(tid) = tid else {
        return;
    };
    let mut tids = WAITING_TIDS.lock();
    let count = tids.entry(tid & FUTEX_TID_MASK).or_insert(0);
    *count = count.saturating_add(1);
}

pub(crate) fn unregister_waiting_tid(tid: Option<u32>) {
    let Some(tid) = tid else {
        return;
    };
    let tid = tid & FUTEX_TID_MASK;
    let mut tids = WAITING_TIDS.lock();
    let remove = if let Some(count) = tids.get_mut(&tid) {
        *count = count.saturating_sub(1);
        *count == 0
    } else {
        false
    };
    if remove {
        tids.remove(&tid);
    }
}

pub fn thread_has_waiter(tid: u32) -> bool {
    let tid = tid & FUTEX_TID_MASK;
    if tid == 0 {
        return false;
    }
    if WAITING_TIDS.lock().get(&tid).copied().unwrap_or(0) > 0 {
        return true;
    }
    EXACT_WAITERS
        .lock()
        .as_ref()
        .map(|table| {
            table.entries.values().any(|waiters| {
                waiters.iter().any(|waiter| {
                    matches!(
                        waiter.kind,
                        FutexWaiterKind::WaitRequeuePi { waiter_tid, .. }
                            if (waiter_tid & FUTEX_TID_MASK) == tid
                    )
                })
            })
        })
        .unwrap_or(false)
        || PI_WAITERS
            .lock()
            .as_ref()
            .map(|table| {
                table.entries.values().any(|state| {
                    state
                        .waiters_by_id
                        .values()
                        .any(|waiter| (waiter.waiter_tid & FUTEX_TID_MASK) == tid)
                })
            })
            .unwrap_or(false)
}

struct FutexPiWaiter {
    waiter_id: WaiterId,
    channel: Channel,
    source_id: u64,
    wait_source: Arc<WaitSource>,
    waiter_tid: u32,
    waiter_task: Option<TaskKey>,
    blocked_owner_tid: u32,
    blocked_on_key: FutexKey,
    order_key: PiWaiterOrder,
    sequence: u64,
}

type WaiterId = u64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PiWaiterOrder {
    priority: u8,
    fifo_rank: u64,
    waiter_id: WaiterId,
}

impl PiWaiterOrder {
    fn new(priority: u8, sequence: u64, waiter_id: WaiterId) -> Self {
        Self {
            priority,
            fifo_rank: u64::MAX - sequence,
            waiter_id,
        }
    }

    fn priority_key(self) -> PriorityKey {
        PriorityKey::rt(self.priority, u64::MAX - self.fifo_rank)
    }
}

struct FutexPiLockState {
    owner_tid: u32,
    waiters_by_order: BTreeMap<PiWaiterOrder, WaiterId>,
    waiters_by_id: BTreeMap<WaiterId, FutexPiWaiter>,
    top_waiter: Option<WaiterId>,
}

impl FutexPiLockState {
    fn new(owner_tid: u32) -> Self {
        Self {
            owner_tid: owner_tid & FUTEX_TID_MASK,
            waiters_by_order: BTreeMap::new(),
            waiters_by_id: BTreeMap::new(),
            top_waiter: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.waiters_by_id.is_empty()
    }

    fn top_waiter(&self) -> Option<&FutexPiWaiter> {
        self.top_waiter
            .and_then(|waiter_id| self.waiters_by_id.get(&waiter_id))
    }

    fn refresh_top_waiter(&mut self) {
        self.top_waiter = self.waiters_by_order.iter().next_back().map(|(_, id)| *id);
    }

    fn insert_waiter(&mut self, waiter: FutexPiWaiter) {
        self.owner_tid = waiter.blocked_owner_tid & FUTEX_TID_MASK;
        self.waiters_by_order
            .insert(waiter.order_key, waiter.waiter_id);
        self.waiters_by_id.insert(waiter.waiter_id, waiter);
        self.refresh_top_waiter();
    }

    fn remove_waiter(&mut self, waiter_id: WaiterId) -> Option<FutexPiWaiter> {
        let waiter = self.waiters_by_id.remove(&waiter_id)?;
        self.waiters_by_order.remove(&waiter.order_key);
        self.refresh_top_waiter();
        Some(waiter)
    }

    fn remove_source(&mut self, source_id: u64) -> Option<FutexPiWaiter> {
        let waiter_id = self
            .waiters_by_id
            .iter()
            .find(|(_, waiter)| waiter.source_id == source_id)
            .map(|(id, _)| *id)?;
        self.remove_waiter(waiter_id)
    }

    fn pop_top_waiter(&mut self) -> Option<FutexPiWaiter> {
        let waiter_id = self.top_waiter?;
        self.remove_waiter(waiter_id)
    }

    fn update_waiter_priority(&mut self, waiter_id: WaiterId, priority: u8) -> bool {
        let Some(waiter) = self.waiters_by_id.get_mut(&waiter_id) else {
            return false;
        };
        self.waiters_by_order.remove(&waiter.order_key);
        waiter.order_key = PiWaiterOrder::new(priority, waiter.sequence, waiter.waiter_id);
        self.waiters_by_order
            .insert(waiter.order_key, waiter.waiter_id);
        self.refresh_top_waiter();
        true
    }
}

struct FutexPiTable {
    entries: BTreeMap<FutexKey, FutexPiLockState>,
    next_sequence: u64,
    next_waiter_id: WaiterId,
}

impl FutexPiTable {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_sequence: 1,
            next_waiter_id: 1,
        }
    }

    fn alloc_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        sequence
    }

    fn alloc_waiter_id(&mut self) -> WaiterId {
        let waiter_id = self.next_waiter_id;
        self.next_waiter_id = self.next_waiter_id.saturating_add(1);
        waiter_id
    }
}

static PI_WAITERS: SpinMutex<Option<FutexPiTable>> = SpinMutex::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FutexRequeuePiResume {
    SourceWake,
    AcquiredPi,
}

static REQUEUE_PI_RESUMES: SpinMutex<Option<BTreeMap<u64, FutexRequeuePiResume>>> =
    SpinMutex::new(None);

const FUTEX_PI_MAX_CHAIN_DEPTH: usize = 16;

fn key_for(aspace: &AddressSpace, uaddr: u64) -> FutexKey {
    let private = || FutexKey {
        namespace: FutexKeyNamespace::Private {
            aspace: aspace as *const AddressSpace as usize,
        },
        offset: uaddr,
    };
    let Some(entry) = aspace.lookup(UserVirtAddr(uaddr as usize)) else {
        return private();
    };
    if entry.flags.shared {
        if let VmBacking::Page { pc, offset } = entry.backing {
            if let Some(delta) = (uaddr as usize).checked_sub(entry.range.start().as_usize()) {
                if let Ok(delta) = u64::try_from(delta) {
                    if let Some(offset) = offset.checked_add(delta) {
                        return FutexKey {
                            namespace: FutexKeyNamespace::SharedPage {
                                page_container: pc.raw(),
                            },
                            offset,
                        };
                    }
                }
            }
        }
    }
    FutexKey {
        // This is the Linux private-futex key shape: same virtual
        // address in two different address spaces is not the same
        // futex. Shared page-backed mappings use the backing-object
        // identity branch above.
        namespace: FutexKeyNamespace::Private {
            aspace: aspace as *const AddressSpace as usize,
        },
        offset: uaddr,
    }
}

fn new_waiter(sequence: u64, interest_mask: u64) -> FutexWaiter {
    let wait_point = notification::new_wait_point();
    new_waiter_from_wait_point(sequence, interest_mask, &wait_point)
}

fn new_waiter_from_wait_point(
    sequence: u64,
    interest_mask: u64,
    wait_point: &notification::FutexWaitPoint,
) -> FutexWaiter {
    FutexWaiter {
        channel: wait_point.channel.clone(),
        source_id: wait_point.source_id,
        wait_source: wait_point.wait_source.clone(),
        interest_mask,
        sequence,
        kind: FutexWaiterKind::Classic,
    }
}

fn futex_pi_lock_token(key: FutexKey) -> PiLockToken {
    let namespace = match key.namespace {
        FutexKeyNamespace::Private { aspace } => aspace as u64,
        FutexKeyNamespace::SharedPage { page_container } => {
            (1u64 << 63) | u64::from(page_container)
        }
    };
    PiLockToken::new(namespace, key.offset)
}

fn pi_waiter_priority(waiter_task: Option<TaskKey>) -> u8 {
    waiter_task
        .and_then(|task| reactor_priority::task_effective_rt_priority(task).ok())
        .unwrap_or(0)
}

fn new_pi_waiter(
    waiter_id: WaiterId,
    sequence: u64,
    blocked_on_key: FutexKey,
    waiter_tid: u32,
    blocked_owner_tid: u32,
) -> FutexPiWaiter {
    let wait_point = notification::new_wait_point();
    let waiter_task = thread_task_by_tid(Tid(waiter_tid & FUTEX_TID_MASK));
    let priority = pi_waiter_priority(waiter_task);
    FutexPiWaiter {
        waiter_id,
        channel: wait_point.channel,
        source_id: wait_point.source_id,
        wait_source: wait_point.wait_source,
        waiter_tid,
        waiter_task,
        blocked_owner_tid: blocked_owner_tid & FUTEX_TID_MASK,
        blocked_on_key,
        order_key: PiWaiterOrder::new(priority, sequence, waiter_id),
        sequence,
    }
}

fn new_wait_requeue_pi_waiter(sequence: u64, target_key: FutexKey, waiter_tid: u32) -> FutexWaiter {
    let wait_point = notification::new_wait_point();
    FutexWaiter {
        channel: wait_point.channel,
        source_id: wait_point.source_id,
        wait_source: wait_point.wait_source,
        interest_mask: FUTEX_WAKE_MASK,
        sequence,
        kind: FutexWaiterKind::WaitRequeuePi {
            target_key,
            waiter_tid,
        },
    }
}

fn pi_waiter_from_requeue_waiter(
    waiter_id: WaiterId,
    sequence: u64,
    blocked_on_key: FutexKey,
    waiter: FutexWaiter,
    blocked_owner_tid: u32,
) -> Option<FutexPiWaiter> {
    let FutexWaiterKind::WaitRequeuePi { waiter_tid, .. } = waiter.kind else {
        return None;
    };
    let waiter_task = thread_task_by_tid(Tid(waiter_tid & FUTEX_TID_MASK));
    let priority = pi_waiter_priority(waiter_task);
    Some(FutexPiWaiter {
        waiter_id,
        channel: waiter.channel,
        source_id: waiter.source_id,
        wait_source: waiter.wait_source,
        waiter_tid,
        waiter_task,
        blocked_owner_tid: blocked_owner_tid & FUTEX_TID_MASK,
        blocked_on_key,
        order_key: PiWaiterOrder::new(priority, sequence, waiter_id),
        sequence,
    })
}

fn pi_owner_tid_is_live(owner_tid: u32) -> bool {
    let owner_tid = owner_tid & FUTEX_TID_MASK;
    owner_tid != 0 && thread_payload_by_tid(Tid(owner_tid)).is_some()
}

fn pi_waiter_blocked_owner_in_table(table: &FutexPiTable, waiter_tid: u32) -> Option<u32> {
    let waiter_tid = waiter_tid & FUTEX_TID_MASK;
    table.entries.values().find_map(|state| {
        state
            .waiters_by_id
            .values()
            .find(|waiter| (waiter.waiter_tid & FUTEX_TID_MASK) == waiter_tid)
            .map(|waiter| waiter.blocked_owner_tid & FUTEX_TID_MASK)
    })
}

fn pi_chain_would_deadlock_in_table(
    table: &FutexPiTable,
    waiter_tid: u32,
    owner_tid: u32,
) -> Result<(), Errno> {
    let waiter_tid = waiter_tid & FUTEX_TID_MASK;
    let mut owner_tid = owner_tid & FUTEX_TID_MASK;
    if waiter_tid == 0 || owner_tid == 0 {
        return Ok(());
    }
    for _ in 0..FUTEX_PI_MAX_CHAIN_DEPTH {
        if owner_tid == waiter_tid {
            return Err(Errno::EDEADLK);
        }
        if !pi_owner_tid_is_live(owner_tid) {
            return Err(Errno::ESRCH);
        }
        let Some(next_owner_tid) = pi_waiter_blocked_owner_in_table(table, owner_tid) else {
            return Ok(());
        };
        owner_tid = next_owner_tid;
    }
    Err(Errno::EDEADLK)
}

fn propagate_pi_donation_from(waiter_tid: u32) {
    propagate_pi_donation_from_inner(waiter_tid & FUTEX_TID_MASK, 0);
}

fn propagate_pi_donation_from_inner(waiter_tid: u32, depth: usize) {
    if waiter_tid == 0 || depth >= FUTEX_PI_MAX_CHAIN_DEPTH {
        return;
    }
    let (next_owner, changed) = {
        let mut table_guard = PI_WAITERS.lock();
        let Some(table) = table_guard.as_mut() else {
            return;
        };
        let mut found = None;
        for (key, state) in table.entries.iter_mut() {
            let Some(waiter_id) = state
                .waiters_by_id
                .iter()
                .find(|(_, waiter)| (waiter.waiter_tid & FUTEX_TID_MASK) == waiter_tid)
                .map(|(id, _)| *id)
            else {
                continue;
            };
            let old_top = state.top_waiter;
            let owner_tid = state.owner_tid & FUTEX_TID_MASK;
            if let Some(waiter) = state.waiters_by_id.get(&waiter_id) {
                debug_assert_eq!(waiter.blocked_on_key, *key);
                let owner_tid = waiter.blocked_owner_tid & FUTEX_TID_MASK;
                if owner_tid == 0 || owner_tid == waiter_tid {
                    return;
                }
            }
            let priority = state
                .waiters_by_id
                .get(&waiter_id)
                .map(|waiter| pi_waiter_priority(waiter.waiter_task))
                .unwrap_or(0);
            let changed = state.update_waiter_priority(waiter_id, priority);
            sync_pi_owner_waiter_from_state(*key, owner_tid, state);
            found = Some((owner_tid, changed || state.top_waiter != old_top));
            break;
        }
        found.unwrap_or((0, false))
    };
    if next_owner != 0 && changed {
        propagate_pi_donation_from_inner(next_owner, depth + 1);
    }
}

fn sync_pi_owner_waiter_from_state(key: FutexKey, owner_tid: u32, state: &FutexPiLockState) {
    let owner_tid = owner_tid & FUTEX_TID_MASK;
    let Some(owner_task) = thread_task_by_tid(Tid(owner_tid)) else {
        return;
    };
    let lock = futex_pi_lock_token(key);
    if let Some(top) = state.top_waiter() {
        if owner_tid != 0 && owner_tid != (top.waiter_tid & FUTEX_TID_MASK) {
            if let Some(waiter_task) = top.waiter_task {
                if top.order_key.priority > 0 {
                    let _ = reactor_priority::upsert_pi_waiter(
                        owner_task,
                        lock,
                        waiter_task,
                        top.order_key.priority_key(),
                    );
                    return;
                }
            }
        }
    }
    let _ = reactor_priority::remove_pi_waiter(owner_task, lock);
}

fn record_requeue_pi_resume(source_id: u64, resume: FutexRequeuePiResume) {
    REQUEUE_PI_RESUMES
        .lock()
        .get_or_insert_with(BTreeMap::new)
        .insert(source_id, resume);
}

fn take_requeue_pi_resume(source_id: u64) -> Option<FutexRequeuePiResume> {
    let mut guard = REQUEUE_PI_RESUMES.lock();
    let table = guard.as_mut()?;
    let resume = table.remove(&source_id);
    if table.is_empty() {
        *guard = None;
    }
    resume
}

#[cfg(test)]
fn debug_exact_waiter_count() -> usize {
    EXACT_WAITERS
        .lock()
        .as_ref()
        .map(|table| table.entries.values().map(Vec::len).sum())
        .unwrap_or(0)
}

#[cfg(test)]
fn debug_pi_waiter_count() -> usize {
    PI_WAITERS
        .lock()
        .as_ref()
        .map(|table| {
            table
                .entries
                .values()
                .map(|state| state.waiters_by_id.len())
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
fn exact_wait_source_for_source_id(source_id: u64) -> Option<Arc<WaitSource>> {
    EXACT_WAITERS
        .lock()
        .as_ref()?
        .entries
        .values()
        .flat_map(|waiters| waiters.iter())
        .find(|waiter| waiter.source_id == source_id)
        .map(|waiter| waiter.wait_source.clone())
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    *EXACT_WAITERS.lock() = None;
    *PI_WAITERS.lock() = None;
    *REQUEUE_PI_RESUMES.lock() = None;
}

#[cfg(test)]
fn reset_exact_waiters_for_tests() {
    reset_for_test();
}

/// Initialise the bucket table. Idempotent: a second call is a
/// no-op. Called once from [`crate::zones::register_all`].
pub(crate) fn register_zones() -> Result<(), ZoneError> {
    if INITIALISED.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut guard = BUCKETS.lock();
    if guard.is_some() {
        INITIALISED.store(true, Ordering::Release);
        return Ok(());
    }
    let buckets: [FutexBucket; FUTEX_BUCKET_COUNT] = core::array::from_fn(|_| {
        let wait_point = notification::new_wait_point();
        FutexBucket {
            channel: wait_point.channel,
            source_id: wait_point.source_id,
            wait_source: wait_point.wait_source,
        }
    });
    *guard = Some(buckets);
    INITIALISED.store(true, Ordering::Release);
    Ok(())
}

/// Hash a user-VA pointer into a bucket index. The user-VA's low
/// 2-3 bits are usually zero (4-byte alignment), so we mix the high
/// half in via a rotation before masking. Distribution is good
/// enough for v1; collisions are absorbed by the per-waiter
/// re-check.
pub fn bucket_index(uaddr: u64) -> usize {
    let h = uaddr ^ uaddr.rotate_right(17);
    (h as usize) & (FUTEX_BUCKET_COUNT - 1)
}

/// `futex(uaddr, FUTEX_WAIT, val, timeout, ...)`.
///
/// Samples `*uaddr`; if it equals `val`, yields on the bucket's wait
/// channel (`OnWaitSource`); otherwise returns `Err(EAGAIN)`.
/// `uaddr` must be non-zero and 4-byte aligned; otherwise `Err(EINVAL)`.
/// `timeout` is ignored in v1.
///
/// Returns a [`StepOutcome`]:
/// - bad uaddr → `Err(Errno::EINVAL)`
/// - `*uaddr != val` → `Err(Errno::EAGAIN)`
/// - `*uaddr == val` → `Yield { progress: NoProgress, shape: OnWaitSource { … } }`
///
/// Wait never produces `Done`: completion arrives via the carrier
/// resolution step driven by the script driver after the yield resolves.
pub fn step_futex_wait(
    aspace: &AddressSpace,
    uaddr: u64,
    val: u32,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    step_futex_wait_masked(aspace, uaddr, val, FUTEX_WAKE_MASK, guard)
}

pub fn step_futex_wait_masked(
    aspace: &AddressSpace,
    uaddr: u64,
    val: u32,
    interest_mask: u64,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // observe — validate uaddr alignment, read *uaddr under guard
    // upgrade: N/A — no IdentRef→Cap needed; uaddr verified directly
    // reserve: N/A — bucket array is static pre-allocated
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    if interest_mask == 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    // Read the futex word through the safe user-access gate
    // (`AddressSpace::read_user` → `copy_from_user`), replacing the
    // retired `core::ptr::read_volatile` bootstrap exemption
    // (TODO(phase-userva) resolved).
    let observed: u32 = match aspace.read_user(UserPtr::<u32>::new(uaddr as usize), guard) {
        StepOutcome::Done(v) => v,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    };
    if observed != val {
        return StepOutcome::Err(Errno::EAGAIN);
    }
    // upgrade — N/A (futex wait doesn't upgrade references)
    // reserve — lazy exact-key entry allocation if this futex word has
    // never been waited on before.
    // commit — while holding the futex table lock, re-read the word.
    // This closes the common lost-wake window between the userspace
    // value check and publishing the wait source.
    let key = key_for(aspace, uaddr);
    let source_id = {
        let mut table_guard = EXACT_WAITERS.lock();
        let table = table_guard.get_or_insert_with(FutexTable::new);
        let observed_again: u32 = match aspace.read_user(UserPtr::<u32>::new(uaddr as usize), guard)
        {
            StepOutcome::Done(v) => v,
            StepOutcome::Err(e) => return StepOutcome::Err(e),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                return StepOutcome::Err(Errno::EFAULT);
            }
        };
        if observed_again != val {
            return StepOutcome::Err(Errno::EAGAIN);
        }
        let sequence = table.alloc_sequence();
        let waiter = new_waiter(sequence, interest_mask);
        let source_id = waiter.source_id;
        table.entries.entry(key).or_default().push(waiter);
        source_id
    };
    // publish — yield on the exact `(AddressSpace, uaddr)` wait source.
    // The old bucket source remains only as a compatibility wake path.
    step_engine::yield_until_wake(source_id, interest_mask)
}

/// `futex(uaddr, FUTEX_WAKE, n, ...)` scoped to one address space.
///
/// This is the syscall/thread-exit path. It uses the exact
/// `(AddressSpace, uaddr)` key that [`step_futex_wait`] published,
/// avoiding the old bucket model's cross-address wakeups.
pub fn step_futex_wake_in(
    aspace: &AddressSpace,
    uaddr: u64,
    n: u32,
    _guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    step_futex_wake_masked_in(aspace, uaddr, n, FUTEX_WAKE_MASK, _guard)
}

pub fn step_futex_wake_masked_in(
    aspace: &AddressSpace,
    uaddr: u64,
    n: u32,
    wake_mask: u64,
    _guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    if n == 0 || wake_mask == 0 {
        return StepOutcome::Done(0);
    }

    let key = key_for(aspace, uaddr);
    let wakes = {
        let mut table_guard = EXACT_WAITERS.lock();
        let Some(table) = table_guard.as_mut() else {
            return StepOutcome::Done(0);
        };
        let Some(waiters) = table.entries.get_mut(&key) else {
            return StepOutcome::Done(0);
        };
        debug_assert!(
            waiters
                .windows(2)
                .all(|pair| pair[0].sequence <= pair[1].sequence),
            "futex waiters should retain FIFO sequence order",
        );
        let mut wakes = Vec::new();
        let mut index = 0;
        while index < waiters.len() && wakes.len() < n as usize {
            let fired_mask = waiters[index].interest_mask & wake_mask;
            if fired_mask == 0 {
                index += 1;
                continue;
            }
            let waiter = waiters.remove(index);
            let resume = match waiter.kind {
                FutexWaiterKind::Classic => None,
                FutexWaiterKind::WaitRequeuePi { .. } => Some(FutexRequeuePiResume::SourceWake),
            };
            wakes.push((
                waiter.source_id,
                waiter.channel,
                waiter.wait_source,
                fired_mask,
                resume,
            ));
        }
        if waiters.is_empty() {
            table.entries.remove(&key);
        }
        wakes
    };
    let woken = wakes.len() as u32;
    for (source_id, channel, wait_source, fired_mask, resume) in wakes {
        if let Some(resume) = resume {
            record_requeue_pi_resume(source_id, resume);
        }
        notification::notify_exact(&channel, &wait_source, fired_mask);
    }
    StepOutcome::Done(woken)
}

pub fn step_futex_cancel_wait_in(
    aspace: &AddressSpace,
    uaddr: u64,
    interest_mask: u64,
    _guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if uaddr == 0 || (uaddr & 0x3) != 0 || interest_mask == 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let key = key_for(aspace, uaddr);
    let mut table_guard = EXACT_WAITERS.lock();
    let Some(table) = table_guard.as_mut() else {
        return StepOutcome::Done(());
    };
    let remove = if let Some(waiters) = table.entries.get_mut(&key) {
        if let Some(index) = waiters
            .iter()
            .position(|waiter| waiter.interest_mask & interest_mask != 0)
        {
            waiters.remove(index);
        }
        waiters.is_empty()
    } else {
        false
    };
    if remove {
        table.entries.remove(&key);
    }
    StepOutcome::Done(())
}

fn step_futex_cancel_wait_source(source_id: u64) -> StepOutcome<(), NoProgress> {
    let _ = take_requeue_pi_resume(source_id);
    let mut table_guard = EXACT_WAITERS.lock();
    let Some(table) = table_guard.as_mut() else {
        return StepOutcome::Done(());
    };
    let mut empty_keys = Vec::new();
    for (key, waiters) in table.entries.iter_mut() {
        waiters.retain(|waiter| waiter.source_id != source_id);
        if waiters.is_empty() {
            empty_keys.push(*key);
        }
    }
    for key in empty_keys {
        table.entries.remove(&key);
    }
    StepOutcome::Done(())
}

fn exact_waiter_source_exists(source_id: u64) -> bool {
    EXACT_WAITERS
        .lock()
        .as_ref()
        .map(|table| {
            table
                .entries
                .values()
                .any(|waiters| waiters.iter().any(|waiter| waiter.source_id == source_id))
        })
        .unwrap_or(false)
}

fn step_futex_pi_cancel_wait_source(source_id: u64) -> StepOutcome<(), NoProgress> {
    let _ = take_requeue_pi_resume(source_id);
    let mut affected_owners = Vec::new();
    {
        let mut table_guard = PI_WAITERS.lock();
        let Some(table) = table_guard.as_mut() else {
            return StepOutcome::Done(());
        };
        let mut empty_keys = Vec::new();
        for (key, state) in table.entries.iter_mut() {
            if let Some(waiter) = state.remove_source(source_id) {
                let owner_tid = waiter.blocked_owner_tid & FUTEX_TID_MASK;
                sync_pi_owner_waiter_from_state(*key, owner_tid, state);
                if owner_tid != 0 {
                    affected_owners.push(owner_tid);
                }
            }
            if state.is_empty() {
                empty_keys.push(*key);
            }
        }
        for key in empty_keys {
            table.entries.remove(&key);
        }
    };
    for owner_tid in affected_owners {
        propagate_pi_donation_from(owner_tid);
    }
    StepOutcome::Done(())
}

fn waitv_published_index(
    aspace: &AddressSpace,
    waits: &[FutexWaitvEntry],
    sequences: &[u64],
    source_id: u64,
) -> Option<u32> {
    let table_guard = EXACT_WAITERS.lock();
    let table = table_guard.as_ref();
    for (idx, (wait, sequence)) in waits.iter().zip(sequences.iter()).enumerate() {
        let key = key_for(aspace, wait.uaddr);
        let still_waiting = table
            .and_then(|table| table.entries.get(&key))
            .map(|waiters| {
                waiters
                    .iter()
                    .any(|waiter| waiter.source_id == source_id && waiter.sequence == *sequence)
            })
            .unwrap_or(false);
        if !still_waiting {
            return Some(idx as u32);
        }
    }
    None
}

pub fn step_futex_requeue_in(
    aspace: &AddressSpace,
    uaddr: u64,
    uaddr2: u64,
    wake_n: u32,
    requeue_n: u32,
    guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if uaddr == 0 || uaddr2 == 0 || (uaddr & 0x3) != 0 || (uaddr2 & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let mut total = 0u32;
    if wake_n > 0 {
        match step_futex_wake_in(aspace, uaddr, wake_n, guard) {
            StepOutcome::Done(woken) => total = total.saturating_add(woken),
            StepOutcome::Err(e) => return StepOutcome::Err(e),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                return StepOutcome::Err(Errno::EIO);
            }
        }
    }

    let source_key = key_for(aspace, uaddr);
    let target_key = key_for(aspace, uaddr2);
    let moved = {
        let mut table_guard = EXACT_WAITERS.lock();
        let Some(table) = table_guard.as_mut() else {
            return StepOutcome::Done(total);
        };
        if source_key == target_key {
            return StepOutcome::Done(total);
        }
        let mut moved_waiters = Vec::new();
        let source_empty = {
            let Some(source_waiters) = table.entries.get_mut(&source_key) else {
                return StepOutcome::Done(total);
            };
            let move_count = source_waiters.len().min(requeue_n as usize);
            if move_count == 0 {
                return StepOutcome::Done(total);
            }
            for _ in 0..move_count {
                moved_waiters.push(source_waiters.remove(0));
            }
            source_waiters.is_empty()
        };
        if source_empty {
            table.entries.remove(&source_key);
        }
        let moved = moved_waiters.len() as u32;
        table
            .entries
            .entry(target_key)
            .or_default()
            .extend(moved_waiters);
        moved
    };
    StepOutcome::Done(total.saturating_add(moved))
}

pub fn step_futex_wait_requeue_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    val: u32,
    uaddr2: u64,
    waiter_tid: u32,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    if uaddr == 0
        || uaddr2 == 0
        || (uaddr & 0x3) != 0
        || (uaddr2 & 0x3) != 0
        || waiter_tid == 0
        || (waiter_tid & !FUTEX_TID_MASK) != 0
    {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let source_key = key_for(aspace, uaddr);
    let target_key = key_for(aspace, uaddr2);
    if source_key == target_key {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let ptr = UserPtr::<u32>::new(uaddr as usize);
    match aspace.read_user(ptr, guard) {
        StepOutcome::Done(observed) if observed == val => {}
        StepOutcome::Done(_) => return StepOutcome::Err(Errno::EAGAIN),
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    }

    let source_id = {
        let mut table_guard = EXACT_WAITERS.lock();
        let table = table_guard.get_or_insert_with(FutexTable::new);
        match aspace.read_user(ptr, guard) {
            StepOutcome::Done(observed) if observed == val => {}
            StepOutcome::Done(_) => return StepOutcome::Err(Errno::EAGAIN),
            StepOutcome::Err(e) => return StepOutcome::Err(e),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                return StepOutcome::Err(Errno::EFAULT);
            }
        }
        if table.entries.get(&source_key).is_some_and(|waiters| {
            waiters
                .iter()
                .any(|waiter| matches!(waiter.kind, FutexWaiterKind::Classic))
        }) {
            return StepOutcome::Err(Errno::EINVAL);
        }
        let sequence = table.alloc_sequence();
        let waiter = new_wait_requeue_pi_waiter(sequence, target_key, waiter_tid & FUTEX_TID_MASK);
        let source_id = waiter.source_id;
        table.entries.entry(source_key).or_default().push(waiter);
        source_id
    };
    step_engine::yield_until_wake(source_id, FUTEX_WAKE_MASK)
}

pub fn step_futex_cmp_requeue_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    uaddr2: u64,
    nr_wake: u32,
    nr_requeue: u32,
    cmpval: u32,
    guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    if uaddr == 0 || uaddr2 == 0 || (uaddr & 0x3) != 0 || (uaddr2 & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    if nr_wake != 1 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let source_key = key_for(aspace, uaddr);
    let target_key = key_for(aspace, uaddr2);
    if source_key == target_key {
        return StepOutcome::Err(Errno::EINVAL);
    }
    match aspace.read_user(UserPtr::<u32>::new(uaddr as usize), guard) {
        StepOutcome::Done(observed) if observed == cmpval => {}
        StepOutcome::Done(_) => return StepOutcome::Err(Errno::EAGAIN),
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    }

    let (selected_tids, take_count) = {
        let exact_guard = EXACT_WAITERS.lock();
        let Some(table) = exact_guard.as_ref() else {
            return StepOutcome::Done(0);
        };
        let Some(source_waiters) = table.entries.get(&source_key) else {
            return StepOutcome::Done(0);
        };
        if source_waiters.iter().any(|waiter| {
            !matches!(
                waiter.kind,
                FutexWaiterKind::WaitRequeuePi {
                    target_key: waiter_target,
                    ..
                } if waiter_target == target_key
            )
        }) {
            return StepOutcome::Err(Errno::EINVAL);
        }
        let take_count = source_waiters
            .len()
            .min(1usize.saturating_add(nr_requeue as usize));
        if take_count == 0 {
            return StepOutcome::Done(0);
        }
        let mut selected_tids = Vec::with_capacity(take_count);
        for waiter in source_waiters.iter().take(take_count) {
            let FutexWaiterKind::WaitRequeuePi { waiter_tid, .. } = waiter.kind else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            selected_tids.push(waiter_tid & FUTEX_TID_MASK);
        }
        (selected_tids, take_count)
    };
    let first_tid = selected_tids[0];

    let target_ptr = UserPtr::<u32>::new(uaddr2 as usize);
    let target_observed = match aspace.read_user(target_ptr, guard) {
        StepOutcome::Done(v) => v,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    };
    if (target_observed & FUTEX_TID_MASK) == (first_tid & FUTEX_TID_MASK) {
        return StepOutcome::Err(Errno::EDEADLK);
    }
    if (target_observed & FUTEX_TID_MASK) != 0 && !pi_owner_tid_is_live(target_observed) {
        return StepOutcome::Err(Errno::ESRCH);
    }
    {
        let pi_guard = PI_WAITERS.lock();
        if let Some(table) = pi_guard.as_ref() {
            let target_owner_tid = target_observed & FUTEX_TID_MASK;
            if target_owner_tid != 0 {
                for waiter_tid in &selected_tids {
                    if let Err(errno) =
                        pi_chain_would_deadlock_in_table(table, *waiter_tid, target_owner_tid)
                    {
                        return StepOutcome::Err(errno);
                    }
                }
            }
        }
    }

    let mut selected = Vec::new();
    {
        let mut exact_guard = EXACT_WAITERS.lock();
        let Some(table) = exact_guard.as_mut() else {
            return StepOutcome::Done(0);
        };
        let Some(source_waiters) = table.entries.get_mut(&source_key) else {
            return StepOutcome::Done(0);
        };
        for _ in 0..take_count {
            selected.push(source_waiters.remove(0));
        }
        if source_waiters.is_empty() {
            table.entries.remove(&source_key);
        }
    }

    let selected_count = selected.len() as u32;
    let first = selected.remove(0);

    let target_has_owner = (target_observed & FUTEX_TID_MASK) != 0;
    let existing_target_waiters = PI_WAITERS
        .lock()
        .as_ref()
        .and_then(|table| table.entries.get(&target_key))
        .is_some_and(|state| !state.is_empty());
    let more_waiters = !selected.is_empty() || existing_target_waiters;
    let propagate_tid = if target_has_owner {
        target_observed & FUTEX_TID_MASK
    } else {
        first_tid & FUTEX_TID_MASK
    };

    if !target_has_owner {
        let mut new_word = first_tid & FUTEX_TID_MASK;
        if more_waiters {
            new_word |= FUTEX_WAITERS;
        }
        match aspace.write_user(target_ptr, new_word, guard) {
            StepOutcome::Done(()) => {}
            StepOutcome::Err(e) => return StepOutcome::Err(e),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                return StepOutcome::Err(Errno::EFAULT);
            }
        }
        record_requeue_pi_resume(first.source_id, FutexRequeuePiResume::AcquiredPi);
        notification::notify_exact(&first.channel, &first.wait_source, FUTEX_WAKE_MASK);
        if !selected.is_empty() {
            let mut pi_guard = PI_WAITERS.lock();
            let pi_table = pi_guard.get_or_insert_with(FutexPiTable::new);
            for waiter in selected {
                let waiter_id = pi_table.alloc_waiter_id();
                let sequence = pi_table.alloc_sequence();
                let pi_waiter = pi_waiter_from_requeue_waiter(
                    waiter_id, sequence, target_key, waiter, first_tid,
                )
                .expect("validated requeue-pi waiter");
                let state = pi_table
                    .entries
                    .entry(target_key)
                    .or_insert_with(|| FutexPiLockState::new(first_tid));
                state.insert_waiter(pi_waiter);
                sync_pi_owner_waiter_from_state(target_key, first_tid, state);
            }
        }
    } else {
        if (target_observed & FUTEX_WAITERS) == 0 {
            match aspace.write_user(target_ptr, target_observed | FUTEX_WAITERS, guard) {
                StepOutcome::Done(()) => {}
                StepOutcome::Err(e) => return StepOutcome::Err(e),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    return StepOutcome::Err(Errno::EFAULT);
                }
            }
        }
        let mut pi_guard = PI_WAITERS.lock();
        let pi_table = pi_guard.get_or_insert_with(FutexPiTable::new);
        let waiter_id = pi_table.alloc_waiter_id();
        let sequence = pi_table.alloc_sequence();
        let target_owner_tid = target_observed & FUTEX_TID_MASK;
        let first_pi =
            pi_waiter_from_requeue_waiter(waiter_id, sequence, target_key, first, target_owner_tid)
                .expect("validated requeue-pi waiter");
        let state = pi_table
            .entries
            .entry(target_key)
            .or_insert_with(|| FutexPiLockState::new(target_owner_tid));
        state.insert_waiter(first_pi);
        sync_pi_owner_waiter_from_state(target_key, target_owner_tid, state);
        for waiter in selected {
            let waiter_id = pi_table.alloc_waiter_id();
            let sequence = pi_table.alloc_sequence();
            let pi_waiter = pi_waiter_from_requeue_waiter(
                waiter_id,
                sequence,
                target_key,
                waiter,
                target_owner_tid,
            )
            .expect("validated requeue-pi waiter");
            let state = pi_table
                .entries
                .entry(target_key)
                .or_insert_with(|| FutexPiLockState::new(target_owner_tid));
            state.insert_waiter(pi_waiter);
            sync_pi_owner_waiter_from_state(target_key, target_owner_tid, state);
        }
    }

    propagate_pi_donation_from(propagate_tid);
    StepOutcome::Done(selected_count)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutexWakeOpSpec {
    pub op: u32,
    pub cmp: u32,
    pub oparg: u32,
    pub cmparg: u32,
}

/// Decode Linux's `FUTEX_WAKE_OP` `val3` field.
///
/// The encoded layout is `(op << 28) | (cmp << 24) | (oparg << 12) | cmparg`.
/// `op` bit 3 is `FUTEX_OP_OPARG_SHIFT`; the low three bits choose the
/// operation.
pub fn decode_futex_wake_op(encoded: u32) -> FutexWakeOpSpec {
    FutexWakeOpSpec {
        op: (encoded >> 28) & 0xf,
        cmp: (encoded >> 24) & 0xf,
        oparg: (encoded >> 12) & 0xfff,
        cmparg: encoded & 0xfff,
    }
}

pub fn step_futex_wake_op_in(
    aspace: &AddressSpace,
    uaddr: u64,
    uaddr2: u64,
    wake_n: u32,
    wake2_n: u32,
    encoded_op: u32,
    guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    if uaddr == 0 || uaddr2 == 0 || (uaddr & 0x3) != 0 || (uaddr2 & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let spec = decode_futex_wake_op(encoded_op);
    let ptr = UserPtr::<u32>::new(uaddr2 as usize);
    let old = match aspace.read_user(ptr, guard) {
        StepOutcome::Done(v) => v,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    };
    let oparg = if (spec.op & 0x8) != 0 {
        1u32.checked_shl(spec.oparg & 31).unwrap_or(0)
    } else {
        spec.oparg
    };
    let new = match spec.op & 0x7 {
        0 => oparg,
        1 => old.wrapping_add(oparg),
        2 => old | oparg,
        3 => old & !oparg,
        4 => old ^ oparg,
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    match aspace.write_user(ptr, new, guard) {
        StepOutcome::Done(()) => {}
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    }

    let first = match step_futex_wake_in(aspace, uaddr, wake_n, guard) {
        StepOutcome::Done(woken) => woken,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EIO);
        }
    };
    let cmp_ok = match spec.cmp {
        0 => old == spec.cmparg,
        1 => old != spec.cmparg,
        2 => old < spec.cmparg,
        3 => old <= spec.cmparg,
        4 => old > spec.cmparg,
        5 => old >= spec.cmparg,
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    if !cmp_ok {
        return StepOutcome::Done(first);
    }
    match step_futex_wake_in(aspace, uaddr2, wake2_n, guard) {
        StepOutcome::Done(second) => StepOutcome::Done(first.saturating_add(second)),
        StepOutcome::Err(e) => StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => StepOutcome::Err(Errno::EIO),
    }
}

/// `futex(uaddr, FUTEX_WAKE, n, ...)`.
///
/// Returns a [`StepOutcome`]:
/// - bad uaddr (zero or unaligned) → `Err(Errno::EINVAL)`
/// - otherwise → `Done(n)` (best-effort: returned count is the
///   requested `n`, not the actually-woken count; same caveat as
///   `step_futex_wake`). `n == 0` is permitted and returns `Done(0)` —
///   Linux `FUTEX_WAKE` with `n=0` is a defined no-op.
///
/// Wake never produces `Yield`/`Continue`.
pub fn step_futex_wake(uaddr: u64, n: u32, _guard: &Guard<'_>) -> StepOutcome<u32, NoProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // observe: validate uaddr alignment
    // upgrade: N/A — no IdentRef→Cap needed
    // reserve: N/A — no zone allocation needed
    // observe: validate uaddr — no futex-value check needed for wake
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    // upgrade — N/A (futex wake doesn't upgrade references)
    // reserve — N/A (no zone allocation for futex wake)
    // commit — fire legacy channel under bucket lock via wait_routing
    let idx = bucket_index(uaddr);
    // Clone the `Arc<WaitSource>` out under the bucket lock so the
    // `WaitSource::notify` call below (which itself takes the
    // source's subscriber-list lock) runs **outside** the BUCKETS
    // lock — keeps the lock-ordering simple if a future subscriber
    // callback ever needs to call back into futex.
    let (channel, wait_source) = {
        let guard = BUCKETS.lock();
        let buckets = guard
            .as_ref()
            .expect("futex buckets uninitialised — register_zones not called");
        (
            buckets[idx].channel.clone(),
            buckets[idx].wait_source.clone(),
        )
    };
    let posted = notification::notify_bucket(&channel, &wait_source, FUTEX_WAKE_MASK);
    StepOutcome::Done(posted.min(n))
}

pub fn step_futex_lock_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    owner_tid: u32,
    waiters: bool,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if uaddr == 0 || (uaddr & 0x3) != 0 || owner_tid == 0 || (owner_tid & !FUTEX_TID_MASK) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let ptr = UserPtr::<u32>::new(uaddr as usize);
    let observed = match aspace.read_user(ptr, guard) {
        StepOutcome::Done(v) => v,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    };
    if (observed & FUTEX_TID_MASK) != 0 {
        if (observed & FUTEX_TID_MASK) == (owner_tid & FUTEX_TID_MASK) {
            return StepOutcome::Err(Errno::EDEADLK);
        }
        if !pi_owner_tid_is_live(observed) {
            return StepOutcome::Err(Errno::ESRCH);
        }
        let key = key_for(aspace, uaddr);
        let (source_id, propagate_tid) = {
            let mut table_guard = PI_WAITERS.lock();
            let table = table_guard.get_or_insert_with(FutexPiTable::new);
            let observed_again = match aspace.read_user(ptr, guard) {
                StepOutcome::Done(v) => v,
                StepOutcome::Err(e) => return StepOutcome::Err(e),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    return StepOutcome::Err(Errno::EFAULT);
                }
            };
            if (observed_again & FUTEX_TID_MASK) == 0 {
                let mut new = owner_tid & FUTEX_TID_MASK;
                if waiters || (observed_again & FUTEX_WAITERS) != 0 {
                    new |= FUTEX_WAITERS;
                }
                return match aspace.write_user(ptr, new, guard) {
                    StepOutcome::Done(()) => StepOutcome::Done(()),
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                        StepOutcome::Err(Errno::EFAULT)
                    }
                };
            }
            if (observed_again & FUTEX_TID_MASK) == (owner_tid & FUTEX_TID_MASK) {
                return StepOutcome::Err(Errno::EDEADLK);
            }
            if !pi_owner_tid_is_live(observed_again) {
                return StepOutcome::Err(Errno::ESRCH);
            }
            let blocked_owner_tid = observed_again & FUTEX_TID_MASK;
            if let Err(errno) =
                pi_chain_would_deadlock_in_table(table, owner_tid, blocked_owner_tid)
            {
                return StepOutcome::Err(errno);
            }
            if (observed_again & FUTEX_WAITERS) == 0 {
                match aspace.write_user(ptr, observed_again | FUTEX_WAITERS, guard) {
                    StepOutcome::Done(()) => {}
                    StepOutcome::Err(e) => return StepOutcome::Err(e),
                    StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                        return StepOutcome::Err(Errno::EFAULT);
                    }
                }
            }
            let waiter_id = table.alloc_waiter_id();
            let sequence = table.alloc_sequence();
            let waiter = new_pi_waiter(
                waiter_id,
                sequence,
                key,
                owner_tid & FUTEX_TID_MASK,
                blocked_owner_tid,
            );
            let source_id = waiter.source_id;
            let state = table
                .entries
                .entry(key)
                .or_insert_with(|| FutexPiLockState::new(blocked_owner_tid));
            state.insert_waiter(waiter);
            sync_pi_owner_waiter_from_state(key, blocked_owner_tid, state);
            (source_id, owner_tid & FUTEX_TID_MASK)
        };
        propagate_pi_donation_from(propagate_tid);
        return step_engine::yield_until_wake(source_id, FUTEX_WAKE_MASK);
    }
    let mut new = owner_tid & FUTEX_TID_MASK;
    if waiters || (observed & FUTEX_WAITERS) != 0 {
        new |= FUTEX_WAITERS;
    }
    match aspace.write_user(ptr, new, guard) {
        StepOutcome::Done(()) => StepOutcome::Done(()),
        StepOutcome::Err(e) => StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => StepOutcome::Err(Errno::EFAULT),
    }
}

pub fn step_futex_trylock_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    owner_tid: u32,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if uaddr == 0 || (uaddr & 0x3) != 0 || owner_tid == 0 || (owner_tid & !FUTEX_TID_MASK) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let ptr = UserPtr::<u32>::new(uaddr as usize);
    let observed = match aspace.read_user(ptr, guard) {
        StepOutcome::Done(v) => v,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    };
    if (observed & FUTEX_TID_MASK) != 0 {
        if (observed & FUTEX_TID_MASK) == (owner_tid & FUTEX_TID_MASK) {
            return StepOutcome::Err(Errno::EDEADLK);
        }
        if !pi_owner_tid_is_live(observed) {
            return StepOutcome::Err(Errno::ESRCH);
        }
        return StepOutcome::Err(Errno::EAGAIN);
    }
    let mut new = owner_tid & FUTEX_TID_MASK;
    if (observed & FUTEX_WAITERS) != 0 {
        new |= FUTEX_WAITERS;
    }
    match aspace.write_user(ptr, new, guard) {
        StepOutcome::Done(()) => StepOutcome::Done(()),
        StepOutcome::Err(e) => StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => StepOutcome::Err(Errno::EFAULT),
    }
}

pub fn step_futex_unlock_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    owner_tid: u32,
    guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if uaddr == 0 || (uaddr & 0x3) != 0 || owner_tid == 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let ptr = UserPtr::<u32>::new(uaddr as usize);
    let observed = match aspace.read_user(ptr, guard) {
        StepOutcome::Done(v) => v,
        StepOutcome::Err(e) => return StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return StepOutcome::Err(Errno::EFAULT);
        }
    };
    if (observed & FUTEX_TID_MASK) != (owner_tid & FUTEX_TID_MASK) {
        return StepOutcome::Err(Errno::EPERM);
    }
    let key = key_for(aspace, uaddr);
    let handoff = {
        let mut table_guard = PI_WAITERS.lock();
        match table_guard.as_mut().and_then(|table| {
            let state = table.entries.get_mut(&key)?;
            if state.is_empty() {
                return None;
            }
            if let Some(owner_task) = thread_task_by_tid(Tid(owner_tid & FUTEX_TID_MASK)) {
                let _ = reactor_priority::remove_pi_waiter(owner_task, futex_pi_lock_token(key));
            }
            let waiter = state.pop_top_waiter()?;
            state.owner_tid = waiter.waiter_tid & FUTEX_TID_MASK;
            for remaining in state.waiters_by_id.values_mut() {
                remaining.blocked_owner_tid = waiter.waiter_tid & FUTEX_TID_MASK;
            }
            let more_waiters = !state.is_empty();
            if more_waiters {
                sync_pi_owner_waiter_from_state(key, waiter.waiter_tid, state);
            } else {
                table.entries.remove(&key);
            }
            Some((waiter, more_waiters))
        }) {
            Some(handoff) => Some(handoff),
            None => None,
        }
    };
    if let Some((waiter, more_waiters)) = handoff {
        let mut new = waiter.waiter_tid & FUTEX_TID_MASK;
        if more_waiters {
            new |= FUTEX_WAITERS;
        }
        match aspace.write_user(ptr, new, guard) {
            StepOutcome::Done(()) => {
                notification::notify_exact(&waiter.channel, &waiter.wait_source, FUTEX_WAKE_MASK);
                StepOutcome::Done(0)
            }
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                StepOutcome::Err(Errno::EFAULT)
            }
        }
    } else {
        match aspace.write_user(ptr, 0u32, guard) {
            StepOutcome::Done(()) => StepOutcome::Done(0),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                StepOutcome::Err(Errno::EFAULT)
            }
        }
    }
}

/// PR-3D-2: look up the `Arc<WaitSource>` for the bucket that
/// `uaddr` hashes to. Returned as a clone so callers (tests, future
/// v3 syscall arms) can hold the source across a wait window
/// independent of the bucket-table lock. `None` only if the bucket
/// table has not been initialised yet (i.e. `register_zones` has not
/// been called) — production callers will always have it
/// initialised.
///
/// `WaitSource::id()` matches the bucket's `source_id` (the same
/// `u64` published into the legacy `wait_source` resolver at
/// `register_zones` time), so the round-trip from
/// `step_futex_wait`'s `YieldShape::OnWaitSource { source, .. }`
/// resolves to the same bucket via
/// [`bucket_wait_source_for_source_id`] without going through the
/// `(uaddr -> bucket_index)` hash.
pub fn bucket_wait_source(uaddr: u64) -> Option<Arc<WaitSource>> {
    let idx = bucket_index(uaddr);
    let guard = BUCKETS.lock();
    let buckets = guard.as_ref()?;
    Some(buckets[idx].wait_source.clone())
}

/// PR-3D-2: look up the bucket's `Arc<WaitSource>` by the
/// `source_id` published into the legacy `wait_source` resolver
/// (i.e. the `u64` carried inside a `WaitSourceId`). Linear scan over
/// the 256 buckets; the alternative is a parallel BTreeMap, but at
/// 256 entries the scan is well under a microsecond and avoids
/// duplicating the bucket-id namespace.
///
/// Returns `None` if `source_id` does not match any current bucket
/// (or buckets are uninitialised).
pub fn bucket_wait_source_for_source_id(source_id: u64) -> Option<Arc<WaitSource>> {
    let guard = BUCKETS.lock();
    let buckets = guard.as_ref()?;
    buckets
        .iter()
        .find(|b| b.source_id == source_id)
        .map(|b| b.wait_source.clone())
}

// -- PR-2 StepOp wraps -------------------------------------------------
//
// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1, PR-2 wraps each free
// `step_*` fn in an `impl StepOp for FooOp` shell. The wrap stores
// args by value (`Copy` scalars) and the guard by reference so the
// wrap's lifetime captures the guard's. The `step()` body delegates
// to the free fn unchanged; the free fn ignores the guard but the
// field is held to pin the lifetime.

/// StepOp wrap for [`step_futex_wait`]. PR-2 pilot.
///
/// Carries no `&Guard` field: each `step()` call acquires its own
/// epoch guard per STEP_MODEL_v2 §1, so the op stays `Send` and the
/// driving future satisfies the reactor's `Send + 'static` contract
/// (REACTOR_v0 §Submission, INVARIANTS_v5 EBR-7).
pub struct FutexWaitOp<'a> {
    pub uaddr: u64,
    pub val: u32,
    pub aspace: &'a AddressSpace,
    pub interest_mask: u64,
    pub tid: Option<u32>,
    pub woken: bool,
    pub waiting: bool,
    pub waiting_source_id: Option<u64>,
}

impl Drop for FutexWaitOp<'_> {
    fn drop(&mut self) {
        if self.woken || !self.waiting {
            return;
        }
        if let Some(source_id) = self.waiting_source_id.take() {
            let _ = step_futex_cancel_wait_source(source_id);
        } else {
            let guard = adapter::step_engine::guard();
            let _ = step_futex_cancel_wait_in(self.aspace, self.uaddr, self.interest_mask, &guard);
        }
        self.waiting = false;
    }
}

impl<I: SubjectIdentity> StepOp<I> for FutexWaitOp<'_> {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        if self.woken {
            return StepOutcome::Done(());
        }
        let guard = adapter::step_engine::guard();
        let outcome = step_futex_wait_masked(
            self.aspace,
            self.uaddr,
            self.val,
            self.interest_mask,
            &guard,
        );
        if let StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, .. },
            ..
        } = &outcome
        {
            self.waiting = true;
            self.waiting_source_id = Some(source.raw());
        }
        outcome
    }

    fn apply_resume(&mut self, resume: adapter::step_engine::ResumeOutcome) -> Result<(), Errno> {
        match resume {
            adapter::step_engine::ResumeOutcome::Retry => {
                if let Some(source_id) = self.waiting_source_id {
                    if exact_waiter_source_exists(source_id) {
                        let _ = step_futex_cancel_wait_source(source_id);
                        self.waiting = false;
                        self.waiting_source_id = None;
                        return Ok(());
                    }
                }
                self.waiting = false;
                self.waiting_source_id = None;
                self.woken = true;
                Ok(())
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::TimedOut,
            ) => {
                if self.waiting {
                    if let Some(source_id) = self.waiting_source_id.take() {
                        let _ = step_futex_cancel_wait_source(source_id);
                    } else {
                        let guard = adapter::step_engine::guard();
                        let _ = step_futex_cancel_wait_in(
                            self.aspace,
                            self.uaddr,
                            self.interest_mask,
                            &guard,
                        );
                    }
                    self.waiting = false;
                }
                Err(Errno::ETIMEDOUT)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

pub struct FutexWaitvOp<'a> {
    pub waits: &'a [FutexWaitvEntry],
    pub aspace: &'a AddressSpace,
    pub woken_index: Option<u32>,
    waiting_source_id: Option<u64>,
    sequences: Vec<u64>,
}

impl<'a> FutexWaitvOp<'a> {
    pub fn new(aspace: &'a AddressSpace, waits: &'a [FutexWaitvEntry]) -> Self {
        Self {
            waits,
            aspace,
            woken_index: None,
            waiting_source_id: None,
            sequences: Vec::new(),
        }
    }

    fn publish_waits(&mut self, guard: &Guard<'_>) -> StepOutcome<u32, NoProgress> {
        if self.waits.is_empty() {
            return StepOutcome::Err(Errno::EINVAL);
        }
        for wait in self.waits {
            if wait.uaddr == 0 || (wait.uaddr & 0x3) != 0 || wait.interest_mask == 0 {
                return StepOutcome::Err(Errno::EINVAL);
            }
            match self
                .aspace
                .read_user(UserPtr::<u32>::new(wait.uaddr as usize), guard)
            {
                StepOutcome::Done(observed) if observed == wait.val => {}
                StepOutcome::Done(_) => return StepOutcome::Err(Errno::EAGAIN),
                StepOutcome::Err(e) => return StepOutcome::Err(e),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    return StepOutcome::Err(Errno::EFAULT);
                }
            }
        }

        let wait_point = notification::new_wait_point();
        let source_id = wait_point.source_id;
        let mut sequences = Vec::new();
        {
            let mut table_guard = EXACT_WAITERS.lock();
            let table = table_guard.get_or_insert_with(FutexTable::new);
            for wait in self.waits {
                match self
                    .aspace
                    .read_user(UserPtr::<u32>::new(wait.uaddr as usize), guard)
                {
                    StepOutcome::Done(observed) if observed == wait.val => {}
                    StepOutcome::Done(_) => return StepOutcome::Err(Errno::EAGAIN),
                    StepOutcome::Err(e) => return StepOutcome::Err(e),
                    StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                        return StepOutcome::Err(Errno::EFAULT);
                    }
                }
            }
            for wait in self.waits {
                let sequence = table.alloc_sequence();
                let waiter = new_waiter_from_wait_point(sequence, wait.interest_mask, &wait_point);
                table
                    .entries
                    .entry(key_for(self.aspace, wait.uaddr))
                    .or_default()
                    .push(waiter);
                sequences.push(sequence);
            }
        }
        self.waiting_source_id = Some(source_id);
        self.sequences = sequences;
        StepOutcome::yield_on_wait_source(NoProgress, source_id, FUTEX_WAKE_MASK)
    }
}

impl<I: SubjectIdentity> StepOp<I> for FutexWaitvOp<'_> {
    type Output = u32;
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        if let Some(index) = self.woken_index {
            return StepOutcome::Done(index);
        }
        let guard = adapter::step_engine::guard();
        self.publish_waits(&guard)
    }

    fn apply_resume(&mut self, resume: adapter::step_engine::ResumeOutcome) -> Result<(), Errno> {
        match resume {
            adapter::step_engine::ResumeOutcome::Retry => {
                let Some(source_id) = self.waiting_source_id else {
                    return Err(Errno::EIO);
                };
                let Some(index) =
                    waitv_published_index(self.aspace, self.waits, &self.sequences, source_id)
                else {
                    return Err(Errno::EIO);
                };
                let _ = step_futex_cancel_wait_source(source_id);
                self.woken_index = Some(index);
                Ok(())
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::TimedOut,
            ) => {
                if let Some(source_id) = self.waiting_source_id.take() {
                    let _ = step_futex_cancel_wait_source(source_id);
                }
                Err(Errno::ETIMEDOUT)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

pub struct FutexPiLockOp<'a> {
    pub uaddr: u64,
    pub aspace: &'a AddressSpace,
    pub owner_tid: u32,
    pub acquired: bool,
    pub waiting: bool,
    pub waiting_source_id: Option<u64>,
}

impl<I: SubjectIdentity> StepOp<I> for FutexPiLockOp<'_> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        if self.acquired {
            return StepOutcome::Done(());
        }
        let guard = adapter::step_engine::guard();
        let outcome = step_futex_lock_pi_in(self.aspace, self.uaddr, self.owner_tid, false, &guard);
        if let StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, .. },
            ..
        } = &outcome
        {
            self.waiting = true;
            self.waiting_source_id = Some(source.raw());
        }
        outcome
    }

    fn apply_resume(&mut self, resume: adapter::step_engine::ResumeOutcome) -> Result<(), Errno> {
        match resume {
            adapter::step_engine::ResumeOutcome::Retry => {
                let guard = adapter::step_engine::guard();
                let observed = match self
                    .aspace
                    .read_user(UserPtr::<u32>::new(self.uaddr as usize), &guard)
                {
                    StepOutcome::Done(v) => v,
                    StepOutcome::Err(e) => return Err(e),
                    StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                        return Err(Errno::EFAULT);
                    }
                };
                if (observed & FUTEX_TID_MASK) == (self.owner_tid & FUTEX_TID_MASK) {
                    self.acquired = true;
                    self.waiting = false;
                    self.waiting_source_id = None;
                    Ok(())
                } else {
                    Err(Errno::EAGAIN)
                }
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::TimedOut,
            ) => {
                if let Some(source_id) = self.waiting_source_id.take() {
                    let _ = step_futex_pi_cancel_wait_source(source_id);
                }
                self.waiting = false;
                Err(Errno::ETIMEDOUT)
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::Canceled,
            ) => {
                if let Some(source_id) = self.waiting_source_id.take() {
                    let _ = step_futex_pi_cancel_wait_source(source_id);
                }
                self.waiting = false;
                Err(Errno::EINTR)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

pub struct FutexWaitRequeuePiOp<'a> {
    pub uaddr: u64,
    pub uaddr2: u64,
    pub val: u32,
    pub aspace: &'a AddressSpace,
    pub waiter_tid: u32,
    pub acquired: bool,
    pub source_woke: bool,
    pub waiting: bool,
    pub waiting_source_id: Option<u64>,
}

impl<I: SubjectIdentity> StepOp<I> for FutexWaitRequeuePiOp<'_> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        if self.acquired {
            return StepOutcome::Done(());
        }
        if self.source_woke {
            return StepOutcome::Err(Errno::EAGAIN);
        }
        let guard = adapter::step_engine::guard();
        let outcome = step_futex_wait_requeue_pi_in(
            self.aspace,
            self.uaddr,
            self.val,
            self.uaddr2,
            self.waiter_tid,
            &guard,
        );
        if let StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, .. },
            ..
        } = &outcome
        {
            self.waiting = true;
            self.waiting_source_id = Some(source.raw());
        }
        outcome
    }

    fn apply_resume(&mut self, resume: adapter::step_engine::ResumeOutcome) -> Result<(), Errno> {
        match resume {
            adapter::step_engine::ResumeOutcome::Retry => {
                let Some(source_id) = self.waiting_source_id.take() else {
                    return Err(Errno::EIO);
                };
                match take_requeue_pi_resume(source_id) {
                    Some(FutexRequeuePiResume::SourceWake) => {
                        self.waiting = false;
                        self.source_woke = true;
                        return Ok(());
                    }
                    Some(FutexRequeuePiResume::AcquiredPi) | None => {}
                }
                let guard = adapter::step_engine::guard();
                let observed = match self
                    .aspace
                    .read_user(UserPtr::<u32>::new(self.uaddr2 as usize), &guard)
                {
                    StepOutcome::Done(v) => v,
                    StepOutcome::Err(e) => return Err(e),
                    StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                        return Err(Errno::EFAULT);
                    }
                };
                if (observed & FUTEX_TID_MASK) == (self.waiter_tid & FUTEX_TID_MASK) {
                    self.acquired = true;
                    self.waiting = false;
                    Ok(())
                } else {
                    Err(Errno::EAGAIN)
                }
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::TimedOut,
            ) => {
                if let Some(source_id) = self.waiting_source_id.take() {
                    let _ = step_futex_cancel_wait_source(source_id);
                    let _ = step_futex_pi_cancel_wait_source(source_id);
                }
                self.waiting = false;
                Err(Errno::ETIMEDOUT)
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::Canceled,
            ) => {
                if let Some(source_id) = self.waiting_source_id.take() {
                    let _ = step_futex_cancel_wait_source(source_id);
                    let _ = step_futex_pi_cancel_wait_source(source_id);
                }
                self.waiting = false;
                Err(Errno::EINTR)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

/// StepOp wrap for [`step_futex_wake`]. PR-2 pilot. Note `Output = u32`,
/// not `()` — wake returns the requested wake count.
pub struct FutexWakeOp<'a> {
    pub uaddr: u64,
    pub n: u32,
    pub aspace: &'a AddressSpace,
}

impl<I: SubjectIdentity> StepOp<I> for FutexWakeOp<'_> {
    type Output = u32;
    type Progress = NoProgress;
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        let guard = adapter::step_engine::guard();
        step_futex_wake_in(self.aspace, self.uaddr, self.n, &guard)
    }
}

impl OneShotStepOp for FutexWakeOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for FutexWakeOp<'_> {}

#[cfg(test)]
mod tests {
    use super::adapter::step_engine::{
        guard, AbortReason, Errno as V3Errno, InterestMask, ProcessIdentity, ResumeOutcome,
        ScriptCtx, StepOp, StepOutcome, StepProgress, YieldShape,
    };
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;
    use alloc::sync::Arc;
    use tx_substrate::wake::{TaskMailbox, WaitGeneration};

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        reset_exact_waiters_for_tests();
        guard
    }

    /// Create an `AddressSpace` with a single private-anon page at
    /// `user_va` seeded with `word` at offset 0 of the page.
    /// Returns the `AddressSpace` (which must live at least as long
    /// as any guard used to access it) and the user VA of the word.
    fn setup_aspace_with_word(user_va: usize, word: u32) -> (AddressSpace, u64) {
        use crate::vm::{
            MapPlacement, MapReserveResult, Prot, UserRange, UserVirtAddr, VmBacking, VmEntry,
            VmEntryFlags, USER_PAGE_SIZE,
        };
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            UserRange::new_aligned(UserVirtAddr(user_va), USER_PAGE_SIZE).unwrap(),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        match aspace.reserve_map(entry, MapPlacement::RequireFree) {
            MapReserveResult::Reserved(reservation) => {
                reservation.commit().expect("commit reservation");
            }
            other => panic!("reserve_map failed: {other:?}"),
        }
        // Materialise the page and write the word via write_user,
        // which publishes the frame to the pmap so read_user hits.
        let guard = guard();
        let dst = tx_hal::UserPtr::<u32>::new(user_va);
        match aspace.write_user(dst, word, &guard) {
            StepOutcome::Done(()) => {}
            other => panic!("write_user failed: {other:?}"),
        }
        drop(guard);
        (aspace, user_va as u64)
    }

    fn map_word(aspace: &AddressSpace, user_va: usize, word: u32) -> u64 {
        use crate::vm::{
            MapPlacement, MapReserveResult, Prot, UserRange, UserVirtAddr, VmBacking, VmEntry,
            VmEntryFlags, USER_PAGE_SIZE,
        };
        let entry = VmEntry::new(
            UserRange::new_aligned(UserVirtAddr(user_va), USER_PAGE_SIZE).unwrap(),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        match aspace.reserve_map(entry, MapPlacement::RequireFree) {
            MapReserveResult::Reserved(reservation) => {
                reservation.commit().expect("commit reservation");
            }
            other => panic!("reserve_map failed: {other:?}"),
        }
        let guard = guard();
        match aspace.write_user(tx_hal::UserPtr::<u32>::new(user_va), word, &guard) {
            StepOutcome::Done(()) => {}
            other => panic!("write_user failed: {other:?}"),
        }
        drop(guard);
        user_va as u64
    }

    fn map_shared_word(
        aspace: &AddressSpace,
        user_va: usize,
        pc: &crate::process::adapter::step_engine::Cap<crate::page_backed::PageContainer>,
        backing_offset: u64,
    ) -> u64 {
        use crate::vm::{
            MapPlacement, MapReserveResult, Prot, UserRange, UserVirtAddr, VmBacking, VmEntry,
            VmEntryFlags, USER_PAGE_SIZE,
        };
        let entry = VmEntry::new(
            UserRange::new_aligned(UserVirtAddr(user_va), USER_PAGE_SIZE).unwrap(),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBacking::Page {
                pc: pc.clone(),
                offset: backing_offset,
            },
        );
        match aspace.reserve_map(entry, MapPlacement::RequireFree) {
            MapReserveResult::Reserved(reservation) => {
                reservation.commit().expect("commit reservation");
            }
            other => panic!("reserve_map failed: {other:?}"),
        }
        user_va as u64
    }

    fn read_word(aspace: &AddressSpace, uaddr: u64) -> u32 {
        let guard = guard();
        let value = match aspace.read_user(tx_hal::UserPtr::<u32>::new(uaddr as usize), &guard) {
            StepOutcome::Done(v) => v,
            other => panic!("read_user expected Done, got {other:?}"),
        };
        drop(guard);
        value
    }

    #[test]
    fn futex_step_wake_returns_n_for_valid_uaddr() {
        let _setup = setup();
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = guard();
        let outcome = step_futex_wake(uaddr, 7, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Done(0));
    }

    #[test]
    fn futex_step_wake_zero_uaddr_returns_einval() {
        let _setup = setup();
        let guard = guard();
        let outcome = step_futex_wake(0, 1, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_step_wake_unaligned_uaddr_returns_einval() {
        let _setup = setup();
        let guard = guard();
        // 0x1 — non-zero, non-zero-mod-4.
        let outcome = step_futex_wake(0x1, 1, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_step_wait_observes_mismatch_returns_eagain() {
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x5000_0000, 0xdead_beef);
        let guard = guard();
        let outcome = step_futex_wait(&aspace, uaddr, 0, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EAGAIN));
    }

    #[test]
    fn futex_step_wait_observes_match_returns_blocked_with_source_id() {
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x6000_0000, 0xdead_beef);
        let guard = guard();
        let outcome = step_futex_wait(&aspace, uaddr, 0xdead_beef, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => {
                assert_eq!(interests.raw(), FUTEX_WAKE_MASK);
                assert!(
                    exact_wait_source_for_source_id(carrier.raw()).is_some(),
                    "futex exact carrier must be registered with the exact waiter table",
                );
            }
            other => panic!("expected Yield, got {other:?}"),
        }
    }

    #[test]
    fn futex_step_wait_zero_uaddr_returns_einval() {
        let _setup = setup();
        let aspace = AddressSpace::new();
        let guard = guard();
        let outcome = step_futex_wait(&aspace, 0, 0, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_step_wait_unaligned_uaddr_returns_einval() {
        let _setup = setup();
        let aspace = AddressSpace::new();
        let guard = guard();
        let outcome = step_futex_wait(&aspace, 0x2, 0, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_bucket_index_distributes_across_buckets() {
        // Sample 1024 4-byte-aligned uaddrs spaced by a non-power-of-two
        // stride so the rotate-and-fold distributes them across
        // many buckets. Strict count would be brittle; assert at
        // least 64 distinct buckets (a quarter of 256) get hit.
        let mut hits = [false; FUTEX_BUCKET_COUNT];
        let base: u64 = 0x4000_0000;
        for i in 0..1024u64 {
            // Stride 0x14 = 20 bytes — coprime with 256 modulus
            // and 4-byte aligned.
            let u = base + i * 0x14;
            hits[bucket_index(u)] = true;
        }
        let unique: usize = hits.iter().filter(|h| **h).count();
        assert!(
            unique >= 64,
            "bucket distribution too narrow: only {unique} of 256 buckets hit",
        );
    }

    // -- step_v3 sibling-fn tests -----------------------------------------
    //
    // The tests below exercise the step_v3-shape sibling fns
    // `step_futex_wait` / `step_futex_wake`. They pin the
    // step_v3 outcome catalog without crossing the tx-shims dispatch
    // boundary.

    #[test]
    fn step_futex_wait_misaligned_uaddr_returns_einval() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let aspace = AddressSpace::new();
        let guard = guard();
        // 0x1 — non-zero, non-zero-mod-4 (misaligned for u32).
        let outcome = step_futex_wait(&aspace, 0x1, 0, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EINVAL) => {}
            StepOutcome::Continue { .. }
            | StepOutcome::Yield { .. }
            | StepOutcome::Done(())
            | StepOutcome::Err(_) => {
                panic!("expected v3 Err(EINVAL), got {outcome:?}");
            }
        }
    }

    #[test]
    fn step_futex_wait_value_mismatch_returns_eagain() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x7000_0000, 0xdead_beef);
        let guard = guard();
        let outcome = step_futex_wait(&aspace, uaddr, 0, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            StepOutcome::Continue { .. }
            | StepOutcome::Yield { .. }
            | StepOutcome::Done(())
            | StepOutcome::Err(_) => {
                panic!("expected v3 Err(EAGAIN), got {outcome:?}");
            }
        }
    }

    #[test]
    fn step_futex_wait_value_match_yields_on_wait_source() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x8000_0000, 0xdead_beef);
        let guard = guard();
        let outcome = step_futex_wait(&aspace, uaddr, 0xdead_beef, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Yield {
                progress,
                shape:
                    YieldShape::OnWaitSource {
                        source: _,
                        interests,
                    },
            } => {
                assert!(progress.is_empty(), "wait yield must carry empty progress");
                assert_eq!(
                    interests.raw(),
                    FUTEX_WAKE_MASK,
                    "interest mask must match the futex wake bit",
                );
            }
            other => panic!("expected v3 Yield::OnWaitSource, got {other:?}"),
        }
    }

    #[test]
    fn step_futex_wake_zero_n_is_a_no_op_done_zero() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        // Linux `FUTEX_WAKE` with `n=0` is a defined no-op;
        // `step_futex_wake` falls through to `Done(0)`. Tightening
        // (rejecting `n=0` as EINVAL) is a separate design decision.
        let _setup = setup();
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = guard();
        let outcome = step_futex_wake(uaddr, 0, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0) for n=0 no-op, got {other:?}"),
        }
    }

    #[test]
    fn step_futex_wake_unwaited_returns_done_zero() {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _setup = setup();
        // Wake on an idle bucket with no waiters parked on it.
        //
        // v1's `step_futex_wake` is best-effort: returns the requested
        // `n` regardless of how many waiters were actually woken. The
        // probe-target semantics ("Done(0)" — number actually woken)
        // require per-bucket waiter counts, which v1 doesn't track.
        // We pin v1 behaviour here (Done(n) of requested n); future
        // tightening per the wake-N comment in the module preamble
        // flips this to Done(0).
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = guard();
        let outcome = step_futex_wake(uaddr, 1, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(woken) => {
                assert_eq!(woken, 0, "wake reports actual posted subscribers");
            }
            other => panic!("expected v3 Done(_), got {other:?}"),
        }
    }

    #[test]
    fn step_futex_wake_unwaited_reports_zero_actual_wake_count() {
        let _setup = setup();
        let guard = guard();
        let uaddr = 0x9100_0000;
        let outcome = step_futex_wake(uaddr, 1, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Done(0));
    }

    #[test]
    fn step_futex_wake_in_reports_actual_registered_waiters_and_cleans_table() {
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x9200_0000, 0);
        let eguard = guard();
        let wait_outcome = step_futex_wait(&aspace, uaddr, 0, &eguard);
        drop(eguard);

        let source_id = match wait_outcome {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => source.raw(),
            other => panic!("expected wait-source yield, got {other:?}"),
        };
        assert_eq!(debug_exact_waiter_count(), 1);

        let mailbox = Arc::new(TaskMailbox::new());
        let source = exact_wait_source_for_source_id(source_id).expect("exact source");
        let gen = WaitGeneration::new(7);
        let _sub = source.register(
            Arc::downgrade(&mailbox),
            gen,
            InterestMask::new(FUTEX_WAKE_MASK),
        );

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace, uaddr, 1, &eguard);
        drop(eguard);
        assert_eq!(wake, StepOutcome::Done(1));
        assert_eq!(debug_exact_waiter_count(), 0);
        assert_eq!(mailbox.len(), 1);
    }

    #[test]
    fn step_futex_wake_in_wakes_only_n_exact_waiters() {
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x9210_0000, 0);
        let mut sources = [0u64; 3];
        for source_id in &mut sources {
            let eguard = guard();
            let wait_outcome = step_futex_wait(&aspace, uaddr, 0, &eguard);
            drop(eguard);
            *source_id = match wait_outcome {
                StepOutcome::Yield {
                    shape: YieldShape::OnWaitSource { source, .. },
                    ..
                } => source.raw(),
                other => panic!("expected wait-source yield, got {other:?}"),
            };
        }
        assert_eq!(debug_exact_waiter_count(), 3);

        let mailboxes: [Arc<TaskMailbox>; 3] =
            core::array::from_fn(|_| Arc::new(TaskMailbox::new()));
        let mut generations = [WaitGeneration::new(0); 3];
        for ((source_id, mailbox), generation) in sources
            .iter()
            .copied()
            .zip(mailboxes.iter())
            .zip(generations.iter_mut())
        {
            let source = exact_wait_source_for_source_id(source_id).expect("exact source");
            *generation = WaitGeneration::new(source_id);
            let _ = source.register(
                Arc::downgrade(mailbox),
                *generation,
                InterestMask::new(FUTEX_WAKE_MASK),
            );
        }

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace, uaddr, 1, &eguard);
        drop(eguard);

        assert_eq!(wake, StepOutcome::Done(1));
        assert_eq!(
            mailboxes
                .iter()
                .filter(|mailbox| mailbox.len() == 1)
                .count(),
            1,
            "FUTEX_WAKE n=1 must notify exactly one exact waiter",
        );
        assert_eq!(debug_exact_waiter_count(), 2);
    }

    #[test]
    fn step_futex_wake_in_keeps_private_same_va_address_spaces_disjoint() {
        let _setup = setup();
        let (aspace_a, uaddr_a) = setup_aspace_with_word(0x9220_0000, 0);
        let (aspace_b, uaddr_b) = setup_aspace_with_word(0x9220_0000, 0);
        assert_eq!(uaddr_a, uaddr_b);

        let eguard = guard();
        let wait_a = step_futex_wait(&aspace_a, uaddr_a, 0, &eguard);
        let wait_b = step_futex_wait(&aspace_b, uaddr_b, 0, &eguard);
        drop(eguard);

        let source_a = match wait_a {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => source.raw(),
            other => panic!("expected wait-source yield, got {other:?}"),
        };
        let source_b = match wait_b {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => source.raw(),
            other => panic!("expected wait-source yield, got {other:?}"),
        };
        let mailbox_a = Arc::new(TaskMailbox::new());
        let mailbox_b = Arc::new(TaskMailbox::new());
        let src_a = exact_wait_source_for_source_id(source_a).expect("source a");
        let src_b = exact_wait_source_for_source_id(source_b).expect("source b");
        let _sub_a = src_a.register(
            Arc::downgrade(&mailbox_a),
            WaitGeneration::new(1),
            InterestMask::new(FUTEX_WAKE_MASK),
        );
        let _sub_b = src_b.register(
            Arc::downgrade(&mailbox_b),
            WaitGeneration::new(2),
            InterestMask::new(FUTEX_WAKE_MASK),
        );

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace_a, uaddr_a, 1, &eguard);
        drop(eguard);

        assert_eq!(wake, StepOutcome::Done(1));
        assert_eq!(mailbox_a.len(), 1);
        assert!(
            mailbox_b.is_empty(),
            "same VA in a different private aspace must not wake"
        );
        assert_eq!(debug_exact_waiter_count(), 1);
    }

    #[test]
    fn step_futex_wake_in_matches_shared_page_backed_key_across_address_spaces() {
        let _setup = setup();
        let pc = crate::page_backed::PageContainer::new_cap(
            crate::page_backed::PageContainerKind::Anon {
                swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("shared anon page container");
        let aspace_waiter = AddressSpace::new();
        let aspace_waker = AddressSpace::new();
        let waiter_uaddr = map_shared_word(&aspace_waiter, 0x9230_0000, &pc, 0);
        let waker_uaddr = map_shared_word(&aspace_waker, 0x9240_0000, &pc, 0);
        assert_ne!(
            waiter_uaddr, waker_uaddr,
            "test must prove backing identity beats virtual-address identity",
        );

        let eguard = guard();
        match aspace_waiter.write_user(
            tx_hal::UserPtr::<u32>::new(waiter_uaddr as usize),
            0,
            &eguard,
        ) {
            StepOutcome::Done(()) => {}
            other => panic!("write_user failed: {other:?}"),
        }
        let wait = step_futex_wait(&aspace_waiter, waiter_uaddr, 0, &eguard);
        drop(eguard);

        let source_id = match wait {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => source.raw(),
            other => panic!("expected wait-source yield, got {other:?}"),
        };
        let mailbox = Arc::new(TaskMailbox::new());
        let source = exact_wait_source_for_source_id(source_id).expect("shared exact source");
        let _sub = source.register(
            Arc::downgrade(&mailbox),
            WaitGeneration::new(1),
            InterestMask::new(FUTEX_WAKE_MASK),
        );

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace_waker, waker_uaddr, 1, &eguard);
        drop(eguard);

        assert_eq!(wake, StepOutcome::Done(1));
        assert_eq!(mailbox.len(), 1);
        assert_eq!(debug_exact_waiter_count(), 0);
    }

    #[test]
    fn step_futex_wake_in_keeps_shared_page_backed_offsets_disjoint() {
        let _setup = setup();
        let pc = crate::page_backed::PageContainer::new_cap(
            crate::page_backed::PageContainerKind::Anon {
                swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("shared anon page container");
        let aspace = AddressSpace::new();
        let base = map_shared_word(&aspace, 0x9250_0000, &pc, 0);
        let waiter_uaddr = base;
        let other_uaddr = base + 4;

        let eguard = guard();
        match aspace.write_user(
            tx_hal::UserPtr::<u32>::new(waiter_uaddr as usize),
            0,
            &eguard,
        ) {
            StepOutcome::Done(()) => {}
            other => panic!("write_user failed: {other:?}"),
        }
        let wait = step_futex_wait(&aspace, waiter_uaddr, 0, &eguard);
        drop(eguard);

        let source_id = match wait {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => source.raw(),
            other => panic!("expected wait-source yield, got {other:?}"),
        };
        let mailbox = Arc::new(TaskMailbox::new());
        let source = exact_wait_source_for_source_id(source_id).expect("shared exact source");
        let _sub = source.register(
            Arc::downgrade(&mailbox),
            WaitGeneration::new(1),
            InterestMask::new(FUTEX_WAKE_MASK),
        );

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace, other_uaddr, 1, &eguard);
        drop(eguard);

        assert_eq!(wake, StepOutcome::Done(0));
        assert!(mailbox.is_empty());
        assert_eq!(debug_exact_waiter_count(), 1);
    }

    #[test]
    fn step_futex_requeue_moves_waiters_to_target_source() {
        let _setup = setup();
        let (aspace, source_uaddr) = setup_aspace_with_word(0x9300_0000, 0);
        let target_uaddr = map_word(&aspace, 0x9300_1000, 0);
        let eguard = guard();
        let wait_outcome = step_futex_wait(&aspace, source_uaddr, 0, &eguard);
        drop(eguard);
        let source_id = match wait_outcome {
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { source, .. },
                ..
            } => source.raw(),
            other => panic!("expected wait-source yield, got {other:?}"),
        };
        let source = exact_wait_source_for_source_id(source_id).expect("source wait entry");
        let mailbox = Arc::new(TaskMailbox::new());
        let gen = WaitGeneration::new(11);
        let _sub = source.register(
            Arc::downgrade(&mailbox),
            gen,
            InterestMask::new(FUTEX_WAKE_MASK),
        );

        let eguard = guard();
        let moved = step_futex_requeue_in(&aspace, source_uaddr, target_uaddr, 0, 1, &eguard);
        drop(eguard);
        assert_eq!(moved, StepOutcome::Done(1));
        assert!(
            mailbox.is_empty(),
            "wake=0 requeue must not wake source waiter"
        );

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace, target_uaddr, 1, &eguard);
        drop(eguard);
        assert_eq!(wake, StepOutcome::Done(1));
        assert_eq!(mailbox.len(), 1, "target wake should reach requeued waiter");
    }

    #[test]
    fn step_futex_cancel_wait_removes_timed_out_waiter_from_count() {
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x9500_0000, 0);
        let eguard = guard();
        let wait_outcome = step_futex_wait(&aspace, uaddr, 0, &eguard);
        drop(eguard);
        assert!(matches!(wait_outcome, StepOutcome::Yield { .. }));
        assert_eq!(debug_exact_waiter_count(), 1);

        let eguard = guard();
        assert_eq!(
            step_futex_cancel_wait_in(&aspace, uaddr, FUTEX_WAKE_MASK, &eguard),
            StepOutcome::Done(())
        );
        let wake = step_futex_wake_in(&aspace, uaddr, 1, &eguard);
        drop(eguard);
        assert_eq!(wake, StepOutcome::Done(0));
        assert_eq!(debug_exact_waiter_count(), 0);
    }

    #[test]
    fn step_futex_pi_lock_trylock_unlock_updates_owner_word() {
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x9400_0000, 0);
        let owner = 42;

        let eguard = guard();
        assert_eq!(
            step_futex_lock_pi_in(&aspace, uaddr, owner, false, &eguard),
            StepOutcome::Done(())
        );
        drop(eguard);
        assert_eq!(read_word(&aspace, uaddr), owner);

        let eguard = guard();
        assert_eq!(
            step_futex_trylock_pi_in(&aspace, uaddr, owner + 1, &eguard),
            StepOutcome::Err(V3Errno::ESRCH)
        );
        drop(eguard);

        let eguard = guard();
        assert_eq!(
            step_futex_unlock_pi_in(&aspace, uaddr, owner, &eguard),
            StepOutcome::Done(0)
        );
        drop(eguard);
        assert_eq!(read_word(&aspace, uaddr), 0);
    }

    #[test]
    fn step_futex_wait_requeue_pi_source_wake_cleans_source_row() {
        let _setup = setup();
        let (aspace, source) = setup_aspace_with_word(0x9410_0000, 0x1234);
        let target = map_word(&aspace, 0x9410_1000, 0);

        let eguard = guard();
        let wait = step_futex_wait_requeue_pi_in(&aspace, source, 0x1234, target, 43, &eguard);
        drop(eguard);
        assert!(matches!(wait, StepOutcome::Yield { .. }));
        assert_eq!(debug_exact_waiter_count(), 1);

        let eguard = guard();
        let wake = step_futex_wake_in(&aspace, source, 1, &eguard);
        drop(eguard);
        assert_eq!(wake, StepOutcome::Done(1));
        assert_eq!(debug_exact_waiter_count(), 0);
        assert_eq!(debug_pi_waiter_count(), 0);
    }

    #[test]
    fn step_futex_cmp_requeue_pi_uncontended_target_acquires_for_waiter() {
        let _setup = setup();
        let (aspace, source) = setup_aspace_with_word(0x9420_0000, 0x1234);
        let target = map_word(&aspace, 0x9420_1000, 0);
        let waiter = 44;

        let eguard = guard();
        assert!(matches!(
            step_futex_wait_requeue_pi_in(&aspace, source, 0x1234, target, waiter, &eguard),
            StepOutcome::Yield { .. }
        ));
        let requeue = step_futex_cmp_requeue_pi_in(&aspace, source, target, 1, 0, 0x1234, &eguard);
        drop(eguard);

        assert_eq!(requeue, StepOutcome::Done(1));
        assert_eq!(read_word(&aspace, target) & FUTEX_TID_MASK, waiter);
        assert_eq!(debug_exact_waiter_count(), 0);
        assert_eq!(debug_pi_waiter_count(), 0);
    }

    #[test]
    fn step_futex_cmp_requeue_pi_contended_target_moves_to_pi_waiters() {
        let _setup = setup();
        let (aspace, source) = setup_aspace_with_word(0x9430_0000, 0x1234);
        let target = map_word(&aspace, 0x9430_1000, 0);
        let proc_cap = crate::process::bootstrap_init_process(
            AddressSpace::new_cap().expect("process aspace cap"),
        )
        .expect("bootstrap init");
        let owner = proc_cap.nth_thread(0).expect("leader thread").tid.0;
        let waiter = 46;

        let eguard = guard();
        assert_eq!(
            step_futex_lock_pi_in(&aspace, target, owner, false, &eguard),
            StepOutcome::Done(())
        );
        assert!(matches!(
            step_futex_wait_requeue_pi_in(&aspace, source, 0x1234, target, waiter, &eguard),
            StepOutcome::Yield { .. }
        ));
        let requeue = step_futex_cmp_requeue_pi_in(&aspace, source, target, 1, 0, 0x1234, &eguard);
        drop(eguard);

        assert_eq!(requeue, StepOutcome::Done(1));
        assert_eq!(read_word(&aspace, target), FUTEX_WAITERS | owner);
        assert_eq!(debug_exact_waiter_count(), 0);
        assert_eq!(debug_pi_waiter_count(), 1);

        let eguard = guard();
        assert_eq!(
            step_futex_unlock_pi_in(&aspace, target, owner, &eguard),
            StepOutcome::Done(0)
        );
        drop(eguard);
        assert_eq!(read_word(&aspace, target) & FUTEX_TID_MASK, waiter);
        assert_eq!(debug_pi_waiter_count(), 0);
    }

    #[test]
    fn futex_wait_requeue_pi_op_timeout_removes_waiter_rows() {
        let _setup = setup();
        let (aspace, source) = setup_aspace_with_word(0x9440_0000, 0x1234);
        let target = map_word(&aspace, 0x9440_1000, 0);
        let mut op = FutexWaitRequeuePiOp {
            uaddr: source,
            uaddr2: target,
            val: 0x1234,
            aspace: &aspace,
            waiter_tid: 47,
            acquired: false,
            source_woke: false,
            waiting: false,
            waiting_source_id: None,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();

        assert!(matches!(op.step(&mut ctx), StepOutcome::Yield { .. }));
        assert_eq!(debug_exact_waiter_count(), 1);
        assert_eq!(
            <FutexWaitRequeuePiOp<'_> as StepOp<ProcessIdentity>>::apply_resume(
                &mut op,
                ResumeOutcome::Aborted(AbortReason::TimedOut),
            ),
            Err(V3Errno::ETIMEDOUT)
        );
        assert_eq!(debug_exact_waiter_count(), 0);
        assert_eq!(debug_pi_waiter_count(), 0);
    }

    #[test]
    fn futex_wait_requeue_pi_op_cancel_removes_waiter_rows() {
        let _setup = setup();
        let (aspace, source) = setup_aspace_with_word(0x9450_0000, 0x1234);
        let target = map_word(&aspace, 0x9450_1000, 0);
        let mut op = FutexWaitRequeuePiOp {
            uaddr: source,
            uaddr2: target,
            val: 0x1234,
            aspace: &aspace,
            waiter_tid: 48,
            acquired: false,
            source_woke: false,
            waiting: false,
            waiting_source_id: None,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();

        assert!(matches!(op.step(&mut ctx), StepOutcome::Yield { .. }));
        assert_eq!(debug_exact_waiter_count(), 1);
        assert_eq!(
            <FutexWaitRequeuePiOp<'_> as StepOp<ProcessIdentity>>::apply_resume(
                &mut op,
                ResumeOutcome::Aborted(AbortReason::Canceled),
            ),
            Err(V3Errno::EINTR)
        );
        assert_eq!(debug_exact_waiter_count(), 0);
        assert_eq!(debug_pi_waiter_count(), 0);
    }

    #[test]
    fn futex_step_wake_fires_channel_observed_by_waiter() {
        // Cross-iteration check: register a wait, fire wake on the
        // same uaddr, observe the channel state through
        // wait_source::lookup. We don't actually drive a future to
        // completion (that requires async coordination — Slice 11's
        // QEMU shell smoke covers it). Instead we confirm the
        // bucket's carrier id matches across calls — the same
        // channel is fired on wake and parked on by wait.
        let _setup = setup();
        let (aspace, uaddr) = setup_aspace_with_word(0x9000_0000, 42);
        let guard = guard();
        let wait_outcome = step_futex_wait(&aspace, uaddr, 42, &guard);
        let waiter_carrier = match wait_outcome {
            StepOutcome::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier, ..
                    },
                ..
            } => carrier.raw(),
            other => panic!("expected Yield, got {other:?}"),
        };
        // wake on the same uaddr; record success.
        let wake_outcome = step_futex_wake_in(&aspace, uaddr, 1, &guard);
        drop(guard);
        assert_eq!(wake_outcome, StepOutcome::Done(1));
        // Both ops index to the same bucket → same carrier id.
        // The channel underlying that carrier id is the one fire()
        // was just called on.
        assert!(
            exact_wait_source_for_source_id(waiter_carrier).is_none(),
            "woken exact waiter should be removed from the exact waiter table",
        );
    }

    // -- PR-2 StepOp wrap tests -------------------------------------------
    //
    // Minimal construction + one `.step()` call per wrap. Pins that the
    // wrap delegates to the free fn unchanged. The compile-check is the
    // primary value — these assert the variant matches what the free
    // fn would have returned.
    mod step_op_wraps {
        use super::super::adapter::step_engine::{
            guard, Errno as V3Errno, NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
            YieldShape,
        };
        use super::super::{step_futex_wake, FutexWaitOp, FutexWakeOp, FUTEX_WAKE_MASK};
        use super::{setup, setup_aspace_with_word};

        #[test]
        fn futex_wait_op_step_delegates_to_free_fn() {
            let _setup = setup();
            // Word holds 0xdead_beef; matching val parks → Yield::OnWaitSource.
            let (aspace, uaddr) = setup_aspace_with_word(0xa000_0000, 0xdead_beef);
            let mut op = FutexWaitOp {
                uaddr,
                aspace: &aspace,
                val: 0xdead_beef,
                interest_mask: FUTEX_WAKE_MASK,
                tid: None,
                woken: false,
                waiting: false,
                waiting_source_id: None,
            };
            let mut ctx = ScriptCtx::<ProcessIdentity>::new();
            let outcome = op.step(&mut ctx);
            match outcome {
                StepOutcome::Yield {
                    progress: NoProgress,
                    shape: YieldShape::OnWaitSource { interests, .. },
                } => {
                    assert_eq!(interests.raw(), FUTEX_WAKE_MASK);
                }
                other => panic!("expected Yield::OnWaitSource, got {other:?}"),
            }
        }

        #[test]
        fn futex_wake_op_step_delegates_to_free_fn() {
            let _setup = setup();
            // Zero uaddr → EINVAL. Output type is u32, not ().
            // FutexWakeOp acquires its own guard inside step() per
            // STEP_MODEL_v2 §1; outer guard would nest (EBR-7).
            let (aspace, _) = setup_aspace_with_word(0xb000_0000, 0);
            let mut op = FutexWakeOp {
                uaddr: 0,
                n: 1,
                aspace: &aspace,
            };
            let mut ctx = ScriptCtx::<ProcessIdentity>::new();
            let outcome: StepOutcome<u32, NoProgress> = op.step(&mut ctx);
            assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
            // Sanity: parallel free-fn call matches.
            let guard = guard();
            let free = step_futex_wake(0, 1, &guard);
            drop(guard);
            assert_eq!(outcome, free);
        }
    }
}
