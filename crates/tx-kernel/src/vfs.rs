//! VFS live-node shells and filesystem namespace backend trait.

use alloc::boxed::Box;
use core::fmt;

use crate::device::CharDeviceBinding;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::mount::{MountIdentity, MountNamespace, MountPayload};
use crate::page_backed::PageContainer;
use crate::tty::{self, structure::TtyIdentity};
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
    fn lookup(&self, parent: FsObjectId, name: &[u8], guard: &Guard<'_>)
        -> StepOutcome<FsObjectId>;

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta>;

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>>;

    fn destroy_inode(&self, fs_object_id: FsObjectId, guard: &Guard<'_>) -> StepOutcome<()>;
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
    Symlink { target: Box<InlineName> },
    StructBacked { payload: StructPayload },
    Projected,
}

#[derive(Clone, Debug)]
pub enum StructPayload {
    Tty(Cap<TtyIdentity>),
    CharDevice(&'static CharDeviceBinding),
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

    /// Dispatch a read against this file's RNode backing.
    ///
    /// This is the Phase D interface slice: full fd tables, UserBuf copying,
    /// page-backed file I/O, and projection schemas are still later work. TTY
    /// and raw char-device struct payloads already route through their owning
    /// subsystems.
    pub fn step_read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize> {
        if !self.flags.read {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_read(tty, out, guard),
                StructPayload::CharDevice(binding) => binding.ops.read(out, guard),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
    }

    /// Dispatch a write against this file's RNode backing.
    pub fn step_write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize> {
        if !self.flags.write {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_write(tty, bytes, guard),
                StructPayload::CharDevice(binding) => binding.ops.write(bytes, guard),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
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
    use crate::device::{CharDeviceOps, DevT};
    use crate::page_backed::{AnonSwapPolicy, PageContainerKind};
    use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};
    use tx_substrate::zone::PayloadCap;

    struct EchoCharOps;

    impl CharDeviceOps for EchoCharOps {
        fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
            if out.is_empty() {
                return StepOutcome::Done(0);
            }
            out[0] = b'R';
            StepOutcome::Done(1)
        }

        fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
            StepOutcome::Done(bytes.len())
        }
    }

    static ECHO_CHAR_OPS: EchoCharOps = EchoCharOps;
    static ECHO_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(240, 0),
        name: "echo-char",
        ops: &ECHO_CHAR_OPS,
    };

    fn init_tty_zones() {
        tx_substrate::testing::init_host_for_test_once();
        crate::tty::structure::registry::register_zones().expect("tty zones");
    }

    fn alloc_tty(kind: TtyKind, index: u32, name: &str, payload: TtyPayload) -> Cap<TtyIdentity> {
        let id_res = zone::reserve_for::<TtyIdentity>().expect("tty identity reservation");
        let payload_res = zone::reserve_for::<TtyPayload>().expect("tty payload reservation");
        let payload = PayloadCap::from_cap(zone::sign_for(payload_res, payload));
        let identity = zone::sign_for(id_res, TtyIdentity::new(kind, index, name));
        identity.install_payload(payload);
        identity
    }

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

    #[test]
    fn rnode_backing_carries_tty_identity_payload() {
        init_tty_zones();
        let tty = alloc_tty(
            TtyKind::SerialHardware,
            0,
            "ttyS0",
            TtyPayload::new_hardware(&ECHO_CHAR_BINDING),
        );
        let rnode = RNode::new(
            FsObjectId::new(43),
            InodeMeta::new(InodeKind::CharDevice, 0o020600),
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty.clone()),
            },
        );

        assert!(matches!(
            rnode.backing(),
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(r_tty)
            } if *r_tty == tty
        ));
    }

    #[test]
    fn open_file_dispatches_struct_payload_read_write() {
        init_tty_zones();
        let guard = tx_substrate::epoch::guard();
        let tty = alloc_tty(
            TtyKind::SerialHardware,
            1,
            "ttyS1",
            TtyPayload::new_hardware(&ECHO_CHAR_BINDING),
        );
        let tty_rnode = RNode::new_cap(
            FsObjectId::new(44),
            InodeMeta::new(InodeKind::CharDevice, 0o020600),
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty.clone()),
            },
        )
        .expect("tty rnode");
        let tty_file = OpenFile::new(
            tty_rnode,
            OpenFileFlags {
                read: true,
                write: true,
                append: false,
            },
        );
        let mut out = [0u8; 8];

        assert!(matches!(
            tty_file.step_read(&mut out, &guard),
            StepOutcome::Blocked(_)
        ));
        assert_eq!(
            crate::tty::execution::step_ingest(&tty, b"ok\n", &guard),
            StepOutcome::Done(crate::tty::execution::IngestOutcome {
                consumed: 3,
                readable_fired: true,
                writable_fired: true,
                ..Default::default()
            })
        );
        assert_eq!(tty_file.step_read(&mut out, &guard), StepOutcome::Done(3));
        assert_eq!(&out[..3], b"ok\n");
        assert_eq!(tty_file.step_write(b"x", &guard), StepOutcome::Done(1));

        let char_rnode = RNode::new_cap(
            FsObjectId::new(45),
            InodeMeta::new(InodeKind::CharDevice, 0o020600),
            RNodeBacking::StructBacked {
                payload: StructPayload::CharDevice(&ECHO_CHAR_BINDING),
            },
        )
        .expect("char rnode");
        let char_file = OpenFile::new(
            char_rnode,
            OpenFileFlags {
                read: true,
                write: true,
                append: false,
            },
        );
        let mut char_out = [0u8; 1];

        assert_eq!(
            char_file.step_read(&mut char_out, &guard),
            StepOutcome::Done(1)
        );
        assert_eq!(char_out, [b'R']);
        assert_eq!(char_file.step_write(b"abc", &guard), StepOutcome::Done(3));
    }
}
