//! VFS structure: SSoT data types and zone-managed live nodes.
//!
//! This module hosts the value vocabulary the rest of the VFS surface
//! consumes — names, ids, metadata, directory cursors — plus the three
//! zone-allocated entities (`DEntry`, `RNode`, `OpenFile`) and their
//! constructors/accessors. Per `SUBSYSTEM_ANATOMY_v2_1` §structure,
//! mutation step bodies live in `execution.rs`; only constructors and
//! pure observation helpers belong here.

use alloc::boxed::Box;

use crate::device::CharDeviceBinding;
use crate::execution::Errno;
use crate::mount::{MountIdentity, MountPayload};
use crate::page_backed::PageContainer;
use crate::tty::structure::TtyIdentity;
use tx_substrate::zone::{self, Cap, Weak, Zone, ZoneAllocated, ZoneError};

pub const VFS_NAME_MAX: usize = 255;

// === zone statics =====================================================

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

// === namespace identifiers =============================================

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

// === inode metadata + POSIX mode constants ============================

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

// POSIX S_IFMT mode-bit constants. Mode carries the file kind in its upper
// nibble; `InodeMeta::kind()` derives `InodeKind` from these bits.
pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFIFO: u16 = 0o010000;
pub const S_IFSOCK: u16 = 0o140000;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i32,
}

impl Timespec {
    pub const EPOCH: Self = Self { sec: 0, nsec: 0 };

    pub const fn new(sec: i64, nsec: i32) -> Self {
        Self { sec, nsec }
    }
}

// Per `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md` §pub-types and
// `bringup_fs_specs_v_1` §load_inode_meta. `mode` carries S_IFMT bits;
// `kind()` derives `InodeKind` from them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeMeta {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub nlinks: u32,
    pub blocks: u64,
    pub flags: u32,
}

impl InodeMeta {
    /// Construct a fresh inode meta. `kind` is asserted into the mode's
    /// S_IFMT bits if not already present; the mode is otherwise preserved.
    pub const fn new(kind: InodeKind, mode: u16) -> Self {
        let mode = if mode & S_IFMT == 0 {
            mode | kind_to_ifmt(kind)
        } else {
            mode
        };
        Self {
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            atime: Timespec::EPOCH,
            mtime: Timespec::EPOCH,
            ctime: Timespec::EPOCH,
            nlinks: 1,
            blocks: 0,
            flags: 0,
        }
    }

    pub const fn kind(&self) -> InodeKind {
        match self.mode & S_IFMT {
            S_IFDIR => InodeKind::Directory,
            S_IFLNK => InodeKind::Symlink,
            S_IFCHR => InodeKind::CharDevice,
            S_IFBLK => InodeKind::BlockDevice,
            S_IFIFO => InodeKind::Fifo,
            S_IFSOCK => InodeKind::Socket,
            _ => InodeKind::Regular,
        }
    }
}

const fn kind_to_ifmt(kind: InodeKind) -> u16 {
    match kind {
        InodeKind::Regular => S_IFREG,
        InodeKind::Directory => S_IFDIR,
        InodeKind::Symlink => S_IFLNK,
        InodeKind::CharDevice => S_IFCHR,
        InodeKind::BlockDevice => S_IFBLK,
        InodeKind::Fifo => S_IFIFO,
        InodeKind::Socket => S_IFSOCK,
    }
}

// === directory iteration cursor =======================================

// Opaque directory iteration cursor per
// `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md` §pub-types: filesystem
// implementations define the internal byte layout. Helpers below cover the
// common case of a u64-shaped cursor stored in the leading 8 bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirCursor(pub [u8; 16]);

impl DirCursor {
    pub const START: Self = Self([0; 16]);

    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn from_u64(value: u64) -> Self {
        let v = value.to_le_bytes();
        Self([
            v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], 0, 0, 0, 0, 0, 0, 0, 0,
        ])
    }

    pub const fn as_u64(self) -> u64 {
        u64::from_le_bytes([
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5], self.0[6], self.0[7],
        ])
    }

    pub const fn bytes(self) -> [u8; 16] {
        self.0
    }
}

// === name types =======================================================

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct InlineName {
    len: u8,
    bytes: [u8; VFS_NAME_MAX],
}

impl InlineName {
    pub fn new(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.is_empty() || bytes.len() > VFS_NAME_MAX || bytes.contains(&b'/') {
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
        &self.bytes[..self.len as usize]
    }
}

impl core::fmt::Debug for InlineName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match core::str::from_utf8(self.as_bytes()) {
            Ok(text) => write!(f, "InlineName({text:?})"),
            Err(_) => write!(f, "InlineName({:?})", self.as_bytes()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsName<'a>(&'a [u8]);

impl<'a> VfsName<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.is_empty() || bytes.len() > VFS_NAME_MAX || bytes.contains(&b'/') {
            return Err(Errno::ENAMETOOLONG);
        }
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

// === open-file flags + RNode backing ==================================

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
    Symlink { target: Box<InlineName> },
    StructBacked { payload: StructPayload },
    Projected,
}

#[derive(Clone, Debug)]
pub enum StructPayload {
    Tty(Cap<TtyIdentity>),
    CharDevice(&'static CharDeviceBinding),
}

// === live-node entities ===============================================

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
    pub(crate) rnode: Cap<RNode>,
    offset: u64,
    pub(crate) flags: OpenFileFlags,
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

    pub fn set_offset(&mut self, offset: u64) {
        self.offset = offset;
    }

    pub const fn flags(&self) -> OpenFileFlags {
        self.flags
    }
}
