use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_substrate::epoch::Guard;
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::step::{Errno, StepOutcome};
use tx_subsystems::vfs::structure::FsObjectId;

use crate::read_backend::{inode_no, Ext4FsInstance};

impl<I> FsPageBacking for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<Frame> {
        if !offset.is_multiple_of(BLOCK_SIZE as u64) {
            return StepOutcome::Err(Errno::Invalid);
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::Err(err),
        };
        let file_page_index = offset / BLOCK_SIZE as u64;
        let mut page: Page4K = [0; BLOCK_SIZE];

        match self.with_pager(|pager| pager.read_page(inode, file_page_index, &mut page)) {
            Ok(_) => match Frame::from_bytes(&page) {
                Ok(frame) => StepOutcome::Done(frame),
                Err(err) => StepOutcome::Err(err),
            },
            Err(err) => StepOutcome::Err(err),
        }
    }

    fn flush_page<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn truncate<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn fsync<'g>(&self, _fs_object_id: FsObjectId, _guard: &'g Guard<'g>) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}
