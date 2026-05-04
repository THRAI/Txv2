use super::*;
use crate::execution::{Errno, StepOutcome};
use crate::vfs::{FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking};
use alloc::vec;
use alloc::vec::Vec;
use tx_hal::{FaultInfo, KernelPtr, UserAccessIf, UserPtr, VirtAddr};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked user-buffer tests: {error:?}"),
    }
}

fn open_file_for_pc(pc: &PageContainer) -> OpenFile {
    let pc = PageContainer::new_cap(pc.kind().clone(), pc.page_count())
        .expect("page container cap for open file");
    let rnode = RNode::new_cap(
        FsObjectId::new(900),
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
        },
    )
}

/// Test `UserAccessIf` whose user-space pointer is just a host pointer into
/// a `Vec<u8>` allocated by the test. Performs a plain
/// `core::ptr::copy_nonoverlapping` between kernel and user sides; never
/// reports a fault.
struct PassthroughHal;

impl UserAccessIf for PassthroughHal {
    unsafe fn copy_from_user(
        dst: KernelPtr<u8>,
        src: UserPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo> {
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
    ) -> Result<(), FaultInfo> {
        if len == 0 {
            return Ok(());
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst.addr() as *mut u8, len);
        }
        Ok(())
    }
}

/// Test `UserAccessIf` that always reports a fault. Used to prove EFAULT
/// propagation through `step_*_user`.
struct FaultingHal;

impl UserAccessIf for FaultingHal {
    unsafe fn copy_from_user(
        _dst: KernelPtr<u8>,
        src: UserPtr<u8>,
        _len: usize,
    ) -> Result<(), FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(src.addr()),
            write: false,
            instruction: false,
            from_user: true,
        })
    }

    unsafe fn copy_to_user(
        dst: UserPtr<u8>,
        _src: KernelPtr<u8>,
        _len: usize,
    ) -> Result<(), FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(dst.addr()),
            write: true,
            instruction: false,
            from_user: true,
        })
    }
}

#[test]
fn pagebacked_round_trip_through_user_buffer_preserves_bytes_in_one_page() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );

    let payload: Vec<u8> = (0u8..200).collect();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user::<PassthroughHal>(
        &pc,
        &mut writer,
        UserPtr::<u8>::new(payload.as_ptr() as usize),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert_eq!(writer.offset(), payload.len() as u64);

    let mut reader = open_file_for_pc(&pc);
    let mut received = vec![0u8; payload.len()];
    let outcome = step_read_to_user::<PassthroughHal>(
        &pc,
        &mut reader,
        UserPtr::<u8>::new(received.as_mut_ptr() as usize),
        received.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert_eq!(reader.offset(), payload.len() as u64);
    assert_eq!(received, payload);
}

#[test]
fn pagebacked_round_trip_through_user_buffer_crosses_page_boundary() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );

    let payload: Vec<u8> = (0..(crate::vm::USER_PAGE_SIZE + 23))
        .map(|i| (i & 0xff) as u8)
        .collect();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user::<PassthroughHal>(
        &pc,
        &mut writer,
        UserPtr::<u8>::new(payload.as_ptr() as usize),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);

    let mut reader = open_file_for_pc(&pc);
    let mut received = vec![0u8; payload.len()];
    let outcome = step_read_to_user::<PassthroughHal>(
        &pc,
        &mut reader,
        UserPtr::<u8>::new(received.as_mut_ptr() as usize),
        received.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert_eq!(received, payload);
}

#[test]
fn pagebacked_step_read_to_user_grows_offset_only_after_copy_progress() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let payload: Vec<u8> = (0u8..32).collect();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user::<PassthroughHal>(
        &pc,
        &mut writer,
        UserPtr::<u8>::new(payload.as_ptr() as usize),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));

    let mut reader = open_file_for_pc(&pc);
    let mut sink = vec![0u8; 16];
    let outcome = step_read_to_user::<FaultingHal>(
        &pc,
        &mut reader,
        UserPtr::<u8>::new(sink.as_mut_ptr() as usize),
        sink.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Err(Errno::EFAULT));
    assert_eq!(reader.offset(), 0);
}

#[test]
fn pagebacked_step_write_from_user_propagates_efault_without_advance() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let scratch: Vec<u8> = vec![0xab; 8];
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user::<FaultingHal>(
        &pc,
        &mut writer,
        UserPtr::<u8>::new(scratch.as_ptr() as usize),
        scratch.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Err(Errno::EFAULT));
    assert_eq!(writer.offset(), 0);
    assert_eq!(
        pc.size_bytes(),
        pc.page_count() * crate::vm::USER_PAGE_SIZE as u64
    );
}
