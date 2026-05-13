//! Tests for `page_backed::read_exact_at` (cross-doc edit B1).

use super::*;
use crate::execution::Errno;
use alloc::vec;
use alloc::vec::Vec;
use crate::page_backed::adapter::step_engine::{self as step_engine, StepOutcome as V3StepOutcome};

fn setup_host_substrate() {
    tx_test_support::init_host();
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for read_exact_at tests: {error:?}"),
    }
}

/// Seed `pc` with the bytes of `bytes` starting at offset 0 by
/// materialising each anon page and writing through the kernel
/// direct-map view. Equivalent to the previous
/// `step_write_from_user::<PassthroughHal>` seed but without going
/// through the user-VA path — `read_exact_at`'s contract is
/// kernel-buffer-only, so seeding via the kernel direct-map directly
/// is the natural shape now that `UserAccessIf` is retired.
fn seed_anon_pc_with_bytes(pc: &PageContainer, bytes: &[u8]) {
    let mut written = 0usize;
    while written < bytes.len() {
        let page_index = PageIndex::new((written / crate::vm::USER_PAGE_SIZE) as u64);
        let within = written % crate::vm::USER_PAGE_SIZE;
        let chunk = core::cmp::min(bytes.len() - written, crate::vm::USER_PAGE_SIZE - within);
        let page = pc
            .materialize_anon(page_index, MaterializeAccess::Write)
            .expect("materialise anon page for seeding");
        let frame_base = page_allocator::frame_kernel_addr(page.ppn)
            .expect("kernel direct-map for materialised page");
        // SAFETY: frame_base is a valid kernel direct-map pointer for
        // USER_PAGE_SIZE bytes; within + chunk <= USER_PAGE_SIZE; the
        // source slice has at least `chunk` bytes remaining.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(written),
                frame_base.add(within),
                chunk,
            );
        }
        written += chunk;
    }
    // Match the post-condition `step_write_from_user` provided: grow
    // the visible size to the seeded length so EOF / size_bytes
    // semantics in subsequent reads are unchanged.
    pc.grow_size_to(bytes.len() as u64);
}

#[test]
fn read_exact_at_within_single_page_returns_bytes() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );

    let payload: Vec<u8> = (0u8..200).collect();
    seed_anon_pc_with_bytes(&pc, &payload);

    let mut out = vec![0u8; 64];
    let outcome = read_exact_at(&pc, 16, &mut out, &guard);

    assert_eq!(outcome, V3StepOutcome::Done(()));
    assert_eq!(out.as_slice(), &payload[16..16 + 64]);
}

#[test]
fn read_exact_at_across_page_boundary_returns_full_buffer() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        3,
    );

    // 5000 bytes seeded at offset 1024 — guaranteed to span two pages.
    let total = crate::vm::USER_PAGE_SIZE * 2;
    let payload: Vec<u8> = (0..total).map(|i| (i & 0xff) as u8).collect();
    seed_anon_pc_with_bytes(&pc, &payload);

    let mut out = vec![0u8; 5000];
    let outcome = read_exact_at(&pc, 1024, &mut out, &guard);

    assert_eq!(outcome, V3StepOutcome::Done(()));
    assert_eq!(out.as_slice(), &payload[1024..1024 + 5000]);
}

#[test]
fn read_exact_at_short_read_returns_err() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );

    // PageContainer::new initialises `size_bytes` to the full byte
    // capacity. Shrink it down to 120 bytes so the read past offset 100
    // hits EOF before fill.
    assert_eq!(
        crate::page_backed::step_truncate(&pc, 120, &guard),
        V3StepOutcome::Done(())
    );
    let payload: Vec<u8> = (0u8..120).collect();
    seed_anon_pc_with_bytes(&pc, &payload);
    // seed_anon_pc_with_bytes grew size_bytes to 120 — confirm.
    assert_eq!(pc.size_bytes(), 120);

    // Request 64 bytes starting at offset 100; size_bytes = 120, so the
    // last 44 bytes are past EOF — short read.
    let mut out = vec![0u8; 64];
    let outcome = read_exact_at(&pc, 100, &mut out, &guard);

    assert_eq!(outcome, V3StepOutcome::Err(Errno::ENOEXEC.into()));
}

#[test]
fn read_exact_at_zero_length_is_done_noop() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );

    let mut out: [u8; 0] = [];
    assert_eq!(
        read_exact_at(&pc, 0, &mut out, &guard),
        V3StepOutcome::Done(())
    );
}
