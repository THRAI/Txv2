use super::*;
use crate::execution::Errno;
use crate::vm::adapter::step_engine::StepOutcome;
use alloc::vec;
use alloc::vec::Vec;
use tx_hal::UserPtr;

fn setup_host_substrate() {
    tx_test_support::init_host();
    crate::zones::register_all().expect("kernel zones");
    match crate::vm::adapter::step_engine::page_allocator::claim_zero_frame() {
        Ok(_)
        | Err(crate::vm::adapter::step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for VM user-access tests: {error:?}"),
    }
}

fn map_private(aspace: &AddressSpace, start: usize, len: usize, prot: Prot) {
    let len = align_up(len.max(1), USER_PAGE_SIZE);
    let range = UserRange::new_aligned(UserVirtAddr(start), len).expect("aligned user range");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::RequireFree,
        prot,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    aspace.try_mmap(request).expect("map test user range");
}

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[test]
fn vm_copy_to_user_then_copy_from_user_walks_recipes_and_pmap() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("vm user-access test lock");
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let user_addr = 0x4000;
    let payload: Vec<u8> = (0..(USER_PAGE_SIZE + 31))
        .map(|i| (i & 0xff) as u8)
        .collect();
    map_private(&aspace, user_addr, payload.len(), Prot::READ_WRITE);
    let guard = crate::vm::adapter::step_engine::guard();

    let copied = aspace.copy_to_user(UserPtr::new(user_addr), &payload, &guard);
    assert_eq!(copied, StepOutcome::Done(payload.len()));

    let mut observed = vec![0u8; payload.len()];
    let copied = aspace.copy_from_user(&mut observed, UserPtr::new(user_addr), &guard);
    assert_eq!(copied, StepOutcome::Done(payload.len()));
    assert_eq!(observed, payload);
    assert!(aspace
        .pmap()
        .lookup(UserVirtAddr(user_addr).containing_page())
        .is_some());
}

#[test]
fn vm_copy_from_user_rejects_unmapped_pointer_before_kernel_copy() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("vm user-access test lock");
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let mut observed = [0xAAu8; 8];
    let guard = crate::vm::adapter::step_engine::guard();

    let copied = aspace.copy_from_user(&mut observed, UserPtr::new(0x8000), &guard);

    assert_eq!(copied, StepOutcome::Err(Errno::EFAULT.into()));
    assert_eq!(observed, [0xAA; 8]);
}

#[test]
fn vm_copy_to_user_rejects_read_only_recipe() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("vm user-access test lock");
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let user_addr = 0x10_000;
    map_private(&aspace, user_addr, 16, Prot::READ);
    let payload = [0x5Au8; 16];
    let guard = crate::vm::adapter::step_engine::guard();

    let copied = aspace.copy_to_user(UserPtr::new(user_addr), &payload, &guard);

    assert_eq!(copied, StepOutcome::Err(Errno::EFAULT.into()));
}

#[test]
fn vm_read_write_user_reject_partial_guard_crossing_structs() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("vm user-access test lock");
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let base = 0x7000;
    let crossing = base + USER_PAGE_SIZE - 16;
    map_private(&aspace, base, USER_PAGE_SIZE, Prot::READ_WRITE);
    map_private(&aspace, base + USER_PAGE_SIZE, USER_PAGE_SIZE, Prot::NONE);
    let guard = crate::vm::adapter::step_engine::guard();

    let read = aspace.read_user::<[u8; 32]>(UserPtr::new(crossing), &guard);
    assert_eq!(read, StepOutcome::Err(Errno::EFAULT.into()));

    let write = aspace.write_user::<[u8; 32]>(UserPtr::new(crossing), [0x5A; 32], &guard);
    assert_eq!(write, StepOutcome::Err(Errno::EFAULT.into()));
}
