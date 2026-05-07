//! Tests for `page_backed::read_exact_at` (cross-doc edit B1).

use super::*;
use crate::execution::{Errno, StepOutcome};
use crate::vfs::{FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking};
use alloc::vec;
use alloc::vec::Vec;
use tx_hal::{KernelPtr, UserAccessIf, UserPtr};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for read_exact_at tests: {error:?}"),
    }
}

fn open_file_for_pc(pc: &PageContainer) -> OpenFile {
    let pc = PageContainer::new_cap(pc.kind().clone(), pc.page_count())
        .expect("page container cap for open file");
    let rnode = RNode::new_cap(
        FsObjectId::new(901),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("rnode cap");
    OpenFile::new(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    )
}

/// `UserAccessIf` whose user-space pointer is just a host pointer into a
/// `Vec<u8>`. Used to seed an Anon `PageContainer` with known bytes via
/// `step_write_from_user` before reading them back through
/// `read_exact_at`.
struct PassthroughHal;

impl UserAccessIf for PassthroughHal {
    unsafe fn copy_from_user(
        dst: KernelPtr<u8>,
        src: UserPtr<u8>,
        len: usize,
    ) -> Result<(), tx_hal::FaultInfo> {
        if len == 0 {
            return Ok(());
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.addr() as *const u8, dst.as_ptr(), len);
        }
        Ok(())
    }

    unsafe fn copy_to_user(
        dst: UserPtr<u8>,
        src: KernelPtr<u8>,
        len: usize,
    ) -> Result<(), tx_hal::FaultInfo> {
        if len == 0 {
            return Ok(());
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst.addr() as *mut u8, len);
        }
        Ok(())
    }
}

/// Seed `pc` with the bytes of `bytes` starting at offset 0 via
/// `step_write_from_user::<PassthroughHal>` and grow `pc.size_bytes` to
/// match. Returns once the write has fully advanced.
fn seed_anon_pc_with_bytes(pc: &PageContainer, bytes: &[u8], guard: &Guard<'_>) {
    let mut writer = open_file_for_pc(pc);
    let outcome = step_write_from_user::<PassthroughHal>(
        pc,
        &mut writer,
        UserPtr::<u8>::new(bytes.as_ptr() as usize),
        bytes.len(),
        guard,
    );
    assert_eq!(outcome, StepOutcome::Done(bytes.len()));
}

#[test]
fn read_exact_at_within_single_page_returns_bytes() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );

    let payload: Vec<u8> = (0u8..200).collect();
    seed_anon_pc_with_bytes(&pc, &payload, &guard);

    let mut out = vec![0u8; 64];
    let outcome = read_exact_at(&pc, 16, &mut out, &guard);

    assert_eq!(outcome, StepOutcome::Done(()));
    assert_eq!(out.as_slice(), &payload[16..16 + 64]);
}

#[test]
fn read_exact_at_across_page_boundary_returns_full_buffer() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        3,
    );

    // 5000 bytes seeded at offset 1024 — guaranteed to span two pages.
    let total = crate::vm::USER_PAGE_SIZE * 2;
    let payload: Vec<u8> = (0..total).map(|i| (i & 0xff) as u8).collect();
    seed_anon_pc_with_bytes(&pc, &payload, &guard);

    let mut out = vec![0u8; 5000];
    let outcome = read_exact_at(&pc, 1024, &mut out, &guard);

    assert_eq!(outcome, StepOutcome::Done(()));
    assert_eq!(out.as_slice(), &payload[1024..1024 + 5000]);
}

#[test]
fn read_exact_at_short_read_returns_err() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
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
        StepOutcome::Done(())
    );
    let payload: Vec<u8> = (0u8..120).collect();
    seed_anon_pc_with_bytes(&pc, &payload, &guard);
    // step_write_from_user grows size_bytes to 120 — confirm.
    assert_eq!(pc.size_bytes(), 120);

    // Request 64 bytes starting at offset 100; size_bytes = 120, so the
    // last 44 bytes are past EOF — short read.
    let mut out = vec![0u8; 64];
    let outcome = read_exact_at(&pc, 100, &mut out, &guard);

    assert_eq!(outcome, StepOutcome::Err(Errno::ENOEXEC));
}

#[test]
fn read_exact_at_zero_length_is_done_noop() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("read_exact_at epoch test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );

    let mut out: [u8; 0] = [];
    assert_eq!(
        read_exact_at(&pc, 0, &mut out, &guard),
        StepOutcome::Done(())
    );
}
