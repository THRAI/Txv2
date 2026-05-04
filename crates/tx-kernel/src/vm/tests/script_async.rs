use super::*;
use crate::vm::RANGE_LOCK_RELEASE_MASK;
use alloc::boxed::Box;
use core::future::Future;
use core::ptr::null;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use tx_reactor::wait::Channel;

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
        .acquire_step(range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("first acquire should succeed"),
    };

    let blocked = match aspace
        .range_lock()
        .acquire_step(range, crate::vm::LockMode::Materializer)
    {
        crate::vm::AcquireResult::WouldBlock(blocked) => blocked,
        _ => panic!("expected WouldBlock for overlapping materializer"),
    };

    let token = blocked.wait_token();
    assert_eq!(token.carrier(), aspace.range_lock().wait_carrier_id());
    assert_eq!(token.interest(), RANGE_LOCK_RELEASE_MASK);
}

#[test]
fn map_script_async_succeeds_in_one_poll_when_uncontended() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let request = VmMapRequest::fixed(
        range(0x1000, 1),
        MapPlacement::RequireFree,
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );

    let mut future = Box::pin(aspace.map_script_async(request));
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
fn map_script_async_yields_on_writer_conflict_and_completes_after_release() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let target_range = range(0x2000, 1);

    let holder = match aspace
        .range_lock()
        .acquire_step(target_range, crate::vm::LockMode::ExclusiveWriter)
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
    let mut future = Box::pin(aspace.map_script_async(request));
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
fn unmap_async_yields_on_writer_conflict_and_completes_after_release() {
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
        .acquire_step(target_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let mut future = Box::pin(aspace.unmap_async(target_range));
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
fn protect_async_yields_on_writer_conflict_and_completes_after_release() {
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
        .acquire_step(target_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let mut future = Box::pin(aspace.protect_async(target_range, Prot::READ));
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
fn remap_async_yields_on_pair_conflict_and_completes_after_release() {
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
        .acquire_step(new_range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let request = VmRemapRequest {
        old_range,
        new_range,
    };
    let mut future = Box::pin(aspace.remap_async(request));
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
fn brk_script_async_grows_anon_mapping_when_requested_above_current() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0xb000);
    let current_brk = crate::vm::UserVirtAddr(0xb000);
    let requested_brk = crate::vm::UserVirtAddr(0xd000);

    let mut future = Box::pin(aspace.brk_script_async(brk_base, current_brk, requested_brk));
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
fn brk_script_async_shrinks_anon_mapping_when_requested_below_current() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0xe000);
    let initial_top = crate::vm::UserVirtAddr(0x10000);

    let mut grow_future = Box::pin(aspace.brk_script_async(brk_base, brk_base, initial_top));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match grow_future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        other => panic!("baseline grow expected Ready(Ok), got {other:?}"),
    }

    let shrunk = crate::vm::UserVirtAddr(0xf000);
    let mut shrink_future = Box::pin(aspace.brk_script_async(brk_base, initial_top, shrunk));
    match shrink_future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(new_brk)) => assert_eq!(new_brk, shrunk),
        other => panic!("brk shrink expected Ready(Ok), got {other:?}"),
    }
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xe000)).is_some());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(0xf000)).is_none());
}

#[test]
fn brk_script_async_returns_current_when_requested_equal() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x14000);
    let current = crate::vm::UserVirtAddr(0x14000);

    let mut future = Box::pin(aspace.brk_script_async(brk_base, current, current));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(brk)) => assert_eq!(brk, current),
        other => panic!("brk no-op expected Ready(Ok), got {other:?}"),
    }
}

#[test]
fn brk_script_async_rejects_below_brk_base_with_invalid_range() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let brk_base = crate::vm::UserVirtAddr(0x16000);
    let current = crate::vm::UserVirtAddr(0x18000);
    let below_base = crate::vm::UserVirtAddr(0x15000);

    let mut future = Box::pin(aspace.brk_script_async(brk_base, current, below_base));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Err(crate::vm::VmMapError::InvalidRange)) => {}
        other => panic!("expected InvalidRange, got {other:?}"),
    }
}

#[test]
fn fault_script_async_succeeds_in_one_poll_when_uncontended() {
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
    let mut future = Box::pin(aspace.fault_script_async(fault));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    match future.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(error)) => panic!("uncontended fault errored: {error:?}"),
        Poll::Pending => panic!("uncontended fault should not yield"),
    }
}

#[test]
fn fault_script_async_yields_on_writer_conflict_and_completes_after_release() {
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
        .acquire_step(target, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("baseline acquire should succeed"),
    };

    let fault = VmFault::new(crate::vm::UserVirtAddr(0x1a000), AccessMode::Write);
    let mut future = Box::pin(aspace.fault_script_async(fault));
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

    let _child =
        crate::vm::AddressSpace::fork_aspace::<crate::vm::pmap::TestPmap>(&parent).expect("fork");

    assert!(
        parent
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x24000).containing_page())
            .is_none(),
        "MAP_PRIVATE PTE should be torn down on fork"
    );
    assert!(
        parent
            .pmap()
            .lookup(crate::vm::UserVirtAddr(0x26000).containing_page())
            .is_some(),
        "MAP_SHARED PTE should remain mapped"
    );
}

#[test]
fn fork_aspace_returns_would_block_when_parent_full_user_range_already_held() {
    setup_host_substrate();
    let parent = AddressSpace::new();
    let _holder = match parent
        .range_lock()
        .acquire_step(range(0x40000, 1), crate::vm::LockMode::ExclusiveWriter)
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

#[test]
fn range_lock_release_fires_registered_channel_for_external_subscribers() {
    let aspace = AddressSpace::new();
    let range = range(0x4000, 1);
    let holder = match aspace
        .range_lock()
        .acquire_step(range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("acquire should succeed"),
    };

    let channel: Channel =
        crate::wait_carrier::lookup_wait_channel(aspace.range_lock().wait_carrier_id())
            .expect("RangeLock channel registered");
    let mut wait_future =
        Box::pin(channel.wait(tx_reactor::wait::Mask::from_bits(RANGE_LOCK_RELEASE_MASK)));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(wait_future.as_mut().poll(&mut cx), Poll::Pending));

    drop(holder);

    assert!(matches!(wait_future.as_mut().poll(&mut cx), Poll::Ready(_)));
}
