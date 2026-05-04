//! Mount identity, payload, and backend bootstrap shells.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::device::BlockDevice;
use crate::execution::KernelResult;
use crate::page_backed::{FsPageBacking, PageContainer};
use crate::vfs::{DEntry, FsObjectId, FsOps, InodeMeta, RNode};
use tx_substrate::zone::{self, Cap, Zone, ZoneAllocated, ZoneError};

static MOUNT_IDENTITY_ZONE: Zone<MountIdentity> = Zone::const_new();
static MOUNT_PAYLOAD_ZONE: Zone<MountPayload> = Zone::const_new();
static MOUNT_NAMESPACE_ZONE: Zone<MountNamespace> = Zone::const_new();

unsafe impl ZoneAllocated for MountIdentity {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for MountPayload {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_PAYLOAD_ZONE
    }
}

unsafe impl ZoneAllocated for MountNamespace {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_NAMESPACE_ZONE
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MountId(u64);

impl MountId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DevId(u32);

impl DevId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountFlags(u64);

impl MountFlags {
    pub const READ_ONLY: Self = Self(1 << 0);
    pub const NO_ATIME: Self = Self(1 << 1);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountOptions {
    pub flags: MountFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceLabel {
    Static(&'static str),
    Anonymous,
}

pub struct MountPayload {
    payload_pin_count: AtomicU32,
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub backing: Option<Arc<dyn BlockDevice>>,
    pub dev_id: DevId,
    pub options: MountOptions,
    pub fstype: &'static str,
    pub source_label: SourceLabel,
}

impl MountPayload {
    pub fn new(
        fs_ops: Arc<dyn FsOps>,
        fs_page_backing: Arc<dyn FsPageBacking>,
        backing: Option<Arc<dyn BlockDevice>>,
        dev_id: DevId,
        options: MountOptions,
        fstype: &'static str,
        source_label: SourceLabel,
    ) -> Self {
        Self {
            payload_pin_count: AtomicU32::new(0),
            fs_ops,
            fs_page_backing,
            backing,
            dev_id,
            options,
            fstype,
            source_label,
        }
    }

    pub fn new_cap(
        fs_ops: Arc<dyn FsOps>,
        fs_page_backing: Arc<dyn FsPageBacking>,
        backing: Option<Arc<dyn BlockDevice>>,
        dev_id: DevId,
        options: MountOptions,
        fstype: &'static str,
        source_label: SourceLabel,
    ) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(
            reservation,
            Self::new(
                fs_ops,
                fs_page_backing,
                backing,
                dev_id,
                options,
                fstype,
                source_label,
            ),
        ))
    }

    pub fn payload_pin_count(&self) -> u32 {
        self.payload_pin_count.load(Ordering::Acquire)
    }
}

impl core::fmt::Debug for MountPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountPayload")
            .field("payload_pin_count", &self.payload_pin_count())
            .field("dev_id", &self.dev_id)
            .field("options", &self.options)
            .field("fstype", &self.fstype)
            .field("source_label", &self.source_label)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct MountPayloadPin {
    payload: Cap<MountPayload>,
}

impl MountPayloadPin {
    pub fn acquire(payload: &Cap<MountPayload>) -> Self {
        payload.payload_pin_count.fetch_add(1, Ordering::AcqRel);
        Self {
            payload: payload.clone(),
        }
    }

    pub fn payload(&self) -> &Cap<MountPayload> {
        &self.payload
    }
}

impl Drop for MountPayloadPin {
    fn drop(&mut self) {
        self.payload
            .payload_pin_count
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub struct MountIdentity {
    id: MountId,
    mountpoint: Option<Cap<DEntry>>,
    root: Cap<RNode>,
    parent: Option<Cap<MountIdentity>>,
    payload: Cap<MountPayload>,
    flags: MountFlags,
}

impl MountIdentity {
    pub fn new(
        id: MountId,
        mountpoint: Option<Cap<DEntry>>,
        root: Cap<RNode>,
        parent: Option<Cap<MountIdentity>>,
        payload: Cap<MountPayload>,
        flags: MountFlags,
    ) -> Self {
        Self {
            id,
            mountpoint,
            root,
            parent,
            payload,
            flags,
        }
    }

    pub fn new_cap(
        id: MountId,
        mountpoint: Option<Cap<DEntry>>,
        root: Cap<RNode>,
        parent: Option<Cap<MountIdentity>>,
        payload: Cap<MountPayload>,
        flags: MountFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(
            reservation,
            Self::new(id, mountpoint, root, parent, payload, flags),
        ))
    }

    pub const fn id(&self) -> MountId {
        self.id
    }

    pub fn mountpoint(&self) -> Option<&Cap<DEntry>> {
        self.mountpoint.as_ref()
    }

    pub fn root(&self) -> &Cap<RNode> {
        &self.root
    }

    pub fn parent(&self) -> Option<&Cap<MountIdentity>> {
        self.parent.as_ref()
    }

    pub fn payload(&self) -> &Cap<MountPayload> {
        &self.payload
    }

    pub const fn flags(&self) -> MountFlags {
        self.flags
    }
}

#[derive(Debug)]
pub struct MountNamespace {
    root: Cap<MountIdentity>,
}

impl MountNamespace {
    pub fn new(root: Cap<MountIdentity>) -> Self {
        Self { root }
    }

    pub fn new_cap(root: Cap<MountIdentity>) -> Result<Cap<Self>, ZoneError> {
        let reservation = zone::reserve_for::<Self>()?;
        Ok(zone::sign_for(reservation, Self::new(root)))
    }

    pub fn root(&self) -> &Cap<MountIdentity> {
        &self.root
    }
}

pub trait MetadataPcFactory: Send + Sync + 'static {
    fn create_metadata_pc(
        &self,
        start_block: u64,
        block_count: u64,
    ) -> KernelResult<Cap<PageContainer>>;
}

pub struct MountInitContext {
    pub block_device: Arc<dyn BlockDevice>,
    pub mount_id: MountId,
    pub metadata_pc_factory: Arc<dyn MetadataPcFactory>,
    pub options: MountOptions,
}

pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{Errno, Guard, StepOutcome};
    use crate::page_backed::{Frame, PageContainerKind};
    use crate::vfs::{Credential, DirCursor, DirEntry, InodeKind};

    struct MockFs;

    impl FsOps for MockFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<FsObjectId> {
            if name == b"root" {
                StepOutcome::Done(FsObjectId::ROOT)
            } else {
                StepOutcome::Err(Errno::ENOENT)
            }
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<InodeMeta> {
            StepOutcome::Done(InodeMeta::new(InodeKind::Directory, 0o040755))
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta)> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta)> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta)> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
            StepOutcome::Done(None)
        }

        fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }
    }

    impl FsPageBacking for MockFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Frame> {
            StepOutcome::Err(Errno::ENOSYS)
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Err(Errno::EROFS)
        }

        fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }
    }

    #[test]
    fn mount_payload_stores_backend_traits_and_pins_are_explicit() {
        tx_substrate::testing::init_host_for_test_once();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap(
            fs.clone(),
            fs,
            None,
            DevId::new(1),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");

        assert_eq!(payload.payload_pin_count(), 0);
        {
            let pin = MountPayloadPin::acquire(&payload);
            assert_eq!(pin.payload().dev_id, DevId::new(1));
            assert_eq!(payload.payload_pin_count(), 1);
        }
        assert_eq!(payload.payload_pin_count(), 0);
    }

    #[test]
    fn file_page_container_kind_carries_mount_payload_and_object_id() {
        tx_substrate::testing::init_host_for_test_once();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap(
            fs.clone(),
            fs,
            None,
            DevId::new(2),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Anonymous,
        )
        .expect("mount payload");
        let kind = PageContainerKind::File {
            mount: payload.clone(),
            fs_object_id: FsObjectId::new(99),
        };

        assert!(matches!(
            kind,
            PageContainerKind::File {
                ref mount,
                fs_object_id
            } if *mount == payload && fs_object_id == FsObjectId::new(99)
        ));
    }
}
