#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::vm::adapter::wait_routing::Channel;
use crate::vm::RANGE_LOCK_RELEASE_MASK;
use alloc::boxed::Box;
use core::future::Future;
use core::ptr::null;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

const NOOP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(null(), &NOOP_WAKER_VTABLE),
    |_| {},
    |_| {},
    |_| {},
);

fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(null(), &NOOP_WAKER_VTABLE)) }
}

#[test]
fn range_lock_would_block_wait_token_carrier_matches_lock() {
    let aspace = AddressSpace::new();
    let range = range(0x1000, 1);
    let _holder = match aspace
        .range_lock()
        .acquire_step_rich(range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("first acquire should succeed"),
    };

    let blocked = match aspace
        .range_lock()
        .acquire_step_rich(range, crate::vm::LockMode::Materializer)
    {
        crate::vm::AcquireResult::WouldBlock(blocked) => blocked,
        _ => panic!("expected WouldBlock for overlapping materializer"),
    };

    let token = blocked.wait_token();
    assert_eq!(token.source_id(), aspace.range_lock().wait_source_id());
    assert_eq!(token.interest(), RANGE_LOCK_RELEASE_MASK);
}

#[test]
fn mmap_script_succeeds_in_one_poll_when_uncontended() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let request = VmMapRequest::fixed(
        range(0x1000, 1),
        MapPlacement::RequireFree,
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );

    let mut future = Box::pin(aspace.mmap_script(request));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(outcome)) => {
            assert_eq!(outcome.range, range(0x1000, 1));
        }
        Poll::Ready(Err(error)) => panic!("uncontended map errored: {error:?}"),
        Poll::Pending => panic!("uncontended map should not yield"),
    }
}

#[test]
fn mmap_script_yields_on_writer_conflict_and_completes_after_release() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target_range = range(0x2000, 1);

    let holder = match aspace
        .range_lock()
        .acquire_step_rich(target_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let request = VmMapRequest::fixed(
        target_range,
        MapPlacement::RequireFree,
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    let mut future = Box::pin(aspace.mmap_script(request));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(outcome)) => {
            assert_eq!(outcome.range, target_range);
        }
        Poll::Ready(Err(error)) => panic!("post-release map errored: {error:?}"),
        Poll::Pending => panic!("expected Ready after holder release"),
    }
}

#[test]
fn munmap_script_yields_on_writer_conflict_and_completes_after_release() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target_range = range(0x6000, 1);

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            target_range,
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("baseline map");

    let holder = match aspace
        .range_lock()
        .acquire_step_rich(target_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let mut future = Box::pin(aspace.munmap_script(target_range));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("post-release unmap errored: {error:?}"),
        Poll::Pending => panic!("expected Ready after holder release"),
    }
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0x6000)).is_none());
}

#[test]
fn mprotect_script_yields_on_writer_conflict_and_completes_after_release() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target_range = range(0x7000, 1);

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            target_range,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("baseline map");

    let holder = match aspace
        .range_lock()
        .acquire_step_rich(target_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let mut future = Box::pin(aspace.mprotect_script(target_range, Prot::READ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("post-release protect errored: {error:?}"),
        Poll::Pending => panic!("expected Ready after holder release"),
    }
    assert_eq!(
        aspace
            .lookup(crate::vm::UserVirtAddr(0x7000))
            .expect("entry exists")
            .prot,
        Prot::READ
    );
}

#[test]
fn mremap_script_yields_on_pair_conflict_and_completes_after_release() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let old_range = range(0x8000, 1);
    let new_range = range(0xa000, 1);

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            old_range,
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("baseline map");

    let holder = match aspace
        .range_lock()
        .acquire_step_rich(new_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let request = VmRemapRequest::new(old_range, new_range);
    let mut future = Box::pin(aspace.mremap_script(request));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(outcome)) => {
            assert_eq!(outcome.old_range, old_range);
            assert_eq!(outcome.new_range, new_range);
        }
        Poll::Ready(Err(error)) => panic!("post-release remap errored: {error:?}"),
        Poll::Pending => panic!("expected Ready after holder release"),
    }
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0x8000)).is_none());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xa000)).is_some());
}

#[test]
fn brk_script_grows_anon_mapping_when_requested_above_current() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0xb000);
    let current_brk = crate::vm::UserVirtAddr(0xb000);
    let requested_brk = crate::vm::UserVirtAddr(0xd000);

    let mut future = Box::pin(aspace.brk_script(brk_base, current_brk, requested_brk));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, requested_brk),
        other => panic!("brk grow expected Ready(Ok), got {other:?}"),
    }
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xb000)).is_some());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xc000)).is_some());
}

#[test]
fn brk_script_repeated_growth_extends_single_recipe() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0xb000);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut current = brk_base;

    for page in 1..=8 {
        let requested = crate::vm::UserVirtAddr(brk_base.0 + page * crate::vm::USER_PAGE_SIZE);
        let mut future = Box::pin(aspace.brk_script(brk_base, current, requested));
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, requested),
            other => panic!("brk grow page {page} expected Ready(Ok), got {other:?}"),
        }
        current = requested;
    }

    assert_eq!(
        aspace.stats().recipe_count,
        1,
        "brk growth should extend the heap mapping instead of appending one recipe per page"
    );
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xb000)).is_some());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0x12000)).is_some());
}

#[test]
fn brk_script_unaligned_base_extends_committed_page_range() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0xb123);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut current = brk_base;

    for page in 1..=4 {
        let requested = crate::vm::UserVirtAddr(0xc000 + page * crate::vm::USER_PAGE_SIZE);
        let mut future = Box::pin(aspace.brk_script(brk_base, current, requested));
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, requested),
            other => panic!("unaligned brk grow page {page} expected Ready(Ok), got {other:?}"),
        }
        current = requested;
    }

    assert_eq!(aspace.stats().recipe_count, 1);
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xb123)).is_none());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xc000)).is_some());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xf000)).is_some());
}

#[test]
fn brk_script_many_unaligned_grows_remain_one_recipe() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x2c2d0);
    let committed_base = crate::vm::UserVirtAddr(0x2d000);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut current = brk_base;

    for page in 1..=1024 {
        let requested =
            crate::vm::UserVirtAddr(committed_base.0 + page * crate::vm::USER_PAGE_SIZE);
        let mut future = Box::pin(aspace.brk_script(brk_base, current, requested));
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, requested),
            other => {
                panic!("many unaligned brk grow page {page} expected Ready(Ok), got {other:?}")
            }
        }
        current = requested;
    }

    assert_eq!(aspace.stats().recipe_count, 1);
    assert!(aspace.lookup(committed_base).is_some());
    assert!(aspace
        .lookup(crate::vm::UserVirtAddr(
            committed_base.0 + 1023 * crate::vm::USER_PAGE_SIZE
        ))
        .is_some());
}

#[test]
fn try_brk_many_unaligned_grows_map_requested_pages() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x2c2d0);
    let committed_base = crate::vm::UserVirtAddr(0x2d000);
    let mut current = brk_base;

    for page in 1..=1024 {
        let requested =
            crate::vm::UserVirtAddr(committed_base.0 + page * crate::vm::USER_PAGE_SIZE);
        let new_brk = aspace
            .try_brk(brk_base, current, requested)
            .expect("uncontended brk grow should complete synchronously");
        assert_eq!(new_brk, requested);
        current = requested;
    }

    assert_eq!(
        aspace.stats().recipe_count,
        1,
        "synchronous brk growth should use the same coalesced recipe contract as brk_script"
    );
    assert!(aspace.lookup(committed_base).is_some());
    assert!(aspace
        .lookup(crate::vm::UserVirtAddr(
            committed_base.0 + 1023 * crate::vm::USER_PAGE_SIZE
        ))
        .is_some());
}

#[test]
fn try_brk_growth_maps_only_requested_committed_range() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x2c2d0);
    let committed_base = crate::vm::UserVirtAddr(0x2d000);
    let requested = crate::vm::UserVirtAddr(committed_base.0 + crate::vm::USER_PAGE_SIZE);

    let new_brk = aspace
        .try_brk(brk_base, brk_base, requested)
        .expect("uncontended brk grow should complete synchronously");

    assert_eq!(new_brk, requested);
    assert_eq!(aspace.stats().recipe_count, 1);
    assert_eq!(
        aspace.stats().vm_size,
        crate::vm::USER_PAGE_SIZE,
        "brk growth should not reserve hidden capacity beyond the published break"
    );
    assert!(aspace.lookup(committed_base).is_some());
    assert!(aspace
        .lookup(crate::vm::UserVirtAddr(
            committed_base.0 + crate::vm::USER_PAGE_SIZE
        ))
        .is_none());
}

#[test]
fn brk_script_many_unaligned_grows_with_faults_do_not_block() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x2c2d0);
    let committed_base = crate::vm::UserVirtAddr(0x2d000);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut current = brk_base;

    for page in 1..=512 {
        let requested =
            crate::vm::UserVirtAddr(committed_base.0 + page * crate::vm::USER_PAGE_SIZE);
        let mut brk_future = Box::pin(aspace.brk_script(brk_base, current, requested));
        match brk_future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, requested),
            other => panic!("brk grow page {page} expected Ready(Ok), got {other:?}"),
        }

        let fault_addr = crate::vm::UserVirtAddr(requested.0 - crate::vm::USER_PAGE_SIZE);
        let mut fault_future =
            Box::pin(aspace.fault_script(VmFault::new(fault_addr, AccessMode::Write)));
        match fault_future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(_)) => {}
            other => panic!("heap fault page {page} expected Ready(Ok), got {other:?}"),
        }

        current = requested;
    }

    assert_eq!(
        aspace.stats().recipe_count,
        1,
        "fault-populated heap growth should keep Linux-like VMA coalescing; private page residency must be cheap to split independently"
    );
}

#[test]
fn brk_growth_after_private_fault_keeps_single_recipe_with_structural_private_split() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x40000);
    let committed_base = crate::vm::UserVirtAddr(0x40000);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    let first_top = crate::vm::UserVirtAddr(committed_base.0 + crate::vm::USER_PAGE_SIZE);
    let mut first_grow = Box::pin(aspace.brk_script(brk_base, brk_base, first_top));
    match first_grow.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, first_top),
        other => panic!("first brk grow expected Ready(Ok), got {other:?}"),
    }
    assert_eq!(aspace.stats().recipe_count, 1);

    let mut first_fault =
        Box::pin(aspace.fault_script(VmFault::new(committed_base, AccessMode::Write)));
    match first_fault.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        other => panic!("first private fault expected Ready(Ok), got {other:?}"),
    }

    let second_top = crate::vm::UserVirtAddr(committed_base.0 + 2 * crate::vm::USER_PAGE_SIZE);
    let mut second_grow = Box::pin(aspace.brk_script(brk_base, first_top, second_top));
    match second_grow.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, second_top),
        other => panic!("second brk grow expected Ready(Ok), got {other:?}"),
    }
    assert_eq!(
        aspace.stats().recipe_count,
        1,
        "clean adjacent growth should still merge into a resident private recipe once private pages split structurally"
    );

    let third_top = crate::vm::UserVirtAddr(committed_base.0 + 3 * crate::vm::USER_PAGE_SIZE);
    let mut third_grow = Box::pin(aspace.brk_script(brk_base, second_top, third_top));
    match third_grow.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, third_top),
        other => panic!("third brk grow expected Ready(Ok), got {other:?}"),
    }
    assert_eq!(
        aspace.stats().recipe_count,
        1,
        "clean tail growth still coalesces with the clean tail recipe"
    );
}

#[test]
fn brk_script_shrinks_anon_mapping_when_requested_below_current() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0xe000);
    let initial_top = crate::vm::UserVirtAddr(0x10000);

    let mut grow_future = Box::pin(aspace.brk_script(brk_base, brk_base, initial_top));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match grow_future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        other => panic!("baseline grow expected Ready(Ok), got {other:?}"),
    }

    let shrunk = crate::vm::UserVirtAddr(0xf000);
    let mut shrink_future = Box::pin(aspace.brk_script(brk_base, initial_top, shrunk));
    match shrink_future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, shrunk),
        other => panic!("brk shrink expected Ready(Ok), got {other:?}"),
    }
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xe000)).is_some());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xf000)).is_none());
}

#[test]
fn brk_script_returns_current_when_requested_equal() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x14000);
    let current = crate::vm::UserVirtAddr(0x14000);

    let mut future = Box::pin(aspace.brk_script(brk_base, current, current));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(brk)) => assert_eq!(brk, current),
        other => panic!("brk no-op expected Ready(Ok), got {other:?}"),
    }
}

#[test]
fn brk_script_rejects_below_brk_base_with_invalid_range() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x16000);
    let current = crate::vm::UserVirtAddr(0x18000);
    let below_base = crate::vm::UserVirtAddr(0x15000);

    let mut future = Box::pin(aspace.brk_script(brk_base, current, below_base));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Err(crate::vm::VmMapError::InvalidRange)) => {}
        other => panic!("expected InvalidRange, got {other:?}"),
    }
}

#[test]
fn fault_script_succeeds_in_one_poll_when_uncontended() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target = range(0x18000, 1);

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            target,
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("baseline map");

    let fault = VmFault::new(crate::vm::UserVirtAddr(0x18000), AccessMode::Write);
    let mut future = Box::pin(aspace.fault_script(fault));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("uncontended fault errored: {error:?}"),
        Poll::Pending => panic!("uncontended fault should not yield"),
    }
}

#[test]
fn fault_script_prefaults_adjacent_private_anon_write_pages() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target = range(0x100000, 256);

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            target,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("baseline map");

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    let first_fault = VmFault::new(crate::vm::UserVirtAddr(0x100000), AccessMode::Write);
    let mut first = Box::pin(aspace.fault_script(first_fault));
    match first.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("first fault errored: {error:?}"),
        Poll::Pending => panic!("uncontended fault should not yield"),
    }

    let second_fault = VmFault::new(crate::vm::UserVirtAddr(0x101000), AccessMode::Write);
    let mut second = Box::pin(aspace.fault_script(second_fault));
    match second.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("second fault errored: {error:?}"),
        Poll::Pending => panic!("uncontended fault should not yield"),
    }

    assert!(
        aspace
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x100000).containing_page())
            .is_some(),
        "the first sequential fault records the stream without batching"
    );
    for page in UserRange::new_aligned(
        crate::vm::UserVirtAddr(0x101000),
        15 * crate::vm::USER_PAGE_SIZE,
    )
    .expect("tail range")
    .iter_pages()
    {
        assert_eq!(
            aspace.pmap().lookup(page).expect("prefaulted page").prot,
            Prot::READ_WRITE
        );
    }
}

#[test]
fn fault_script_yields_on_writer_conflict_and_completes_after_release() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target = range(0x1a000, 1);

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            target,
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("baseline map");

    let holder = match aspace
        .range_lock()
        .acquire_step_rich(target, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let fault = VmFault::new(crate::vm::UserVirtAddr(0x1a000), AccessMode::Write);
    let mut future = Box::pin(aspace.fault_script(fault));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("post-release fault errored: {error:?}"),
        Poll::Pending => panic!("expected Ready after holder release"),
    }
}

#[test]
fn fork_aspace_clones_parent_recipes_into_fresh_child() {
    setup_host_substrate();
    let parent = AddressSpace::new();
    let private_range = range(0x20000, 1);
    let shared_range = range(0x22000, 1);

    map_reserved(parent.reserve_map(
        VmEntry::new(
            private_range,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("private map");

    map_reserved(parent.reserve_map(
        VmEntry::new(
            shared_range,
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("shared map");

    let child =
        crate::vm::AddressSpace::fork_aspace::<crate::vm::pmap::TestPmap>(&parent).expect("fork");

    assert_eq!(
        child
            .lookup(crate::vm::UserVirtAddr(0x20000))
            .map(|e| e.flags),
        Some(VmEntryFlags::PRIVATE)
    );
    assert_eq!(
        child
            .lookup(crate::vm::UserVirtAddr(0x22000))
            .map(|e| e.flags),
        Some(VmEntryFlags::SHARED)
    );
}

#[test]
fn fork_aspace_shares_recipe_root_for_unchanged_entries() {
    setup_host_substrate();
    let parent = AddressSpace::new();

    for i in 0..2048 {
        map_reserved(parent.reserve_map(
            VmEntry::new(
                range(0x1000 + i * 0x4000, 1),
                Prot::NONE,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ),
            MapPlacement::RequireFree,
        ))
        .commit()
        .expect("map prot-none private entry");
    }

    let child =
        crate::vm::AddressSpace::fork_aspace::<crate::vm::pmap::TestPmap>(&parent).expect("fork");

    assert_eq!(child.stats().recipe_count, 2048);
    assert_eq!(
        child.recipes.debug_last_publish_touched_entries(),
        0,
        "fork should clone the immutable recipe root, not rebuild unchanged entries"
    );
}

#[test]
fn fork_aspace_demotes_parent_pmap_for_private_entries_only() {
    setup_host_substrate();
    let parent = AddressSpace::new();
    let private_range = range(0x24000, 1);
    let shared_range = range(0x26000, 1);

    map_reserved(parent.reserve_map(
        VmEntry::new(
            private_range,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("private map");
    map_reserved(parent.reserve_map(
        VmEntry::new(
            shared_range,
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("shared map");

    for fault_addr in [0x24000_usize, 0x26000] {
        let outcome = parent
            .resolve_fault(VmFault::new(
                crate::vm::UserVirtAddr(fault_addr),
                AccessMode::Read,
            ))
            .expect("fault resolves");
        let materialized = outcome.materialize_pagebacked().expect("materialize");
        parent
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }

    assert!(parent
        .pmap()
        .lookup(crate::vm::UserVirtAddr(0x24000).containing_page())
        .is_some());
    assert!(parent
        .pmap()
        .lookup(crate::vm::UserVirtAddr(0x26000).containing_page())
        .is_some());

    let child =
        crate::vm::AddressSpace::fork_aspace::<crate::vm::pmap::TestPmap>(&parent).expect("fork");

    assert_eq!(
        parent
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x24000).containing_page())
            .expect("MAP_PRIVATE PTE should remain mapped after fork")
            .prot,
        Prot::READ,
        "MAP_PRIVATE PTE should be demoted read-only on fork"
    );
    assert_eq!(
        parent
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x26000).containing_page())
            .expect("MAP_SHARED PTE should remain mapped")
            .prot,
        Prot::READ_WRITE,
        "MAP_SHARED PTE should keep its original protection"
    );
    assert_eq!(
        child
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x24000).containing_page())
            .expect("child should inherit demoted private resident PTE")
            .prot,
        Prot::READ,
        "child inherited private PTE must be read-only for CoW"
    );
    assert!(
        child
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x26000).containing_page())
            .is_none(),
        "shared writable PTEs should still refault through the recipe path"
    );
}

#[test]
fn fork_aspace_returns_would_block_when_parent_full_user_range_already_held() {
    setup_host_substrate();
    let parent = AddressSpace::new();
    let _holder = match parent
        .range_lock()
        .acquire_step_rich(range(0x40000, 1), crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let result = crate::vm::AddressSpace::fork_aspace::<crate::vm::pmap::TestPmap>(&parent);
    assert!(matches!(result, Err(crate::vm::VmMapError::WouldBlock)));
}

#[test]
fn exec_aspace_tears_down_all_resident_ptes() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x28000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("map");

    let outcome = aspace
        .resolve_fault(VmFault::new(
            crate::vm::UserVirtAddr(0x28000),
            AccessMode::Read,
        ))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked().expect("materialize");
    aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish");
    assert!(aspace
        .pmap()
        .lookup(crate::vm::UserVirtAddr(0x28000).containing_page())
        .is_some());

    let torn = crate::vm::AddressSpace::exec_aspace(&aspace);

    assert_eq!(torn, 1);
    assert!(aspace
        .pmap()
        .lookup(crate::vm::UserVirtAddr(0x28000).containing_page())
        .is_none());
}

/// Hot-path verification for the D15 / PC CoW plan: parent writes a
/// pattern into a private anon page, forks, both sides observe the
/// pattern, and a parent write does not affect the child.
#[test]
fn fork_aspace_preserves_parent_private_anon_bytes_in_child_via_sharedcow() {
    setup_host_substrate();

    let parent = AddressSpace::new();
    let stack_range = range(0x40000, 1);

    map_reserved(parent.reserve_map(
        VmEntry::new(
            stack_range,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("private anon map");

    // Parent writes a distinctive pattern into the page. The write
    // fault path must allocate a private frame, install it into
    // vme.private as Exclusive, and copy the pattern bytes.
    let pattern = [0xABu8; 16];
    let user_addr = 0x40080usize;
    let guard = crate::vm::adapter::step_engine::guard();
    let copied = parent.copy_to_user(tx_hal::UserPtr::new(user_addr), &pattern, &guard);
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern.len()),
        "parent write should publish a private page"
    );

    // Verify parent reads back the pattern (round-trip).
    let mut parent_read = [0u8; 16];
    let copied = parent.copy_from_user(&mut parent_read, tx_hal::UserPtr::new(user_addr), &guard);
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern.len()),
        "parent should read back its own write"
    );
    assert_eq!(parent_read, pattern, "parent must see its own pattern");
    drop(guard);

    // Fork: parent's vme.private should fork_share into child as
    // SharedCow. Parent's PTE in the private range is demoted read-only.
    let child =
        crate::vm::AddressSpace::fork_aspace::<crate::vm::pmap::TestPmap>(&parent).expect("fork");

    // Sanity: parent's PTE for the private page remains mapped, but no
    // longer writable. The next parent write must fault and CoW.
    assert_eq!(
        parent
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x40000).containing_page())
            .expect("parent mapping remains after fork")
            .prot,
        Prot::READ,
        "fork must demote parent private PTE to read-only"
    );
    assert_eq!(
        child
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x40000).containing_page())
            .expect("child should inherit the demoted private PTE")
            .prot,
        Prot::READ,
        "child inherited private PTE must be read-only"
    );

    // Parent re-reads after fork: read fault should consult vme.private
    // (SharedCow hit if a refault is needed) and observe the pre-fork frame.
    // Result: parent must still see its own pattern.
    let guard = crate::vm::adapter::step_engine::guard();
    let mut parent_post_fork = [0u8; 16];
    let copied = parent.copy_from_user(
        &mut parent_post_fork,
        tx_hal::UserPtr::new(user_addr),
        &guard,
    );
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern.len()),
        "parent post-fork read must succeed"
    );
    assert_eq!(
        parent_post_fork, pattern,
        "parent post-fork read must observe pre-fork bytes (SharedCow refault)"
    );

    // Child reads at the same VA: child's vme.private got the SharedCow
    // entry via fork_share, and fork copied the resident RO PTE into the
    // child pmap. Result: child must see parent's pre-fork pattern.
    let mut child_read = [0u8; 16];
    let copied = child.copy_from_user(&mut child_read, tx_hal::UserPtr::new(user_addr), &guard);
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern.len()),
        "child post-fork read must succeed"
    );
    assert_eq!(
        child_read, pattern,
        "child must observe parent's pre-fork bytes via SharedCow"
    );

    // Parent writes a new pattern. SharedCow CoW: parent should
    // allocate a fresh frame, copy the shared content, replace_if_match
    // its private entry to Exclusive, install RW PTE on the new frame.
    let pattern2 = [0xCDu8; 16];
    let copied = parent.copy_to_user(tx_hal::UserPtr::new(user_addr), &pattern2, &guard);
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern2.len()),
        "parent post-fork write must succeed (SharedCow → Exclusive)"
    );

    // Parent now reads back the new pattern.
    let mut parent_after_write = [0u8; 16];
    let copied = parent.copy_from_user(
        &mut parent_after_write,
        tx_hal::UserPtr::new(user_addr),
        &guard,
    );
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern2.len()),
        "parent post-write read must succeed"
    );
    assert_eq!(
        parent_after_write, pattern2,
        "parent must see its new pattern after CoW"
    );

    // Child still observes the OLD shared pattern (its SharedCow entry
    // points at the original frame, parent's CoW only allocated a new
    // frame for parent).
    let mut child_after_parent_write = [0u8; 16];
    let copied = child.copy_from_user(
        &mut child_after_parent_write,
        tx_hal::UserPtr::new(user_addr),
        &guard,
    );
    assert_eq!(
        copied,
        crate::vm::adapter::step_engine::StepOutcome::Done(pattern.len()),
        "child read after parent CoW must succeed"
    );
    assert_eq!(
        child_after_parent_write, pattern,
        "child must still see pre-fork bytes after parent CoW (no cross-process leak)"
    );
}

#[test]
fn range_lock_release_fires_registered_channel_for_external_subscribers() {
    let aspace = AddressSpace::new();
    let range = range(0x4000, 1);
    let holder = match aspace
        .range_lock()
        .acquire_step_rich(range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("acquire should succeed"),
    };

    let channel: Channel =
        crate::wait_source::lookup_wait_channel(aspace.range_lock().wait_source_id())
            .expect("RangeLock channel registered");
    let mut wait_future = Box::pin(channel.wait(
        crate::vm::adapter::wait_routing::Mask::from_bits(RANGE_LOCK_RELEASE_MASK),
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(wait_future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    assert!(matches!(wait_future.as_mut().poll(&mut cx), Poll::Ready(_)));
}
