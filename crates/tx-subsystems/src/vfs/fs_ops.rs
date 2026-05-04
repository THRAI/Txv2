use alloc::sync::Arc;

use tx_substrate::epoch::Guard;
use tx_substrate::zone::Cap;

use crate::mount::structure::{BlockDevice, MountId, MountOptions, PhysicalBlockNumber};
use crate::page_backed::{FsPageBacking, PageContainer};
use crate::step::{Errno, StepOutcome};
use crate::vfs::structure::{
    FsObjectId, InodeMeta, NameOwned, ProjectionKey, ProjectionSchema, StructPayload,
};

pub struct Credential {
    pub uid: u32,
    pub gid: u32,
    pub egid: u32,
    pub groups: [u32; 32],
    pub group_count: u8,
    pub capabilities: CapSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapSet {
    pub bits: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirCursor(pub [u8; 16]);

pub struct DirEntry {
    pub name: NameOwned,
    pub fs_object_id: FsObjectId,
    pub d_type: u8,
}

pub enum RNodeBackingInit {
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

pub trait FsOps: Send + Sync + 'static {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &'g Guard<'g>,
    ) -> StepOutcome<FsObjectId>;

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<InodeMeta>;

    fn backing_for<'g>(
        &self,
        fs_object_id: FsObjectId,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<RNodeBackingInit> {
        let _ = (fs_object_id, guard);
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn serialize_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn unlink<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn rename<'g>(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn link<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn mkdir<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn rmdir<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn symlink<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn readdir<'g>(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>>;

    fn destroy_inode<'g>(&self, fs_object_id: FsObjectId, guard: &'g Guard<'g>) -> StepOutcome<()>;
}

pub struct MountInitContext {
    pub block_device: Arc<dyn BlockDevice>,
    pub mount_id: MountId,
    pub metadata_pc_factory: Arc<dyn MetadataPcFactory>,
    pub options: MountOptions,
}

pub trait MetadataPcFactory: Send + Sync {
    fn create_metadata_pc(
        &self,
        start_block: PhysicalBlockNumber,
        block_count: u64,
    ) -> Result<Cap<PageContainer>, crate::step::Errno>;
}

pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}
