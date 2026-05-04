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
        .acquire(range, crate::vm::LockMode::ExclusiveWriter)
    {
        crate::vm::AcquireResult::Acquired(guard) => guard,
        _ => panic!("first acquire should succeed"),
    };

    let blocked = match aspace
        .range_lock()
        .acquire(range, crate::vm::LockMode::Materializer)
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
        .acquire(target_range, crate::vm::LockMode::ExclusiveWriter)
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
        .acquire(target_range, crate::vm::LockMode::ExclusiveWriter)
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
        .acquire(target_range, crate::vm::LockMode::ExclusiveWriter)
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
        .acquire(new_range, crate::vm::LockMode::ExclusiveWriter)
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
fn range_lock_release_fires_registered_channel_for_external_subscribers() {
    let aspace = AddressSpace::new();
    let range = range(0x4000, 1);
    let holder = match aspace
        .range_lock()
        .acquire(range, crate::vm::LockMode::ExclusiveWriter)
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
