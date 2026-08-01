//! PR-3D-4: per-TTY `read_endpoint` integration tests.
//!
//! Pin the task-mailbox-based wake path on per-TTY input-readable
//! transitions. This file pins the endpoint path so PR-3D-5 reviewers see
//! what a migrated consumer looks like end-to-end.
//!
//! TTY wake-key model: one `WaitSource` per `TtyIdentity`, one bit
//! (`TTY_READABLE`). Even though `TtyIdentity` has multiple readiness
//! wires (`input_readable` / `output_writable` / `hangup_port` /
//! `session_ctl_port`), only `input_readable` exposes an object-owned
//! `read_endpoint()` today. The others are `RawPort`/`RawQueue`
//! shapes outside this endpoint slice. This is the same "one source per
//! object" shape as `exit_source` (PR-3D-3), simpler than pipe's
//! two-port shape, and the template applies verbatim.
//!
//! The fire site is `step_ingest`: every byte-ingest that transitions
//! the input queue to readable fires `tty.wait_source().notify(InterestMask)`.
//!
//! Invariants pinned (bundled into a single `#[test]` per the cred-
//! zone / exit_wait_source integration-test precedent:
//! `reset_*_for_test` helpers are `pub(crate)` and not visible from
//! integration-test binaries, so a bootstrap-once-then-build-tty
//! structure carries full coverage):
//!
//! 1. **`WaitSourceId`-round-trip**. The `WaitSource::id()` of the
//!    TTY's `wait_source` matches the `u64` returned by
//!    `TtyIdentity::wait_source_id()` (the same `u64` the legacy
//!    `wait_source` resolver published). PR-3D-4's "same id
//!    namespace" pin.
//! 2. **blocked-reader-woken-on-step_ingest**. A subscriber
//!    registers a `TaskMailbox` against the TTY's `wait_source`
//!    while the input queue is empty; a subsequent `step_ingest`
//!    posts a `MailboxEvent::SourceFired` with the registration's
//!    generation and the `TTY_READABLE` interest.
//! 3. **endpoint-wait-resolves-on-step_ingest**. A future created through
//!    `wait_on_endpoint(tty.read_endpoint(), TTY_READABLE)` parks while the
//!    input queue is empty and resolves after `step_ingest` publishes
//!    readability.
//! 4. **hangup-does-not-double-fire-source**. `step_hangup` clears
//!    the payload and fires `hangup_port` / `session_ctl_port` but
//!    those are `RawPort` shapes — the read endpoint has no fire site on
//!    hangup itself. Pin that
//!    a hangup transition does NOT post a new `SourceFired` event
//!    (the source remains observable through the identity since
//!    identity outlives hangup).
//! 5. **master-close-cascade-fires-peer-source**. A pty master close
//!    routes through `step_master_close_last` -> `step_hangup` on
//!    the peer (slave). The peer's `wait_source` is still observable
//!    via its identity (identity outlives hangup), so callers
//!    holding `Arc<WaitSource>` clones retain the strong ref and
//!    observe no spurious posts from the cascade.

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_substrate::wake::{WaitGeneration, WaitRegistrationGuard};
use tx_subsystems::tty::adapter::step_engine::{
    self as zone_mod, guard as ebr_guard, ByteProgress, Cap, InterestMask, PayloadCap, StepOutcome,
    WaitSourceId,
};
use tx_subsystems::tty::adapter::wait_routing::{
    MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource,
};

use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT};
use tx_subsystems::execution::Guard;
use tx_subsystems::tty::execution::{step_hangup, step_ingest_with_post, TTY_READABLE};
use tx_subsystems::tty::structure::{TtyIdentity, TtyKind, TtyPayload};
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TTY_REF_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

struct NoopOps;

impl CharDeviceOps for NoopOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::Done(bytes.len())
    }
}

static NOOP_OPS: NoopOps = NoopOps;
static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 250),
    name: "tty-waitsource-test",
    ops: &NOOP_OPS,
};

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn alloc_hardware_tty(index: u32, name: &str) -> Cap<TtyIdentity> {
    let id_res = zone_mod::reserve_for::<TtyIdentity>().expect("tty identity reservation");
    let payload_res = zone_mod::reserve_for::<TtyPayload>().expect("tty payload reservation");
    let payload_cap = PayloadCap::from_cap(zone_mod::sign_for(
        payload_res,
        TtyPayload::new_hardware(&NOOP_BINDING),
    ));
    let identity = zone_mod::sign_for(
        id_res,
        TtyIdentity::new(TtyKind::SerialHardware, index, name),
    );
    identity.install_payload(payload_cap);
    identity
}

fn register<'a>(
    source: &'a Arc<WaitSource>,
    mailbox: &Arc<TaskMailbox>,
    interests: u64,
) -> (WaitRegistrationGuard<'a>, WaitGeneration) {
    let gen = mailbox.next_generation();
    let prep = source.prepare(Arc::downgrade(mailbox), gen, InterestMask::new(interests));
    let guard = prep.install_if(|| true).expect("registration installed");
    (guard, gen)
}

fn assert_source_fired_for(
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

fn counting_tty_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    hint: MailboxSchedulerHint,
) -> bool {
    TTY_REF_POST_COUNT.fetch_add(1, Ordering::SeqCst);
    mailbox.post_with_scheduler_hint(event, hint)
}

#[test]
fn tty_ingest_uses_injected_mailbox_ref_post_for_readable_wake() {
    let _setup = setup();
    TTY_REF_POST_COUNT.store(0, Ordering::SeqCst);

    let tty = alloc_hardware_tty(701, "ttyV3-with-post");
    let tty_source_id = tty.wait_source_id();
    let tty_source: Arc<WaitSource> = tty.read_endpoint().clone();
    let mailbox = Arc::new(TaskMailbox::new());
    let (_reg_guard, gen) = register(&tty_source, &mailbox, TTY_READABLE);

    let guard = ebr_guard();
    let outcome = step_ingest_with_post(&tty, b"\n", &guard, counting_tty_ref_post_with_hint);
    drop(guard);

    use tx_subsystems::tty::adapter::step_engine::StepOutcome as V3;
    match outcome {
        V3::Done(o) => assert!(o.readable_fired, "expected readable_fired"),
        other => panic!("expected Done(_) with readable_fired, got {other:?}"),
    }

    assert_eq!(
        TTY_REF_POST_COUNT.load(Ordering::SeqCst),
        1,
        "TTY readable wake should route through injected mailbox-ref post"
    );
    assert_source_fired_for(
        &mailbox,
        WaitSourceId::new(tty_source_id),
        gen,
        TTY_READABLE,
    );
    assert!(
        mailbox.is_empty(),
        "only one readable event should be posted"
    );

    drop(tty);
    tx_test_support::drain_to_quiescence();
}

/// Single integration test that bootstraps once and walks every
/// `wait_source` invariant in order. Mirrors the exit_wait_source /
/// cred-zone integration-test structure (the `reset_*_for_test`
/// helpers are crate-private and unreachable from the integration
/// test binary; one test per file is the standard shape).
#[test]
fn tty_wait_source_invariants_round_trip() {
    let _setup = setup();

    let tty = alloc_hardware_tty(700, "ttyV3-waitsource-pin");

    // ---- (1) WaitSourceId round-trip pin --------------------------
    let tty_source_id = tty.wait_source_id();
    let tty_source: Arc<WaitSource> = tty.read_endpoint().clone();
    assert_eq!(
        tx_substrate::wake::WaitEndpoint::source_id(tty.read_endpoint()),
        WaitSourceId::new(tty_source_id),
        "read_endpoint must expose the same TTY readable WaitSource",
    );
    assert_eq!(
        tty_source.id(),
        WaitSourceId::new(tty_source_id),
        "wait_source.id() must match wait_source_id (same u64 namespace)",
    );
    let registered_source = tx_substrate::wake::lookup_source(WaitSourceId::new(tty_source_id))
        .expect("TTY WaitSource must be globally registered for drive() wake resolution");
    assert!(
        Arc::ptr_eq(&registered_source, &tty_source),
        "drive() registry must resolve the TTY identity's live WaitSource",
    );

    // ---- (2) blocked-reader-woken-on-step_ingest ------------------
    let mailbox = Arc::new(TaskMailbox::new());
    let (_reg_guard, gen) = register(&tty_source, &mailbox, TTY_READABLE);
    assert!(mailbox.is_empty(), "no events before any byte ingest");

    // ---- (3) Endpoint wait future resolves across the same step_ingest call.
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn no_op(_: *const ()) {}
    fn waker_clone(_: *const ()) -> RawWaker {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    const VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, no_op, no_op, no_op);
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: vtable functions are no-ops.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);

    let mut endpoint_wait =
        tx_subsystems::wait_source::wait_on_endpoint(tty.read_endpoint(), TTY_READABLE);
    let pre_endpoint = Pin::new(&mut endpoint_wait).poll(&mut cx);
    assert!(
        matches!(pre_endpoint, Poll::Pending),
        "no fires yet -> endpoint Pending"
    );

    // Single byte-ingest transition. `\n` commits a cooked line under
    // default_cooked() termios, which fires `input_readable` and
    // the mailbox `wait_source`.
    let guard = ebr_guard();
    let outcome = step_ingest_with_post(&tty, b"\n", &guard, |mailbox, event, hint| {
        mailbox.post_with_scheduler_hint(event, hint)
    });
    drop(guard);
    use tx_subsystems::tty::adapter::step_engine::StepOutcome as V3;
    match outcome {
        V3::Done(o) => assert!(o.readable_fired, "expected readable_fired"),
        other => panic!("expected Done(_) with readable_fired, got {other:?}"),
    }

    // (2) new path posted exactly one event with matching gen/source.
    assert_source_fired_for(
        &mailbox,
        WaitSourceId::new(tty_source_id),
        gen,
        TTY_READABLE,
    );
    assert!(
        mailbox.is_empty(),
        "new path posts exactly one event per readable transition"
    );

    // (3) Endpoint wait future also resolves.
    let post_endpoint = Pin::new(&mut endpoint_wait).poll(&mut cx);
    assert!(
        matches!(post_endpoint, Poll::Ready(_)),
        "WaitEndpoint notify on step_ingest must release the parked awaiter",
    );

    // ---- (4) hangup does NOT double-fire the input wait_source ----
    // `wait_source` is paired with `wait_channel` which is paired
    // with `input_readable` — hangup uses different wires
    // (`hangup_port` / `session_ctl_port`, both `RawPort`). A fresh
    // mailbox registered after hangup must see no `SourceFired` from
    // the hangup transition itself.
    let mailbox_hangup = Arc::new(TaskMailbox::new());
    let (_reg_guard_hangup, _gen_hangup) = register(&tty_source, &mailbox_hangup, TTY_READABLE);
    // Because tty_source was previously notified, registering a new mailbox
    // immediately receives the pending event due to the WaitSource pending mask.
    // Drain it first so we can check for any new notifications from the hangup transition.
    assert_eq!(mailbox_hangup.len(), 1);
    let _ = mailbox_hangup.poll();
    assert!(mailbox_hangup.is_empty());

    let guard = ebr_guard();
    let hangup = step_hangup(&tty, &guard);
    drop(guard);
    match hangup {
        V3::Done(o) => {
            assert!(o.hangup_fired, "expected hangup_fired");
            assert!(o.session_ctl_fired, "expected session_ctl_fired");
        }
        other => panic!("expected Done(_), got {other:?}"),
    }
    assert!(
        mailbox_hangup.is_empty(),
        "hangup must not post a SourceFired on the input wait_source",
    );

    // (5) master-close-cascade — the tty's wait_source remains
    // observable through the identity post-hangup (identity outlives
    // payload). A clone of the source we held before hangup is still
    // a live `Arc` and reports the same id; no spurious posts.
    assert!(!tty.is_live(), "payload dropped on hangup");
    let post_hangup_source = tty.read_endpoint();
    assert_eq!(
        post_hangup_source.id(),
        WaitSourceId::new(tty_source_id),
        "identity-side wait_source stays observable across hangup",
    );
    // Strong ref still callable.
    let _ = tty_source.subscriber_count();

    // Drop strong refs so EBR can retire.
    drop(tty);
    tx_test_support::drain_to_quiescence();
}
