use super::*;
use crate::page_backed::adapter::step_engine::{self as step_engine, StepOutcome as V3Out};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for reflink tests: {error:?}"),
    }
}

fn anon_pc(page_count: u64) -> PageContainer {
    PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    )
}

fn read_frame_bytes(ppn: Ppn, len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec![0u8; len];
    step_engine::page_allocator::testing::read_frame_bytes_for_test(ppn, 0, &mut out);
    out
}

#[test]
fn pagebacked_install_shared_page_attaches_existing_frame_to_new_pc() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed reflink test lock");
    setup_host_substrate();
    let guard = step_engine::guard();

    let source = anon_pc(1);
    let dest = anon_pc(1);

    let materialized =
        match source.materialize_page(PageIndex::new(0), MaterializeAccess::Write, &guard) {
            V3Out::Done(m) => m,
            other => panic!("source materialize: {other:?}"),
        };
    let source_ppn = materialized.ppn;
    drop(materialized);

    let pattern = alloc::vec![0xa7u8; crate::vm::USER_PAGE_SIZE];
    step_engine::page_allocator::testing::write_frame_bytes_for_test(source_ppn, 0, &pattern);

    install_shared_page(&dest, PageIndex::new(0), source_ppn).expect("share page");

    assert_eq!(dest.lookup(PageIndex::new(0)), Some(source_ppn));
    assert_eq!(
        read_frame_bytes(source_ppn, crate::vm::USER_PAGE_SIZE),
        pattern
    );
}

#[test]
fn pagebacked_install_shared_page_rejects_already_present_entry() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed reflink test lock");
    setup_host_substrate();
    let guard = step_engine::guard();

    let source = anon_pc(1);
    let dest = anon_pc(1);

    let source_materialized =
        match source.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
            V3Out::Done(m) => m,
            other => panic!("source materialize: {other:?}"),
        };
    let source_ppn = source_materialized.ppn;
    drop(source_materialized);
    let dest_materialized =
        match dest.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
            V3Out::Done(m) => m,
            other => panic!("dest materialize: {other:?}"),
        };
    drop(dest_materialized);

    let result = install_shared_page(&dest, PageIndex::new(0), source_ppn);
    assert!(matches!(result, Err(PageCacheError::AlreadyPresent { .. })));
}

#[test]
fn pagebacked_install_shared_page_rejects_device_destination() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed reflink test lock");
    setup_host_substrate();
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_5000),
            page_count: 1,
        },
        1,
    );

    let result = install_shared_page(&device, PageIndex::new(0), Ppn(0xface_5000));
    assert_eq!(result, Err(PageCacheError::UnsupportedKind));
}

#[test]
fn pagebacked_cow_replace_into_private_swaps_to_fresh_frame_with_matching_bytes() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed reflink test lock");
    setup_host_substrate();
    let guard = step_engine::guard();

    let source = anon_pc(1);
    let dest = anon_pc(1);

    let source_materialized =
        match source.materialize_page(PageIndex::new(0), MaterializeAccess::Write, &guard) {
            V3Out::Done(m) => m,
            other => panic!("source materialize: {other:?}"),
        };
    let source_ppn = source_materialized.ppn;
    drop(source_materialized);

    let pattern: alloc::vec::Vec<u8> = (0..crate::vm::USER_PAGE_SIZE)
        .map(|i| (i & 0xff) as u8)
        .collect();
    step_engine::page_allocator::testing::write_frame_bytes_for_test(source_ppn, 0, &pattern);

    install_shared_page(&dest, PageIndex::new(0), source_ppn).expect("share page");
    assert_eq!(dest.lookup(PageIndex::new(0)), Some(source_ppn));

    let new_ppn = cow_replace_into_private(&dest, PageIndex::new(0)).expect("cow replace");

    assert_ne!(new_ppn, source_ppn);
    assert_eq!(dest.lookup(PageIndex::new(0)), Some(new_ppn));
    assert_eq!(source.lookup(PageIndex::new(0)), Some(source_ppn));
    assert_eq!(
        read_frame_bytes(new_ppn, crate::vm::USER_PAGE_SIZE),
        pattern
    );
}

#[test]
fn pagebacked_cow_replace_into_private_errors_on_missing_page() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed reflink test lock");
    setup_host_substrate();
    let dest = anon_pc(1);

    let result = cow_replace_into_private(&dest, PageIndex::new(0));
    assert_eq!(result, Err(PageCacheError::MissingPage));
}

#[test]
fn pagebacked_cow_replace_into_private_rejects_device_backing() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed reflink test lock");
    setup_host_substrate();
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_6000),
            page_count: 1,
        },
        1,
    );

    let result = cow_replace_into_private(&device, PageIndex::new(0));
    assert_eq!(result, Err(PageCacheError::UnsupportedKind));
}
