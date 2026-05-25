//! PR-3D-5: per-RNode `read_wait_source` / `write_wait_source`
//! integration tests.
//!
//! Pin the task-mailbox-based wake path for per-inode read/write
//! readiness. Unlike pipe (one payload — two sources) / futex (one
//! registry — 256 buckets) / exit_source (one process — one source) /
//! tty (one identity — one source), VFS is the **per-inode unbounded
//! count** consumer: every RNode created at runtime mints two sources
//! (read + write) and releases them on retirement.
//!
//! VFS wake-key model: one `Arc<WaitSource>` per direction
//! (`read_wait_source` / `write_wait_source`) per `RNode`. The bits
//! `VFS_READABLE` (0x1) and `VFS_WRITABLE` (0x2) live in their own
//! per-direction source so a subscriber interested only in readability
//! sees no spurious writable-side posts and vice versa. This is
//! mechanically the same as pipe's two-source-per-payload shape; the
//! novelty here is **unbounded object count** — the brief calls out
//! that a large-N inode-create-destroy stress run must not leak
//! registry rows.
//!
//! Invariants pinned (bundled into a single `#[test]` per the cred-
//! zone / exit_wait_source / tty_waitsource integration-test
//! precedent: `reset_*_for_test` helpers are `pub(crate)` and not
//! visible from integration-test binaries, so a bootstrap-once-then-
//! mint-rnodes structure carries full coverage):
//!
//! 1. **`WaitSourceId`-round-trip-both-directions**. The
//!    `WaitSource::id()` of the RNode's `read_wait_source` /
//!    `write_wait_source` each matches the `u64` returned by
//!    `RNode::read_wait_source_id()` / `write_wait_source_id()`.
//!    Read and write ids are distinct so v3 callers can park on the
//!    right direction without collision.
//! 2. **blocked-reader-woken-on-fire_read_wait**. A subscriber
//!    registers a `TaskMailbox` against the RNode's
//!    `read_wait_source` while no fire has happened; a subsequent
//!    `fire_read_wait(VFS_READABLE)` posts exactly one
//!    `MailboxEvent::SourceFired` with the registration's generation
//!    and the `VFS_READABLE` interest.
//! 3. **blocked-writer-woken-on-fire_write_wait**. Symmetric: writer
//!    registers against `write_wait_source` and observes its post.
//! 4. **drive-registry-resolves-live-sources**. The global v3 source
//!    registry resolves each live source id to the same `Arc<WaitSource>`
//!    that the RNode owns, so `drive()` can subscribe parked tasks by
//!    `WaitSourceId`.
//! 5. **direction-isolation**. `fire_read_wait` must NOT post to a
//!    subscriber registered against the writer source, and vice versa.
//!    This is the "two independent sources, one per direction" pin —
//!    without it a poll/select on POLLOUT would spuriously fire on
//!    every byte-arrival.
//! 6. **drop-cleanup-retires-registry-slots**. When the last
//!    `Cap<RNode>` drops and EBR retires the slot, the v3 source
//!    registry stops resolving the per-inode ids. Subscribers cloning
//!    the `Arc<WaitSource>` before retirement still hold the strong
//!    ref and observe no spurious posts, matching the exit_source /
//!    pipe / tty templates.
//! 7. **large-N-inode-create-destroy-no-arc-leak**. The crux of the
//!    per-inode unbounded-count flag: minting and immediately dropping
//!    N inodes back-to-back must release N pairs of registry slots.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::sync::Arc;

use tx_subsystems::vfs::adapter::step_engine::{Cap, InterestMask, WaitSourceId};
use tx_subsystems::vfs::adapter::wait_routing::{
    MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
};

use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, RNode, RNodeBacking, VFS_READABLE, VFS_WRITABLE,
};
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn make_rnode(id: u64) -> Cap<RNode> {
    RNode::new_cap(
        FsObjectId::new(id),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::Directory,
    )
    .expect("rnode reservation")
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

/// Single integration test that bootstraps once and walks every
/// per-RNode `wait_source` invariant in order. Mirrors the tty /
/// exit_wait_source / cred-zone integration-test structure (the
/// `reset_*_for_test` helpers are crate-private and unreachable from
/// the integration test binary; one test per file is the standard
/// shape).
#[test]
fn vfs_wait_source_invariants_round_trip() {
    let _setup = setup();

    let rnode = make_rnode(1001);

    // ---- (1) WaitSourceId round-trip pin --------------------------
    let read_id = rnode.read_wait_source_id();
    let write_id = rnode.write_wait_source_id();
    assert_ne!(
        read_id, write_id,
        "read and write directions must mint distinct registry slots",
    );
    let read_source: Arc<WaitSource> = rnode.read_wait_source().clone();
    let write_source: Arc<WaitSource> = rnode.write_wait_source().clone();
    assert_eq!(
        read_source.id(),
        WaitSourceId::new(read_id),
        "read_wait_source.id() must match read_wait_source_id (same u64 namespace)",
    );
    assert_eq!(
        write_source.id(),
        WaitSourceId::new(write_id),
        "write_wait_source.id() must match write_wait_source_id (same u64 namespace)",
    );

    // ---- (4) drive-registry-resolves-live-sources -----------------
    let registered_read = tx_substrate::wake::lookup_source(WaitSourceId::new(read_id))
        .expect("live RNode read source must be registered for drive() wake resolution");
    assert!(
        Arc::ptr_eq(&registered_read, &read_source),
        "drive() registry must resolve the RNode's live read WaitSource",
    );
    let registered_write = tx_substrate::wake::lookup_source(WaitSourceId::new(write_id))
        .expect("live RNode write source must be registered for drive() wake resolution");
    assert!(
        Arc::ptr_eq(&registered_write, &write_source),
        "drive() registry must resolve the RNode's live write WaitSource",
    );

    // ---- (2) blocked-reader-woken-on-fire_read_wait ---------------
    let reader_mb = Arc::new(TaskMailbox::new());
    let (_reader_guard, reader_gen) = register(&read_source, &reader_mb, VFS_READABLE);
    assert!(reader_mb.is_empty(), "no events before any fire");

    let _ = rnode.fire_read_wait(VFS_READABLE);

    // New path posted exactly one event with matching gen/source.
    assert_source_fired_for(
        &reader_mb,
        WaitSourceId::new(read_id),
        reader_gen,
        VFS_READABLE,
    );
    assert!(
        reader_mb.is_empty(),
        "new path posts exactly one event per fire",
    );

    // ---- (3) blocked-writer-woken-on-fire_write_wait --------------
    let writer_mb = Arc::new(TaskMailbox::new());
    let (_writer_guard, writer_gen) = register(&write_source, &writer_mb, VFS_WRITABLE);
    assert!(writer_mb.is_empty(), "no events before fire_write_wait");

    let _ = rnode.fire_write_wait(VFS_WRITABLE);
    assert_source_fired_for(
        &writer_mb,
        WaitSourceId::new(write_id),
        writer_gen,
        VFS_WRITABLE,
    );
    assert!(writer_mb.is_empty(), "exactly one event");

    // ---- (5) direction-isolation ----------------------------------
    // Fire the read side again — the writer mailbox must NOT receive
    // any event, even though both sources live on the same RNode.
    let _ = rnode.fire_read_wait(VFS_READABLE);
    // The reader mailbox sees the second event; the writer mailbox
    // sees nothing new.
    assert_source_fired_for(
        &reader_mb,
        WaitSourceId::new(read_id),
        reader_gen,
        VFS_READABLE,
    );
    assert!(
        writer_mb.is_empty(),
        "fire_read_wait must NOT post to write_wait_source subscribers",
    );

    // And vice versa: fire write_wait_source — reader mailbox empty.
    let _ = rnode.fire_write_wait(VFS_WRITABLE);
    assert_source_fired_for(
        &writer_mb,
        WaitSourceId::new(write_id),
        writer_gen,
        VFS_WRITABLE,
    );
    assert!(
        reader_mb.is_empty(),
        "fire_write_wait must NOT post to read_wait_source subscribers",
    );

    // ---- (6) drop-cleanup-retires-registry-slots ------------------
    // Pre-drop: v3 registry resolves both ids.
    assert!(
        tx_substrate::wake::lookup_source(WaitSourceId::new(read_id)).is_some(),
        "live RNode keeps read slot registered"
    );
    assert!(
        tx_substrate::wake::lookup_source(WaitSourceId::new(write_id)).is_some(),
        "live RNode keeps write slot registered"
    );

    // Drop registration guards before retiring the cap so the Arc<WaitSource>
    // subscriber lists are clean (matches production teardown shape).
    drop(_reader_guard);
    drop(_writer_guard);
    drop(rnode);
    tx_test_support::drain_to_quiescence();

    assert!(
        tx_substrate::wake::lookup_source(WaitSourceId::new(read_id)).is_none(),
        "Drop for RNode must unregister the read source id",
    );
    assert!(
        tx_substrate::wake::lookup_source(WaitSourceId::new(write_id)).is_none(),
        "Drop for RNode must unregister the write source id",
    );

    // Strong-ref clones we held survive the drop: cloning an Arc
    // doesn't depend on the RNode. Notify into the orphaned source
    // is a no-op (no live subscribers) and must not panic.
    assert_eq!(
        read_source.id(),
        WaitSourceId::new(read_id),
        "orphaned source Arc keeps reporting its id",
    );
    let posted = read_source.notify(InterestMask::new(VFS_READABLE));
    assert_eq!(posted, 0, "orphaned source has no subscribers");
    let posted = write_source.notify(InterestMask::new(VFS_WRITABLE));
    assert_eq!(posted, 0, "orphaned source has no subscribers");

    // ---- (7) large-N-inode-create-destroy-no-arc-leak -------------
    // Mint N inodes back-to-back, snapshot the ids, drop the caps,
    // and confirm every id is unique, live while held, and unresolvable
    // after teardown.
    const N: usize = 64;
    let mut minted_ids: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::with_capacity(N);
    {
        let mut caps: alloc::vec::Vec<Cap<RNode>> = alloc::vec::Vec::with_capacity(N);
        let mut unique = BTreeSet::new();
        for i in 0..N {
            let inode = make_rnode(2000 + i as u64);
            let ids = (inode.read_wait_source_id(), inode.write_wait_source_id());
            assert_ne!(ids.0, ids.1, "read/write ids must stay distinct");
            assert!(unique.insert(ids.0), "duplicate read source id {}", ids.0);
            assert!(unique.insert(ids.1), "duplicate write source id {}", ids.1);
            minted_ids.push(ids);
            caps.push(inode);
        }
        // Every minted id is currently resolvable.
        for (r, w) in &minted_ids {
            assert!(
                tx_substrate::wake::lookup_source(WaitSourceId::new(*r)).is_some(),
                "live RNode read slot must resolve mid-stress"
            );
            assert!(
                tx_substrate::wake::lookup_source(WaitSourceId::new(*w)).is_some(),
                "live RNode write slot must resolve mid-stress"
            );
        }
        // Drop all caps.
        drop(caps);
    }
    tx_test_support::drain_to_quiescence();

    // Post-drop: every minted id is unresolvable. This is the no-leak
    // proof for the drive-facing v3 source registry.
    for (r, w) in &minted_ids {
        assert!(
            tx_substrate::wake::lookup_source(WaitSourceId::new(*r)).is_none(),
            "read slot {r} must be released after EBR retire",
        );
        assert!(
            tx_substrate::wake::lookup_source(WaitSourceId::new(*w)).is_none(),
            "write slot {w} must be released after EBR retire",
        );
    }
}
