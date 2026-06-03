#![cfg_attr(test, allow(unused_imports))]
use super::*;

#[test]
fn vm_try_mmap_places_nonfixed_mapping_in_first_recipe_gap() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed left");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x4000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed right");

    let outcome = aspace
        .try_mmap(VmMapRequest::anywhere(
            range(0x1000, 5),
            2,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("map into gap");

    assert_eq!(outcome.range, range(0x2000, 2));
    assert_eq!(outcome.commit.changed_pages, 2);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x2000)).expect("mapped").prot,
        Prot::READ_WRITE
    );
    assert_eq!(
        aspace.try_mmap(VmMapRequest::anywhere(
            range(0x1000, 5),
            2,
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        )),
        Err(VmMapError::NoFreeRange)
    );
}

#[test]
fn vm_try_mmap_fixed_replace_uses_declared_range() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 3),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(original.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("seed");

    let replacement = VmMapRequest::fixed(
        range(0x2000, 1),
        MapPlacement::FixedReplace,
        Prot::READ,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    let outcome = aspace.try_mmap(replacement).expect("fixed replace");

    assert_eq!(outcome.range, range(0x2000, 1));
    assert_eq!(outcome.commit.changed_pages, 2);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
        range(0x1000, 1)
    );
    assert_eq!(
        aspace
            .lookup(UserVirtAddr(0x2000))
            .expect("replacement")
            .prot,
        Prot::READ
    );
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x3000)).expect("right").range,
        range(0x3000, 1)
    );
}

#[test]
fn vm_try_mremap_moves_disjoint_range_and_preserves_source_survivors() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    let original = VmEntry::new(
        range(0x1000, 4),
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        page_backing(0),
    );
    map_reserved(aspace.reserve_map(original.clone(), MapPlacement::RequireFree))
        .commit()
        .expect("seed");

    let outcome = aspace
        .try_mremap(VmRemapRequest::new(range(0x2000, 2), range(0x8000, 2)))
        .expect("remap");

    assert_eq!(outcome.old_range, range(0x2000, 2));
    assert_eq!(outcome.new_range, range(0x8000, 2));
    assert_eq!(outcome.commit.changed_pages, 4);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
        range(0x1000, 1)
    );
    assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), None);
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x4000)).expect("right").range,
        range(0x4000, 1)
    );
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x8000)),
        Some(VmEntry::new(
            range(0x8000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            page_backing_like_entry(&original, USER_PAGE_SIZE as u64),
        ))
    );
}

#[test]
fn vm_try_mremap_disjoint_grow_extends_destination_tail_from_last_moved_entry() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");

    let outcome = aspace
        .try_mremap(VmRemapRequest::new(range(0x1000, 1), range(0x8000, 3)))
        .expect("grow move");

    assert_eq!(outcome.new_range, range(0x8000, 3));
    assert!(aspace.lookup(UserVirtAddr(0x1000)).is_none());
    assert_eq!(
        aspace.lookup(UserVirtAddr(0xa000)).expect("tail").range,
        range(0x8000, 3)
    );
}

#[test]
fn vm_try_mremap_disjoint_shrink_drops_source_tail() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");

    let outcome = aspace
        .try_mremap(VmRemapRequest::new(range(0x1000, 3), range(0x8000, 1)))
        .expect("shrink move");

    assert_eq!(outcome.new_range, range(0x8000, 1));
    assert!(aspace.lookup(UserVirtAddr(0x1000)).is_none());
    assert!(aspace.lookup(UserVirtAddr(0x2000)).is_none());
    assert!(aspace.lookup(UserVirtAddr(0x3000)).is_none());
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x8000)).expect("dest").range,
        range(0x8000, 1)
    );
}

#[test]
fn vm_try_mremap_rejects_overlapping_or_occupied_destination() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 4),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x8000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed dest");

    assert_eq!(
        aspace.try_mremap(VmRemapRequest::new(range(0x1000, 2), range(0x2000, 2))),
        Err(VmMapError::InvalidRange)
    );
    assert_eq!(
        aspace.try_mremap(VmRemapRequest::new(range(0x1000, 1), range(0x8000, 1))),
        Err(VmMapError::AlreadyMapped)
    );
}

#[test]
fn vm_try_mremap_fixed_replace_overwrites_occupied_destination() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x1000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x8000, 2),
            Prot::READ,
            VmEntryFlags::SHARED,
            page_backing(0),
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed dest");

    let outcome = aspace
        .try_mremap(VmRemapRequest::fixed_replace(
            range(0x1000, 1),
            range(0x8000, 1),
        ))
        .expect("fixed replace move");

    assert_eq!(outcome.new_range, range(0x8000, 1));
    assert!(aspace.lookup(UserVirtAddr(0x1000)).is_none());
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x8000)).expect("dest").prot,
        Prot::READ_WRITE
    );
    assert_eq!(
        aspace
            .lookup(UserVirtAddr(0x9000))
            .expect("dest survivor")
            .range,
        range(0x9000, 1)
    );
}

#[test]
fn vm_try_mremap_fixed_replace_tears_down_destination_pmap() {
    setup_host_substrate();
    let aspace = AddressSpace::new();

    let old_range = range(0x1000, 1);
    let new_range = range(0x8000, 1);
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            old_range,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");
    map_reserved(aspace.reserve_map(
        VmEntry::new(new_range, Prot::READ, VmEntryFlags::SHARED, page_backing(0)),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed dest");

    let write_outcome = aspace
        .resolve_fault(VmFault::new(UserVirtAddr(0x8000), AccessMode::Read))
        .expect("fault resolves");
    let write_materialized = write_outcome
        .materialize_pagebacked()
        .expect("dest materializes");
    aspace
        .publish_fault_materialization(write_outcome, write_materialized)
        .expect("publish dest pmap");
    assert!(aspace.pmap().lookup(UserPage(8)).is_some());

    let outcome = aspace
        .try_mremap(VmRemapRequest::fixed_replace(old_range, new_range))
        .expect("fixed replace move");

    assert_eq!(outcome.new_range, new_range);
    assert!(aspace.pmap().lookup(UserPage(8)).is_none());
    assert!(aspace.lookup(UserVirtAddr(0x1000)).is_none());
    assert_eq!(
        aspace.lookup(UserVirtAddr(0x8000)).expect("dest").prot,
        Prot::READ_WRITE
    );
}

#[test]
fn vm_try_mremap_shrinks_in_place_and_preserves_prefix() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x10000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");

    let outcome = aspace
        .try_mremap(VmRemapRequest::in_place(
            range(0x10000, 3),
            range(0x10000, 1),
        ))
        .expect("shrink");

    assert_eq!(outcome.old_range, range(0x10000, 3));
    assert_eq!(outcome.new_range, range(0x10000, 1));
    assert!(aspace.lookup(UserVirtAddr(0x10000)).is_some());
    assert!(aspace.lookup(UserVirtAddr(0x11000)).is_none());
}

#[test]
fn vm_try_mremap_grows_in_place_only_when_extension_is_free() {
    setup_host_substrate();
    let aspace = AddressSpace::new();
    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x20000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed old");

    let outcome = aspace
        .try_mremap(VmRemapRequest::in_place(
            range(0x20000, 1),
            range(0x20000, 2),
        ))
        .expect("grow");

    assert_eq!(outcome.new_range, range(0x20000, 2));
    assert_eq!(
        aspace
            .lookup(UserVirtAddr(0x21000))
            .expect("extension")
            .range,
        range(0x20000, 2)
    );

    map_reserved(aspace.reserve_map(
        VmEntry::new(
            range(0x24000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ),
        MapPlacement::RequireFree,
    ))
    .commit()
    .expect("seed blocker");

    assert_eq!(
        aspace.try_mremap(VmRemapRequest::in_place(
            range(0x20000, 2),
            range(0x20000, 5)
        )),
        Err(VmMapError::AlreadyMapped)
    );
}
