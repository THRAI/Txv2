//! Pure VM observation predicates.
//!
//! These `require_*` helpers are staged check-result values, not the final
//! guard-scoped `IdentRef` witness shape. They observe authoritative recipes
//! and return the evidence execution needs, while reservation and mutation stay
//! in `execution.rs`.

use crate::vm::{
    AccessMode, AddressSpace, MapPlacement, UserRange, VmEntry, VmFault, VmFaultError,
    VmFaultMaterialization, VmFaultMaterializationBacking, VmFaultOutcome, VmMapError,
};

pub fn require_fault_recipe(
    aspace: &AddressSpace,
    fault: VmFault,
) -> Result<VmFaultOutcome, VmFaultError> {
    let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
    let entry = aspace.lookup(fault.addr).ok_or(VmFaultError::NoRecipe)?;
    if !permits_fault(&entry, fault.access) {
        return Err(VmFaultError::ProtectionViolation);
    }

    Ok(VmFaultOutcome {
        page_range,
        entry,
        access: fault.access,
        pmap_materialization_deferred: aspace.pmap().materialization_deferred(),
    })
}

pub fn require_fault_publication(
    aspace: &AddressSpace,
    outcome: &VmFaultOutcome,
    materialization: &VmFaultMaterialization,
) -> Result<VmEntry, VmFaultError> {
    let entry = aspace
        .lookup(outcome.page_range.start())
        .ok_or(VmFaultError::StaleRecipe)?;
    if entry != outcome.entry || !permits_fault(&entry, outcome.access) {
        return Err(VmFaultError::StaleRecipe);
    }

    match (&entry.backing, materialization.backing) {
        (crate::vm::VmBacking::Page { .. }, VmFaultMaterializationBacking::PageBacked) => {
            if outcome.backing_page_index()? != materialization.page_index {
                return Err(VmFaultError::StaleRecipe);
            }
        }
        (crate::vm::VmBacking::PrivateAnon, VmFaultMaterializationBacking::PrivateAnon) => {
            if outcome.private_anon_page_index()? != materialization.page_index {
                return Err(VmFaultError::StaleRecipe);
            }
        }
        _ => return Err(VmFaultError::StaleRecipe),
    }

    Ok(entry)
}

pub fn require_map_admission(
    aspace: &AddressSpace,
    entry: &VmEntry,
    placement: MapPlacement,
) -> Result<(), VmMapError> {
    let guard = tx_substrate::epoch::guard();
    aspace.recipes.validate_map(entry, placement, &guard)
}

pub const fn require_disjoint_remap(
    old_range: UserRange,
    new_range: UserRange,
) -> Result<(), VmMapError> {
    if old_range.overlaps(new_range) || old_range.len() != new_range.len() {
        return Err(VmMapError::InvalidRange);
    }
    Ok(())
}

pub(in crate::vm) const fn permits_fault(entry: &VmEntry, access: AccessMode) -> bool {
    entry.prot.permits(access)
}
