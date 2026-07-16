use tx_ext4::journal::{JournalBio, JournalTransactionPlan, JournalTransactionPlanError};
use tx_subsystems::fs_iface::{BackendBioDependency, BackendBioNodeId, IoDataSource};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};

fn write(device: DeviceKey, lba: u64, buffer: u64) -> JournalBio {
    JournalBio::new(
        BioPlan::new(
            device,
            BlockOp::Write,
            LbaRange::new(lba, 8),
            vec![BioVec::new(buffer, 0, 4096)],
            BlockFlags::EMPTY,
        ),
        IoDataSource::None,
    )
}

#[test]
fn commit_graph_orders_data_journal_fences_and_durable_commit() {
    let device = DeviceKey::new(9);
    let plan = JournalTransactionPlan::new(
        41,
        vec![write(device, 100, 1)],
        write(device, 200, 2),
        vec![write(device, 208, 3)],
        write(device, 216, 4),
        vec![write(device, 300, 5)],
    )
    .unwrap();

    let graph = plan.commit_graph().unwrap();
    let nodes = graph.nodes();

    assert_eq!(nodes.len(), 6);
    assert_eq!(nodes[0].bio.lba, LbaRange::new(100, 8));
    assert_eq!(nodes[1].bio.op, BlockOp::Barrier);
    assert_eq!(nodes[2].bio.lba, LbaRange::new(200, 8));
    assert_eq!(nodes[3].bio.lba, LbaRange::new(208, 8));
    assert_eq!(nodes[4].bio.op, BlockOp::Barrier);
    assert!(nodes[5].bio.flags.contains(BlockFlags::FUA));
    assert_eq!(nodes[5].bio.lba, LbaRange::new(216, 8));

    assert_eq!(
        graph.dependencies(),
        [
            BackendBioDependency::new(BackendBioNodeId::new(1), BackendBioNodeId::new(2)),
            BackendBioDependency::new(BackendBioNodeId::new(2), BackendBioNodeId::new(3)),
            BackendBioDependency::new(BackendBioNodeId::new(2), BackendBioNodeId::new(4)),
            BackendBioDependency::new(BackendBioNodeId::new(3), BackendBioNodeId::new(5)),
            BackendBioDependency::new(BackendBioNodeId::new(4), BackendBioNodeId::new(5)),
            BackendBioDependency::new(BackendBioNodeId::new(5), BackendBioNodeId::new(6)),
        ]
    );

    let checkpoint = plan.checkpoint_graph_after_commit().unwrap().unwrap();
    assert_eq!(checkpoint.nodes().len(), 2);
    assert_eq!(checkpoint.nodes()[0].bio.lba, LbaRange::new(300, 8));
    assert_eq!(checkpoint.nodes()[1].bio.op, BlockOp::Barrier);
    assert_eq!(
        checkpoint.dependencies(),
        [BackendBioDependency::new(
            BackendBioNodeId::new(1),
            BackendBioNodeId::new(2)
        )]
    );
}

#[test]
fn transaction_plan_rejects_cross_device_journal_writes() {
    let error = JournalTransactionPlan::new(
        1,
        vec![write(DeviceKey::new(9), 100, 1)],
        write(DeviceKey::new(10), 200, 2),
        vec![write(DeviceKey::new(10), 208, 3)],
        write(DeviceKey::new(10), 216, 4),
        vec![],
    )
    .unwrap_err();

    assert_eq!(error, JournalTransactionPlanError::CrossDevice);
}

#[test]
fn split_graphs_fence_data_before_journal_commit() {
    let device = DeviceKey::new(9);
    let plan = JournalTransactionPlan::new(
        41,
        vec![write(device, 100, 1)],
        write(device, 200, 2),
        vec![write(device, 208, 3)],
        write(device, 216, 4),
        vec![],
    )
    .unwrap();

    let data = plan.data_graph().unwrap();
    assert_eq!(data.nodes().len(), 2);
    assert_eq!(data.nodes()[0].bio.lba, LbaRange::new(100, 8));
    assert_eq!(data.nodes()[1].bio.op, BlockOp::Barrier);

    let commit = plan.commit_graph_after_data().unwrap();
    assert_eq!(commit.nodes().len(), 4);
    assert_eq!(commit.nodes()[0].bio.lba, LbaRange::new(200, 8));
    assert_eq!(commit.nodes()[1].bio.lba, LbaRange::new(208, 8));
    assert_eq!(commit.nodes()[2].bio.op, BlockOp::Barrier);
    assert!(commit.nodes()[3].bio.flags.contains(BlockFlags::FUA));
}
