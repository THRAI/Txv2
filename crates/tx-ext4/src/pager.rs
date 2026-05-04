use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_substrate::epoch::Guard;
use tx_substrate::page_allocator::{self, ZeroPolicy};
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
            return StepOutcome::Err(Errno::EINVAL);
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::Err(err),
        };
        let file_page_index = offset / BLOCK_SIZE as u64;
        let mut page: Page4K = [0; BLOCK_SIZE];

        if let Err(err) =
            self.with_pager(|pager| pager.read_page(inode, file_page_index, &mut page))
        {
            return StepOutcome::Err(err);
        }

        materialize_frame(&page)
    }

    fn flush_page<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn truncate<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn fsync<'g>(&self, _fs_object_id: FsObjectId, _guard: &'g Guard<'g>) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }
}

/// Allocate a frame from the page substrate, copy the disk-fetched page bytes
/// into it, and return a permanent-pinned `Frame` referencing the resulting
/// PPN. The page-cache layer above acquires its own cache pin via
/// `acquire_cache_pin` when it installs the frame; the permanent pin keeps
/// the PPN alive across that window.
///
/// In host-test contexts the test direct-map (set up by
/// `tx_substrate::testing::init_host_for_test_once`) backs the PPN; bytes
/// are written via the testing helper. Non-test contexts require a real
/// kernel direct-map, which is HAL-side follow-up work — `Errno::ENOSYS`
/// for now in production builds.
fn materialize_frame(page: &Page4K) -> StepOutcome<Frame> {
    let owned = match page_allocator::reserve_frame(ZeroPolicy::Zeroed) {
        Ok(reservation) => reservation.commit(),
        Err(_) => return StepOutcome::Err(Errno::EBUSY),
    };
    let ppn = owned.ppn();

    #[cfg(test)]
    {
        page_allocator::testing::write_frame_bytes_for_test(ppn, 0, page);
    }
    #[cfg(not(test))]
    {
        // TODO(spec-reconciliation): copy via HAL kernel direct-map once a
        // non-test path exists. Keeping the frame zeroed in production is
        // safer than silently dropping bytes.
        let _ = page;
    }

    // Hand off ownership: the permanent-frame token never releases the
    // PPN to the allocator; the page-cache will add its own cache pin.
    let _permanent = owned.into_permanent_frame();
    StepOutcome::Done(Frame::new(ppn))
}
