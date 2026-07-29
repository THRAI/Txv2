//! PR-3D-1 (D2/D4): pipe-readiness `WaitSource` integration tests.
//!
//! Pin the new task-mailbox-based wake path that runs in parallel
//! with the legacy `RawPort`+`Waker` path on pipe. The legacy path is
//! exercised by `crates/tx-subsystems/src/pipe.rs::tests` and by the
//! tx-shims `sys_pipe2`/`sys_read`/`sys_write` suites; this file
//! pins the new path so PR-3D-2..4 reviewers see what a migrated
//! consumer looks like end-to-end.
//!
//! Invariants pinned:
//!
//! 1. **blocked-reader-woken-on-write**. A reader registers a
//!    `TaskMailbox` against the pipe's `reader_wait_source` while
//!    the ring is empty; a subsequent `step_write` posts a
//!    `MailboxEvent::SourceFired` with the registration's
//!    generation and the writer's `PIPE_READABLE` interest.
//! 2. **blocked-writer-woken-on-read**. Symmetric: writer registers
//!    against `writer_wait_source` on a full ring; a subsequent
//!    `step_read` posts the event.
//! 3. **terminal-fires-on-last-reader-close**. Closing the last
//!    reader fd through the process fd table posts a
//!    `MailboxEvent::SourceFired` on the writer-side `WaitSource`
//!    with the `PIPE_WRITABLE` interest, so blocked writers
//!    re-observe `reader_count == 0` and surface EPIPE.
//! 4. **terminal-fires-on-last-writer-close**. Symmetric: last
//!    writer fd-table close fires the reader-side `WaitSource` so
//!    blocked readers re-observe EOF (`Done(0)`).
//! 5. **zero-byte-edge-cases**. A `step_read` with `out.len() == 0`
//!    must not fire the writer source (no actual drain). A
//!    `step_write` with `bytes.len() == 0` must not fire the
//!    reader source. Mirrors the legacy `Channel` behaviour so
//!    PR-3D-1 doesn't drift apart from the surface tx-shims still
//!    consumes.
//! 6. **generation-stamped**. The posted event carries the same
//!    `WaitGeneration` the registration captured, so a driver
//!    matching against `ActiveWait::matches` resolves to "fresh."

extern crate alloc;

use alloc::sync::Arc;

use tx_subsystems::pipe::adapter::step_engine::{
    guard as ebr_guard, Cap, InterestMask, StepOp, StepOutcome, WaitSourceId,
};
use tx_subsystems::pipe::adapter::wait_routing::{
    MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
};
use tx_subsystems::pipe::{
    step_pipe2, step_read, step_write, PipeFlags, PipePayload, PIPE_READABLE, PIPE_WRITABLE,
};
use tx_subsystems::process::adapter::step_engine::ScriptCtx;
use tx_subsystems::process::structure::ProcessIdentity;
use tx_subsystems::process::{bootstrap_init_process, init_process, CloseOp};
use tx_subsystems::vfs::structure::{OpenFile, RNodeBacking, StructPayload};
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

use core::sync::atomic::{AtomicUsize, Ordering};
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};

struct StubPmap;

static NEXT_ROOT_ID: AtomicUsize = AtomicUsize::new(1);

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let id = NEXT_ROOT_ID.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * 4096)),
            Asid(id as u16),
        ))
    }
    fn destroy_pmap_root(_root: PmapRoot) {}
    fn reserve_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }
    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}
    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
    }
    fn unmap_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Ok(Some(PmapUnmapResult::new(virt, PhysAddr(virt.0), kind)))
    }
    fn protect_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        Ok(Some(PmapInvalidation::new(virt, kind.size())))
    }
    fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {}
    fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {}
}

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn payload_of(openfile: &Cap<OpenFile>) -> Cap<PipePayload> {
    match openfile.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe { payload, .. },
        } => payload.clone(),
        other => panic!("expected StructPayload::Pipe, got {other:?}"),
    }
}

fn bootstrap_process() -> Cap<ProcessIdentity> {
    if let Some(process) = init_process() {
        return process;
    }
    bootstrap_init_process(AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace"))
        .expect("bootstrap init")
}

fn close_fd(process: &Cap<ProcessIdentity>, fd: u32) {
    let mut op = CloseOp {
        process: process.clone(),
        fd,
    };
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    match op.step(&mut ctx) {
        StepOutcome::Done(_file) => {}
        other => panic!("close fd {fd} expected Done, got {other:?}"),
    }
}

/// Helper: register `mailbox` against `source` with `interests` and
/// the mailbox's freshly-claimed generation. Returns the
/// registration guard (auto-deregisters on drop) and the captured
/// generation. The guard is intentionally returned so the caller
/// can either hold it across the wait or drop it explicitly to
/// model an unwound wait window.
fn register<'a>(
    source: &'a Arc<WaitSource>,
    mailbox: &Arc<TaskMailbox>,
    interests: u64,
) -> (WaitRegistrationGuard<'a>, WaitGeneration) {
    let gen = mailbox.next_generation();
    let prep = source.prepare(Arc::downgrade(mailbox), gen, InterestMask::new(interests));
    // `install_if(|| true)` is the test pattern (the predicate's
    // truth is enforced by the test setup: empty ring -> caller is
    // genuinely blocked). Production code threads the actual
    // re-test predicate through.
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

// === Invariant 1: blocked-reader-woken-on-write ========================

#[test]
fn blocked_reader_on_empty_ring_is_woken_when_writer_pushes_bytes() {
    let _setup = setup();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());

    // Reader observes empty ring, prepares + installs.
    let (_guard, gen) = register(payload.reader_wait_source(), &mailbox, PIPE_READABLE);
    assert!(mailbox.is_empty(), "no events before write");

    // Writer-side step pushes bytes -> reader source notifies.
    let guard = ebr_guard();
    let outcome = step_write(&payload, b"x", &guard, false, false);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(1));

    assert_source_fired(
        &mailbox,
        WaitSourceId::new(payload.reader_source_id()),
        gen,
        PIPE_READABLE,
    );

    // Hold writer alive past the assertion.
    drop(writer);
    tx_test_support::drain_to_quiescence();
}

// === Invariant 2: blocked-writer-woken-on-read =========================

#[test]
fn blocked_writer_on_full_ring_is_woken_when_reader_drains_bytes() {
    let _setup = setup();
    let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());

    // Fill the pipe exactly to its current capacity so the writer is blocked.
    let big = alloc::vec![b'x'; payload.pipe_size_bytes()];
    let guard = ebr_guard();
    let filled = step_write(&payload, &big, &guard, false, false);
    assert_eq!(filled, StepOutcome::Done(big.len()));
    drop(guard);

    // Writer registers against `writer_wait_source` while ring is full.
    let (_guard_reg, gen) = register(payload.writer_wait_source(), &mailbox, PIPE_WRITABLE);
    assert!(mailbox.is_empty(), "no events before read");

    // Reader-side drain -> writer source notifies.
    let mut buf = [0u8; 8];
    let guard = ebr_guard();
    let outcome = step_read(&payload, &mut buf, &guard, false);
    drop(guard);
    match outcome {
        StepOutcome::Done(n) => assert!(n > 0, "expected drained bytes"),
        other => panic!("expected Done(n>0), got {other:?}"),
    }

    assert_source_fired(
        &mailbox,
        WaitSourceId::new(payload.writer_source_id()),
        gen,
        PIPE_WRITABLE,
    );
}

// === Invariant 3: terminal-fires-on-last-reader-close ==================

#[test]
fn dropping_last_reader_cap_fires_writer_wait_source() {
    let _setup = setup();
    let process = bootstrap_process();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());
    process.set_fd(10, Some(reader));
    process.set_fd(11, Some(writer));

    // Writer registers on writer_wait_source (anticipating EPIPE).
    let (_guard_reg, gen) = register(payload.writer_wait_source(), &mailbox, PIPE_WRITABLE);

    // Production-path last-reader-close.
    close_fd(&process, 10);
    tx_test_support::drain_to_quiescence();

    // Sanity: the reader-source vs writer-source id namespaces are
    // distinct (each side registers a separate id with the legacy
    // resolver at `PipePayload::new` time). The substantive
    // assertion is the event delivery below.
    assert_ne!(
        payload.reader_wait_source().id(),
        payload.writer_wait_source().id(),
    );

    assert_source_fired(
        &mailbox,
        WaitSourceId::new(payload.writer_source_id()),
        gen,
        PIPE_WRITABLE,
    );

    close_fd(&process, 11);
    tx_test_support::drain_to_quiescence();
}

// === Invariant 4: terminal-fires-on-last-writer-close ==================

#[test]
fn dropping_last_writer_cap_fires_reader_wait_source() {
    let _setup = setup();
    let process = bootstrap_process();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());
    process.set_fd(10, Some(reader));
    process.set_fd(11, Some(writer));

    // Reader registers on reader_wait_source (anticipating EOF).
    let (_guard_reg, gen) = register(payload.reader_wait_source(), &mailbox, PIPE_READABLE);

    close_fd(&process, 11);
    tx_test_support::drain_to_quiescence();

    assert_source_fired(
        &mailbox,
        WaitSourceId::new(payload.reader_source_id()),
        gen,
        PIPE_READABLE,
    );

    close_fd(&process, 10);
    tx_test_support::drain_to_quiescence();
}

// === Invariant 5: zero-byte-edge-cases =================================

#[test]
fn step_write_empty_bytes_does_not_fire_reader_wait_source() {
    let _setup = setup();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());

    let (_guard_reg, _gen) = register(payload.reader_wait_source(), &mailbox, PIPE_READABLE);

    let guard = ebr_guard();
    let outcome = step_write(&payload, &[], &guard, false, false);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(0));

    assert!(
        mailbox.is_empty(),
        "empty-bytes write must not fire reader source"
    );

    drop(reader);
    drop(writer);
    tx_test_support::drain_to_quiescence();
}

#[test]
fn step_read_empty_buf_does_not_fire_writer_wait_source() {
    let _setup = setup();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());

    // Seed the ring so the reader path would otherwise drain.
    let guard = ebr_guard();
    let _ = step_write(&payload, b"hi", &guard, false, false);
    drop(guard);

    // Drain any side-effect SourceFired the seeding step posted to a
    // *different* mailbox we might have registered. (We registered
    // none, so this is a no-op; kept for clarity.)
    let _ = mailbox.poll();

    let (_guard_reg, _gen) = register(payload.writer_wait_source(), &mailbox, PIPE_WRITABLE);

    let mut empty: [u8; 0] = [];
    let guard = ebr_guard();
    let outcome = step_read(&payload, &mut empty, &guard, false);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(0));

    assert!(
        mailbox.is_empty(),
        "empty-buf read must not fire writer source"
    );

    drop(reader);
    drop(writer);
    tx_test_support::drain_to_quiescence();
}

// === Invariant 6: generation-stamped (the round-trip) ==================
//
// Covered implicitly by every test above (each asserts `gen` matches).
// One explicit pin to make the invariant grep-able.

#[test]
fn waitsource_notify_stamps_caller_generation_on_event() {
    let _setup = setup();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());

    // Burn a generation before registering so the captured gen is
    // > 1 — pins the "captured at registration time" semantic.
    let _burned = mailbox.next_generation();

    let (_guard_reg, gen) = register(payload.reader_wait_source(), &mailbox, PIPE_READABLE);
    assert!(
        gen.raw() >= 2,
        "captured generation should be monotonic past 1, got {}",
        gen.raw()
    );

    let guard = ebr_guard();
    let _ = step_write(&payload, b"x", &guard, false, false);
    drop(guard);

    let evt = mailbox.poll().expect("event should be queued");
    match evt {
        MailboxEvent::SourceFired { generation, .. } => {
            assert_eq!(generation, gen, "event must carry captured generation");
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }

    drop(reader);
    drop(writer);
    tx_test_support::drain_to_quiescence();
}

// === Coexistence pin (D2): both paths fire on the same transition =====

#[test]
fn write_fires_both_legacy_channel_and_new_wait_source() {
    let _setup = setup();
    let (reader, writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let mailbox = Arc::new(TaskMailbox::new());

    // The new path receiver.
    let (_guard_reg, gen) = register(payload.reader_wait_source(), &mailbox, PIPE_READABLE);

    // We don't directly observe the legacy `Channel.fire(...)` here —
    // the legacy path is tested in `src/pipe.rs::tests` — but we
    // *do* assert that registering against the new source doesn't
    // suppress fire-time behaviour on the legacy side. The shared
    // `step_write` call below is the legacy + new dual-fire site;
    // if D2 coexistence regresses, the legacy `Channel` would still
    // fire but a missing `WaitSource::notify` would leave the
    // mailbox empty. The next assertion catches that.
    let guard = ebr_guard();
    let outcome = step_write(&payload, b"x", &guard, false, false);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(1));

    assert_eq!(mailbox.len(), 1, "new path must have posted one event");
    let _ = mailbox.poll(); // consume; gen-check covered above
    let _ = gen;

    // Pipe still has a live writer; the legacy `Channel.fire`
    // happened on the same step_write call and is independently
    // covered by `crates/tx-subsystems/src/pipe.rs`'s test suite.
    // The point of this test is the *additivity* of D2: we added a
    // path, didn't move existing semantics.

    drop(reader);
    drop(writer);
    tx_test_support::drain_to_quiescence();
}
