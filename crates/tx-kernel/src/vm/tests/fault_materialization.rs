use super::*;

#[test]
fn vm_fault_resolution_requires_authoritative_recipe_and_permissions() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x4008), AccessMode::Read))
        .expect("read fault resolves");

    assert_eq!(outcome.page_range, range(0x4000, 1));
    assert_eq!(outcome.entry, entry);
    assert!(!outcome.pmap_materialization_deferred);
    assert_eq!(
        aspace.resolve_fault(VmFault::new(UserVirtAddr(0x4008), AccessMode::Write)),
        Err(VmFaultError::ProtectionViolation)
    );
    assert_eq!(
        aspace.resolve_fault(VmFault::new(UserVirtAddr(0x8000), AccessMode::Read)),
        Err(VmFaultError::NoRecipe)
    );
}

#[test]
fn vm_fault_resolution_waits_behind_overlapping_writer() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x1000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let _writer = acquired(
        aspace
            .range_lock()
            .acquire(range(0x1000, 1), LockMode::ExclusiveWriter),
    );

    assert_eq!(
        aspace.resolve_fault(VmFault::new(UserVirtAddr(0x1000), AccessMode::Read)),
        Err(VmFaultError::WouldBlock)
    );
}

#[test]
fn vm_fault_materializes_pagebacked_anon_page_from_recipe_offset() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x2000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(USER_PAGE_SIZE as u64),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x3000), AccessMode::Write))
        .expect("fault resolves");
    let materialized = outcome
        .materialize_pagebacked_anon()
        .expect("pagebacked materialization");

    assert_eq!(
        materialized.page_index,
        crate::page_backed::PageIndex::new(2)
    );
    assert!(materialized.page.newly_installed);
    assert!(materialized.page.dirty);
}

#[test]
fn vm_fault_private_anon_read_uses_zero_frame_read_only() {
    setup_host_substrate();
    let zero_ppn = tx_substrate::page_allocator::zero_frame_ppn().expect("zero frame");
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x5000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x5000), AccessMode::Read))
        .expect("fault resolves");
    let materialized = outcome.materialize_pagebacked().expect("zero frame");

    assert_eq!(
        materialized.page_index,
        crate::page_backed::PageIndex::new(0)
    );
    assert_eq!(materialized.page.ppn, zero_ppn);
    assert_eq!(materialized.publish_prot, Prot::READ);

    aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish zero frame");

    assert_eq!(
        aspace.pmap().lookup(UserPage(5)),
        Some(PmapMappingSnapshot {
            ppn: zero_ppn,
            prot: Prot::READ,
        })
    );
}

#[test]
fn vm_fault_private_anon_write_uses_fresh_private_frame() {
    setup_host_substrate();
    let zero_ppn = tx_substrate::page_allocator::zero_frame_ppn().expect("zero frame");
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x6000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let read_outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x6000), AccessMode::Read))
        .expect("read fault resolves");
    let read_materialized = read_outcome
        .materialize_pagebacked()
        .expect("zero frame materializes");
    aspace
        .publish_fault_materialization(read_outcome, read_materialized)
        .expect("publish zero frame");

    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x6000), AccessMode::Write))
        .expect("fault resolves");
    let materialized = outcome
        .materialize_pagebacked()
        .expect("private frame materializes");
    let private_ppn = materialized.page.ppn;

    assert_ne!(private_ppn, zero_ppn);
    assert_eq!(materialized.publish_prot, Prot::READ_WRITE);

    let published = aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish private frame");

    assert!(published.replaced);
    assert_eq!(
        aspace.pmap().lookup(UserPage(6)),
        Some(PmapMappingSnapshot {
            ppn: private_ppn,
            prot: Prot::READ_WRITE,
        })
    );
}

#[test]
fn vm_fault_map_private_page_read_is_read_only_and_write_cows() {
    let aspace = AddressSpace::new();
    let backing = page_backing(0);
    let pc = match &backing {
        VmBacking::Page { pc, .. } => pc.clone(),
        _ => unreachable!("page_backing returns page backing"),
    };
    let entry = VmEntry::new(
        range(0x7000, 1),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        backing,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let read_outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x7000), AccessMode::Read))
        .expect("read fault resolves");
    let read_materialized = read_outcome
        .materialize_pagebacked()
        .expect("read materializes shared page");
    let shared_ppn = read_materialized.page.ppn;
    assert_eq!(read_materialized.publish_prot, Prot::READ);

    aspace
        .publish_fault_materialization(read_outcome, read_materialized)
        .expect("publish read-only shared page");
    assert_eq!(
        aspace.pmap().lookup(UserPage(7)),
        Some(PmapMappingSnapshot {
            ppn: shared_ppn,
            prot: Prot::READ,
        })
    );
    assert_eq!(pc.resident_pages(), 1);
    assert!(
        !pc.page_marks(crate::page_backed::PageIndex::new(0))
            .expect("source page marks")
            .dirty
    );

    let write_outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x7000), AccessMode::Write))
        .expect("write fault resolves");
    let write_materialized = write_outcome
        .materialize_pagebacked()
        .expect("write CoW materializes private page");
    let private_ppn = write_materialized.page.ppn;
    assert_ne!(private_ppn, shared_ppn);
    assert_eq!(write_materialized.publish_prot, Prot::READ_WRITE);
    assert_eq!(
        pc.lookup(crate::page_backed::PageIndex::new(0)),
        Some(shared_ppn)
    );

    let published = aspace
        .publish_fault_materialization(write_outcome, write_materialized)
        .expect("publish CoW replacement");

    assert!(published.replaced);
    assert_eq!(pc.resident_pages(), 1);
    assert_eq!(
        aspace.pmap().lookup(UserPage(7)),
        Some(PmapMappingSnapshot {
            ppn: private_ppn,
            prot: Prot::READ_WRITE,
        })
    );
}

#[test]
fn vm_fault_materialization_rejects_non_pagebacked_recipe() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x4000, 1),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x4000), AccessMode::Read))
        .expect("fault resolves");

    assert_eq!(
        outcome.materialize_pagebacked_anon().map(|_| ()),
        Err(VmFaultError::BackingMismatch)
    );
}
