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

use alloc::collections::{BTreeMap, VecDeque};
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
pub const FUTEX_WAKE_MASK: u64 = 0x1;
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

struct FutexEntry {
    waiters: VecDeque<FutexWaiter>,
}

struct FutexWaiter {
    channel: Channel,
    source_id: u64,
    wait_source: Arc<WaitSource>,
    tid: Option<u32>,
    interest_mask: u64,
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

fn new_entry() -> FutexEntry {
    FutexEntry {
        waiters: VecDeque::new(),
    }
}

fn new_waiter(tid: Option<u32>, interest_mask: u64) -> FutexWaiter {
    let channel = Channel::new();
    let source_id = wait_source::register_wait_channel(channel.clone());
    let wait_source = wait_routing::new_wait_source(source_id);
    FutexWaiter {
        channel,
        source_id,
        wait_source,
        tid,
        interest_mask,
    }
}

fn unregister_exact_waiter(aspace: &AddressSpace, uaddr: u64, source_id: u64) {
    let key = key_for(aspace, uaddr);
    let mut table_guard = EXACT_WAITERS.lock();
    let Some(table) = table_guard.as_mut() else {
        return;
    };
    let Some(entry) = table.get_mut(&key) else {
        return;
    };
    entry.waiters.retain(|waiter| waiter.source_id != source_id);
    if entry.waiters.is_empty() {
        table.remove(&key);
    }
}

fn exact_waiter_registered(aspace: &AddressSpace, uaddr: u64, source_id: u64) -> bool {
    let key = key_for(aspace, uaddr);
    let table_guard = EXACT_WAITERS.lock();
    let Some(table) = table_guard.as_ref() else {
        return false;
    };
    table
        .get(&key)
        .map(|entry| {
            entry
                .waiters
                .iter()
                .any(|waiter| waiter.source_id == source_id)
        })
        .unwrap_or(false)
}

pub(crate) fn register_waiting_tid(tid: Option<u32>) {
    let Some(tid) = tid else {
        return;
    };
    PiLockToken::new(namespace, key.offset)
}

pub(crate) fn unregister_waiting_tid(tid: Option<u32>) {
    let Some(tid) = tid else {
        return;
    };
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
    if WAITING_TIDS.lock().get(&tid).copied().unwrap_or(0) > 0 {
        return true;
    }
    let table_guard = EXACT_WAITERS.lock();
    let Some(table) = table_guard.as_ref() else {
        return false;
    };
    table
        .values()
        .any(|entry| entry.waiters.iter().any(|waiter| waiter.tid == Some(tid)))
}

pub fn debug_waiter_count(uaddr: u64) -> usize {
    let table_guard = EXACT_WAITERS.lock();
    let Some(table) = table_guard.as_ref() else {
        return 0;
    };
    table
        .iter()
        .filter(|(key, _)| key.uaddr == uaddr)
        .map(|(_, entry)| entry.waiters.len())
        .sum()
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
    step_futex_wait_masked_with_tid(aspace, uaddr, val, FUTEX_WAKE_MASK, guard, None)
}

pub fn step_futex_wait_masked(
    aspace: &AddressSpace,
    uaddr: u64,
    val: u32,
    interest_mask: u64,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    step_futex_wait_masked_with_tid(aspace, uaddr, val, interest_mask, guard, None)
}

fn step_futex_wait_masked_with_tid(
    aspace: &AddressSpace,
    uaddr: u64,
    val: u32,
    interest_mask: u64,
    guard: &Guard<'_>,
    tid: Option<u32>,
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
        let entry = table.entry(key).or_insert_with(new_entry);
        let waiter = new_waiter(tid, interest_mask);
        let source_id = waiter.source_id;
        entry.waiters.push_back(waiter);
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
    step_futex_wake_masked_in(aspace, uaddr, n, FUTEX_WAKE_MASK, _guard)
}

pub fn step_futex_wake_masked_in(
    aspace: &AddressSpace,
    uaddr: u64,
    n: u32,
    wake_mask: u64,
    _guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    if n == 0 || wake_mask == 0 {
        return StepOutcome::Done(0);
    }

    let key = key_for(aspace, uaddr);
    let waiters = {
        let mut table_guard = EXACT_WAITERS.lock();
        let Some(table) = table_guard.as_mut() else {
            return StepOutcome::Done(0);
        };
        let Some(entry) = table.get_mut(&key) else {
            return StepOutcome::Done(0);
        };
        let mut waiters = alloc::vec::Vec::new();
        let mut remaining = VecDeque::new();
        while let Some(waiter) = entry.waiters.pop_front() {
            if waiters.len() < n as usize && (waiter.interest_mask & wake_mask) != 0 {
                waiters.push(waiter);
            } else {
                remaining.push_back(waiter);
            }
        }
        entry.waiters = remaining;
        if entry.waiters.is_empty() {
            table.remove(&key);
        }
        waiters
    };
    let woken = waiters.len() as u32;
    for waiter in waiters {
        let fired_mask = waiter.interest_mask & wake_mask;
        wait_routing::fire_legacy_channel(&waiter.channel, fired_mask);
        wait_routing::notify_v3_source(&waiter.wait_source, fired_mask);
    }
    StepOutcome::Done(woken)
}

pub fn step_futex_requeue_in(
    aspace: &AddressSpace,
    uaddr: u64,
    uaddr2: u64,
    wake_n: u32,
    requeue_n: u32,
    guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
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
    if requeue_n == 0 {
        return StepOutcome::Done(total);
    }

    let source_key = key_for(aspace, uaddr);
    let target_key = key_for(aspace, uaddr2);
    let moved = {
        let mut table_guard = EXACT_WAITERS.lock();
        let Some(table) = table_guard.as_mut() else {
            return StepOutcome::Done(total);
        };
        let Some(source_entry) = table.get_mut(&source_key) else {
            return StepOutcome::Done(total);
        };
        let mut moved_waiters = VecDeque::new();
        for _ in 0..requeue_n {
            let Some(waiter) = source_entry.waiters.pop_front() else {
                break;
            };
            moved_waiters.push_back(waiter);
        }
        let moved = moved_waiters.len();
        if moved == 0 {
            return StepOutcome::Done(total);
        }
        if source_entry.waiters.is_empty() {
            table.remove(&source_key);
        }
        let target_entry = table.entry(target_key).or_insert_with(new_entry);
        target_entry.waiters.append(&mut moved_waiters);
        moved as u32
    };
    StepOutcome::Done(total.saturating_add(moved))
}

pub fn step_futex_lock_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    owner_tid: u32,
    waiters: bool,
    guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
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
        return StepOutcome::Err(Errno::EAGAIN);
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
    step_futex_lock_pi_in(aspace, uaddr, owner_tid, false, guard)
}

pub fn step_futex_unlock_pi_in(
    aspace: &AddressSpace,
    uaddr: u64,
    owner_tid: u32,
    guard: &Guard<'_>,
) -> StepOutcome<u32, NoProgress> {
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
    match aspace.write_user(ptr, 0u32, guard) {
        StepOutcome::Done(()) => step_futex_wake_in(aspace, uaddr, 1, guard),
        StepOutcome::Err(e) => StepOutcome::Err(e),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => StepOutcome::Err(Errno::EFAULT),
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
    // PR-3D-2 new path: post `MailboxEvent::SourceFired` to any v3
    // caller that registered a `TaskMailbox` against this bucket's
    // source. Per the v1 bucket model this remains best-effort and
    // reports the requested wake count.
    let posted = wait_routing::notify_v3_source(&wait_source, FUTEX_WAKE_MASK) as u32;
    StepOutcome::Done(posted.min(n))
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
    pub registered_source_id: Option<u64>,
}

impl<I: SubjectIdentity> StepOp<I> for FutexWaitOp<'_> {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        if self.woken {
            return StepOutcome::Done(());
        }
        if let Some(source_id) = self.registered_source_id {
            return step_engine::yield_until_wake(source_id, self.interest_mask);
        }
        let guard =
            tx_substrate::epoch::borrow_current_guard().unwrap_or_else(adapter::step_engine::guard);
        let outcome = step_futex_wait_masked_with_tid(
            self.aspace,
            self.uaddr,
            self.val,
            self.interest_mask,
            &guard,
            self.tid,
        );
        if let StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, .. },
            ..
        } = &outcome
        {
            self.registered_source_id = Some(source.raw());
        }
        outcome
    }

    fn apply_resume(&mut self, resume: adapter::step_engine::ResumeOutcome) -> Result<(), Errno> {
        match resume {
            adapter::step_engine::ResumeOutcome::Retry => {
                if let Some(source_id) = self.registered_source_id {
                    if exact_waiter_registered(self.aspace, self.uaddr, source_id) {
                        return Ok(());
                    }
                }
                self.unregister();
                self.woken = true;
                Ok(())
            }
            adapter::step_engine::ResumeOutcome::Aborted(
                adapter::step_engine::AbortReason::TimedOut,
            ) => {
                self.unregister();
                Err(Errno::ETIMEDOUT)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

impl FutexWaitOp<'_> {
    fn unregister(&mut self) {
        if let Some(source_id) = self.registered_source_id.take() {
            unregister_exact_waiter(self.aspace, self.uaddr, source_id);
            wait_source::release_wait_channel(source_id);
        }
    }
}

impl Drop for FutexWaitOp<'_> {
    fn drop(&mut self) {
        self.unregister();
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
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let guard =
            tx_substrate::epoch::borrow_current_guard().unwrap_or_else(adapter::step_engine::guard);
        step_futex_wake_in(self.aspace, self.uaddr, self.n, &guard)
    }
}

impl OneShotStepOp for FutexWakeOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for FutexWakeOp<'_> {}

#[cfg(test)]
mod tests {
    use super::adapter::step_engine::{
        guard, Errno as V3Errno, StepOutcome, StepProgress, YieldShape,
    };
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
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

    #[test]
    fn futex_step_wake_returns_actual_bucket_notifications() {
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
        // Wake on an idle bucket with no subscribers parked on it.
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = guard();
        let outcome = step_futex_wake(uaddr, 1, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(woken) => {
                assert_eq!(woken, 0, "wake returns actual bucket notifications");
            }
            other => panic!("expected v3 Done(_), got {other:?}"),
        }
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
                registered_source_id: None,
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
            let (aspace, _) = setup_aspace_with_word(0xa100_0000, 0);
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
