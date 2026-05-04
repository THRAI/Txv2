use tx_substrate::epoch::Guard;
use tx_substrate::zone::IdentRef;

use crate::step::Errno;
use crate::vfs::checks::resolution::state::RootCtxRef;
use crate::vfs::fs_ops::Credential;
use crate::vfs::structure::{DEntry, DEntryChildLookup, NameOwned, RNode};

// RFX-VFS-P1-001/RFX-VFS-P2-003: Replace this conservative predicate with
// full binding-chain reachability from RootCtxRef once namespace proof policy is
// available.
pub fn namespace_live<'g>(_dentry: &IdentRef<'g, DEntry>, _ctx: &RootCtxRef<'g>) -> bool {
    true
}

pub fn child_binding_points_to<'g>(
    parent: &IdentRef<'g, DEntry>,
    name: &NameOwned,
    expected_child: &IdentRef<'g, DEntry>,
    guard: &'g Guard<'_>,
) -> bool {
    match parent.children.lookup(name, guard) {
        DEntryChildLookup::Found(child) => child.raw() == expected_child.raw(),
        DEntryChildLookup::Missing => false,
    }
}

// RFX-VFS-P1-002: Replace this conservative predicate with real payload/link/open
// pin checks once RNode payload retention is implemented.
pub fn payload_live<'g>(_rnode: &IdentRef<'g, RNode>) -> bool {
    true
}

pub fn is_directory<'g>(rnode: &IdentRef<'g, RNode>) -> bool {
    rnode.meta.is_directory()
}

pub fn is_symlink<'g>(rnode: &IdentRef<'g, RNode>) -> bool {
    rnode.meta.is_symlink()
}

pub fn is_regular_file<'g>(rnode: &IdentRef<'g, RNode>) -> bool {
    rnode.meta.is_regular_file()
}

// RFX-VFS-P1-009/RFX-VFS-P2-006: Delegate to MOUNT once its check API can be
// consumed without moving guard-scoped observations out of VFS predicates.
pub fn is_mount_root<'g>(
    _dentry: &IdentRef<'g, DEntry>,
    _ctx: &RootCtxRef<'g>,
    _guard: &'g Guard<'_>,
) -> bool {
    false
}

// RFX-VFS-P1-003/RFX-VFS-P2-005: Replace this conservative predicate with
// credential and execute/search permission checks once cred/process interfaces
// exist.
pub fn traverse_permitted<'g>(_dentry: &IdentRef<'g, DEntry>, _cred: &Credential) -> bool {
    true
}

// RFX-VFS-P1-004: Replace with backend-backed directory emptiness once directory
// IO / readdir is available.
pub fn is_empty_directory<'g>(
    _dentry: &IdentRef<'g, DEntry>,
    _guard: &'g Guard<'_>,
) -> Result<bool, Errno> {
    Err(Errno::NotImplemented)
}
