//! Terminal acceptance and witness construction per
//! `txdoc:VFS-CHECKS-TERMINAL-ACCEPT-BUILD-WITNESS-1` (§8).
//!
//! Two responsibilities:
//!
//! 1. `accepts` — answers whether a terminal `WalkState` satisfies
//!    the caller's `WalkMode` expectations.
//! 2. `build_*_witness` — converts a terminal `PathResolution` into
//!    an `IdentRef`-carrying witness type for the consuming step's
//!    upgrade sub-phase.
//!
//! All witness constructors borrow an EBR guard so the returned
//! `IdentRef` handles are valid for the guard's epoch duration.

use crate::execution::{Errno, Guard};
use crate::vfs::checks::{DirectoryAtPath, EntityAtPath, ParentAndName};
use crate::vfs::structure::{InlineName, InodeKind};

use super::state::{PathResolution, WalkMode, WalkState};

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

/// Check whether a terminal `WalkState` satisfies the expectations
/// of the given `WalkMode`.
///
/// Per `txdoc:VFS-CHECKS-TERMINAL-ACCEPT-1`.
pub fn accepts(state: &WalkState, mode: WalkMode) -> bool {
    let WalkState::Terminal(_resolved) = state else {
        return false;
    };
    match mode {
        WalkMode::Entity
        | WalkMode::EntityUnfollowed
        | WalkMode::MountPoint
        | WalkMode::EntityOrParentAndName => true,
        WalkMode::ParentAndName => false,
        WalkMode::ParentAndNamedChild => true,
    }
}

// ---------------------------------------------------------------------------
// Witness constructors
// ---------------------------------------------------------------------------

/// Build an `EntityAtPath` witness from a terminal `PathResolution`.
///
/// The returned witness carries `IdentRef` handles valid for `guard`'s
/// epoch. The consuming step promotes them to `Cap` during STEP-4
/// phase 2.
pub fn build_entity_witness<'g>(
    resolved: &PathResolution,
    guard: &'g Guard<'_>,
) -> Result<EntityAtPath<'g>, Errno> {
    Ok(EntityAtPath::from_caps(
        &resolved.dentry,
        &resolved.rnode,
        guard,
    ))
}

/// Build a `DirectoryAtPath` witness from a terminal `PathResolution`.
///
/// Validates that the resolved RNode is a directory before
/// constructing the witness.
pub fn build_directory_witness<'g>(
    resolved: &PathResolution,
    guard: &'g Guard<'_>,
) -> Result<DirectoryAtPath<'g>, Errno> {
    if resolved.meta.kind() != InodeKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    Ok(DirectoryAtPath::from_caps(
        &resolved.dentry,
        &resolved.rnode,
        guard,
    ))
}

/// Build a `ParentAndName` witness from a terminal `PathResolution`
/// and the final path component name.
pub fn build_parent_and_name_witness<'g>(
    resolved: &PathResolution,
    name: InlineName,
    guard: &'g Guard<'_>,
) -> Result<ParentAndName<'g>, Errno> {
    Ok(ParentAndName::from_cap(&resolved.dentry, name, guard))
}
