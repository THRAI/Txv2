use std::sync::Arc;
use tx_ext4::journal::{
    Ext4MutationPlanSource, JournalMutationRuntime, JournalMutationWriteSource, JournalPagePool,
    JournalRecordLayout, MutationJournalImage, MutationJournalLayout, PreparedJournalTransaction,
};
use tx_ext4::planner::{Ext4FsyncPlanSource, Ext4WritePlanSource};
use tx_ext4_format::journal::{Jbd2MetadataUpdate, Jbd2TransactionImage, JBD2_BLOCK_SIZE};
use tx_ext4_format::mutation::{
    Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin, SealedDataWrite,
};
use tx_subsystems::fs_iface::{IoDataLeaseId, IoDataSource, PageFrameRef};
use tx_subsystems::io_manager::block::{DeviceKey, LbaRange};

fn setup() {
    tx_test_support::init_host();
    tx_subsystems::zones::register_all().expect("kernel zones");
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
    assert_eq!(prepared.plan().commit_graph().unwrap().nodes().len(), 5);
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
    assert_eq!(commit.nodes().len(), 6);
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
    assert_eq!(checkpoint.nodes().len(), 1);
    assert_eq!(checkpoint.nodes()[0].bio.lba, LbaRange::new(264, 8));
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
    assert_eq!(commit.nodes().len(), 4);

    source.complete_fsync(tx_subsystems::fs_iface::BackendPageCompletion::new(
        fsync.object,
        fsync.id,
        fsync.op,
        tx_subsystems::io_manager::page::PageIoResult::Done,
    ));
    assert!(source
        .take_checkpoint_graph()
        .expect("durable commit exposes checkpoint")
        .is_some());
    assert!(source.take_checkpoint_graph().is_err());
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
