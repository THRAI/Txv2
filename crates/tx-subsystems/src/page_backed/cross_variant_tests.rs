use super::*;
use crate::execution::StepOutcome;
use alloc::vec;
use alloc::vec::Vec;
use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3Out};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for cross-variant tests: {error:?}"),
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

fn write_pattern(pc: &PageContainer, offset: u64, bytes: &[u8], guard: &Guard<'_>) {
    let mut left = bytes;
    let mut cursor = offset;
    while !left.is_empty() {
        let page = PageIndex::new(cursor / crate::vm::USER_PAGE_SIZE as u64);
        let within = (cursor % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(left.len(), crate::vm::USER_PAGE_SIZE - within);
        let materialized = match pc.materialize_page(page, MaterializeAccess::Write, guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
            other => panic!("materialize_page during pattern write: {other:?}"),
        };
        tx_substrate::page_allocator::testing::write_frame_bytes_for_test(
            materialized.ppn,
            within,
            &left[..chunk],
        );
        left = &left[chunk..];
        cursor += chunk as u64;
    }
    pc.grow_size_to(offset + bytes.len() as u64);
}

fn read_pattern(pc: &PageContainer, offset: u64, len: usize, guard: &Guard<'_>) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut left: &mut [u8] = &mut out;
    let mut cursor = offset;
    while !left.is_empty() {
        let page = PageIndex::new(cursor / crate::vm::USER_PAGE_SIZE as u64);
        let within = (cursor % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(left.len(), crate::vm::USER_PAGE_SIZE - within);
        let materialized = match pc.materialize_page(page, MaterializeAccess::Read, guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
            other => panic!("materialize_page during pattern read: {other:?}"),
        };
        tx_substrate::page_allocator::testing::read_frame_bytes_for_test(
            materialized.ppn,
            within,
            &mut left[..chunk],
        );
        let advance = chunk;
        left = &mut left[advance..];
        cursor += advance as u64;
    }
    out
}

#[test]
fn pagebacked_step_copy_file_range_anon_to_anon_within_one_page_each() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed cross-variant test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let src = anon_pc(2);
    let dst = anon_pc(2);
    assert_eq!(step_truncate(&dst, 0, &guard), V3Out::Done(()));

    let payload: Vec<u8> = (0u8..200).collect();
    write_pattern(&src, 0, &payload, &guard);

    let outcome = step_copy_file_range(&src, 0, &dst, 0, payload.len(), &guard);
    assert_eq!(outcome, V3Out::Done(payload.len()));
    assert_eq!(dst.size_bytes(), payload.len() as u64);
    assert_eq!(read_pattern(&dst, 0, payload.len(), &guard), payload);
}

#[test]
fn pagebacked_step_copy_file_range_crosses_page_boundary_at_different_alignments() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed cross-variant test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let src = anon_pc(3);
    let dst = anon_pc(3);

    let payload: Vec<u8> = (0..(crate::vm::USER_PAGE_SIZE + 100))
        .map(|i| ((i & 0xff) | 0x80) as u8)
        .collect();
    write_pattern(&src, 50, &payload, &guard);

    let outcome = step_copy_file_range(&src, 50, &dst, 4096 - 7, payload.len(), &guard);
    assert_eq!(outcome, V3Out::Done(payload.len()));
    assert_eq!(read_pattern(&dst, 4096 - 7, payload.len(), &guard), payload);
}

#[test]
fn pagebacked_step_copy_file_range_truncates_to_source_eof() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed cross-variant test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let src = anon_pc(2);
    let dst = anon_pc(2);

    let payload: Vec<u8> = (0u8..40).collect();
    write_pattern(&src, 0, &payload, &guard);
    assert_eq!(step_truncate(&src, 40, &guard), V3Out::Done(()));

    let outcome = step_copy_file_range(&src, 10, &dst, 0, 1024, &guard);
    assert_eq!(outcome, V3Out::Done(30));
    assert_eq!(read_pattern(&dst, 0, 30, &guard), payload[10..40]);
}

#[test]
fn pagebacked_step_copy_file_range_returns_done_zero_when_source_is_at_eof() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed cross-variant test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let src = anon_pc(1);
    let dst = anon_pc(1);

    assert_eq!(step_truncate(&src, 32, &guard), V3Out::Done(()));

    let outcome = step_copy_file_range(&src, 32, &dst, 0, 100, &guard);
    assert_eq!(outcome, V3Out::Done(0));
    assert_eq!(
        dst.size_bytes(),
        dst.page_count() * crate::vm::USER_PAGE_SIZE as u64
    );
}

#[test]
fn pagebacked_step_copy_file_range_rejects_device_destination() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed cross-variant test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let src = anon_pc(1);
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_4000),
            page_count: 1,
        },
        1,
    );

    let payload = vec![0xa5u8; 16];
    write_pattern(&src, 0, &payload, &guard);

    let outcome = step_copy_file_range(&src, 0, &device, 0, payload.len(), &guard);
    assert_eq!(outcome, V3Out::Err(V3Errno::EINVAL));
}

#[test]
fn pagebacked_step_copy_file_range_rejects_writes_past_destination_capacity() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed cross-variant test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let src = anon_pc(2);
    let dst = anon_pc(1);

    let payload: Vec<u8> = (0u8..200).collect();
    write_pattern(&src, 0, &payload, &guard);

    let outcome = step_copy_file_range(
        &src,
        0,
        &dst,
        crate::vm::USER_PAGE_SIZE as u64 - 50,
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, V3Out::Err(V3Errno::EINVAL));
    assert_eq!(dst.size_bytes(), crate::vm::USER_PAGE_SIZE as u64);
}
