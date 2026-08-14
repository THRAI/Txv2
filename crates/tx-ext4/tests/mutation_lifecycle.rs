use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use tx_ext4::journal::{
    JournalFsyncSource, JournalMutationRuntime, JournalPagePool, JournalRing, JournalRingError,
    JournalSettlementObserver,
};
use tx_ext4::planner::Ext4FsyncPlanSource;
use tx_ext4_format::journal::{Jbd2Features, Jbd2Superblock, JBD2_BLOCK_SIZE};
use tx_ext4_format::mutation::{
    Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin, SealedDataWrite,
};
use tx_ext4_format::pager::JournalGeometry;
use tx_subsystems::fs_iface::{
    BackendPageCompletion, BackendPageRequest, BackendPlan, FsObjectKey, IoDataLeaseId,
    IoDataSource, IoDataTarget, PageFrameRef,
};
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::io_manager::page::{
    PageGeneration, PageIoFlags, PageIoOp, PageIoRange, PageIoRequestId, PageIoResult,
};
use tx_subsystems::mount::MountTransactionFrontier;

fn setup() {
    tx_test_support::init_host();
    tx_subsystems::zones::register_all().expect("kernel zones");
}

fn ring() -> Arc<JournalRing> {
    Arc::new(
        JournalRing::new(
            DeviceKey::new(9),
            8,
            JournalGeometry {
                superblock: Jbd2Superblock {
                    block_type: 4,
                    block_size: JBD2_BLOCK_SIZE as u32,
                    max_len: 8,
                    first: 1,
                    sequence: 7,
                    start: 0,
                    uuid: [1; 16],
                },
                features: Jbd2Features::REVOKE,
                blocks: vec![9, 10, 11, 12, 13, 14, 15, 16],
                superblock_page: None,
            },
        )
        .unwrap(),
    )
}

fn mutation() -> Ext4MutationPlan {
    let mut mutation = Ext4MutationPlan::new(MutationOrigin::FlushPage, 12, FsyncStamp::new(7));
    mutation.data.push(SealedDataWrite {
        logical_page: 0,
        physical_block: 7,
        bytes: [0; JBD2_BLOCK_SIZE],
    });
    mutation
        .push_metadata(MetadataBlock {
            home: 33,
            role: MetaRole::InodeTable,
            before_version: 1,
            after: [0; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    mutation
}

fn data_request() -> BackendPageRequest {
    BackendPageRequest::new_with_source_and_target(
        FsObjectKey::new(12),
        PageIoRequestId::new(70),
        PageIoRange::new(0, 1),
        PageIoOp::Writeback,
        PageIoFlags::WRITEBACK,
        Some(PageGeneration::new(7)),
        IoDataSource::None,
        IoDataTarget::None,
    )
}

fn fsync_request() -> BackendPageRequest {
    BackendPageRequest::new(
        FsObjectKey::new(12),
        PageIoRequestId::new(71),
        PageIoRange::new(0, 1),
        PageIoOp::Fsync,
        PageIoFlags::BARRIER,
        None,
    )
}

struct SettlementSpy {
    calls: AtomicUsize,
    ring: Arc<JournalRing>,
}

struct DeferredFreeSettlementSpy {
    source: std::sync::Weak<JournalFsyncSource>,
    physical_block: u64,
}

impl JournalSettlementObserver for DeferredFreeSettlementSpy {
    fn settle_after_checkpoint(&self) {
        let source = self.source.upgrade().expect("source remains mounted");
        assert_eq!(
            source.try_reuse_for_test(self.physical_block),
            Err(tx_subsystems::execution::Errno::EBUSY)
        );
    }
}

impl SettlementSpy {
    fn new(ring: Arc<JournalRing>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            ring,
        }
    }
}

impl JournalSettlementObserver for SettlementSpy {
    fn settle_after_checkpoint(&self) {
        assert_eq!(self.ring.reserve(1), Err(JournalRingError::Busy));
        self.calls.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
fn data_failure_releases_the_handle_owned_journal_extent() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        Arc::clone(&ring),
    );
    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation(),
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    let request = data_request();
    assert!(matches!(
        runtime.plan_data(&request),
        BackendPlan::SubmitGraph(_)
    ));

    runtime.complete_data(BackendPageCompletion::new(
        request.object,
        request.id,
        request.op,
        PageIoResult::Err(tx_subsystems::execution::Errno::EIO),
    ));

    assert_ne!(ring.reserve(1), Err(JournalRingError::Busy));
}

#[test]
fn runtime_snapshot_frontier_tracks_active_journal_sequence() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        Arc::clone(&ring),
    );
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        MountTransactionFrontier::default()
    );

    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation(),
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();

    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        MountTransactionFrontier::new(7)
    );
}

#[test]
fn unknown_commit_retains_the_extent_and_blocks_new_mutations() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        Arc::clone(&ring),
    );
    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation(),
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    let data = data_request();
    let fsync = fsync_request();
    assert!(matches!(
        runtime.plan_data(&data),
        BackendPlan::SubmitGraph(_)
    ));
    runtime.complete_data(BackendPageCompletion::new(
        data.object,
        data.id,
        data.op,
        PageIoResult::Done,
    ));
    assert!(matches!(
        source.plan_fsync(&fsync),
        BackendPlan::SubmitGraph(_)
    ));

    source.commit_unknown(fsync.id, tx_subsystems::execution::Errno::EIO);

    assert_eq!(ring.reserve(1), Err(JournalRingError::Busy));
    assert_eq!(
        source.mount_error(),
        Some(tx_subsystems::execution::Errno::EIO)
    );
    assert!(matches!(
        runtime.plan_data(&data),
        BackendPlan::Err(tx_subsystems::execution::Errno::EIO)
    ));
    assert!(matches!(
        source.plan_fsync(&fsync),
        BackendPlan::Err(tx_subsystems::execution::Errno::EIO)
    ));
}

#[test]
fn known_commit_failure_rolls_back_and_releases_the_extent() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        Arc::clone(&ring),
    );
    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation(),
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    let data = data_request();
    let fsync = fsync_request();
    assert!(matches!(
        runtime.plan_data(&data),
        BackendPlan::SubmitGraph(_)
    ));
    runtime.complete_data(BackendPageCompletion::new(
        data.object,
        data.id,
        data.op,
        PageIoResult::Done,
    ));
    assert!(matches!(
        source.plan_fsync(&fsync),
        BackendPlan::SubmitGraph(_)
    ));

    source.complete_fsync(BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        PageIoResult::Err(tx_subsystems::execution::Errno::EIO),
    ));

    assert_ne!(ring.reserve(1), Err(JournalRingError::Busy));
    assert_eq!(source.mount_error(), None);
}

#[test]
fn checkpoint_failure_records_an_error_and_retains_the_retry_owner() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        Arc::clone(&ring),
    );
    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation(),
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    let data = data_request();
    let fsync = fsync_request();
    assert!(matches!(
        runtime.plan_data(&data),
        BackendPlan::SubmitGraph(_)
    ));
    runtime.complete_data(BackendPageCompletion::new(
        data.object,
        data.id,
        data.op,
        PageIoResult::Done,
    ));
    assert!(matches!(
        source.plan_fsync(&fsync),
        BackendPlan::SubmitGraph(_)
    ));
    source.complete_fsync(BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        PageIoResult::Done,
    ));
    assert!(source.take_checkpoint_graph().unwrap().is_some());

    source
        .complete_checkpoint_result(Err(tx_subsystems::execution::Errno::EIO))
        .unwrap();

    assert_eq!(
        source.mount_error(),
        Some(tx_subsystems::execution::Errno::EIO)
    );
    assert_eq!(ring.reserve(1), Err(JournalRingError::Busy));
    assert!(source.take_checkpoint_graph().unwrap().is_some());
}

#[test]
fn successful_checkpoint_notifies_the_mount_cache_settlement_observer() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let observer = Arc::new(SettlementSpy::new(Arc::clone(&ring)));
    source.bind_settlement_observer(observer.clone());
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        ring,
    );
    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation(),
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    let data = data_request();
    let fsync = fsync_request();
    assert!(matches!(
        runtime.plan_data(&data),
        BackendPlan::SubmitGraph(_)
    ));
    runtime.complete_data(BackendPageCompletion::new(
        data.object,
        data.id,
        data.op,
        PageIoResult::Done,
    ));
    assert!(matches!(
        source.plan_fsync(&fsync),
        BackendPlan::SubmitGraph(_)
    ));
    source.complete_fsync(BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        PageIoResult::Done,
    ));
    assert!(source.take_checkpoint_graph().unwrap().is_some());

    assert_eq!(observer.calls.load(Ordering::Acquire), 0);
    source.complete_checkpoint().unwrap();
    assert_eq!(observer.calls.load(Ordering::Acquire), 1);
}

#[test]
fn allocator_cannot_reuse_before_tail_reclaim() {
    setup();
    let source = Arc::new(JournalFsyncSource::new());
    let ring = ring();
    let observer = Arc::new(DeferredFreeSettlementSpy {
        source: Arc::downgrade(&source),
        physical_block: 33,
    });
    source.bind_settlement_observer(observer);
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(5).unwrap(),
        ring,
    );
    let guard = tx_substrate::epoch::guard();
    let mut freeing_mutation = mutation();
    freeing_mutation.defer_free(33);
    runtime
        .begin_mutation_with_data_sources(
            &freeing_mutation,
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    assert_eq!(
        source.try_reuse_for_test(33),
        Err(tx_subsystems::execution::Errno::EBUSY)
    );

    let data = data_request();
    let fsync = fsync_request();
    assert!(matches!(
        runtime.plan_data(&data),
        BackendPlan::SubmitGraph(_)
    ));
    runtime.complete_data(BackendPageCompletion::new(
        data.object,
        data.id,
        data.op,
        PageIoResult::Done,
    ));
    assert!(matches!(
        source.plan_fsync(&fsync),
        BackendPlan::SubmitGraph(_)
    ));
    source.complete_fsync(BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        PageIoResult::Done,
    ));
    assert!(source.take_checkpoint_graph().unwrap().is_some());
    source.complete_checkpoint().unwrap();

    assert_eq!(source.try_reuse_for_test(33), Ok(()));
}
