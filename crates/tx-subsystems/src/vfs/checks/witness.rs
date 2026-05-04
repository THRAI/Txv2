use core::marker::PhantomData;

use tx_substrate::zone::IdentRef;

use crate::mount::structure::MountIdentity;
use crate::vfs::structure::{DEntry, NameOwned, RNode};

pub struct EntityAtPath<'g> {
    pub dentry: IdentRef<'g, DEntry>,
    pub rnode: IdentRef<'g, RNode>,
    pub mount: IdentRef<'g, MountIdentity>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> EntityAtPath<'g> {
    pub(in crate::vfs::checks) fn new(
        dentry: IdentRef<'g, DEntry>,
        rnode: IdentRef<'g, RNode>,
        mount: IdentRef<'g, MountIdentity>,
    ) -> Self {
        Self {
            dentry,
            rnode,
            mount,
            _not_send_sync: PhantomData,
        }
    }
}

pub struct DirectoryAtPath<'g>(EntityAtPath<'g>);
pub struct SymlinkAtPath<'g>(EntityAtPath<'g>);

impl<'g> DirectoryAtPath<'g> {
    pub(in crate::vfs::checks) fn new(entity: EntityAtPath<'g>) -> Self {
        Self(entity)
    }

    pub fn entity(&self) -> &EntityAtPath<'g> {
        &self.0
    }
}

impl<'g> SymlinkAtPath<'g> {
    pub(in crate::vfs::checks) fn new(entity: EntityAtPath<'g>) -> Self {
        Self(entity)
    }

    pub fn entity(&self) -> &EntityAtPath<'g> {
        &self.0
    }
}

pub struct ParentAndName<'g> {
    pub parent: IdentRef<'g, DEntry>,
    pub parent_mount: IdentRef<'g, MountIdentity>,
    pub name: NameOwned,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> ParentAndName<'g> {
    pub(in crate::vfs::checks) fn new(
        parent: IdentRef<'g, DEntry>,
        parent_mount: IdentRef<'g, MountIdentity>,
        name: NameOwned,
    ) -> Self {
        Self {
            parent,
            parent_mount,
            name,
            _not_send_sync: PhantomData,
        }
    }
}

pub struct ParentAndNamedChild<'g> {
    pub parent: IdentRef<'g, DEntry>,
    pub parent_mount: IdentRef<'g, MountIdentity>,
    pub child: IdentRef<'g, DEntry>,
    pub child_rnode: IdentRef<'g, RNode>,
    pub child_mount: IdentRef<'g, MountIdentity>,
    pub name: NameOwned,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> ParentAndNamedChild<'g> {
    pub(in crate::vfs::checks) fn new(
        parent: IdentRef<'g, DEntry>,
        parent_mount: IdentRef<'g, MountIdentity>,
        child: IdentRef<'g, DEntry>,
        child_rnode: IdentRef<'g, RNode>,
        child_mount: IdentRef<'g, MountIdentity>,
        name: NameOwned,
    ) -> Self {
        Self {
            parent,
            parent_mount,
            child,
            child_rnode,
            child_mount,
            name,
            _not_send_sync: PhantomData,
        }
    }

    pub fn child_rnode(&self) -> &IdentRef<'g, RNode> {
        &self.child_rnode
    }
}

pub struct UnlinkableNonDirChild<'g>(ParentAndNamedChild<'g>);
pub struct RmdirableDirChild<'g>(ParentAndNamedChild<'g>);

impl<'g> UnlinkableNonDirChild<'g> {
    pub(in crate::vfs::checks) fn new(child: ParentAndNamedChild<'g>) -> Self {
        Self(child)
    }

    pub fn child(&self) -> &ParentAndNamedChild<'g> {
        &self.0
    }
}

impl<'g> RmdirableDirChild<'g> {
    pub(in crate::vfs::checks) fn new(child: ParentAndNamedChild<'g>) -> Self {
        Self(child)
    }

    pub fn child(&self) -> &ParentAndNamedChild<'g> {
        &self.0
    }
}

pub enum EntityOrParentAndName<'g> {
    Present(EntityAtPath<'g>),
    Absent(ParentAndName<'g>),
}

impl<'g> EntityOrParentAndName<'g> {
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent(_))
    }
}

pub struct MountPointAtPath<'g> {
    pub dentry: IdentRef<'g, DEntry>,
    pub rnode: IdentRef<'g, RNode>,
    pub mount: IdentRef<'g, MountIdentity>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> MountPointAtPath<'g> {
    pub(in crate::vfs::checks) fn new(
        dentry: IdentRef<'g, DEntry>,
        rnode: IdentRef<'g, RNode>,
        mount: IdentRef<'g, MountIdentity>,
    ) -> Self {
        Self {
            dentry,
            rnode,
            mount,
            _not_send_sync: PhantomData,
        }
    }
}

pub struct RealPath<'g> {
    pub dentry: IdentRef<'g, DEntry>,
    pub mount: IdentRef<'g, MountIdentity>,
    pub path: CanonicalPath,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> RealPath<'g> {
    pub(in crate::vfs::checks) fn new(
        dentry: IdentRef<'g, DEntry>,
        mount: IdentRef<'g, MountIdentity>,
        path: CanonicalPath,
    ) -> Self {
        Self {
            dentry,
            mount,
            path,
            _not_send_sync: PhantomData,
        }
    }
}

pub struct CanonicalPath {
    pub len: u16,
    pub bytes: [u8; 4096],
}

pub(crate) enum WalkWitness<'g> {
    Entity(EntityAtPath<'g>),
    EntityUnfollowed(EntityAtPath<'g>),
    ParentAndName(ParentAndName<'g>),
    ParentAndNamedChild(ParentAndNamedChild<'g>),
    EntityOrParent(EntityOrParentAndName<'g>),
    MountPoint(MountPointAtPath<'g>),
    RealPath(RealPath<'g>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_not_send_sync<T: ?Sized>() {}

    #[test]
    fn witness_types_keep_non_send_sync_marker() {
        assert_not_send_sync::<EntityAtPath<'_>>();
        assert_not_send_sync::<ParentAndName<'_>>();
        assert_not_send_sync::<MountPointAtPath<'_>>();
    }
}
