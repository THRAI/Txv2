//! VFS live-node shells and filesystem namespace backend trait.

use core::fmt;

use crate::device::CharDeviceBinding;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::mount::{MountIdentity, MountNamespace, MountPayload};
use crate::page_backed::PageContainer;
use tx_substrate::zone::{self, Cap, Weak, Zone, ZoneAllocated, ZoneError};

pub const VFS_NAME_MAX: usize = 255;

static DENTRY_ZONE: Zone<DEntry> = Zone::const_new();
static RNODE_ZONE: Zone<RNode> = Zone::const_new();
static OPEN_FILE_ZONE: Zone<OpenFile> = Zone::const_new();

unsafe impl ZoneAllocated for DEntry {
    fn zone() -> &'static Zone<Self> {
        &DENTRY_ZONE
    }
}

unsafe impl ZoneAllocated for RNode {
    fn zone() -> &'static Zone<Self> {
        &RNODE_ZONE
    }
}

unsafe impl ZoneAllocated for OpenFile {
    fn zone() -> &'static Zone<Self> {
        &OPEN_FILE_ZONE
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FsObjectId(u64);

impl FsObjectId {
    pub const ROOT: Self = Self(1);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Credential {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeKind {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeMeta {
    pub kind: InodeKind,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u32,
    pub rdev: Option<u64>,
}

impl InodeMeta {
    pub const fn new(kind: InodeKind, mode: u16) -> Self {
        Self {
            kind,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            nlink: 1,
            rdev: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirCursor(u64);

impl DirCursor {
    pub const START: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct InlineName {
    len: u8,
    bytes: [u8; VFS_NAME_MAX],
}

impl InlineName {
    pub fn new(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.is_empty() || bytes.len() > VFS_NAME_MAX || bytes.iter().any(|byte| *byte == b'/')
        {
            return Err(Errno::ENAMETOOLONG);
        }

        let mut name = Self {
            len: bytes.len() as u8,
            bytes: [0; VFS_NAME_MAX],
        };
        name.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(name)
    }

    pub const fn len(self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len()]
    }
}

impl fmt::Debug for InlineName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineName")
            .field("len", &self.len())
            .field("bytes", &self.as_bytes())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsName<'a>(&'a [u8]);

impl<'a> VfsName<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Errno> {
        InlineName::new(bytes)?;
        Ok(Self(bytes))
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub fs_object_id: FsObjectId,
    pub kind: InodeKind,
    pub name: InlineName,
}

impl DirEntry {
    pub fn new(fs_object_id: FsObjectId, kind: InodeKind, name: &[u8]) -> Result<Self, Errno> {
        Ok(Self {
            fs_object_id,
            kind,
            name: InlineName::new(name)?,
        })
    }
}

pub trait FsOps: Send + Sync + 'static {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &'g Guard<'_>,
    ) -> StepOutcome<FsObjectId>;

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<InodeMeta>;

    fn serialize_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<()>;

    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn unlink<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<()>;

    fn rename<'g>(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &'g Guard<'_>,
    ) -> StepOutcome<()>;

    fn link<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<()>;

    fn mkdir<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn rmdir<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<()>;

    fn symlink<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn readdir<'g>(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &'g Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>>;

    fn destroy_inode<'g>(&self, fs_object_id: FsObjectId, guard: &'g Guard<'_>) -> StepOutcome<()>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenFileFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
}

#[derive(Clone, Debug)]
pub enum RNodeBacking {
    PageBacked { pc: Cap<PageContainer> },
    Directory,
    Symlink { target: InlineName },
    StructBackedChar { binding: &'static CharDeviceBinding },
    Projected,
}

#[derive(Debug)]
pub struct RNode {
    fs_object_id: FsObjectId,
    meta: InodeMeta,
    backing: RNodeBacking,
    containing_mount: Option<Weak<MountPayload>>,
}

impl RNode {
    pub fn new(fs_object_id: FsObjectId, meta: InodeMeta, backing: RNodeBacking) -> Self {
        Self {
            fs_object_id,
            meta,
            backing,
            containing_mount: None,
        }
    }

    pub fn new_cap(
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        backing: RNodeBacking,
    ) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(
            reservation,
            Self::new(fs_object_id, meta, backing),
        ))
    }

    pub const fn fs_object_id(&self) -> FsObjectId {
        self.fs_object_id
    }

    pub const fn meta(&self) -> InodeMeta {
        self.meta
    }

    pub const fn backing(&self) -> &RNodeBacking {
        &self.backing
    }

    pub fn with_containing_mount(mut self, mount: &Cap<MountPayload>) -> Self {
        self.containing_mount = Some(mount.downgrade());
        self
    }
}

#[derive(Debug)]
pub struct DEntry {
    name: InlineName,
    parent: Option<Weak<DEntry>>,
    rnode: Cap<RNode>,
    mounted: Option<Weak<MountIdentity>>,
}

impl DEntry {
    pub fn new(name: InlineName, rnode: Cap<RNode>) -> Self {
        Self {
            name,
            parent: None,
            rnode,
            mounted: None,
        }
    }

    pub fn new_cap(name: InlineName, rnode: Cap<RNode>) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(reservation, Self::new(name, rnode)))
    }

    pub const fn name(&self) -> InlineName {
        self.name
    }

    pub fn rnode(&self) -> &Cap<RNode> {
        &self.rnode
    }

    pub fn set_parent_hint(&mut self, parent: &Cap<DEntry>) {
        self.parent = Some(parent.downgrade());
    }

    pub fn set_mounted_hint(&mut self, mount: &Cap<MountIdentity>) {
        self.mounted = Some(mount.downgrade());
    }
}

#[derive(Debug)]
pub struct OpenFile {
    rnode: Cap<RNode>,
    offset: u64,
    flags: OpenFileFlags,
}

impl OpenFile {
    pub fn new(rnode: Cap<RNode>, flags: OpenFileFlags) -> Self {
        Self {
            rnode,
            offset: 0,
            flags,
        }
    }

    pub fn new_cap(rnode: Cap<RNode>, flags: OpenFileFlags) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(reservation, Self::new(rnode, flags)))
    }

    pub fn rnode(&self) -> &Cap<RNode> {
        &self.rnode
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn flags(&self) -> OpenFileFlags {
        self.flags
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootCtx {
    pub root: Cap<DEntry>,
    pub mount_ns: Option<Cap<MountNamespace>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveCtx {
    pub root: RootCtx,
    pub cwd: Cap<DEntry>,
    pub credential: Credential,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntityAtPath {
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryAtPath {
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParentAndName {
    pub parent: Cap<DEntry>,
    pub name: InlineName,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_backed::{AnonSwapPolicy, PageContainerKind};

    #[test]
    fn inline_name_rejects_empty_slash_and_oversized_names() {
        assert_eq!(InlineName::new(b"etc").unwrap().as_bytes(), b"etc");
        assert_eq!(InlineName::new(b""), Err(Errno::ENAMETOOLONG));
        assert_eq!(InlineName::new(b"a/b"), Err(Errno::ENAMETOOLONG));
        assert_eq!(
            InlineName::new(&[b'x'; VFS_NAME_MAX + 1]),
            Err(Errno::ENAMETOOLONG)
        );
    }

    #[test]
    fn rnode_backing_uses_page_container_cap_without_backend_live_nodes() {
        tx_substrate::testing::init_host_for_test_once();
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Persistent,
            },
            8,
        )
        .expect("page container");
        let rnode = RNode::new(
            FsObjectId::new(42),
            InodeMeta::new(InodeKind::Regular, 0o100644),
            RNodeBacking::PageBacked { pc: pc.clone() },
        );

        assert_eq!(rnode.fs_object_id(), FsObjectId::new(42));
        assert!(matches!(rnode.backing(), RNodeBacking::PageBacked { pc: r_pc } if *r_pc == pc));
    }
}
