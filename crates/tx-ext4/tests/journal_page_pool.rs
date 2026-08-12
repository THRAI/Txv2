use tx_ext4::journal::{JournalPagePool, JournalPagePoolError};
use tx_subsystems::io_manager::block::{DeviceKey, LbaRange};

fn setup() {
    tx_test_support::init_host();
    tx_subsystems::zones::register_all().expect("kernel zones");
}

#[test]
fn journal_page_pool_retains_encoded_bytes_and_exports_their_ppn() {
    setup();
    let pool = JournalPagePool::new(1).unwrap();
    let guard = tx_substrate::epoch::guard();
    let bytes = [0xA5; 4096];

    let record = pool.stage(&bytes, &guard).unwrap();
    let bio = record.as_journal_bio(DeviceKey::new(9), LbaRange::new(80, 8));
    let address = tx_substrate::page_allocator::frame_kernel_addr(record.ppn()).unwrap();
    let observed = unsafe { core::slice::from_raw_parts(address, bytes.len()) };

    assert_eq!(observed, bytes);
    assert_eq!(bio.plan.vecs[0].buffer_key, record.ppn().0 as u64);
    assert_eq!(bio.plan.lba, LbaRange::new(80, 8));
    assert!(matches!(
        pool.stage(&[0; 4096], &guard),
        Err(JournalPagePoolError::Capacity)
    ));
    let ppn = record.ppn();
    drop(record);

    // A settled transaction can reuse its private journal page immediately,
    // even while the caller remains in the same epoch.
    let second = pool.stage(&[0x22; 4096], &guard).unwrap();
    assert_eq!(second.ppn(), ppn);
    let address = tx_substrate::page_allocator::frame_kernel_addr(second.ppn()).unwrap();
    let observed = unsafe { core::slice::from_raw_parts(address, 4096) };
    assert_eq!(observed, &[0x22; 4096]);
}
