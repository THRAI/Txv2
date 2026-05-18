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
use super::structure::{Credential, DEntry, InodeKind, InodeMeta, OpenFileFlags, RNode, S_ISVTX};
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

/// DAC remove-entry check for `unlink(2)` / `unlinkat(2)` /
/// `rmdir(2)`. Pure POSIX rule, applied against the *parent* directory:
///
/// 1. **Write** bit on parent's appropriate triplet must be set
///    (writing to a directory means modifying its entry list).
///    `CAP_DAC_OVERRIDE` bypasses.
/// 2. **Search** bit must also be set (the entry's name is being
///    addressed). `CAP_DAC_OVERRIDE` bypasses. Note: most callers
///    have already exercised this through `check_descend_perm`
///    during path resolution; we re-check defensively because
///    `unlinkat(AT_REMOVEDIR=0, ".")` and similar shapes can land
///    here without a prior descend.
/// 3. **Sticky bit** (`S_ISVTX`) on parent: when set, only the
///    file's owner, the parent's owner, or a caller with
///    `CAP_FOWNER` (or `euid == 0` per the standard
///    is_privileged_for shortcut) may remove the entry. This is
///    the "/tmp protection rule" — POSIX `man 2 unlink`,
///    `man 7 inode` §"sticky bit".
///
/// Maps to the standard `Errno::EACCES` (rules 1–2) and
/// `Errno::EPERM` (rule 3) — matching Linux's `unlink(2)`
/// distinction: write-bit failure is EACCES, sticky-bit ownership
/// failure is EPERM.
pub fn check_unlink_perm(
    parent_meta: &InodeMeta,
    child_meta: &InodeMeta,
    cred: &Credential,
) -> Result<(), Errno> {
    let has_dac_override = cred.effective_caps.contains(Capability::DAC_OVERRIDE);
    if !has_dac_override {
        let bits = select_perm_triplet(parent_meta, cred);
        // Write + search are both needed to remove an entry by name.
        if bits & 0o3 != 0o3 {
            return Err(Errno::EACCES);
        }
    }
    // Sticky-bit rule. CAP_DAC_OVERRIDE does NOT bypass sticky —
    // POSIX requires either ownership match or CAP_FOWNER.
    if (parent_meta.mode & S_ISVTX) != 0 {
        let owns_child = cred.uid == child_meta.uid;
        let owns_parent = cred.uid == parent_meta.uid;
        let has_fowner = cred.effective_caps.contains(Capability::FOWNER);
        // Privileged shortcut: euid 0 short-circuits, consistent with
        // Cred::is_privileged_for's POSIX-style behaviour.
        let is_root = cred.uid == 0;
        if !(owns_child || owns_parent || has_fowner || is_root) {
            return Err(Errno::EPERM);
        }
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
