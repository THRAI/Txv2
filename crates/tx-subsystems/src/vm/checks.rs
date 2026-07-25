//! Pure VM observation predicates.
//!
//! These `require_*` helpers are staged check-result values, not the final
//! guard-scoped `IdentRef` witness shape. They observe authoritative recipes
//! and return the evidence execution needs, while reservation and mutation stay
//! in `execution.rs`.

use crate::vm::adapter::step_engine::{self as step_engine};
use crate::vm::structure::recipe_tree::VmEntryView;
use crate::vm::{
    AccessMode, AddressSpace, MapPlacement, UserRange, VmEntry, VmEntryBacking, VmFault,
    VmFaultError, VmFaultMaterialization, VmFaultMaterializationBacking, VmFaultOutcome,
    VmMapError, VmRemapPlacement,
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
        private_identity: entry.private_identity(),
        entry,
        access: fault.access,
        pmap_materialization_deferred: aspace.pmap().materialization_deferred(),
    })
}

pub fn require_fault_publication(
    aspace: &AddressSpace,
    outcome: &VmFaultOutcome,
    materialization: &VmFaultMaterialization,
) -> Result<(), VmFaultError> {
    let guard = step_engine::guard();
    let entry = aspace
        .recipes
        .lookup_view(outcome.page_range.start(), &guard)
        .ok_or(VmFaultError::StaleRecipe)?;
    if !view_matches_entry(entry, &outcome.entry) || !entry.permits_fault(outcome.access) {
        return Err(VmFaultError::StaleRecipe);
    }
    if entry.private.map(|private| private.raw()) != outcome.private_identity {
        return Err(VmFaultError::StaleRecipe);
    }

    match (entry.backing, materialization.backing) {
        (VmEntryBacking::None, VmFaultMaterializationBacking::Special(special)) => {
            if view_matches_entry(entry, &outcome.entry)
                && outcome.entry.special_backing() == Some(special)
            {
                return Ok(());
            }
            return Err(VmFaultError::StaleRecipe);
        }
        (VmEntryBacking::Page { .. }, VmFaultMaterializationBacking::PageBacked) => {
            if outcome.backing_page_index()? != materialization.page_index {
                return Err(VmFaultError::StaleRecipe);
            }
        }
        (VmEntryBacking::PrivateAnon, VmFaultMaterializationBacking::PrivateAnon) => {
            if outcome.private_anon_page_index()? != materialization.page_index {
                return Err(VmFaultError::StaleRecipe);
            }
        }
        _ => return Err(VmFaultError::StaleRecipe),
    }

    Ok(())
}

fn view_matches_entry(view: VmEntryView<'_>, entry: &VmEntry) -> bool {
    if view.range != entry.range
        || view.prot != entry.prot
        || view.flags != entry.flags
        || view.ufd_registration != entry.ufd_registration
        || view.backing != entry.backing_kind()
        || view.special != entry.special_backing()
    {
        return false;
    }

    match view.backing {
        VmEntryBacking::None | VmEntryBacking::PrivateAnon => true,
        VmEntryBacking::Page { offset } => {
            let Some((view_pc, view_offset)) = view.page else {
                return false;
            };
            let Some((entry_pc, entry_offset)) = entry.page_backing() else {
                return false;
            };
            view_offset == offset && entry_offset == offset && view_pc == entry_pc
        }
    }
}

pub fn require_map_admission(
    aspace: &AddressSpace,
    entry: &VmEntry,
    placement: MapPlacement,
) -> Result<(), VmMapError> {
    let guard = step_engine::guard();
    aspace.recipes.validate_map(entry, placement, &guard)
}

pub const fn require_disjoint_remap(
    old_range: UserRange,
    new_range: UserRange,
) -> Result<(), VmMapError> {
    if old_range.overlaps(new_range) {
        return Err(VmMapError::InvalidRange);
    }
    Ok(())
}

pub const fn require_remap_shape(
    old_range: UserRange,
    new_range: UserRange,
    placement: VmRemapPlacement,
) -> Result<(), VmMapError> {
    match placement {
        VmRemapPlacement::Move => require_disjoint_remap(old_range, new_range),
        VmRemapPlacement::InPlace => {
            if old_range.start().0 != new_range.start().0 {
                return Err(VmMapError::InvalidRange);
            }
            Ok(())
        }
    }
}

pub(in crate::vm) const fn permits_fault(entry: &VmEntry, access: AccessMode) -> bool {
    entry.prot.permits(access)
}
