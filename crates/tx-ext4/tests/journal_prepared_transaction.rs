use std::sync::Arc;
use tx_ext4::journal::{
    JournalMutationRuntime, JournalPagePool, JournalRecordLayout, MutationJournalImage,
    MutationJournalLayout, PreparedJournalTransaction,
};
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
