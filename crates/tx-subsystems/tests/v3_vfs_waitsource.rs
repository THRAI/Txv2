//! PR-3D-5: per-RNode `read_endpoint` / `write_endpoint` integration tests.
//!
//! Pin the task-mailbox-based wake path for per-inode read/write readiness.
//! Unlike pipe (one payload — two sources) / futex (one
//! registry — 256 buckets) / exit_source (one process — one source) /
//! tty (one identity — one source), VFS is the **per-inode unbounded
//! count** consumer: an RNode mints two sources (read + write) the
//! first time a caller asks for per-inode readiness and releases them
//! on retirement.
//!
//! VFS wake-key model: one `Arc<WaitSource>` per direction
//! (`read_endpoint` / `write_endpoint`) per `RNode`. The bits
//! `VFS_READABLE` (0x1) and `VFS_WRITABLE` (0x2) live in their own
//! per-direction source so a subscriber interested only in readability
//! sees no spurious writable-side posts and vice versa. This is
//! mechanically the same as pipe's two-source-per-payload shape; the
//! novelty here is **unbounded object count** — the brief calls out
//! that a large-N inode-create-destroy stress run must not allocate
//! readiness rows for ordinary path cache entries, and must not leak
//! rows for RNodes that do use the endpoint.
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
//!    `RNode::read_wait_source_id()` / `write_wait_source_id()` (the
//!    `u64` the compatibility `wait_source` resolver published). PR-3D-5's
//!    "same id namespace" pin, applied per-direction. Read and write
//!    ids are distinct so v3 callers can park on the right direction
//!    without collision.
//! 2. **blocked-reader-woken-on-read-wait publication**. A subscriber
//!    registers a `TaskMailbox` against the RNode's
//!    `read_wait_source` while no fire has happened; a subsequent
//!    `fire_read_wait_with_post(VFS_READABLE, post)` posts exactly one
//!    `MailboxEvent::SourceFired` with the registration's generation
//!    and the `VFS_READABLE` interest.
//! 3. **blocked-writer-woken-on-write-wait publication**. Symmetric: writer
//!    registers against `write_wait_source` and observes its post.
//! 4. **endpoint-wait-resolves-on-publication**. A future created through
//!    `wait_on_endpoint(rnode.read_endpoint(), VFS_READABLE)` parks before
//!    publication and resolves after the read wait source fires.
//! 5. **direction-isolation**. Read-wait publication must NOT post to a
//!    subscriber registered against the writer source, and vice versa.
//!    This is the "two independent sources, one per direction" pin —
//!    without it a poll/select on POLLOUT would spuriously fire on
//!    every byte-arrival.
//! 6. **drop-cleanup-retires-registry-slots**. When the last
//!    `Cap<RNode>` drops and EBR retires the slot, the compatibility
//!    `wait_source` registry stops resolving the per-inode ids
//!    (`lookup_wait_source` returns `None`). Subscribers cloning the
//!    `Arc<WaitSource>` before retirement still hold the strong ref
//!    and observe no spurious posts, matching the exit_source /
//!    pipe / tty templates.
//! 7. **lazy-allocation**. Constructing plain RNodes does not register
//!    readiness rows. The first accessor allocates the read/write pair,
//!    and drop releases it.
//! 8. **large-N-inode-create-destroy-no-arc-leak**. The crux of the
//!    per-inode unbounded-count flag: minting and immediately dropping
//!    N inodes back-to-back must release N pairs of registry slots,
//!    so a later `lookup_wait_source` on any of those ids returns
//!    `None`. Without `Drop for RNode` releasing the slots, the
//!    `BTreeMap` would grow without bound.

extern crate alloc;

use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use tx_subsystems::vfs::adapter::step_engine::{Cap, InterestMask, WaitSourceId};
use tx_subsystems::vfs::adapter::wait_routing::{
    MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource,
};

use tx_substrate::wake::{WaitGeneration, WaitRegistrationGuard};
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, RNode, RNodeBacking, VFS_READABLE, VFS_WRITABLE,
};
use tx_subsystems::wait_source;
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static VFS_REF_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

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

fn counting_vfs_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    hint: MailboxSchedulerHint,
) -> bool {
    VFS_REF_POST_COUNT.fetch_add(1, Ordering::SeqCst);
    mailbox.post_with_scheduler_hint(event, hint)
}

fn direct_vfs_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    hint: MailboxSchedulerHint,
) -> bool {
    mailbox.post_with_scheduler_hint(event, hint)
}

#[test]
fn vfs_rnode_fire_wait_uses_injected_mailbox_ref_post() {
    let _setup = setup();
    VFS_REF_POST_COUNT.store(0, Ordering::SeqCst);

    let rnode = make_rnode(9101);
    let read_id = rnode.read_wait_source_id();
    let write_id = rnode.write_wait_source_id();
    let read_source: Arc<WaitSource> = rnode.read_endpoint();
    let write_source: Arc<WaitSource> = rnode.write_endpoint();
    let reader_mb = Arc::new(TaskMailbox::new());
    let writer_mb = Arc::new(TaskMailbox::new());
    let (_reader_guard, reader_gen) = register(&read_source, &reader_mb, VFS_READABLE);
    let (_writer_guard, writer_gen) = register(&write_source, &writer_mb, VFS_WRITABLE);

    let _ = rnode.fire_read_wait_with_post(VFS_READABLE, counting_vfs_ref_post_with_hint);
    let _ = rnode.fire_write_wait_with_post(VFS_WRITABLE, counting_vfs_ref_post_with_hint);

    assert_eq!(
        VFS_REF_POST_COUNT.load(Ordering::SeqCst),
        2,
        "RNode read/write waits should route through injected mailbox-ref post"
    );
    assert_source_fired_for(
        &reader_mb,
        WaitSourceId::new(read_id),
        reader_gen,
        VFS_READABLE,
    );
    assert_source_fired_for(
        &writer_mb,
        WaitSourceId::new(write_id),
        writer_gen,
        VFS_WRITABLE,
    );
    assert!(reader_mb.is_empty(), "only one read event should be posted");
    assert!(
        writer_mb.is_empty(),
        "only one write event should be posted"
    );

    drop(_reader_guard);
    drop(_writer_guard);
    drop(rnode);
    tx_test_support::drain_to_quiescence();
}

#[test]
fn vfs_rnode_wait_sources_are_allocated_lazily() {
    let _setup = setup();
    let baseline = wait_source::registered_wait_source_count();
    {
        let rnode = make_rnode(9001);
        tx_test_support::drain_to_quiescence();
        assert_eq!(
            wait_source::registered_wait_source_count(),
            baseline,
            "plain RNode construction must not allocate global wait-source rows",
        );

        let read_id = rnode.read_wait_source_id();
        let write_id = rnode.write_wait_source_id();
        assert_ne!(read_id, write_id);
        assert!(
            wait_source::lookup_wait_source(read_id).is_some(),
            "first readiness accessor registers the read carrier",
        );
        assert!(
            wait_source::lookup_wait_source(write_id).is_some(),
            "first readiness accessor registers the write carrier",
        );
        assert_eq!(
            wait_source::registered_wait_source_count(),
            baseline + 2,
            "one RNode readiness endpoint owns exactly two registry rows",
        );
    }
    tx_test_support::drain_to_quiescence();
    assert_eq!(
        wait_source::registered_wait_source_count(),
        baseline,
        "dropping a lazily-initialized RNode releases its readiness rows",
    );
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
    let read_endpoint = rnode.read_endpoint();
    let write_endpoint = rnode.write_endpoint();
    let read_source: Arc<WaitSource> = read_endpoint.clone();
    let write_source: Arc<WaitSource> = write_endpoint.clone();
    assert_eq!(
        read_source.id(),
        WaitSourceId::new(read_id),
        "read_wait_source.id() must match read_wait_source_id (same u64 namespace)",
    );
    assert_eq!(
        tx_substrate::wake::WaitEndpoint::source_id(&read_endpoint),
        WaitSourceId::new(read_id),
        "read_endpoint source id must match read_wait_source_id",
    );
    assert_eq!(
        write_source.id(),
        WaitSourceId::new(write_id),
        "write_wait_source.id() must match write_wait_source_id (same u64 namespace)",
    );
    assert_eq!(
        tx_substrate::wake::WaitEndpoint::source_id(&write_endpoint),
        WaitSourceId::new(write_id),
        "write_endpoint source id must match write_wait_source_id",
    );

    // ---- (2) blocked-reader-woken-on-read-wait publication --------
    let reader_mb = Arc::new(TaskMailbox::new());
    let (_reader_guard, reader_gen) = register(&read_source, &reader_mb, VFS_READABLE);
    assert!(reader_mb.is_empty(), "no events before any fire");

    // ---- (4) endpoint wait resolves on read-wait publication -------
    let waker = core::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut endpoint_read_wait = wait_source::wait_on_endpoint(&read_endpoint, VFS_READABLE);
    let pre_endpoint_read = Pin::new(&mut endpoint_read_wait).poll(&mut cx);
    assert!(
        matches!(pre_endpoint_read, Poll::Pending),
        "no fires yet -> endpoint read wait Pending"
    );

    rnode.fire_read_wait_with_post(VFS_READABLE, direct_vfs_ref_post_with_hint);

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

    let post_endpoint_read = Pin::new(&mut endpoint_read_wait).poll(&mut cx);
    assert!(
        matches!(post_endpoint_read, Poll::Ready(_)),
        "read-wait publication must release the parked endpoint awaiter",
    );

    // ---- (3) blocked-writer-woken-on-write-wait publication -------
    let writer_mb = Arc::new(TaskMailbox::new());
    let (_writer_guard, writer_gen) = register(&write_source, &writer_mb, VFS_WRITABLE);
    assert!(
        writer_mb.is_empty(),
        "no events before write-wait publication"
    );

    let _ = rnode.fire_write_wait_with_post(VFS_WRITABLE, direct_vfs_ref_post_with_hint);
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
    let _ = rnode.fire_read_wait_with_post(VFS_READABLE, direct_vfs_ref_post_with_hint);
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
        "read-wait publication must NOT post to write_wait_source subscribers",
    );

    // And vice versa: fire write_wait_source — reader mailbox empty.
    let _ = rnode.fire_write_wait_with_post(VFS_WRITABLE, direct_vfs_ref_post_with_hint);
    assert_source_fired_for(
        &writer_mb,
        WaitSourceId::new(write_id),
        writer_gen,
        VFS_WRITABLE,
    );
    assert!(
        reader_mb.is_empty(),
        "write-wait publication must NOT post to read_wait_source subscribers",
    );

    // ---- (6) drop-cleanup-retires-registry-slots ------------------
    // Pre-drop: compatibility resolver returns Some for both ids.
    assert!(
        wait_source::lookup_wait_source(read_id).is_some(),
        "live RNode keeps read slot registered"
    );
    assert!(
        wait_source::lookup_wait_source(write_id).is_some(),
        "live RNode keeps write slot registered"
    );

    // Drop registration guards before retiring the cap so the Arc<WaitSource>
    // subscriber lists are clean (matches production teardown shape).
    drop(_reader_guard);
    drop(_writer_guard);
    drop(rnode);
    tx_test_support::drain_to_quiescence();

    assert!(
        wait_source::lookup_wait_source(read_id).is_none(),
        "Drop for RNode must release the read carrier id",
    );
    assert!(
        wait_source::lookup_wait_source(write_id).is_none(),
        "Drop for RNode must release the write carrier id",
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

    // ---- (8) large-N-inode-create-destroy-no-arc-leak -------------
    // Mint N inodes back-to-back, snapshot the ids, drop the caps,
    // and confirm every id is unresolvable. Without `Drop for RNode`
    // releasing slots, the `BTreeMap` would retain `2 * N` rows.
    const N: usize = 64;
    let mut minted_ids: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::with_capacity(N);
    {
        let mut caps: alloc::vec::Vec<Cap<RNode>> = alloc::vec::Vec::with_capacity(N);
        for i in 0..N {
            let inode = make_rnode(2000 + i as u64);
            minted_ids.push((inode.read_wait_source_id(), inode.write_wait_source_id()));
            caps.push(inode);
        }
        // Every minted id is currently resolvable.
        for (r, w) in &minted_ids {
            assert!(
                wait_source::lookup_wait_source(*r).is_some(),
                "live RNode read slot must resolve mid-stress"
            );
            assert!(
                wait_source::lookup_wait_source(*w).is_some(),
                "live RNode write slot must resolve mid-stress"
            );
        }
        // Drop all caps.
        drop(caps);
    }
    tx_test_support::drain_to_quiescence();

    // Post-drop: every minted id is unresolvable. This is the no-leak
    // proof — without `Drop for RNode` releasing the slots, the registry
    // would still hold all 2*N source clones.
    for (r, w) in &minted_ids {
        assert!(
            wait_source::lookup_wait_source(*r).is_none(),
            "read slot {r} must be released after EBR retire",
        );
        assert!(
            wait_source::lookup_wait_source(*w).is_none(),
            "write slot {w} must be released after EBR retire",
        );
    }
}
