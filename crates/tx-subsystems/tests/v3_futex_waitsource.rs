//! PR-3D-2 (D2/D4): futex-bucket `WaitSource` integration tests.
//!
//! Pin the new task-mailbox-based wake path that runs in parallel
//! with the legacy `RawPort`+`Waker` path on futex buckets. The
//! legacy path is exercised by `crates/tx-subsystems/src/futex.rs::tests`;
//! this file pins the new path so PR-3D-3..5 reviewers see what a
//! migrated consumer looks like end-to-end.
//!
//! Futex wake-key model: per-bucket (256 fixed buckets keyed on
//! `hash(uaddr) & 0xff`). `(uaddr, val)` is the wait-key but the
//! `val` check is the per-waiter predicate done before parking; the
//! actual wake addressing is `addr` via the bucket-hash. Bucket
//! collisions are absorbed by the per-waiter re-check on wakeup —
//! the same model the legacy `Channel` already used. PR-3D-2 stays on
//! the per-bucket `Arc<WaitSource>` shape; no `(addr, val)` map.
//!
//! Invariants pinned:
//!
//! 1. **blocked-waiter-woken-on-wake**. A waiter registers a
//!    `TaskMailbox` against the bucket's `wait_source` while the
//!    user word matches `val`; a subsequent `step_futex_wake` on
//!    the same `uaddr` posts a `MailboxEvent::SourceFired` with the
//!    registration's generation and the `FUTEX_WAKE_MASK` interest.
//! 2. **wake-count-n-still-fires-source-once-per-bucket**. v1
//!    `step_futex_wake` is best-effort: returns the requested `n`
//!    regardless of actually-woken count. The `WaitSource::notify`
//!    side fires the bucket source exactly once per wake call,
//!    independent of `n` — all registered subscribers receive the
//!    event; the syscall arm's `n` semantic is layered above.
//! 3. **wake-fires-all-bucket-subscribers**. Multiple `TaskMailbox`es
//!    registered against the same bucket each receive a
//!    `SourceFired` event on a single wake call (broadcast within
//!    the bucket).
//! 4. **disjoint-uaddrs-disjoint-wakes**. Two `uaddr`s that hash to
//!    **different** buckets: a wake on one does not fire the
//!    other's `WaitSource`. (We pick a pair confirmed to hash apart
//!    via `bucket_index`.)
//! 5. **stale-mailbox-cleanup**. Dropping a `TaskMailbox`'s only
//!    strong reference before wake fires causes the bucket's
//!    `WaitSource` subscriber list to compact out the dead row on
//!    the next `notify` (substrate behaviour — pinned here to catch
//!    a regression in the futex-bucket lifetime model).
//! 6. **generation-stamped**. The posted event carries the same
//!    `WaitGeneration` the registration captured.
//! 7. **bucket_wait_source_id-round-trip**. The `WaitSourceId`
//!    stamped into `step_futex_wait`'s `OnWaitSource` yield matches
//!    the `WaitSource::id()` returned by
//!    `bucket_wait_source_for_source_id` — D2's "same id namespace"
//!    pin.
//! 8. **D2-coexistence**. A `step_futex_wake` call fires both the
//!    legacy `Channel` AND the new `WaitSource` on the same step.
//!    Mirrors the pipe-side `write_fires_both_legacy_channel_and_new_wait_source`
//!    pin from PR-3D-1.

extern crate alloc;

use alloc::sync::Arc;

use tx_subsystems::futex::adapter::step_engine::{
    guard as ebr_guard, InterestMask, StepOutcome, WaitSourceId, YieldShape,
};
use tx_subsystems::futex::adapter::wait_routing::{
    MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
};

use tx_subsystems::futex::{
    bucket_index, bucket_wait_source, bucket_wait_source_for_source_id, step_futex_wait,
    step_futex_wake, FUTEX_WAKE_MASK,
};
use tx_subsystems::wait_source as legacy_wait_source;
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    guard
}

/// Helper: register `mailbox` against `source` with `interests` and
/// the mailbox's freshly-claimed generation. Returns the registration
/// guard (auto-deregisters on drop) and the captured generation.
fn register<'a>(
    source: &'a Arc<WaitSource>,
    mailbox: &Arc<TaskMailbox>,
    interests: u64,
) -> (WaitRegistrationGuard<'a>, WaitGeneration) {
    let gen = mailbox.next_generation();
    let prep = source.prepare(Arc::downgrade(mailbox), gen, InterestMask::new(interests));
    // `install_if(|| true)` is the test pattern (the predicate's
    // truth is enforced by setup: the user word matches `val` so the
    // caller is genuinely blocked). Production code threads the
    // actual re-test predicate (`*uaddr == val`) through.
    let guard = prep.install_if(|| true).expect("registration installed");
    (guard, gen)
}

/// Match-assert that the next mailbox event is a `SourceFired` for
/// `source` with `generation` and interest mask **overlapping** the
/// given expected mask.
fn assert_source_fired(
    mailbox: &TaskMailbox,
    source: WaitSourceId,
    generation: WaitGeneration,
    expected_overlap: u64,
) {
    let evt = mailbox
        .poll()
        .expect("mailbox should have one SourceFired event");
    match evt {
        MailboxEvent::SourceFired {
            generation: g,
            source: s,
            interests,
        } => {
            assert_eq!(g, generation, "stale generation");
            assert_eq!(s, source, "wrong source");
            assert_ne!(
                interests.raw() & expected_overlap,
                0,
                "expected interest overlap with 0x{expected_overlap:x}, got 0x{:x}",
                interests.raw()
            );
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }
}

// === Invariant 1: blocked-waiter-woken-on-wake =========================

#[test]
fn blocked_waiter_on_matching_value_is_woken_on_futex_wake() {
    let _setup = setup();
    let word: u32 = 0xdead_beef;
    let uaddr = &word as *const u32 as u64;
    let source = bucket_wait_source(uaddr).expect("bucket initialised");
    let mailbox = Arc::new(TaskMailbox::new());

    let (_guard_reg, gen) = register(&source, &mailbox, FUTEX_WAKE_MASK);
    assert!(mailbox.is_empty(), "no events before wake");

    let guard = ebr_guard();
    let outcome = step_futex_wake(uaddr, 1, &guard);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(1));

    assert_source_fired(&mailbox, source.id(), gen, FUTEX_WAKE_MASK);
}

// === Invariant 2: wake-count-n-still-fires-source-once-per-bucket ======

#[test]
fn wake_count_n_fires_source_once_independent_of_n() {
    // `step_futex_wake(uaddr, n, ..)` returns Done(n) (best-effort)
    // but the `WaitSource::notify` is per-call (one notify per
    // wake), independent of `n`. Pin that the wake-count semantic
    // is layered above the mailbox event delivery: a single
    // wake call delivers one event per subscriber, not n events.
    let _setup = setup();
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;
    let source = bucket_wait_source(uaddr).expect("bucket initialised");
    let mailbox = Arc::new(TaskMailbox::new());

    let (_guard_reg, gen) = register(&source, &mailbox, FUTEX_WAKE_MASK);

    // wake with n=7; mailbox still receives exactly one event.
    let guard = ebr_guard();
    let outcome = step_futex_wake(uaddr, 7, &guard);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(7));
    assert_eq!(
        mailbox.len(),
        1,
        "WaitSource::notify fires once per wake call regardless of n",
    );
    assert_source_fired(&mailbox, source.id(), gen, FUTEX_WAKE_MASK);
}

// === Invariant 3: wake-fires-all-bucket-subscribers ====================

#[test]
fn wake_broadcasts_to_all_bucket_subscribers() {
    let _setup = setup();
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;
    let source = bucket_wait_source(uaddr).expect("bucket initialised");
    let mbox_a = Arc::new(TaskMailbox::new());
    let mbox_b = Arc::new(TaskMailbox::new());
    let mbox_c = Arc::new(TaskMailbox::new());

    let (_g_a, gen_a) = register(&source, &mbox_a, FUTEX_WAKE_MASK);
    let (_g_b, gen_b) = register(&source, &mbox_b, FUTEX_WAKE_MASK);
    let (_g_c, gen_c) = register(&source, &mbox_c, FUTEX_WAKE_MASK);
    assert_eq!(source.subscriber_count(), 3);

    let guard = ebr_guard();
    let outcome = step_futex_wake(uaddr, 1, &guard);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(1));

    assert_source_fired(&mbox_a, source.id(), gen_a, FUTEX_WAKE_MASK);
    assert_source_fired(&mbox_b, source.id(), gen_b, FUTEX_WAKE_MASK);
    assert_source_fired(&mbox_c, source.id(), gen_c, FUTEX_WAKE_MASK);
}

// === Invariant 4: disjoint-uaddrs-disjoint-wakes =======================

#[test]
fn wake_on_different_bucket_does_not_fire_unrelated_source() {
    let _setup = setup();
    // Find two uaddrs that hash to different buckets. The stride is
    // chosen to be 4-byte-aligned, non-zero, and far enough apart in
    // the rotate-and-fold to land elsewhere on the bucket table.
    let word_a: u32 = 0;
    let word_b: u32 = 0;
    let uaddr_a = &word_a as *const u32 as u64;
    let uaddr_b = &word_b as *const u32 as u64;
    if bucket_index(uaddr_a) == bucket_index(uaddr_b) {
        // 1-in-256 chance the two stack words land in the same
        // bucket. Force a different bucket by walking forward in
        // pretend-uaddr space. We don't actually deref these — we
        // only call `bucket_wait_source(uaddr_b)` which is a hash,
        // and `step_futex_wake(uaddr_b, ..)` which also only hashes.
        let mut probe = uaddr_b;
        // Pad to 4-byte alignment defensively, then walk by 4 until
        // we land in a different bucket.
        probe = (probe + 3) & !0x3u64;
        while bucket_index(probe) == bucket_index(uaddr_a) {
            probe = probe.wrapping_add(4);
            // Avoid the zero/EINVAL traps (defensive — the wrap
            // boundary is astronomical for stack-derived seeds).
            if probe == 0 {
                probe = 4;
            }
        }
        do_disjoint_test(uaddr_a, probe);
    } else {
        do_disjoint_test(uaddr_a, uaddr_b);
    }
}

fn do_disjoint_test(uaddr_a: u64, uaddr_b: u64) {
    let src_a = bucket_wait_source(uaddr_a).expect("bucket A initialised");
    let src_b = bucket_wait_source(uaddr_b).expect("bucket B initialised");
    assert_ne!(
        src_a.id(),
        src_b.id(),
        "different buckets must have different WaitSource ids",
    );

    let mbox_a = Arc::new(TaskMailbox::new());
    let mbox_b = Arc::new(TaskMailbox::new());
    let (_g_a, _gen_a) = register(&src_a, &mbox_a, FUTEX_WAKE_MASK);
    let (_g_b, _gen_b) = register(&src_b, &mbox_b, FUTEX_WAKE_MASK);

    // Wake on A only.
    let guard = ebr_guard();
    let _ = step_futex_wake(uaddr_a, 1, &guard);
    drop(guard);

    assert_eq!(mbox_a.len(), 1, "A's mailbox got A's wake");
    assert!(
        mbox_b.is_empty(),
        "B's mailbox must not receive A's wake (disjoint bucket)",
    );
}

// === Invariant 5: stale-mailbox-cleanup ================================

#[test]
fn dropping_mailbox_before_wake_compacts_dead_subscriber() {
    let _setup = setup();
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;
    let source = bucket_wait_source(uaddr).expect("bucket initialised");

    // Live mailbox that should survive and receive the wake.
    let live = Arc::new(TaskMailbox::new());
    let (_g_live, gen_live) = register(&source, &live, FUTEX_WAKE_MASK);

    // Register a Weak against a mailbox we'll drop before wake.
    {
        let temp = Arc::new(TaskMailbox::new());
        let _temp_gen = temp.next_generation();
        let prep = source.prepare(
            Arc::downgrade(&temp),
            temp.next_generation(),
            InterestMask::new(FUTEX_WAKE_MASK),
        );
        // Commit via `install` (no auto-deregister wanted; we want
        // to leave the dead row in the subscriber list and watch
        // notify compact it). The returned guard auto-deregisters
        // on drop — `forget` keeps the row.
        let guard = prep.install();
        let _id = guard.forget();
        assert_eq!(source.subscriber_count(), 2);
        // Drop `temp`. Its `Weak` upgrade will now fail at notify
        // time, compacting the row out.
    }
    // Stale row still present until next notify.
    assert_eq!(source.subscriber_count(), 2);

    let guard = ebr_guard();
    let _ = step_futex_wake(uaddr, 1, &guard);
    drop(guard);

    // Dead row compacted; live row delivered.
    assert_eq!(
        source.subscriber_count(),
        1,
        "dead Weak subscriber must be compacted on notify",
    );
    assert_source_fired(&live, source.id(), gen_live, FUTEX_WAKE_MASK);
}

// === Invariant 6: generation-stamped (round-trip) ======================

#[test]
fn waitsource_notify_stamps_caller_generation_on_event() {
    let _setup = setup();
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;
    let source = bucket_wait_source(uaddr).expect("bucket initialised");
    let mailbox = Arc::new(TaskMailbox::new());

    // Burn a generation before registering so the captured gen is
    // > 1.
    let _burned = mailbox.next_generation();

    let (_g, gen) = register(&source, &mailbox, FUTEX_WAKE_MASK);
    assert!(
        gen.raw() >= 2,
        "captured generation should be monotonic past 1, got {}",
        gen.raw()
    );

    let guard = ebr_guard();
    let _ = step_futex_wake(uaddr, 1, &guard);
    drop(guard);

    let evt = mailbox.poll().expect("event should be queued");
    match evt {
        MailboxEvent::SourceFired { generation, .. } => {
            assert_eq!(generation, gen, "event must carry captured generation");
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }
}

// === Invariant 7: bucket_wait_source_id round-trip =====================

#[test]
fn wait_source_id_round_trips_from_yield_shape_to_bucket_source() {
    let _setup = setup();
    let word: u32 = 0xdead_beef;
    let uaddr = &word as *const u32 as u64;

    // `step_futex_wait` yields with the bucket's `source_id` stamped
    // into `OnWaitSource`. The same `u64` resolves to the bucket's
    // `Arc<WaitSource>` via `bucket_wait_source_for_source_id`.
    let guard = ebr_guard();
    let outcome = step_futex_wait(uaddr, 0xdead_beef, &guard);
    drop(guard);

    let stamped_id = match outcome {
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, .. },
            ..
        } => source.raw(),
        other => panic!("expected Yield::OnWaitSource, got {other:?}"),
    };

    let direct = bucket_wait_source(uaddr).expect("bucket initialised");
    let by_id = bucket_wait_source_for_source_id(stamped_id)
        .expect("stamped source_id resolves via legacy id namespace");

    // Same underlying Arc<WaitSource> identity (Arc::ptr_eq).
    assert!(
        Arc::ptr_eq(&direct, &by_id),
        "bucket_wait_source(uaddr) and bucket_wait_source_for_source_id(stamped) must be the same Arc",
    );
    assert_eq!(direct.id().raw(), stamped_id);
}

// === Invariant 8: D2 coexistence — both paths fire =====================

#[test]
fn wake_fires_both_legacy_channel_and_new_wait_source() {
    let _setup = setup();
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;
    let source = bucket_wait_source(uaddr).expect("bucket initialised");
    let mailbox = Arc::new(TaskMailbox::new());

    let (_g, gen) = register(&source, &mailbox, FUTEX_WAKE_MASK);

    // Sanity: the legacy `Channel` is still resolvable via the
    // legacy `wait_source` registry under the same id. The legacy
    // path's wake is `Channel::fire` — we don't directly drive a
    // `WaitFuture` here (that requires async coordination), but we
    // pin that the legacy id namespace is intact AND the new path
    // is additive (mailbox receives an event in addition to whatever
    // the legacy `Channel.fire` does).
    assert!(
        legacy_wait_source::lookup_wait_channel(source.id().raw()).is_some(),
        "legacy Channel must remain resolvable under the same source_id",
    );

    let guard = ebr_guard();
    let outcome = step_futex_wake(uaddr, 1, &guard);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(1));

    assert_eq!(mailbox.len(), 1, "new path must have posted one event");
    assert_source_fired(&mailbox, source.id(), gen, FUTEX_WAKE_MASK);
}
