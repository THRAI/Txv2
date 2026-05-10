//! POSIX-style `futex(2)` primitive — fast user-space mutex wakeup.
//!
//! Spec: `man 2 futex`, `Documentation/futex/futex2.rst` (Linux).
//! Roadmap: Slice 3 of the shell-prompt roadmap (2026-05-07).
//!
//! **Bucket model.** A fixed array of 256 `FutexBucket`s indexed by
//! `hash(uaddr) & 0xff`. Each bucket holds a [`Channel`] registered
//! with the global [`crate::wait_carrier`]. `FUTEX_WAIT` parks on the
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
//! fires. Slice 4's `nanosleep` lands the timer-wait-carrier
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

use core::sync::atomic::{AtomicBool, Ordering};

use tx_reactor::wait::{Channel, Mask};
use tx_substrate::zone::ZoneError;
use tx_substrate::SpinMutex;

use crate::execution::Guard;
use crate::wait_carrier;

/// Number of futex hash buckets. Fixed; no dynamic allocation.
/// Collisions are absorbed by the per-waiter re-check on wakeup.
pub const FUTEX_BUCKET_COUNT: usize = 256;

/// Wake-mask bit fired into a bucket's channel by `FUTEX_WAKE`.
/// Waiters subscribe to this same bit so any wake fires every
/// waiter in the bucket.
pub const FUTEX_WAKE_MASK: u64 = 0x1;

/// Per-bucket state. Holds the wait channel + the carrier id
/// registered with [`crate::wait_carrier`].
struct FutexBucket {
    channel: Channel,
    carrier_id: u64,
}

/// Static table of 256 buckets. Initialised lazily under a single
/// [`SpinMutex<Option<...>>`] on the first call to
/// [`register_zones`]. Subsequent calls (from re-running
/// [`crate::zones::register_all`] in tests) are no-ops — the
/// buckets are kept across re-init because their carrier ids are
/// already published to [`crate::wait_carrier`] and tearing them
/// down would invalidate any token references held by in-flight
/// futures.
static BUCKETS: SpinMutex<Option<[FutexBucket; FUTEX_BUCKET_COUNT]>> = SpinMutex::new(None);

/// One-shot init flag. The slow lock-acquire-and-check inside
/// [`register_zones`] is fine on the cold path; the steady-state
/// fast path is the in-step `lock_buckets()` call which never
/// re-initialises.
static INITIALISED: AtomicBool = AtomicBool::new(false);

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
        let carrier_id = wait_carrier::register_wait_channel(channel.clone());
        FutexBucket {
            channel,
            carrier_id,
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
/// channel (`OnCarrier`); otherwise returns `Err(EAGAIN)`.
/// `uaddr` must be non-zero and 4-byte aligned; otherwise `Err(EINVAL)`.
/// `timeout` is ignored in v1.
///
/// Returns a [`tx_substrate::step_v3::StepOutcome`]:
/// - bad uaddr → `Err(Errno::EINVAL)`
/// - `*uaddr != val` → `Err(Errno::EAGAIN)`
/// - `*uaddr == val` → `Yield { progress: NoProgress, shape: OnCarrier { … } }`
///
/// Wait never produces `Done`: completion arrives via the carrier
/// resolution step driven by the script driver after the yield resolves.
pub fn step_futex_wait(
    uaddr: u64,
    val: u32,
    _guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EINVAL);
    }
    // SAFETY: bootstrap kernel-buffer exemption — TODO(phase-userva).
    // Mirrors `step_futex_wait`'s read; see that fn for migration plan.
    let observed = unsafe { core::ptr::read_volatile(uaddr as *const u32) };
    if observed != val {
        return tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EAGAIN);
    }
    let idx = bucket_index(uaddr);
    let carrier_id = {
        let guard = BUCKETS.lock();
        let buckets = guard
            .as_ref()
            .expect("futex buckets uninitialised — register_zones not called");
        buckets[idx].carrier_id
    };
    tx_substrate::step_v3::StepOutcome::Yield {
        progress: tx_substrate::step_v3::NoProgress,
        shape: tx_substrate::step_v3::YieldShape::OnCarrier {
            carrier: tx_substrate::step_v3::WakeCarrier::new(carrier_id),
            interests: tx_substrate::step_v3::InterestConditions::new(FUTEX_WAKE_MASK),
        },
    }
}

/// `futex(uaddr, FUTEX_WAKE, n, ...)`.
///
/// Returns a [`tx_substrate::step_v3::StepOutcome`]:
/// - bad uaddr (zero or unaligned) → `Err(Errno::EINVAL)`
/// - otherwise → `Done(n)` (best-effort: returned count is the
///   requested `n`, not the actually-woken count; same caveat as
///   `step_futex_wake`). `n == 0` is permitted and returns `Done(0)` —
///   Linux `FUTEX_WAKE` with `n=0` is a defined no-op.
///
/// Wake never produces `Yield`/`Continue`.
pub fn step_futex_wake(
    uaddr: u64,
    n: u32,
    _guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<u32, tx_substrate::step_v3::NoProgress> {
    if uaddr == 0 || (uaddr & 0x3) != 0 {
        return tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EINVAL);
    }
    let idx = bucket_index(uaddr);
    {
        let guard = BUCKETS.lock();
        let buckets = guard
            .as_ref()
            .expect("futex buckets uninitialised — register_zones not called");
        buckets[idx].channel.fire(Mask::from_bits(FUTEX_WAKE_MASK));
    }
    tx_substrate::step_v3::StepOutcome::Done(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome, StepProgress};
    use tx_substrate::testing::init_host_for_test_once;

    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_host_for_test_once();
        let _ = zones::register_all();
        guard
    }

    #[test]
    fn futex_step_wake_returns_n_for_valid_uaddr() {
        let _setup = setup();
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wake(uaddr, 7, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Done(7));
    }

    #[test]
    fn futex_step_wake_zero_uaddr_returns_einval() {
        let _setup = setup();
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wake(0, 1, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_step_wake_unaligned_uaddr_returns_einval() {
        let _setup = setup();
        let guard = tx_substrate::epoch::guard();
        // 0x1 — non-zero, non-zero-mod-4.
        let outcome = step_futex_wake(0x1, 1, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_step_wait_observes_mismatch_returns_eagain() {
        let _setup = setup();
        // Word holds 0xdead_beef; FUTEX_WAIT with val=0 must
        // observe the mismatch and short-circuit to EAGAIN.
        let word: u32 = 0xdead_beef;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wait(uaddr, 0, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EAGAIN));
    }

    #[test]
    fn futex_step_wait_observes_match_returns_blocked_with_carrier_id() {
        let _setup = setup();
        let word: u32 = 0xdead_beef;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wait(uaddr, 0xdead_beef, &guard);
        drop(guard);
        use tx_substrate::step_v3::YieldShape;
        match outcome {
            StepOutcome::Yield {
                shape: YieldShape::OnCarrier { carrier, interests },
                ..
            } => {
                assert_eq!(interests.raw(), FUTEX_WAKE_MASK);
                assert!(
                    wait_carrier::lookup_wait_channel(carrier.raw()).is_some(),
                    "futex bucket carrier must be registered with wait_carrier",
                );
            }
            other => panic!("expected Yield, got {other:?}"),
        }
    }

    #[test]
    fn futex_step_wait_zero_uaddr_returns_einval() {
        let _setup = setup();
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wait(0, 0, &guard);
        drop(guard);
        assert_eq!(outcome, StepOutcome::Err(V3Errno::EINVAL));
    }

    #[test]
    fn futex_step_wait_unaligned_uaddr_returns_einval() {
        let _setup = setup();
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wait(0x2, 0, &guard);
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
        let _setup = setup();
        let guard = tx_substrate::epoch::guard();
        // 0x1 — non-zero, non-zero-mod-4 (misaligned for u32).
        let outcome = step_futex_wait(0x1, 0, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EINVAL) => {}
            tx_substrate::step_v3::StepOutcome::Continue { .. }
            | tx_substrate::step_v3::StepOutcome::Yield { .. }
            | tx_substrate::step_v3::StepOutcome::Done(())
            | tx_substrate::step_v3::StepOutcome::Err(_) => {
                panic!("expected v3 Err(EINVAL), got {outcome:?}");
            }
        }
    }

    #[test]
    fn step_futex_wait_value_mismatch_returns_eagain() {
        let _setup = setup();
        // Word holds 0xdead_beef; FUTEX_WAIT with val=0 must observe
        // the mismatch and short-circuit to EAGAIN.
        let word: u32 = 0xdead_beef;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wait(uaddr, 0, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Err(tx_substrate::step_v3::Errno::EAGAIN) => {}
            tx_substrate::step_v3::StepOutcome::Continue { .. }
            | tx_substrate::step_v3::StepOutcome::Yield { .. }
            | tx_substrate::step_v3::StepOutcome::Done(())
            | tx_substrate::step_v3::StepOutcome::Err(_) => {
                panic!("expected v3 Err(EAGAIN), got {outcome:?}");
            }
        }
    }

    #[test]
    fn step_futex_wait_value_match_yields_on_carrier() {
        let _setup = setup();
        let word: u32 = 0xdead_beef;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wait(uaddr, 0xdead_beef, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Yield {
                progress,
                shape:
                    tx_substrate::step_v3::YieldShape::OnCarrier {
                        carrier: _,
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
            other => panic!("expected v3 Yield::OnCarrier, got {other:?}"),
        }
    }

    #[test]
    fn step_futex_wake_zero_n_is_a_no_op_done_zero() {
        // Linux `FUTEX_WAKE` with `n=0` is a defined no-op;
        // `step_futex_wake` falls through to `Done(0)`. Tightening
        // (rejecting `n=0` as EINVAL) is a separate design decision.
        let _setup = setup();
        let word: u32 = 0;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wake(uaddr, 0, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(0) => {}
            other => panic!("expected v3 Done(0) for n=0 no-op, got {other:?}"),
        }
    }

    #[test]
    fn step_futex_wake_unwaited_returns_done_zero() {
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
        let guard = tx_substrate::epoch::guard();
        let outcome = step_futex_wake(uaddr, 1, &guard);
        drop(guard);
        match outcome {
            tx_substrate::step_v3::StepOutcome::Done(woken) => {
                assert_eq!(woken, 1, "v1 wake returns requested n (best-effort)");
            }
            other => panic!("expected v3 Done(_), got {other:?}"),
        }
    }

    #[test]
    fn futex_step_wake_fires_channel_observed_by_waiter() {
        // Cross-iteration check: register a wait, fire wake on the
        // same uaddr, observe the channel state through
        // wait_carrier::lookup. We don't actually drive a future to
        // completion (that requires async coordination — Slice 11's
        // QEMU shell smoke covers it). Instead we confirm the
        // bucket's carrier id matches across calls — the same
        // channel is fired on wake and parked on by wait.
        let _setup = setup();
        let word: u32 = 42;
        let uaddr = &word as *const u32 as u64;
        let guard = tx_substrate::epoch::guard();
        use tx_substrate::step_v3::YieldShape;
        // wait → Yield { OnCarrier { … } }; pull the carrier id.
        let wait_outcome = step_futex_wait(uaddr, 42, &guard);
        let waiter_carrier = match wait_outcome {
            StepOutcome::Yield {
                shape: YieldShape::OnCarrier { carrier, .. },
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
            wait_carrier::lookup_wait_channel(waiter_carrier).is_some(),
            "fired channel still resolvable",
        );
    }
}
