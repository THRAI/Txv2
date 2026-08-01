use tx_subsystems::execution::Errno;

use crate::read_backend::Ext4FsInstance;

impl<I> Ext4FsInstance<I>
where
    I: tx_ext4_format::pager::BlockImage + Send + 'static,
{
    pub(crate) fn shutdown_mount(&self) -> Result<(), Errno> {
        self.settle_metadata_caches();
        *self.mount_pin.lock() = None;
        Ok(())
    }
}
