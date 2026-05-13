use core::marker::Send;

use crate::adapter::step_engine::Cap;
use alloc::sync::Arc;
use tx_ext4_format::pager::{BlockImage, Page4K};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta};
use tx_subsystems::vfs::FsOps;

use crate::read_backend::{map_inode_meta, Ext4FsInstance, EXT4_ROOT_INODE};

pub struct MountedExt4<I> {
    backend: Arc<Ext4FsInstance<I>>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

impl<I> MountedExt4<I>
where
    I: BlockImage + Send + 'static,
{
    pub fn fs_ops(&self) -> Arc<dyn FsOps> {
        self.backend.clone().fs_ops_arc()
    }

    pub fn fs_page_backing(&self) -> Arc<dyn FsPageBacking> {
        self.backend.clone().fs_page_backing_arc()
    }

    pub fn bind_mount_payload(&self, payload: &Cap<tx_subsystems::mount::MountPayload>) {
        self.backend.bind_mount_payload(payload);
    }
}

/// Wire type so the tx-fs bridge crate can name the mounted
/// type without depending on the full `tx_ext4` lib.
pub type Ext4MountWire = MountedExt4<tx_ext4_format::pager::Page4K>;

pub fn mount_ext4_read_only<I>(image: I) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    let backend = Ext4FsInstance::open(image)?;
    let root_fs_object_id = FsObjectId::new(EXT4_ROOT_INODE as u64);
    let root_inode_meta = backend
        .with_pager(|pager| pager.inode_meta(tx_ext4_format::pager::InodeNo::new(EXT4_ROOT_INODE)))
        .map(map_inode_meta)?;

    Ok(MountedExt4 {
        backend,
        root_fs_object_id,
        root_inode_meta,
    })
}
