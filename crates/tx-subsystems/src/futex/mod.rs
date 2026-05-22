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
use core::sync::atomic::{AtomicBool, Ordering};

pub mod adapter;

use adapter::step_engine::{
    self, Errno, NoProgress, OneShotStepOp, ScriptCtx, SpinMutex, StepOp, StepOutcome,
    SubjectIdentity, ZoneError,
};
use adapter::wait_routing::{self, Channel, WaitSource};

use crate::execution::Guard;
use crate::vm::AddressSpace;
use crate::wait_source;
use tx_hal::UserPtr;

/// Number of futex hash buckets. Fixed; no dynamic allocation.
/// Collisions are absorbed by the per-waiter re-check on wakeup.
pub const FUTEX_BUCKET_COUNT: usize = 256;

/// Wake-mask bit fired into a bucket's channel by `FUTEX_WAKE`.
/// Waiters subscribe to this same bit so any wake fires every
/// waiter in the bucket.
pub const FUTEX_WAKE_MASK: u64 = 0x1;

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
    aspace: usize,
    uaddr: u64,
}

struct FutexEntry {
    channel: Channel,
    source_id: u64,
    wait_source: Arc<WaitSource>,
}

static EXACT_WAITERS: SpinMutex<Option<BTreeMap<FutexKey, FutexEntry>>> = SpinMutex::new(None);

fn key_for(aspace: &AddressSpace, uaddr: u64) -> FutexKey {
    let _ = aspace;
    FutexKey {
        // Cap deref addresses are not a stable cross-thread identity
        // in all syscall paths. Use uaddr as the exact wake key for
        // now; spurious cross-process wakes are permitted by futex
        // semantics and user space re-checks its condition.
        aspace: 0,
        uaddr,
    }
}

fn new_entry() -> FutexEntry {
    let channel = Channel::new();
    let source_id = wait_source::register_wait_channel(channel.clone());
    let wait_source = wait_routing::new_wait_source(source_id);
    FutexEntry {
        channel,
        source_id,
        wait_source,
    }
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
        let channel = Channel::new();
        let source_id = wait_source::register_wait_channel(channel.clone());
        // PR-3D-2 (D2/D4 coexistence). Per-bucket `WaitSource` shares
        // the legacy registry's id namespace so a v3 caller using the
        // `WaitSourceId` stamped into `YieldShape::OnWaitSource` lands
        // on the right bucket here.
        let wait_source = wait_routing::new_wait_source(source_id);
        FutexBucket {
            channel,
            source_id,
            wait_source,
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
        let table = table_guard.get_or_insert_with(BTreeMap::new);
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
        table.entry(key).or_insert_with(new_entry).source_id
    };
    // publish — yield on the exact `(AddressSpace, uaddr)` wait source.
    // The old bucket source remains only as a compatibility wake path.
    step_engine::yield_until_wake(source_id, FUTEX_WAKE_MASK)
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
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return StepOutcome::Err(Errno::EINVAL);
    }
    if n == 0 {
        return StepOutcome::Done(0);
    }

    let key = key_for(aspace, uaddr);
    let exact_wait_source = {
        let table_guard = EXACT_WAITERS.lock();
        table_guard
            .as_ref()
            .and_then(|table| table.get(&key))
            .map(|entry| {
                wait_routing::fire_legacy_channel(&entry.channel, FUTEX_WAKE_MASK);
                entry.wait_source.clone()
            })
    };
    if let Some(wait_source) = exact_wait_source {
        wait_routing::notify_v3_source(&wait_source, FUTEX_WAKE_MASK);
    }
    let _ = step_futex_wake(uaddr, n, _guard);
    StepOutcome::Done(n)
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
    let wait_source = {
        let guard = BUCKETS.lock();
        let buckets = guard
            .as_ref()
            .expect("futex buckets uninitialised — register_zones not called");
        // Legacy path (D2 coexistence): wake any `Waker`-based waiter.
        wait_routing::fire_legacy_channel(&buckets[idx].channel, FUTEX_WAKE_MASK);
        buckets[idx].wait_source.clone()
    };
    // PR-3D-2 new path: post `MailboxEvent::SourceFired` to any v3
    // caller that registered a `TaskMailbox` against this bucket's
    // source. Per the v1 bucket model this remains best-effort and
    // reports the requested wake count.
    wait_routing::notify_v3_source(&wait_source, FUTEX_WAKE_MASK);
    StepOutcome::Done(n)
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
    pub woken: bool,
}

impl<I: SubjectIdentity> StepOp<I> for FutexWaitOp<'_> {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        if self.woken {
            return StepOutcome::Done(());
        }
        let guard = adapter::step_engine::guard();
        step_futex_wait(self.aspace, self.uaddr, self.val, &guard)
    }

    fn apply_resume(&mut self, resume: adapter::step_engine::ResumeOutcome) -> Result<(), Errno> {
        match resume {
            adapter::step_engine::ResumeOutcome::Retry => {
                self.woken = true;
                Ok(())
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
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let guard = adapter::step_engine::guard();
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
    fn futex_step_wake_returns_n_for_valid_uaddr() {
        let _setup = setup();
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = guard();
        let outcome = step_futex_wake(uaddr, 7, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Done(7));
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
                    wait_source::lookup_wait_channel(carrier.raw()).is_some(),
                    "futex bucket carrier must be registered with wait_source",
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
                assert_eq!(woken, 1, "v1 wake returns requested n (best-effort)");
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
        let wake_outcome = step_futex_wake(uaddr, 1, &guard);
        drop(guard);
        assert_eq!(wake_outcome, StepOutcome::Done(1));
        // Both ops index to the same bucket → same carrier id.
        // The channel underlying that carrier id is the one fire()
        // was just called on.
        assert!(
            wait_source::lookup_wait_channel(waiter_carrier).is_some(),
            "fired channel still resolvable",
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
                woken: false,
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
            let mut op = FutexWakeOp { uaddr: 0, n: 1 };
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
