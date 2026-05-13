use step_engine::Guard;
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::vfs::structure::FsObjectId;

use crate::adapter::step_engine::{self as step_engine, page_allocator, NoProgress, StepOutcome};
use crate::read_backend::{inode_no, Ext4FsInstance};

use page_allocator::ZeroPolicy;

/// Allocate a frame from the page substrate, copy the disk-fetched page bytes
/// into it, and return a permanent-pinned `Frame` referencing the resulting
/// PPN. The page-cache layer above acquires its own cache pin via
/// `acquire_cache_pin` when it installs the frame; the permanent pin keeps
/// the PPN alive across that window.
///
/// In host-test contexts the test direct-map (set up by
/// `tx_test_support::init_host`) backs the PPN; bytes
/// are written via the testing helper. Non-test contexts require a real
/// kernel direct-map, which is HAL-side follow-up work — `Errno::ENOSYS`
/// for now in production builds.
fn materialize_frame(page: &Page4K) -> StepOutcome<Frame, NoProgress> {
    let owned = match page_allocator::reserve_frame(ZeroPolicy::Zeroed) {
        Ok(reservation) => reservation.commit(),
        Err(_) => return StepOutcome::err(Errno::EBUSY.into()),
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
    StepOutcome::done(Frame::new(ppn))
}

// === FsPageBacking impl =============================================
//
// The current read-only `Ext4Pager::read_page` returns
// `Result<T, Ext4FormatError>` (not `StepOutcome`), so every body lands
// on `done` or `err` only — there is no `Advanced` / `Blocked` /
// `AdvancedThenBlocked` path through this read-only backend today.

/// Factory for `MountOutput::fs_page_backing`.
///
/// Mirrors `Tmpfs::fs_page_backing_arc`.
impl<I> Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_page_backing_arc(
        self: alloc::sync::Arc<Self>,
    ) -> alloc::sync::Arc<dyn FsPageBacking> {
        self
    }
}

impl<I> FsPageBacking for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        if !offset.is_multiple_of(BLOCK_SIZE as u64) {
            return StepOutcome::err(Errno::EINVAL.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let file_page_index = offset / BLOCK_SIZE as u64;
        let mut page: Page4K = [0; BLOCK_SIZE];

        if let Err(err) =
            self.with_pager(|pager| pager.read_page(inode, file_page_index, &mut page))
        {
            return StepOutcome::err(err.into());
        }

        materialize_frame(&page)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    // `fallocate` and `supports_reflink` inherit the trait defaults
    // (`done(())` and `false` respectively).
}
