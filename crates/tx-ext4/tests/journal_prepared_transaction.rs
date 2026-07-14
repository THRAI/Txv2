use tx_ext4::journal::{JournalPagePool, JournalRecordLayout, PreparedJournalTransaction};
use tx_ext4_format::journal::{Jbd2MetadataUpdate, Jbd2TransactionImage, JBD2_BLOCK_SIZE};
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
