use tx_ext4_format::pager::BlockImage;
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::MountPayloadPin;
use tx_subsystems::vfs::execution::MountOutput;
use tx_subsystems::vfs::structure::FsObjectId;

use crate::read_backend::{map_inode_meta, Ext4FsInstance, EXT4_ROOT_INODE};

/// Opaque handle returned by [`mount_ext4_read_only`] to wire the
/// `MountPayloadPin` into the ext4 backend after the caller builds the
/// `MountPayload`.  Call [`Ext4MountWire::register_pin`] exactly once.
pub struct Ext4MountWire {
    register: alloc::boxed::Box<dyn FnOnce(MountPayloadPin) + Send>,
}

impl Ext4MountWire {
    pub fn register_pin(self, pin: MountPayloadPin) {
        (self.register)(pin);
    }
}

pub fn mount_ext4_read_only<I>(image: I) -> Result<(MountOutput, Ext4MountWire), Errno>
where
    I: BlockImage + Send + 'static,
{
    let backend = Ext4FsInstance::open(image)?;
    let root_fs_object_id = FsObjectId::new(EXT4_ROOT_INODE as u64);
    let root_inode_meta = backend
        .with_pager(|pager| pager.inode_meta(tx_ext4_format::pager::InodeNo::new(EXT4_ROOT_INODE)))
        .map(map_inode_meta)?;

    let wire_backend = backend.clone();
    let fs_ops = backend.clone().fs_ops_arc();
    let fs_page_backing = backend.fs_page_backing_arc();
    let wire = Ext4MountWire {
        register: alloc::boxed::Box::new(move |pin| wire_backend.register_mount_pin(pin)),
    };
    Ok((
        MountOutput {
            fs_ops,
            fs_page_backing,
            root_fs_object_id,
            root_inode_meta,
        },
        wire,
    ))
}
