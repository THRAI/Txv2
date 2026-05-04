use alloc::boxed::Box;

use tx_substrate::epoch::Guard;
use tx_substrate::index::Index;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use tx_substrate::index::IndexError;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use tx_substrate::zone::ZoneError;
use tx_substrate::zone::{Cap, IdentRef, Zone, ZoneAllocated};

use crate::mount::structure::{MountIdentity, MountPayloadPin};
use crate::page_backed::PageContainer;
use crate::step::Errno;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DEntryKey(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RNodeKey(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OpenFileKey(pub u64);

pub struct DEntry {
    pub key: DEntryKey,
    pub name: NameOwned,
    pub rnode: Cap<RNode>,
    pub children: DEntryChildren,
}

pub struct RNode {
    pub key: RNodeKey,
    pub fs_object_id: FsObjectId,
    pub meta: InodeMeta,
    pub backing: RNodeBacking,
}

pub struct OpenFile {
    pub key: OpenFileKey,
    pub rnode: Cap<RNode>,
    pub mount: Cap<MountIdentity>,
    pub mount_payload_pin: MountPayloadPin,
    pub offset: u64,
    pub flags: OpenFlags,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FsObjectId(pub u64);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NameOwned {
    pub len: u16,
    pub bytes: [u8; 255],
}

impl NameOwned {
    pub const MAX_LEN: usize = 255;

    pub fn from_component(component: &[u8]) -> Result<Self, Errno> {
        if component.len() > Self::MAX_LEN {
            return Err(Errno::NameTooLong);
        }

        let mut bytes = [0; Self::MAX_LEN];
        bytes[..component.len()].copy_from_slice(component);
        Ok(Self {
            len: component.len() as u16,
            bytes,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

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
    pub const TYPE_MASK: u16 = 0o170000;
    pub const TYPE_DIRECTORY: u16 = 0o040000;
    pub const TYPE_REGULAR: u16 = 0o100000;
    pub const TYPE_SYMLINK: u16 = 0o120000;

    pub fn file_type(&self) -> RNodeFileType {
        match self.mode & Self::TYPE_MASK {
            Self::TYPE_DIRECTORY => RNodeFileType::Directory,
            Self::TYPE_REGULAR => RNodeFileType::RegularFile,
            Self::TYPE_SYMLINK => RNodeFileType::Symlink,
            other => RNodeFileType::Other(other),
        }
    }

    pub fn is_directory(&self) -> bool {
        self.file_type() == RNodeFileType::Directory
    }

    pub fn is_regular_file(&self) -> bool {
        self.file_type() == RNodeFileType::RegularFile
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == RNodeFileType::Symlink
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RNodeFileType {
    Directory,
    RegularFile,
    Symlink,
    Other(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i32,
}

pub enum RNodeBacking {
    PageBacked {
        pc: Cap<PageContainer>,
    },
    StructBacked {
        payload: StructPayload,
    },
    Projected {
        schema: &'static dyn ProjectionSchema,
        key: ProjectionKey,
    },
}

pub const RENDER_BUFFER_CAPACITY: usize = crate::page_backed::FRAME_CAPACITY * 2;

pub struct ProjectionReadCtx<'g> {
    _guard: core::marker::PhantomData<&'g Guard<'g>>,
}

impl<'g> ProjectionReadCtx<'g> {
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    pub(crate) fn new(_guard: &'g Guard<'g>) -> Self {
        Self {
            _guard: core::marker::PhantomData,
        }
    }
}

pub struct RenderBuffer {
    len: usize,
    bytes: [u8; RENDER_BUFFER_CAPACITY],
}

impl RenderBuffer {
    pub const CAPACITY: usize = RENDER_BUFFER_CAPACITY;

    pub fn new() -> Self {
        Self {
            len: 0,
            bytes: [0; RENDER_BUFFER_CAPACITY],
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        let end = self.len.checked_add(bytes.len()).ok_or(Errno::Busy)?;
        if end > Self::CAPACITY {
            return Err(Errno::Busy);
        }

        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn slice_from(&self, offset: u64, max_len: usize) -> &[u8] {
        let Ok(offset) = usize::try_from(offset) else {
            return &[];
        };
        if offset >= self.len {
            return &[];
        }

        let end = core::cmp::min(self.len, offset.saturating_add(max_len));
        &self.bytes[offset..end]
    }
}

impl Default for RenderBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub nonblock: bool,
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) struct NewRNodeSpec {
    pub key: RNodeKey,
    pub fs_object_id: FsObjectId,
    pub meta: InodeMeta,
    pub backing: RNodeBacking,
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) struct NewDEntrySpec {
    pub key: DEntryKey,
    pub name: NameOwned,
    pub rnode: Cap<RNode>,
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) struct NewOpenFileSpec {
    pub key: OpenFileKey,
    pub rnode: Cap<RNode>,
    pub mount: Cap<MountIdentity>,
    pub mount_payload_pin: MountPayloadPin,
    pub offset: u64,
    pub flags: OpenFlags,
}

pub const DENTRY_CHILDREN_CAPACITY: usize = 64;

pub struct DEntryChildren {
    state: Box<DEntryChildrenState>,
}

struct DEntryChildrenState {
    // RFX-VFS-P2-002: Replace this fixed-capacity bringup namespace index with
    // the final kernel namespace container policy once that policy is available.
    // P4A moves the storage behind a separate zone object so DEntry itself no
    // longer embeds the entire fixed-capacity namespace index.
    index: Index<NameOwned, Cap<DEntry>, DENTRY_CHILDREN_CAPACITY>,
}

impl DEntryChildren {
    pub fn new() -> Self {
        Self {
            state: Box::new(DEntryChildrenState::empty()),
        }
    }

    pub fn lookup<'g>(&self, name: &NameOwned, guard: &'g Guard<'_>) -> DEntryChildLookup<'g> {
        match self.state.index.lookup(name, guard) {
            Some(entry) => DEntryChildLookup::Found(Box::new(DEntryChildRef {
                name: entry.key().clone(),
                child: entry.value().ident_ref(guard),
            })),
            None => DEntryChildLookup::Missing,
        }
    }

    // RFX-VFS-P2-001: Replace this bootstrap/test helper with vfs::execution
    // reserve/commit helpers once create/mkdir/link/rename/unlink steps exist.
    #[cfg(test)]
    pub(crate) fn install_committed_for_test_or_bootstrap(
        &self,
        name: NameOwned,
        child: Cap<DEntry>,
    ) -> Result<(), DEntryChildrenInstallError> {
        let reservation = self.reserve_insert(name)?;
        reservation.commit(child);
        Ok(())
    }

    #[cfg(any(test, feature = "vfs-read-test-support"))]
    pub(crate) fn reserve_insert(
        &self,
        name: NameOwned,
    ) -> Result<DEntryChildrenInsertReservation<'_>, DEntryChildrenInstallError> {
        match self.state.index.reserve(name) {
            Ok(reservation) => Ok(DEntryChildrenInsertReservation { reservation }),
            Err(IndexError::Duplicate) => Err(DEntryChildrenInstallError::AlreadyPresent),
            Err(IndexError::Full) => Err(DEntryChildrenInstallError::Full),
            Err(IndexError::Busy) => Err(DEntryChildrenInstallError::Busy),
            Err(IndexError::Missing) => Err(DEntryChildrenInstallError::Missing),
        }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) struct DEntryChildrenInsertReservation<'a> {
    reservation:
        tx_substrate::index::IndexReservation<'a, NameOwned, Cap<DEntry>, DENTRY_CHILDREN_CAPACITY>,
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl DEntryChildrenInsertReservation<'_> {
    #[cfg(test)]
    pub(crate) fn key(&self) -> &NameOwned {
        self.reservation.key()
    }

    pub(crate) fn commit(self, child: Cap<DEntry>) {
        self.reservation.commit(child);
    }
}

impl DEntryChildrenState {
    fn empty() -> Self {
        Self {
            index: Index::new(),
        }
    }
}

impl Default for DEntryChildren {
    fn default() -> Self {
        Self::new()
    }
}

pub enum DEntryChildLookup<'g> {
    Found(Box<DEntryChildRef<'g>>),
    Missing,
}

pub struct DEntryChildRef<'g> {
    name: NameOwned,
    child: IdentRef<'g, DEntry>,
}

impl<'g> DEntryChildRef<'g> {
    pub fn name(&self) -> &NameOwned {
        &self.name
    }

    pub fn raw(&self) -> u32 {
        self.child.raw()
    }

    pub fn into_ident_ref(self) -> IdentRef<'g, DEntry> {
        self.child
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DEntryChildrenInstallError {
    AlreadyPresent,
    Full,
    Busy,
    Missing,
}

pub enum StructPayload {
    // RFX-VFS-P0-007: Replace this with role-shaped caps for pipe/socket/tty/etc.
    Deferred,
}

pub trait ProjectionSchema: Send + Sync {
    fn schema_id(&self) -> u32;

    fn render<'g>(
        &self,
        key: &ProjectionKey,
        ctx: &ProjectionReadCtx<'g>,
        out: &mut RenderBuffer,
        guard: &'g Guard<'g>,
    ) -> Result<(), Errno> {
        let _ = (key, ctx, out, guard);
        Err(Errno::NotImplemented)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionKey {
    pub object_id: u64,
    pub file_type: u32,
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) fn create_rnode_for_create_lane(spec: NewRNodeSpec) -> Result<Cap<RNode>, ZoneError> {
    let reservation = tx_substrate::zone::reserve_for::<RNode>()?;
    Ok(tx_substrate::zone::sign_for(
        reservation,
        RNode {
            key: spec.key,
            fs_object_id: spec.fs_object_id,
            meta: spec.meta,
            backing: spec.backing,
        },
    ))
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) fn create_dentry_for_create_lane(spec: NewDEntrySpec) -> Result<Cap<DEntry>, ZoneError> {
    let reservation = tx_substrate::zone::reserve_for::<DEntry>()?;
    Ok(tx_substrate::zone::sign_for(
        reservation,
        DEntry {
            key: spec.key,
            name: spec.name,
            rnode: spec.rnode,
            children: DEntryChildren::new(),
        },
    ))
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) fn create_open_file_for_create_lane(
    spec: NewOpenFileSpec,
) -> Result<Cap<OpenFile>, ZoneError> {
    let reservation = tx_substrate::zone::reserve_for::<OpenFile>()?;
    Ok(tx_substrate::zone::sign_for(
        reservation,
        OpenFile {
            key: spec.key,
            rnode: spec.rnode,
            mount: spec.mount,
            mount_payload_pin: spec.mount_payload_pin,
            offset: spec.offset,
            flags: spec.flags,
        },
    ))
}

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

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::*;
    use tx_substrate::{epoch, zone};

    fn meta(mode: u16) -> InodeMeta {
        InodeMeta {
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            nlinks: 1,
            blocks: 0,
            flags: 0,
        }
    }

    fn setup() {
        tx_substrate::testing::init_host_for_test_once();
        let _ = zone::register_zone_for::<RNode>();
        let _ = zone::register_zone_for::<DEntry>();
    }

    fn make_rnode_for_test(key: u64, mode: u16) -> Cap<RNode> {
        create_rnode_for_create_lane(NewRNodeSpec {
            key: RNodeKey(key),
            fs_object_id: FsObjectId(key),
            meta: meta(mode),
            backing: RNodeBacking::StructBacked {
                payload: StructPayload::Deferred,
            },
        })
        .expect("RNode reservation")
    }

    fn make_dentry_for_test(key: u64, name: &[u8], rnode: Cap<RNode>) -> Cap<DEntry> {
        create_dentry_for_create_lane(NewDEntrySpec {
            key: DEntryKey(key),
            name: NameOwned::from_component(name).expect("valid name"),
            rnode,
        })
        .expect("DEntry reservation")
    }

    fn make_open_file_for_test(
        key: u64,
        rnode: Cap<RNode>,
        mount: Cap<MountIdentity>,
        mount_payload_pin: MountPayloadPin,
    ) -> Cap<OpenFile> {
        create_open_file_for_create_lane(NewOpenFileSpec {
            key: OpenFileKey(key),
            rnode,
            mount,
            mount_payload_pin,
            offset: 0,
            flags: OpenFlags {
                read: true,
                write: false,
                append: false,
                nonblock: false,
            },
        })
        .expect("OpenFile reservation")
    }

    #[test]
    fn name_owned_copies_component_bytes() {
        let name = NameOwned::from_component(b"alpha").expect("valid component");

        assert_eq!(name.len, 5);
        assert_eq!(name.as_bytes(), b"alpha");
    }

    #[test]
    fn name_owned_rejects_overlong_component() {
        let bytes = [b'a'; NameOwned::MAX_LEN + 1];

        assert_eq!(NameOwned::from_component(&bytes), Err(Errno::NameTooLong));
    }

    #[test]
    fn inode_meta_reports_file_type_from_mode_bits() {
        assert!(meta(InodeMeta::TYPE_DIRECTORY | 0o755).is_directory());
        assert!(meta(InodeMeta::TYPE_REGULAR | 0o644).is_regular_file());
        assert!(meta(InodeMeta::TYPE_SYMLINK | 0o777).is_symlink());
    }

    #[test]
    fn create_rnode_for_create_lane_preserves_metadata() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let cap = create_rnode_for_create_lane(NewRNodeSpec {
            key: RNodeKey(30),
            fs_object_id: FsObjectId(77),
            meta: meta(InodeMeta::TYPE_REGULAR | 0o640),
            backing: RNodeBacking::StructBacked {
                payload: StructPayload::Deferred,
            },
        })
        .expect("create rnode");
        let guard = epoch::guard();
        let rnode = cap.ident_ref(&guard);

        assert_eq!(rnode.key, RNodeKey(30));
        assert_eq!(rnode.fs_object_id, FsObjectId(77));
        assert!(rnode.meta.is_regular_file());
        assert_eq!(rnode.meta.mode, InodeMeta::TYPE_REGULAR | 0o640);
    }

    #[test]
    fn create_dentry_for_create_lane_preserves_name_and_rnode() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let rnode = make_rnode_for_test(31, InodeMeta::TYPE_DIRECTORY | 0o755);
        let dentry = create_dentry_for_create_lane(NewDEntrySpec {
            key: DEntryKey(31),
            name: NameOwned::from_component(b"fresh").expect("valid name"),
            rnode: rnode.clone(),
        })
        .expect("create dentry");
        let guard = epoch::guard();
        let dentry_ref = dentry.ident_ref(&guard);

        assert_eq!(dentry_ref.key, DEntryKey(31));
        assert_eq!(dentry_ref.name.as_bytes(), b"fresh");
        assert_eq!(dentry_ref.rnode.raw(), rnode.raw());
    }

    #[test]
    fn create_lane_objects_stay_hidden_before_children_commit() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rnode = make_rnode_for_test(32, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root = make_dentry_for_test(32, b".", root_rnode);
        let child_rnode = create_rnode_for_create_lane(NewRNodeSpec {
            key: RNodeKey(33),
            fs_object_id: FsObjectId(33),
            meta: meta(InodeMeta::TYPE_REGULAR | 0o644),
            backing: RNodeBacking::StructBacked {
                payload: StructPayload::Deferred,
            },
        })
        .expect("create child rnode");
        let child_name = NameOwned::from_component(b"hidden").expect("valid name");
        let _child = create_dentry_for_create_lane(NewDEntrySpec {
            key: DEntryKey(33),
            name: child_name.clone(),
            rnode: child_rnode,
        })
        .expect("create child dentry");
        let guard = epoch::guard();
        let root_ref = root.ident_ref(&guard);

        assert!(matches!(
            root_ref.children.lookup(&child_name, &guard),
            DEntryChildLookup::Missing
        ));
    }

    #[test]
    fn create_open_file_for_create_lane_preserves_mount_and_flags() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let rnode = make_rnode_for_test(34, InodeMeta::TYPE_REGULAR | 0o644);
        let mount_root = make_dentry_for_test(
            34,
            b".",
            make_rnode_for_test(35, InodeMeta::TYPE_DIRECTORY | 0o755),
        );
        let (mount, _ns, payload) =
            crate::mount::structure::testing::make_bootstrap_pair_with_payload_for_test(mount_root);
        let pin = crate::mount::structure::MountPayloadPin::acquire(&payload);
        let open = make_open_file_for_test(34, rnode.clone(), mount.clone(), pin);
        let guard = epoch::guard();
        let open_ref = open.ident_ref(&guard);

        assert_eq!(open_ref.key, OpenFileKey(34));
        assert_eq!(open_ref.rnode.raw(), rnode.raw());
        assert_eq!(open_ref.mount.raw(), mount.raw());
        assert!(open_ref.flags.read);
        assert!(!open_ref.flags.write);
    }

    #[test]
    fn dentry_children_empty_lookup_is_missing() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let children = DEntryChildren::new();
        let name = NameOwned::from_component(b"missing").expect("valid component");
        let guard = epoch::guard();

        assert!(matches!(
            children.lookup(&name, &guard),
            DEntryChildLookup::Missing
        ));
    }

    #[test]
    fn dentry_children_lookup_returns_installed_child() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rnode = make_rnode_for_test(1, InodeMeta::TYPE_DIRECTORY | 0o755);
        let child_rnode = make_rnode_for_test(2, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root = make_dentry_for_test(1, b".", root_rnode);
        let child = make_dentry_for_test(2, b"child", child_rnode);
        let name = NameOwned::from_component(b"child").expect("valid component");
        let install_guard = epoch::guard();

        root.ident_ref(&install_guard)
            .children
            .install_committed_for_test_or_bootstrap(name.clone(), child.clone())
            .expect("install child");
        drop(install_guard);

        let guard = epoch::guard();
        let root_ref = root.ident_ref(&guard);
        let lookup = root_ref.children.lookup(&name, &guard);

        match lookup {
            DEntryChildLookup::Found(entry) => {
                assert_eq!(entry.name().as_bytes(), b"child");
                assert_eq!(entry.raw(), child.ident_ref(&guard).raw());
            }
            DEntryChildLookup::Missing => panic!("expected installed child"),
        }
    }

    #[test]
    fn dentry_children_reserve_insert_commit_installs_child() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rnode = make_rnode_for_test(3, InodeMeta::TYPE_DIRECTORY | 0o755);
        let child_rnode = make_rnode_for_test(4, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root = make_dentry_for_test(3, b".", root_rnode);
        let child = make_dentry_for_test(4, b"commit", child_rnode);
        let name = NameOwned::from_component(b"commit").expect("valid component");
        let install_guard = epoch::guard();
        let root_ref = root.ident_ref(&install_guard);

        let reservation = root_ref
            .children
            .reserve_insert(name.clone())
            .expect("reserve insert");
        assert_eq!(reservation.key().as_bytes(), b"commit");
        reservation.commit(child.clone());
        drop(install_guard);

        let guard = epoch::guard();
        let root_ref = root.ident_ref(&guard);
        match root_ref.children.lookup(&name, &guard) {
            DEntryChildLookup::Found(entry) => {
                assert_eq!(entry.raw(), child.ident_ref(&guard).raw());
            }
            DEntryChildLookup::Missing => panic!("expected installed child"),
        }
    }

    #[test]
    fn dentry_children_reserve_insert_drop_rolls_back() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rnode = make_rnode_for_test(5, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root = make_dentry_for_test(5, b".", root_rnode);
        let name = NameOwned::from_component(b"rollback").expect("valid component");
        let install_guard = epoch::guard();
        let root_ref = root.ident_ref(&install_guard);

        let reservation = root_ref
            .children
            .reserve_insert(name.clone())
            .expect("reserve insert");
        assert_eq!(reservation.key().as_bytes(), b"rollback");
        drop(reservation);
        drop(install_guard);

        let guard = epoch::guard();
        let root_ref = root.ident_ref(&guard);
        assert!(matches!(
            root_ref.children.lookup(&name, &guard),
            DEntryChildLookup::Missing
        ));
    }

    #[test]
    fn dentry_children_duplicate_install_is_rejected() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rnode = make_rnode_for_test(10, InodeMeta::TYPE_DIRECTORY | 0o755);
        let child_rnode = make_rnode_for_test(11, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root = make_dentry_for_test(10, b".", root_rnode);
        let child = make_dentry_for_test(11, b"dup", child_rnode);
        let name = NameOwned::from_component(b"dup").expect("valid component");
        let guard = epoch::guard();
        let root_ref = root.ident_ref(&guard);

        root_ref
            .children
            .install_committed_for_test_or_bootstrap(name.clone(), child.clone())
            .expect("first install");

        assert_eq!(
            root_ref
                .children
                .install_committed_for_test_or_bootstrap(name, child),
            Err(DEntryChildrenInstallError::AlreadyPresent)
        );
    }

    #[test]
    fn dentry_children_reports_full_at_capacity() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rnode = make_rnode_for_test(20, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root = make_dentry_for_test(20, b".", root_rnode);
        let guard = epoch::guard();
        let root_ref = root.ident_ref(&guard);

        for slot in 0..DENTRY_CHILDREN_CAPACITY {
            let key = 100 + slot as u64;
            let child_rnode = make_rnode_for_test(key, InodeMeta::TYPE_DIRECTORY | 0o755);
            let component = [b'a' + (slot % 26) as u8, b'0' + ((slot / 26) % 10) as u8];
            let child = make_dentry_for_test(key, &component, child_rnode);
            let name = NameOwned::from_component(&component).expect("valid component");

            root_ref
                .children
                .install_committed_for_test_or_bootstrap(name, child)
                .expect("install within capacity");
        }

        let overflow_rnode = make_rnode_for_test(999, InodeMeta::TYPE_DIRECTORY | 0o755);
        let overflow = make_dentry_for_test(999, b"zz", overflow_rnode);
        let overflow_name = NameOwned::from_component(b"zz").expect("valid component");

        assert_eq!(
            root_ref
                .children
                .install_committed_for_test_or_bootstrap(overflow_name, overflow),
            Err(DEntryChildrenInstallError::Full)
        );
    }

    #[test]
    fn dentry_is_smaller_than_children_storage_state() {
        assert_eq!(DENTRY_CHILDREN_CAPACITY, 64);
        assert!(size_of::<DEntry>() < size_of::<DEntryChildrenState>());
    }
}
