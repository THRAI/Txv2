#![cfg_attr(test, allow(unused_imports))]
use super::*;

#[test]
fn vm_pmap_walk_range_returns_only_mapped_pages_in_ascending_order() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x6000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    for fault_addr in [0x6000, 0x8000] {
        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(fault_addr), AccessMode::Write))
            .expect("fault resolves");
        let materialized = outcome.materialize_pagebacked_anon().expect("materialize");
        aspace
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }

    let walked = aspace.pmap().walk_range(range(0x6000, 4));

    assert_eq!(walked.len(), 2);
    assert_eq!(walked[0].0, UserPage(6));
    assert_eq!(walked[1].0, UserPage(8));
    assert!(walked[0].1.prot == Prot::READ_WRITE);
}

#[test]
fn vm_pmap_walk_range_excludes_pages_outside_the_query_range() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0xa000, 3),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    for fault_addr in [0xa000, 0xb000, 0xc000] {
        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(fault_addr), AccessMode::Write))
            .expect("fault resolves");
        let materialized = outcome.materialize_pagebacked_anon().expect("materialize");
        aspace
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }

    let walked_middle = aspace.pmap().walk_range(range(0xb000, 1));

    assert_eq!(walked_middle.len(), 1);
    assert_eq!(walked_middle[0].0, UserPage(11));
}

#[test]
fn vm_pmap_walk_range_returns_empty_when_no_mappings_exist() {
    let aspace = AddressSpace::new();
    let walked = aspace.pmap().walk_range(range(0x4000, 4));
    assert!(walked.is_empty());
}

#[test]
fn vm_mincore_reports_resident_and_absent_pages_in_range_order() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0xd000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    for fault_addr in [0xd000_usize, 0xf000] {
        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(fault_addr), AccessMode::Write))
            .expect("fault resolves");
        let materialized = outcome.materialize_pagebacked_anon().expect("materialize");
        aspace
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }

    let snapshot = aspace.mincore(range(0xd000, 4));

    assert_eq!(snapshot, alloc::vec![true, false, true, false]);
}

#[test]
fn vm_mincore_reports_all_absent_for_unmapped_range() {
    let aspace = AddressSpace::new();
    let snapshot = aspace.mincore(range(0xe000, 3));
    assert_eq!(snapshot, alloc::vec![false, false, false]);
}

#[test]
fn vm_madvise_noop_advice_preserves_recipes_and_pmap() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x12000, 2),
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    for advice in [
        crate::vm::MadviseAdvice::Normal,
        crate::vm::MadviseAdvice::Random,
        crate::vm::MadviseAdvice::Sequential,
        crate::vm::MadviseAdvice::WillNeed,
    ] {
        assert!(aspace.madvise(range(0x12000, 2), advice).is_ok());
    }

    assert_eq!(
        aspace.lookup(UserVirtAddr(0x12000)),
        Some(VmEntry::new(
            range(0x12000, 2),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
    );
}

#[test]
fn vm_madvise_dontneed_tears_down_ptes_and_preserves_recipe() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let mapped = range(0x14000, 2);
    let entry = VmEntry::new(
        mapped,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("map");

    for fault_addr in [0x14000_usize, 0x15000] {
        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(fault_addr), AccessMode::Write))
            .expect("fault");
        let materialized = outcome.materialize_pagebacked().expect("materialize");
        aspace
            .publish_fault_materialization(outcome, materialized)
            .expect("publish");
    }
    assert_eq!(aspace.mincore(mapped), alloc::vec![true, true]);

    aspace
        .madvise(mapped, crate::vm::MadviseAdvice::DontNeed)
        .expect("dontneed");

    assert_eq!(aspace.mincore(mapped), alloc::vec![false, false]);
    assert_eq!(aspace.lookup(UserVirtAddr(0x14000)), Some(entry.clone()));
    assert_eq!(aspace.lookup(UserVirtAddr(0x15000)), Some(entry));
}

#[test]
fn vm_madvise_free_acts_as_dontneed_for_anon() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let mapped = range(0x16000, 1);
    let entry = VmEntry::new(
        mapped,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("map");
    let outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x16000), AccessMode::Write))
        .expect("fault");
    let materialized = outcome.materialize_pagebacked().expect("materialize");
    aspace
        .publish_fault_materialization(outcome, materialized)
        .expect("publish");
    assert_eq!(aspace.mincore(mapped), alloc::vec![true]);

    aspace
        .madvise(mapped, crate::vm::MadviseAdvice::Free)
        .expect("free");

    assert_eq!(aspace.mincore(mapped), alloc::vec![false]);
    assert_eq!(aspace.lookup(UserVirtAddr(0x16000)), Some(entry));
}

#[test]
fn vm_msync_is_done_for_anon_only_address_space() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x14000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        aspace.msync(range(0x14000, 2), &guard),
        tx_substrate::step_v3::StepOutcome::Done(())
    );
}

#[test]
fn vm_msync_skips_anon_page_containers_and_returns_done() {
    let aspace = AddressSpace::new();
    let entry = VmEntry::new(
        range(0x16000, 2),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
        .commit()
        .expect("map");

    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        aspace.msync(range(0x16000, 2), &guard),
        tx_substrate::step_v3::StepOutcome::Done(())
    );
}
