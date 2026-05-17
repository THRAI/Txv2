//! VFS predicates — pure bool / Result<bool> functions over IdentRef + context.
//!
//! Per `txdoc:VFS-CHECKS-PREDICATES-1`
//! (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` §12): single source
//! of truth for structural and permission projections. Reading
//! `rnode.mode` and testing bits outside this module is a lint
//! violation.
//!
//! The DAC traversal and open-permission checks live here too — they
//! are the predicates the walker consumes at each component.

use crate::execution::Errno;
use crate::vfs::adapter::step_engine::IdentRef;

use super::checks::RootCtx;
use super::structure::{Credential, DEntry, InodeKind, InodeMeta, OpenFileFlags, RNode};
use crate::cred::Capability;

// ---------------------------------------------------------------------------
// Structural predicates (spec §12)
// ---------------------------------------------------------------------------

/// True when `d` is reachable from its mount namespace root (not
/// detached / unlinked).
pub fn namespace_live<'g>(_d: &IdentRef<'g, DEntry>, _ctx: &RootCtx) -> bool {
    // v1: stale-tolerant — always true for bringup.  Full
    // implementation checks the DEntry's `removed` flag and the
    // mount-namespace reachability chain.
    true
}

/// True when `r` has an installed payload (the backing is alive).
pub fn payload_live<'g>(r: &IdentRef<'g, RNode>) -> bool {
    matches!(
        r.backing(),
        super::structure::RNodeBacking::PageBacked { .. }
            | super::structure::RNodeBacking::StructBacked { .. }
            | super::structure::RNodeBacking::Symlink { .. }
            | super::structure::RNodeBacking::Directory
            | super::structure::RNodeBacking::Projected
    )
}

pub fn is_directory<'g>(r: &IdentRef<'g, RNode>) -> bool {
    r.meta().kind() == InodeKind::Directory
}

pub fn is_symlink<'g>(r: &IdentRef<'g, RNode>) -> bool {
    r.meta().kind() == InodeKind::Symlink
}

pub fn is_regular_file<'g>(r: &IdentRef<'g, RNode>) -> bool {
    r.meta().kind() == InodeKind::Regular
}

pub fn is_mount_root<'g>(d: &IdentRef<'g, DEntry>) -> bool {
    d.mounted_hint().is_some()
}

pub fn traverse_permitted<'g>(d: &IdentRef<'g, DEntry>, cred: &Credential) -> bool {
    check_descend_perm(&d.rnode().meta(), cred).is_ok()
}

/// May yield for directory-block read (informational I/O).  v1
/// returns `Ok(false)` — full implementation would check the
/// directory's child count via `FsOps::readdir`.
pub async fn is_empty_directory<'g>(_d: &IdentRef<'g, DEntry>) -> Result<bool, Errno> {
    Ok(false)
}

// ---------------------------------------------------------------------------
// DAC permission predicates (used by walker)
// ---------------------------------------------------------------------------

/// Pick the relevant POSIX mode-triplet bits for `cred` against the
/// inode's owner/group: owner (`>> 6`) > group (`>> 3`) > other.
/// Returns the bottom 3 bits — `(rwx)` for the chosen triplet.
pub fn select_perm_triplet(meta: &InodeMeta, cred: &Credential) -> u32 {
    let mode = meta.mode as u32;
    if cred.uid == meta.uid {
        (mode >> 6) & 0o7
    } else if cred.gid == meta.gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    }
}

/// DAC search/traversal check for an interior directory component.
/// POSIX rule: the appropriate triplet must have the `X` (execute =
/// search) bit set, unless the caller carries `CAP_DAC_OVERRIDE`.
///
/// Slice simplification: directories with at least one X bit also
/// satisfy `CAP_DAC_OVERRIDE`'s execute-bit constraint by definition,
/// so the override branch returns success unconditionally for
/// directories. Per `txdoc:VFS-CHECKS-PERMISSIONS-1`.
pub fn check_descend_perm(meta: &InodeMeta, cred: &Credential) -> Result<(), Errno> {
    if cred.effective_caps.contains(Capability::DAC_OVERRIDE) {
        return Ok(());
    }
    let bits = select_perm_triplet(meta, cred);
    if bits & 0o1 == 0 {
        return Err(Errno::EACCES);
    }
    Ok(())
}

/// DAC R/W check for terminal-component open.
/// Validates `OpenFileFlags::{read, write}` against the inode's
/// owner/group permission triplet.  `CAP_DAC_OVERRIDE` short-circuits.
///
/// Slice simplification: exec permission is NOT enforced here — that
/// lives in `exec_script` (Wave 4). Per `txdoc:VFS-CHECKS-PERMISSIONS-1`.
pub fn check_open_perm(
    meta: &InodeMeta,
    flags: OpenFileFlags,
    cred: &Credential,
) -> Result<(), Errno> {
    if cred.effective_caps.contains(Capability::DAC_OVERRIDE) {
        return Ok(());
    }
    let bits = select_perm_triplet(meta, cred);
    if flags.read && bits & 0o4 == 0 {
        return Err(Errno::EACCES);
    }
    if flags.write && bits & 0o2 == 0 {
        return Err(Errno::EACCES);
    }
    Ok(())
}
