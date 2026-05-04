//! Read-only VM projections.
//!
//! Procfs/sysfs adapters should consume projection helpers from this module
//! rather than becoming owners of AddressSpace structure. Projection rows are
//! non-authoritative views: they expose stable VM facts without leaking caps or
//! granting backend authority.

use alloc::vec::Vec;

use crate::vm::{AddressSpace, AddressSpaceStats, Prot, UserRange, VmBacking, VmEntryFlags};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddressSpaceProjection {
    pub stats: AddressSpaceStats,
    pub mappings: Vec<VmMappingProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmMappingProjection {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBackingProjection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmBackingProjection {
    None,
    PrivateAnon,
    Page { offset: u64 },
}

pub fn project_address_space(aspace: &AddressSpace) -> AddressSpaceProjection {
    AddressSpaceProjection {
        stats: aspace.stats(),
        mappings: aspace
            .recipes_snapshot()
            .into_iter()
            .map(|entry| VmMappingProjection {
                range: entry.range,
                prot: entry.prot,
                flags: entry.flags,
                backing: project_backing(&entry.backing),
            })
            .collect(),
    }
}

pub fn address_space_stats(aspace: &AddressSpace) -> AddressSpaceStats {
    aspace.stats()
}

const fn project_backing(backing: &VmBacking) -> VmBackingProjection {
    match backing {
        VmBacking::None => VmBackingProjection::None,
        VmBacking::PrivateAnon => VmBackingProjection::PrivateAnon,
        VmBacking::Page { offset, .. } => VmBackingProjection::Page { offset: *offset },
    }
}
