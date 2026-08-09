use std::sync::Arc;
use tx_ext4::journal::{
    Ext4MutationPlanSource, JournalFsyncSource, JournalMutationRuntime, JournalMutationWriteSource,
    JournalPagePool, JournalRecordLayout, JournalRing, JournalRingError, MutationJournalImage,
    MutationJournalLayout, PreparedJournalTransaction,
};
use tx_ext4::planner::{Ext4FsyncPlanSource, Ext4WritePlanSource};
use tx_ext4_format::journal::{
    JBD2_BLOCK_SIZE, Jbd2MetadataUpdate, Jbd2Revoke, Jbd2Superblock, Jbd2TransactionImage,
};
use tx_ext4_format::mutation::{
    Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin, RevokeRecord,
    SealedDataWrite,
};
use tx_ext4_format::pager::JournalGeometry;
use tx_subsystems::fs_iface::{IoDataLeaseId, IoDataSource, PageFrameRef};
use tx_subsystems::io_manager::block::{BioVec, BlockOp, DeviceKey, LbaRange};

fn setup() {
    tx_test_support::init_host();
    tx_subsystems::zones::register_all().expect("kernel zones");
}

#[test]
fn journal_page_pool_reuses_record_page_after_lease_drop() {
    setup();
    let pool = JournalPagePool::new(1).unwrap();
    let guard = tx_substrate::epoch::guard();
    let first = pool.stage(&[0; JBD2_BLOCK_SIZE], &guard).unwrap();
    let page = first.page();
    drop(first);
    let second = pool.stage(&[1; JBD2_BLOCK_SIZE], &guard).unwrap();
    assert_eq!(second.page(), page);
}

#[derive(Clone)]
struct FixedMutationPlan(Ext4MutationPlan);

impl Ext4MutationPlanSource for FixedMutationPlan {
    fn plan_writeback_mutation(
        &self,
        _request: &tx_subsystems::fs_iface::BackendPageRequest,
    ) -> Result<Ext4MutationPlan, tx_subsystems::execution::Errno> {
        Ok(self.0.clone())
    }
}

#[test]
fn prepared_transaction_keeps_all_commit_record_leases() {
    setup();
    let image = Jbd2TransactionImage::encode_legacy(
        7,
        [1; 16],
        vec![Jbd2MetadataUpdate::new(33, [2; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    let pool = JournalPagePool::new(3).unwrap();
    let guard = tx_substrate::epoch::guard();
    let prepared = PreparedJournalTransaction::stage(
        &pool,
        image,
        JournalRecordLayout::new(
            LbaRange::new(80, 8),
            vec![LbaRange::new(88, 8)],
            LbaRange::new(96, 8),
        ),
        DeviceKey::new(9),
        vec![],
        vec![],
        &guard,
    )
    .unwrap();

    assert_eq!(prepared.record_count(), 3);
    assert_eq!(prepared.plan().commit_graph().unwrap().nodes().len(), 6);
}

#[test]
fn prepared_transaction_keeps_every_revoke_page_until_checkpoint_completion() {
    setup();
    let mut mutation = Ext4MutationPlan::new(MutationOrigin::FlushPage, 12, FsyncStamp::new(7));
    mutation
        .push_metadata(MetadataBlock {
            home: 33,
            role: MetaRole::InodeTable,
            before_version: 1,
            after: [2; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    mutation.revokes = (100..)
        .take(Jbd2Revoke::MAX_BLOCKS_PER_PAGE + 1)
        .map(|physical_block| RevokeRecord { physical_block })
        .collect();
    let image = MutationJournalImage::from_plan(
        &mutation,
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(120, 8),
            )
            .with_revokes(vec![LbaRange::new(96, 8), LbaRange::new(104, 8)]),
        ),
    )
    .unwrap();
    assert_eq!(image.image.revokes.len(), 2);

    let pool = JournalPagePool::new(6).unwrap();
    let guard = tx_substrate::epoch::guard();
    let prepared = PreparedJournalTransaction::stage_mutation(&pool, image, &guard).unwrap();
    assert_eq!(prepared.record_count(), 6);

    let source = JournalFsyncSource::new();
    source.begin(prepared).unwrap();
    let data = tx_subsystems::fs_iface::BackendPageRequest::new(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        tx_subsystems::io_manager::page::PageIoRequestId::new(69),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
        tx_subsystems::io_manager::page::PageIoOp::Writeback,
        tx_subsystems::io_manager::page::PageIoFlags::WRITEBACK,
        None,
    );
    assert!(matches!(
        source.plan_data(&data),
        tx_subsystems::fs_iface::BackendPlan::SubmitGraph(_)
    ));
    source.complete_data(tx_subsystems::fs_iface::BackendPageCompletion::new(
        data.object,
        data.id,
        data.op,
        tx_subsystems::io_manager::page::PageIoResult::Done,
    ));
    let fsync = tx_subsystems::fs_iface::BackendPageRequest::new(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        tx_subsystems::io_manager::page::PageIoRequestId::new(70),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
        tx_subsystems::io_manager::page::PageIoOp::Fsync,
        tx_subsystems::io_manager::page::PageIoFlags::BARRIER,
        None,
    );
    assert!(matches!(
        source.plan_fsync(&fsync),
        tx_subsystems::fs_iface::BackendPlan::SubmitGraph(_)
    ));
    source.complete_fsync(tx_subsystems::fs_iface::BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        tx_subsystems::io_manager::page::PageIoResult::Done,
    ));
    assert!(source.take_checkpoint_graph().unwrap().is_some());
    assert!(pool.stage(&[0; JBD2_BLOCK_SIZE], &guard).is_err());

    source.complete_checkpoint_result(Ok(())).unwrap();
    assert!(pool.stage(&[0; JBD2_BLOCK_SIZE], &guard).is_ok());
}

#[test]
fn prepared_transaction_stages_mutation_data_journal_and_checkpoint_leases() {
    setup();
    let mut mutation = Ext4MutationPlan::new(MutationOrigin::FlushPage, 12, FsyncStamp::new(7));
    mutation.data.push(SealedDataWrite {
        logical_page: 3,
        physical_block: 7,
        bytes: [0xD3; JBD2_BLOCK_SIZE],
    });
    mutation
        .push_metadata(MetadataBlock {
            home: 33,
            role: MetaRole::InodeTable,
            before_version: 1,
            after: [0xC3; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    let image = MutationJournalImage::from_plan(
        &mutation,
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(96, 8),
            ),
        ),
    )
    .unwrap();
    let pool = JournalPagePool::new(5).unwrap();
    let guard = tx_substrate::epoch::guard();

    let prepared = PreparedJournalTransaction::stage_mutation(&pool, image, &guard).unwrap();

    assert_eq!(prepared.record_count(), 5);
    let commit = prepared.plan().commit_graph().unwrap();
    assert_eq!(commit.nodes().len(), 7);
    assert_eq!(commit.nodes()[0].bio.lba, LbaRange::new(56, 8));
    assert_eq!(
        commit.nodes()[1].bio.op,
        tx_subsystems::io_manager::block::BlockOp::Barrier
    );
    let checkpoint = prepared
        .plan()
        .checkpoint_graph_after_commit()
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.nodes().len(), 2);
    assert_eq!(checkpoint.nodes()[0].bio.lba, LbaRange::new(264, 8));
    assert_eq!(
        checkpoint.nodes()[1].bio.op,
        tx_subsystems::io_manager::block::BlockOp::Barrier
    );
}

#[test]
fn mutation_runtime_stages_plan_into_its_fsync_source() {
    setup();
    let source = Arc::new(tx_ext4::journal::JournalFsyncSource::new());
    let runtime = JournalMutationRuntime::new(
        Arc::clone(&source),
        JournalPagePool::new(5).unwrap(),
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(96, 8),
            ),
        ),
    );
    let mut mutation = Ext4MutationPlan::new(MutationOrigin::FlushPage, 12, FsyncStamp::new(7));
    mutation.data.push(SealedDataWrite {
        logical_page: 0,
        physical_block: 7,
        bytes: [0xD3; JBD2_BLOCK_SIZE],
    });
    mutation
        .push_metadata(MetadataBlock {
            home: 33,
            role: MetaRole::InodeTable,
            before_version: 1,
            after: [0xC3; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    let guard = tx_substrate::epoch::guard();

    runtime.begin_mutation(&mutation, &guard).unwrap();
    assert!(source.take_checkpoint_graph().is_err());
}

#[test]
fn prepared_transaction_uses_l4_owned_data_source_without_copying_it() {
    setup();
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
            after: [0xC3; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    let image = MutationJournalImage::from_plan(
        &mutation,
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(96, 8),
            ),
        ),
    )
    .unwrap();
    let source = IoDataSource::page_cache(
        IoDataLeaseId::new(77),
        PageFrameRef::new(tx_hal::Ppn(0x123)),
        0,
        JBD2_BLOCK_SIZE as u32,
    );
    let pool = JournalPagePool::new(4).unwrap();
    let guard = tx_substrate::epoch::guard();

    let prepared = PreparedJournalTransaction::stage_mutation_with_data_sources(
        &pool,
        image,
        vec![source.clone()],
        &guard,
    )
    .unwrap();
    let graph = prepared.plan().commit_graph().unwrap();
    assert_eq!(graph.nodes()[0].source, source);
    assert_eq!(graph.nodes()[0].bio.vecs[0].buffer_key, 0x123);
}

#[test]
fn mutation_runtime_accepts_l4_owned_data_source() {
    setup();
    let source = Arc::new(tx_ext4::journal::JournalFsyncSource::new());
    let runtime = JournalMutationRuntime::new(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(96, 8),
            ),
        ),
    );
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
            after: [0xC3; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    let data = IoDataSource::page_cache(
        IoDataLeaseId::new(77),
        PageFrameRef::new(tx_hal::Ppn(0x123)),
        0,
        JBD2_BLOCK_SIZE as u32,
    );
    let guard = tx_substrate::epoch::guard();

    runtime
        .begin_mutation_with_data_sources(&mutation, vec![data], &guard)
        .unwrap();
    assert!(source.take_checkpoint_graph().is_err());
}

#[test]
fn journal_source_commits_only_after_data_graph_completion() {
    setup();
    let source = Arc::new(tx_ext4::journal::JournalFsyncSource::new());
    let ring = Arc::new(
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
                blocks: vec![9, 10, 11, 12, 13, 14, 15, 16],
                superblock_page: None,
            },
        )
        .unwrap(),
    );
    let runtime = JournalMutationRuntime::with_ring(
        Arc::clone(&source),
        JournalPagePool::new(4).unwrap(),
        Arc::clone(&ring),
    );
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
    let guard = tx_substrate::epoch::guard();
    runtime
        .begin_mutation_with_data_sources(
            &mutation,
            vec![IoDataSource::page_cache(
                IoDataLeaseId::new(77),
                PageFrameRef::new(tx_hal::Ppn(0x123)),
                0,
                JBD2_BLOCK_SIZE as u32,
            )],
            &guard,
        )
        .unwrap();
    let data = tx_subsystems::fs_iface::BackendPageRequest::new_with_source_and_target(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        tx_subsystems::io_manager::page::PageIoRequestId::new(70),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
        tx_subsystems::io_manager::page::PageIoOp::Writeback,
        tx_subsystems::io_manager::page::PageIoFlags::WRITEBACK,
        Some(tx_subsystems::io_manager::page::PageGeneration::new(7)),
        IoDataSource::None,
        tx_subsystems::fs_iface::IoDataTarget::None,
    );
    assert!(matches!(
        runtime.plan_data(&data),
        tx_subsystems::fs_iface::BackendPlan::SubmitGraph(_)
    ));
    let fsync = tx_subsystems::fs_iface::BackendPageRequest::new(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        tx_subsystems::io_manager::page::PageIoRequestId::new(71),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
        tx_subsystems::io_manager::page::PageIoOp::Fsync,
        tx_subsystems::io_manager::page::PageIoFlags::BARRIER,
        None,
    );
    assert!(matches!(
        source.plan_fsync(&fsync),
        tx_subsystems::fs_iface::BackendPlan::Err(_)
    ));
    runtime.complete_data(tx_subsystems::fs_iface::BackendPageCompletion::new(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        data.id,
        data.op,
        tx_subsystems::io_manager::page::PageIoResult::Done,
    ));
    let tx_subsystems::fs_iface::BackendPlan::SubmitGraph(commit) = source.plan_fsync(&fsync)
    else {
        panic!("data durable must admit commit");
    };
    assert_eq!(commit.nodes().len(), 5);

    source.complete_fsync(tx_subsystems::fs_iface::BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        tx_subsystems::io_manager::page::PageIoResult::Done,
    ));
    assert!(
        source
            .take_checkpoint_graph()
            .expect("durable commit exposes checkpoint")
            .is_some()
    );
    assert!(source.take_checkpoint_graph().is_err());
    source
        .complete_checkpoint_result(Err(tx_subsystems::execution::Errno::EIO))
        .expect("failed checkpoint remains retryable");
    assert_eq!(ring.reserve(1), Err(JournalRingError::Busy));
    assert!(
        source
            .take_checkpoint_graph()
            .expect("failed checkpoint retries")
            .is_some()
    );
    source
        .complete_checkpoint_result(Ok(()))
        .expect("successful retry releases ring reservation");
    assert!(ring.reserve(1).is_ok());
}

#[test]
fn mutation_write_source_stages_l4_data_during_guarded_admission() {
    setup();
    let fsync = Arc::new(tx_ext4::journal::JournalFsyncSource::new());
    let runtime = Arc::new(JournalMutationRuntime::new(
        Arc::clone(&fsync),
        JournalPagePool::new(4).unwrap(),
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(96, 8),
            ),
        ),
    ));
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
            after: [0xC3; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    let writeback = JournalMutationWriteSource::new(FixedMutationPlan(mutation), runtime);
    let request = tx_subsystems::fs_iface::BackendPageRequest::new_with_source(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        tx_subsystems::io_manager::page::PageIoRequestId::new(70),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
        tx_subsystems::io_manager::page::PageIoOp::Writeback,
        tx_subsystems::io_manager::page::PageIoFlags::WRITEBACK,
        Some(tx_subsystems::io_manager::page::PageGeneration::new(7)),
        IoDataSource::page_cache(
            IoDataLeaseId::new(77),
            PageFrameRef::new(tx_hal::Ppn(0x123)),
            0,
            JBD2_BLOCK_SIZE as u32,
        ),
    );
    let guard = tx_substrate::epoch::guard();

    writeback.prepare_writeback(&request, &guard).unwrap();
    let tx_subsystems::fs_iface::BackendPlan::SubmitGraph(data) = writeback.plan_writeback(
        tx_ext4::planner::Ext4BlockGeometry {
            device: DeviceKey::new(9),
            sectors_per_block: 8,
        },
        &request,
        tx_ext4::planner::Ext4ReadMapping::Hole,
    ) else {
        panic!("prepared mutation must submit ordered data");
    };
    assert_eq!(data.nodes()[0].source, request.source);
    assert_eq!(data.nodes()[0].bio.lba, LbaRange::new(56, 8));

    writeback.complete_writeback(tx_subsystems::fs_iface::BackendPageCompletion::new(
        request.object,
        request.id,
        request.op,
        tx_subsystems::io_manager::page::PageIoResult::Done,
    ));
    let fsync_request = tx_subsystems::fs_iface::BackendPageRequest::new(
        request.object,
        tx_subsystems::io_manager::page::PageIoRequestId::new(71),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
        tx_subsystems::io_manager::page::PageIoOp::Fsync,
        tx_subsystems::io_manager::page::PageIoFlags::BARRIER,
        None,
    );
    assert!(matches!(
        fsync.plan_fsync(&fsync_request),
        tx_subsystems::fs_iface::BackendPlan::SubmitGraph(_)
    ));
}

#[test]
fn mutation_write_source_splits_multi_page_direct_sources_per_data_write() {
    setup();
    let fsync = Arc::new(tx_ext4::journal::JournalFsyncSource::new());
    let runtime = Arc::new(JournalMutationRuntime::new(
        Arc::clone(&fsync),
        JournalPagePool::new(4).unwrap(),
        MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [1; 16],
            8,
            JournalRecordLayout::new(
                LbaRange::new(80, 8),
                vec![LbaRange::new(88, 8)],
                LbaRange::new(96, 8),
            ),
        ),
    ));
    let mut mutation = Ext4MutationPlan::new(MutationOrigin::FlushPage, 12, FsyncStamp::new(8));
    mutation.data.push(SealedDataWrite {
        logical_page: 0,
        physical_block: 7,
        bytes: [0; JBD2_BLOCK_SIZE],
    });
    mutation.data.push(SealedDataWrite {
        logical_page: 1,
        physical_block: 8,
        bytes: [0; JBD2_BLOCK_SIZE],
    });
    mutation
        .push_metadata(MetadataBlock {
            home: 33,
            role: MetaRole::InodeTable,
            before_version: 1,
            after: [0xC4; JBD2_BLOCK_SIZE],
            depends_on: Vec::new(),
        })
        .unwrap();
    let writeback = JournalMutationWriteSource::new(FixedMutationPlan(mutation), runtime);
    let source_vecs = vec![
        BioVec::new(0x401, 0, JBD2_BLOCK_SIZE as u32),
        BioVec::new(0x402, 0, JBD2_BLOCK_SIZE as u32),
    ];
    let request = tx_subsystems::fs_iface::BackendPageRequest::new_with_source(
        tx_subsystems::fs_iface::FsObjectKey::new(12),
        tx_subsystems::io_manager::page::PageIoRequestId::new(72),
        tx_subsystems::io_manager::page::PageIoRange::new(0, 2),
        tx_subsystems::io_manager::page::PageIoOp::Writeback,
        tx_subsystems::io_manager::page::PageIoFlags::WRITEBACK,
        Some(tx_subsystems::io_manager::page::PageGeneration::new(8)),
        IoDataSource::direct(IoDataLeaseId::new(88), source_vecs.clone()),
    );
    let guard = tx_substrate::epoch::guard();

    writeback.prepare_writeback(&request, &guard).unwrap();
    let tx_subsystems::fs_iface::BackendPlan::SubmitGraph(data) = writeback.plan_writeback(
        tx_ext4::planner::Ext4BlockGeometry {
            device: DeviceKey::new(9),
            sectors_per_block: 8,
        },
        &request,
        tx_ext4::planner::Ext4ReadMapping::Hole,
    ) else {
        panic!("prepared mutation must submit ordered data");
    };

    assert_eq!(data.nodes().len(), 3);
    assert_eq!(data.nodes()[0].bio.lba, LbaRange::new(56, 8));
    assert_eq!(data.nodes()[1].bio.lba, LbaRange::new(64, 8));
    assert_eq!(data.nodes()[2].bio.op, BlockOp::Barrier);
    assert_eq!(data.nodes()[2].source, IoDataSource::None);
    assert_eq!(
        data.nodes()[0].source,
        IoDataSource::direct(IoDataLeaseId::new(88), vec![source_vecs[0]])
    );
    assert_eq!(
        data.nodes()[1].source,
        IoDataSource::direct(IoDataLeaseId::new(88), vec![source_vecs[1]])
    );
}
