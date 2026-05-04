use tx_substrate::epoch::Guard;
use tx_substrate::zone::IdentRef;

use crate::mount::structure::{MountIdentity, MountNamespace};
use crate::vfs::structure::DEntry;

pub struct MountTraversal<'g> {
    pub mount: IdentRef<'g, MountIdentity>,
    pub root_dentry: IdentRef<'g, DEntry>,
}

pub enum DotDotResult<'g> {
    StayAtRoot,
    Cross(DotDotCross<'g>),
    DetachedFail,
}

pub struct DotDotCross<'g> {
    pub parent_mount: IdentRef<'g, MountIdentity>,
    pub mountpoint: IdentRef<'g, DEntry>,
}

pub fn lookup_mount_at<'g>(
    _mountpoint: IdentRef<'g, DEntry>,
    _mnt_ns: IdentRef<'g, MountNamespace>,
    _guard: &'g Guard<'g>,
) -> Option<MountTraversal<'g>> {
    None
}

pub fn is_mount_root<'g>(
    _dentry: IdentRef<'g, DEntry>,
    _mnt_ns: IdentRef<'g, MountNamespace>,
    _guard: &'g Guard<'g>,
) -> bool {
    false
}

pub fn is_mountpoint_in<'g>(
    _dentry: IdentRef<'g, DEntry>,
    _mnt_ns: IdentRef<'g, MountNamespace>,
    _guard: &'g Guard<'g>,
) -> bool {
    false
}

pub fn synthesize_dotdot_cross<'g>(
    _current_mount: IdentRef<'g, MountIdentity>,
    _mnt_ns: IdentRef<'g, MountNamespace>,
    _guard: &'g Guard<'g>,
) -> DotDotResult<'g> {
    DotDotResult::DetachedFail
}
