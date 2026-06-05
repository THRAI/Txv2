#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::page_backed::{PageContainerKind, PageIndex};
use crate::vm::adapter::step_engine::StepOutcome;
use tx_hal::Ppn;

fn device_backing() -> VmBacking {
    setup_host_substrate();
    VmBacking::Page {
        pc: PageContainer::new_cap(
            PageContainerKind::Device {
                base_ppn: Ppn(0xfeed),
                page_count: 1,
            },
            1,
        )
        .expect("device page container cap")
        .into(),
        offset: 0,
    }
}

#[test]
fn user_gift_iov_plan_splits_unaligned_ranges_to_copy_fallback() {
    let plan =
        UserGiftIovPlan::new(UserVirtAddr(0x1003), USER_PAGE_SIZE).expect("nonzero user iov");

    assert_eq!(plan.total_bytes(), USER_PAGE_SIZE);
    assert_eq!(plan.gift_range(), None);
    assert_eq!(plan.copy_prefix_bytes(), USER_PAGE_SIZE);
    assert_eq!(plan.copy_suffix_bytes(), 0);
    assert_eq!(plan.copy_bytes(), USER_PAGE_SIZE);

    let mixed = UserGiftIovPlan::new(UserVirtAddr(0x1003), 2 * USER_PAGE_SIZE)
        .expect("mixed copy/gift user iov");

    assert_eq!(
        mixed.gift_range(),
        Some(UserRange::new_aligned(UserVirtAddr(0x2000), USER_PAGE_SIZE).expect("gift page"))
    );
    assert_eq!(mixed.copy_prefix_bytes(), USER_PAGE_SIZE - 3);
    assert_eq!(mixed.copy_suffix_bytes(), 3);
    assert_eq!(mixed.copy_bytes(), USER_PAGE_SIZE);
}

#[test]
fn private_anon_full_page_is_giftable_detached_private() {
    let entry = VmEntry::new(
        range(0x4000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );

    assert_eq!(
        classify_user_gift_page(&entry, range(0x5000, 1)),
        UserGiftEligibility::Giftable {
            freeze: UserPageGiftFreeze::DetachedPrivate,
        }
    );
}

#[test]
fn shared_readonly_and_device_mappings_use_copy_fallback() {
    let shared = VmEntry::new(
        range(0x4000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        VmBacking::PrivateAnon,
    );
    assert_eq!(
        classify_user_gift_page(&shared, range(0x4000, 1)),
        UserGiftEligibility::CopyFallback(UserGiftFallbackReason::SharedMapping)
    );

    let readonly = VmEntry::new(
        range(0x5000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    assert_eq!(
        classify_user_gift_page(&readonly, range(0x5000, 1)),
        UserGiftEligibility::CopyFallback(UserGiftFallbackReason::MissingWritePermission)
    );

    let device = VmEntry::new(
        range(0x6000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        device_backing(),
    );
    assert_eq!(
        classify_user_gift_page(&device, range(0x6000, 1)),
        UserGiftEligibility::CopyFallback(UserGiftFallbackReason::DeviceMapping)
    );
}

#[test]
fn private_pagebacked_mapping_is_giftable_as_demoted_cow() {
    let entry = VmEntry::new(
        range(0x8000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        page_backing(0),
    );

    assert_eq!(
        classify_user_gift_page(&entry, range(0x9000, 1)),
        UserGiftEligibility::Giftable {
            freeze: UserPageGiftFreeze::DemotedCow,
        }
    );
}

#[test]
fn user_page_gift_value_records_source_and_frame_accessors() {
    setup_host_substrate();
    let aspace = AddressSpace::new_cap().expect("address space cap");
    let source_range = range(0xa000, 1);
    let source = UserPageGiftSource::new(aspace.clone(), source_range);
    let frame = crate::vm::adapter::step_engine::page_allocator::reserve_frame(
        crate::vm::adapter::step_engine::page_allocator::ZeroPolicy::UninitFullOverwrite,
    )
    .expect("gift source frame")
    .commit();
    let ppn = frame.ppn();
    let gift_pin = frame.try_gift_pin().expect("gift pin");

    let gift = UserPageGift::new_for_vm(ppn, source, gift_pin, UserPageGiftFreeze::DetachedPrivate);

    assert_eq!(gift.ppn(), ppn);
    assert_eq!(gift.range(), source_range);
    assert_eq!(gift.len(), USER_PAGE_SIZE);
    assert_eq!(gift.freeze(), UserPageGiftFreeze::DetachedPrivate);
    assert_eq!(gift.source().aspace().raw(), aspace.raw());

    let batch = GiftBatch::new(alloc::vec![gift], USER_PAGE_SIZE);
    assert_eq!(batch.bytes(), USER_PAGE_SIZE);
    assert_eq!(batch.gift_count(), 1);
    assert!(!batch.is_empty());

    drop(frame);
}

#[test]
fn private_anon_gift_removes_writable_source_and_next_write_gets_fresh_frame() {
    setup_host_substrate();
    let aspace = AddressSpace::new_cap().expect("address space cap");
    let source_range = range(0x10000, 1);
    let entry = VmEntry::new(
        source_range,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let first = aspace
        .resolve_fault(VmFault::new(source_range.start(), AccessMode::Write))
        .expect("write fault resolves");
    let first_page = first.materialize_pagebacked().expect("materialize");
    let gifted_ppn = first_page.page.ppn;
    aspace
        .publish_fault_materialization(first, first_page)
        .expect("publish private page");
    assert_eq!(
        aspace.pmap().lookup(source_range.start().containing_page()),
        Some(PmapMappingSnapshot {
            ppn: gifted_ppn,
            prot: Prot::READ_WRITE,
        })
    );

    let batch = match aspace.gift_user_pages_step(aspace.clone(), source_range) {
        StepOutcome::Done(batch) => batch,
        StepOutcome::Yield { .. } => panic!("gift step should not block"),
        StepOutcome::Continue { .. } => panic!("gift step should not continue"),
        StepOutcome::Err(errno) => panic!("gift step failed: {errno:?}"),
    };
    assert_eq!(batch.bytes(), USER_PAGE_SIZE);
    assert_eq!(batch.gift_count(), 1);
    let gifts = batch.into_gifts();
    let gift = gifts.first().expect("one gift");
    assert_eq!(gift.ppn(), gifted_ppn);
    assert_eq!(gift.freeze(), UserPageGiftFreeze::DetachedPrivate);
    assert_eq!(gift.source().aspace().raw(), aspace.raw());
    assert_eq!(
        aspace.pmap().lookup(source_range.start().containing_page()),
        None
    );

    let second = aspace
        .resolve_fault(VmFault::new(source_range.start(), AccessMode::Write))
        .expect("second write fault resolves");
    let second_page = second.materialize_pagebacked().expect("rematerialize");
    let fresh_ppn = second_page.page.ppn;
    assert_ne!(fresh_ppn, gifted_ppn);
    aspace
        .publish_fault_materialization(second, second_page)
        .expect("publish fresh page");
    assert_eq!(
        aspace.pmap().lookup(source_range.start().containing_page()),
        Some(PmapMappingSnapshot {
            ppn: fresh_ppn,
            prot: Prot::READ_WRITE,
        })
    );
}

#[test]
fn shared_mapping_gift_uses_empty_copy_fallback_without_changing_pte() {
    setup_host_substrate();
    let aspace = AddressSpace::new_cap().expect("address space cap");
    let source_range = range(0x12000, 1);
    let entry = VmEntry::new(
        source_range,
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let fault = aspace
        .resolve_fault(VmFault::new(source_range.start(), AccessMode::Write))
        .expect("fault resolves");
    let page = fault.materialize_pagebacked().expect("materialize");
    let ppn = page.page.ppn;
    aspace
        .publish_fault_materialization(fault, page)
        .expect("publish shared mapping");

    let batch = match aspace.gift_user_pages_step(aspace.clone(), source_range) {
        StepOutcome::Done(batch) => batch,
        StepOutcome::Yield { .. } => panic!("gift step should not block"),
        StepOutcome::Continue { .. } => panic!("gift step should not continue"),
        StepOutcome::Err(errno) => panic!("gift fallback failed: {errno:?}"),
    };

    assert_eq!(batch.bytes(), 0);
    assert_eq!(batch.gift_count(), 0);
    assert_eq!(
        aspace.pmap().lookup(source_range.start().containing_page()),
        Some(PmapMappingSnapshot {
            ppn,
            prot: Prot::READ_WRITE,
        })
    );
}
