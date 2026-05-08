use super::*;
use crate::execution::{Errno, StepOutcome};
use crate::vfs::{FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking};
use alloc::vec;
use alloc::vec::Vec;
use tx_hal::{KernelPtr, UserPtr};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    crate::zones::register_all().expect("kernel zones");
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked user-buffer tests: {error:?}"),
    }
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
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

fn map_user_buffer(aspace: &crate::vm::AddressSpace, start: usize, len: usize) {
    let len = align_up(len.max(1), crate::vm::USER_PAGE_SIZE);
    let range = crate::vm::UserRange::new_aligned(crate::vm::UserVirtAddr(start), len)
        .expect("aligned user buffer range");
    let request = crate::vm::VmMapRequest::fixed(
        range,
        crate::vm::MapPlacement::RequireFree,
        crate::vm::Prot::READ_WRITE,
        crate::vm::VmEntryFlags::PRIVATE,
        crate::vm::VmBacking::PrivateAnon,
    );
    aspace.try_mmap(request).expect("map test user buffer");
}

fn populate_user(aspace: &crate::vm::AddressSpace, addr: usize, bytes: &[u8]) {
    let outcome = unsafe {
        crate::vm::copy_to_user(
            aspace,
            UserPtr::new(addr),
            KernelPtr::new(bytes.as_ptr().cast_mut()),
            bytes.len(),
        )
    };
    assert_eq!(outcome, StepOutcome::Done(bytes.len()));
}

fn read_user(aspace: &crate::vm::AddressSpace, addr: usize, dest: &mut [u8]) {
    let outcome = unsafe {
        crate::vm::copy_from_user(
            aspace,
            KernelPtr::new(dest.as_mut_ptr()),
            UserPtr::new(addr),
            dest.len(),
        )
    };
    assert_eq!(outcome, StepOutcome::Done(dest.len()));
}

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[test]
fn pagebacked_round_trip_through_user_buffer_preserves_bytes_in_one_page() {
    let _lock = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let aspace = crate::vm::AddressSpace::new();
    let src_addr = 0x4000;
    let dst_addr = 0x20_000;

    let payload: Vec<u8> = (0u8..200).collect();
    map_user_buffer(&aspace, src_addr, payload.len());
    map_user_buffer(&aspace, dst_addr, payload.len());
    populate_user(&aspace, src_addr, &payload);

    let guard = tx_substrate::epoch::guard();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &aspace,
        &pc,
        &mut writer,
        UserPtr::<u8>::new(src_addr),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert_eq!(writer.offset(), payload.len() as u64);

    let mut reader = open_file_for_pc(&pc);
    let mut received = vec![0u8; payload.len()];
    let outcome = step_read_to_user(
        &aspace,
        &pc,
        &mut reader,
        UserPtr::<u8>::new(dst_addr),
        received.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert_eq!(reader.offset(), payload.len() as u64);
    drop(guard);
    read_user(&aspace, dst_addr, &mut received);
    assert_eq!(received, payload);
}

#[test]
fn pagebacked_round_trip_through_user_buffer_crosses_page_boundary() {
    let _lock = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let aspace = crate::vm::AddressSpace::new();
    let src_addr = 0x40_000;
    let dst_addr = 0x80_000;

    let payload: Vec<u8> = (0..(crate::vm::USER_PAGE_SIZE + 23))
        .map(|i| (i & 0xff) as u8)
        .collect();
    map_user_buffer(&aspace, src_addr, payload.len());
    map_user_buffer(&aspace, dst_addr, payload.len());
    populate_user(&aspace, src_addr, &payload);

    let guard = tx_substrate::epoch::guard();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &aspace,
        &pc,
        &mut writer,
        UserPtr::<u8>::new(src_addr),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);

    let mut reader = open_file_for_pc(&pc);
    let mut received = vec![0u8; payload.len()];
    let outcome = step_read_to_user(
        &aspace,
        &pc,
        &mut reader,
        UserPtr::<u8>::new(dst_addr),
        received.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));
    drop(guard);
    read_user(&aspace, dst_addr, &mut received);
    assert_eq!(received, payload);
}

#[test]
fn pagebacked_step_read_to_user_grows_offset_only_after_copy_progress() {
    let _lock = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let aspace = crate::vm::AddressSpace::new();
    let src_addr = 0x100_000;
    let unmapped_dst = 0x180_000;
    let payload: Vec<u8> = (0u8..32).collect();
    map_user_buffer(&aspace, src_addr, payload.len());
    populate_user(&aspace, src_addr, &payload);

    let guard = tx_substrate::epoch::guard();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &aspace,
        &pc,
        &mut writer,
        UserPtr::<u8>::new(src_addr),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(payload.len()));

    let mut reader = open_file_for_pc(&pc);
    let outcome = step_read_to_user(
        &aspace,
        &pc,
        &mut reader,
        UserPtr::<u8>::new(unmapped_dst),
        16,
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Err(Errno::EFAULT));
    assert_eq!(reader.offset(), 0);
}

#[test]
fn pagebacked_truncate_shrink_then_grow_reads_zeros_for_post_eof_region() {
    let _lock = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let aspace = crate::vm::AddressSpace::new();
    let src_addr = 0x200_000;
    let dst_addr = 0x240_000;

    let pattern: Vec<u8> = (0..(crate::vm::USER_PAGE_SIZE + 32))
        .map(|i| ((i & 0xff) | 0x20) as u8)
        .collect();
    map_user_buffer(&aspace, src_addr, pattern.len());
    map_user_buffer(&aspace, dst_addr, 32);
    populate_user(&aspace, src_addr, &pattern);

    let guard = tx_substrate::epoch::guard();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &aspace,
        &pc,
        &mut writer,
        UserPtr::<u8>::new(src_addr),
        pattern.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(pattern.len()));

    let shrink_size = crate::vm::USER_PAGE_SIZE as u64 + 4;
    assert_eq!(
        step_truncate(&pc, shrink_size, &guard),
        StepOutcome::Done(())
    );

    let grow_size = crate::vm::USER_PAGE_SIZE as u64 + 32;
    assert_eq!(step_truncate(&pc, grow_size, &guard), StepOutcome::Done(()));

    let mut reader = open_file_for_pc(&pc);
    reader.set_offset(crate::vm::USER_PAGE_SIZE as u64);
    let mut received = vec![0xCCu8; 32];
    let outcome = step_read_to_user(
        &aspace,
        &pc,
        &mut reader,
        UserPtr::<u8>::new(dst_addr),
        received.len(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(32));
    drop(guard);
    read_user(&aspace, dst_addr, &mut received);
    assert_eq!(
        &received[..4],
        &pattern[crate::vm::USER_PAGE_SIZE..crate::vm::USER_PAGE_SIZE + 4]
    );
    assert!(
        received[4..].iter().all(|b| *b == 0),
        "post-EOF region must read as zeros after shrink-then-grow, got {:?}",
        &received[4..]
    );
}

#[test]
fn pagebacked_step_write_from_user_propagates_efault_without_advance() {
    let _lock = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let aspace = crate::vm::AddressSpace::new();
    let unmapped_src = 0x300_000;
    let guard = tx_substrate::epoch::guard();
    let mut writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &aspace,
        &pc,
        &mut writer,
        UserPtr::<u8>::new(unmapped_src),
        8,
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Err(Errno::EFAULT));
    assert_eq!(writer.offset(), 0);
    assert_eq!(
        pc.size_bytes(),
        pc.page_count() * crate::vm::USER_PAGE_SIZE as u64
    );
}
