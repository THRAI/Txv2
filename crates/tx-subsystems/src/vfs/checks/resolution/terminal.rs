use alloc::boxed::Box;

use tx_substrate::epoch::Guard;

use crate::step::Errno;
use crate::vfs::checks::predicates;
use crate::vfs::checks::resolution::state::{WalkMode, WalkState};
use crate::vfs::checks::witness::{
    EntityAtPath, EntityOrParentAndName, MountPointAtPath, ParentAndName, ParentAndNamedChild,
    WalkWitness,
};
use crate::vfs::structure::{DEntryChildLookup, NameOwned};

pub(crate) fn accepts<'a, 'g>(
    mode: WalkMode,
    state: &WalkState<'a, 'g>,
    guard: &'g Guard<'_>,
) -> bool {
    if !state.remaining.is_empty() {
        return false;
    }

    match mode {
        WalkMode::Entity => {
            let rnode = state.cursor.rnode.ident_ref(guard);
            predicates::payload_live(&rnode)
        }
        WalkMode::EntityUnfollowed => true,
        WalkMode::MountPoint => predicates::is_mount_root(&state.cursor, &state.root_ctx, guard),
        WalkMode::ParentAndName
        | WalkMode::ParentAndNamedChild
        | WalkMode::EntityOrParentAndName => false,
    }
}

pub(crate) fn build_witness<'a, 'g>(
    mode: WalkMode,
    state: WalkState<'a, 'g>,
    guard: &'g Guard<'_>,
) -> Result<WalkWitness<'g>, Errno> {
    match mode {
        WalkMode::Entity => {
            let rnode = state.cursor.rnode.ident_ref(guard);
            if predicates::is_symlink(&rnode) {
                // RFX-VFS-P2-004: Follow terminal symlink once symlink payload
                // read is available. Entity mode cannot resolve a path whose
                // final component is a symlink.
                return Err(Errno::NotImplemented);
            }
            Ok(WalkWitness::Entity(EntityAtPath::new(
                state.cursor,
                rnode,
                state.current_mount,
            )))
        }
        WalkMode::EntityUnfollowed => {
            let rnode = state.cursor.rnode.ident_ref(guard);
            Ok(WalkWitness::EntityUnfollowed(EntityAtPath::new(
                state.cursor,
                rnode,
                state.current_mount,
            )))
        }
        WalkMode::MountPoint => {
            let rnode = state.cursor.rnode.ident_ref(guard);
            Ok(WalkWitness::MountPoint(Box::new(MountPointAtPath::new(
                state.cursor,
                rnode,
                state.current_mount,
            ))))
        }
        WalkMode::ParentAndName
        | WalkMode::ParentAndNamedChild
        | WalkMode::EntityOrParentAndName => build_penultimate_witness(mode, state, guard),
    }
}

pub(crate) fn build_penultimate_witness<'a, 'g>(
    mode: WalkMode,
    state: WalkState<'a, 'g>,
    guard: &'g Guard<'_>,
) -> Result<WalkWitness<'g>, Errno> {
    let name = state
        .remaining
        .peek_last_component()?
        .ok_or(Errno::NoEntry)
        .and_then(NameOwned::from_component)?;

    match mode {
        WalkMode::ParentAndName => Ok(WalkWitness::ParentAndName(ParentAndName::new(
            state.cursor,
            state.current_mount,
            name,
        ))),
        WalkMode::ParentAndNamedChild => match state.cursor.children.lookup(&name, guard) {
            DEntryChildLookup::Found(child) => {
                let child_ref = (*child).into_ident_ref();
                let child_rnode = child_ref.rnode.ident_ref(guard);
                // RFX-VFS-P2-006: child_mount remains current_mount until MOUNT
                // forward crossing is wired into final child resolution. The
                // temporary Cap only duplicates the same guard-scoped mount
                // observation while IdentRef itself is intentionally non-Copy.
                let mount_cap = state.current_mount.to_cap().map_err(|_| Errno::Stale)?;
                let parent_mount = mount_cap.ident_ref(guard);
                let child_mount = mount_cap.ident_ref(guard);
                Ok(WalkWitness::ParentAndNamedChild(ParentAndNamedChild::new(
                    state.cursor,
                    parent_mount,
                    child_ref,
                    child_rnode,
                    child_mount,
                    name,
                )))
            }
            DEntryChildLookup::Missing => Err(Errno::NoEntry),
        },
        WalkMode::EntityOrParentAndName => match state.cursor.children.lookup(&name, guard) {
            DEntryChildLookup::Found(child) => {
                let child_ref = (*child).into_ident_ref();
                let child_rnode = child_ref.rnode.ident_ref(guard);
                Ok(WalkWitness::EntityOrParent(EntityOrParentAndName::Present(
                    EntityAtPath::new(child_ref, child_rnode, state.current_mount),
                )))
            }
            DEntryChildLookup::Missing => {
                // RFX-VFS-P2-007: Missing currently means "not in this known
                // children index"; split Found/Negative/Unknown when dcache
                // negative entries and backend probe policy are implemented.
                Ok(WalkWitness::EntityOrParent(EntityOrParentAndName::Absent(
                    Box::new(ParentAndName::new(state.cursor, state.current_mount, name)),
                )))
            }
        },
        WalkMode::Entity | WalkMode::EntityUnfollowed | WalkMode::MountPoint => Err(Errno::Invalid),
    }
}
