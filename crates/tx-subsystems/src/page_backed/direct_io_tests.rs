use super::*;
use crate::vm::{
    AddressSpace, MapPlacement, Prot, UserAccessKind, UserRange, UserVirtAddr, VmBacking, VmEntry,
    VmEntryFlags, USER_PAGE_SIZE,
};
use tx_hal::UserPtr;

fn setup_host_substrate() {
    tx_test_support::init_host();
    crate::zones::register_all().expect("kernel zones");
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for direct-I/O tests: {error:?}"),
    }
}

fn mapped_aspace() -> AddressSpace {
    let aspace = AddressSpace::new();
    let range = UserRange::new_aligned(UserVirtAddr::new(0x10_000), USER_PAGE_SIZE * 2)
        .expect("aligned user range");
    let commit = match aspace.reserve_map(
        VmEntry::new(
            range,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ) {
        crate::vm::MapReserveResult::Reserved(reservation) => reservation.commit(),
        other => panic!("expected mapped range, got {other:?}"),
    };
    commit.expect("map user range");
    aspace
}

#[test]
fn direct_io_buffer_rejects_unpublished_user_pages() {
    setup_host_substrate();
    let aspace = mapped_aspace();
    let result = DirectIoBuffer::pin(&aspace, UserPtr::new(0x10_080), 5000, UserAccessKind::Read);
    assert!(matches!(result, Err(DirectIoBufferError::NotMaterialized)));
}

#[test]
fn direct_io_buffer_builds_cross_page_biovecs_after_eager_access() {
    setup_host_substrate();
    let aspace = mapped_aspace();
    let range = UserRange::new_aligned(UserVirtAddr::new(0x10_000), USER_PAGE_SIZE * 2)
        .expect("aligned user range");
    assert!(matches!(
        aspace.reserve_user_range_for_access(range, UserAccessKind::Read),
        step_engine::StepOutcome::Done(())
    ));

    let buffer = DirectIoBuffer::pin(&aspace, UserPtr::new(0x10_080), 5000, UserAccessKind::Read)
        .expect("published user pages can be pinned");
    assert_eq!(buffer.vecs().len(), 2);
    assert_eq!(buffer.vecs()[0].offset, 128);
    assert_eq!(buffer.vecs()[0].len, 3968);
    assert_eq!(buffer.vecs()[1].offset, 0);
    assert_eq!(buffer.vecs()[1].len, 1032);
    assert_eq!(buffer.len(), 5000);
    assert_eq!(buffer.pin_count(), 2);
    assert_eq!(buffer.as_source().lease(), Some(buffer.lease_id()));
    assert_eq!(buffer.as_target().lease(), Some(buffer.lease_id()));
    let crate::fs_iface::IoDataSource::Direct { vecs, .. } = buffer.as_source() else {
        panic!("direct buffer must produce a direct source");
    };
    assert_eq!(vecs, buffer.vecs());
}
