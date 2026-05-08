#![allow(unused_imports)]
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
            page_backing_like(&original.backing, USER_PAGE_SIZE as u64),
        ))
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
