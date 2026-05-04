use tx_ext4_format::pager::BlockImage;
use tx_subsystems::step::Errno;
use tx_subsystems::vfs::fs_ops::MountOutput;
use tx_subsystems::vfs::structure::FsObjectId;

use crate::read_backend::{map_inode_meta, Ext4FsInstance, EXT4_ROOT_INODE};

pub fn mount_ext4_read_only<I>(image: I) -> Result<MountOutput, Errno>
where
    I: BlockImage + Send + 'static,
{
    let backend = Ext4FsInstance::open(image)?;
    let root_fs_object_id = FsObjectId(EXT4_ROOT_INODE as u64);
    let root_inode_meta = backend
        .with_pager(|pager| pager.inode_meta(tx_ext4_format::pager::InodeNo::new(EXT4_ROOT_INODE)))
        .map(map_inode_meta)?;

    Ok(MountOutput {
        fs_ops: backend.clone(),
        fs_page_backing: backend,
        root_fs_object_id,
        root_inode_meta,
    })
}
