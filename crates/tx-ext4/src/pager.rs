use step_engine::Guard;
use tx_ext4_format::pager::{BlockImage, CheckedPageRead, Page4K, BLOCK_SIZE};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{
    reserve_frame_with_reclaim, FilesystemStats, Frame, FsPageBacking,
};
use tx_subsystems::vfs::structure::FsObjectId;

use crate::adapter::step_engine::{self as step_engine, page_allocator, NoProgress, StepOutcome};
use crate::read_backend::{inode_no, Ext4FsInstance};

use page_allocator::ZeroPolicy;

/// Allocate a frame from the page substrate, copy the disk-fetched page bytes
/// into it, and return an owned `Frame` referencing the resulting PPN. The
/// page-cache layer above acquires its own cache pin when it installs the
/// frame, then releases this temporary owner.
///
/// In host-test contexts the test direct-map (set up by
/// `tx_test_support::init_host`) backs the PPN; bytes
/// are written via the testing helper. Non-test contexts require a real
/// kernel direct-map, which is HAL-side follow-up work — `Errno::ENOSYS`
/// for now in production builds.
fn materialize_frame(page: &Page4K) -> StepOutcome<Frame, NoProgress> {
    let owned = match reserve_frame_with_reclaim(ZeroPolicy::Zeroed) {
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
        let dst = match page_allocator::frame_kernel_addr(ppn) {
            Ok(ptr) => ptr,
            Err(_) => return StepOutcome::err(Errno::EIO.into()),
        };
        // SAFETY: `dst` is the kernel direct-map VA of a freshly
        // allocated frame we own through `owned`. `page` is a
        // `&[u8; BLOCK_SIZE]`.  Both regions are disjoint and valid
        // for BLOCK_SIZE bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(page.as_ptr(), dst, BLOCK_SIZE);
        }
    }

    // Hand off the temporary owner to the page-cache. It will acquire
    // the cache pin and then release this owner after installation.
    StepOutcome::done(Frame::from_owned(owned))
}

/// Copy a frame's `BLOCK_SIZE` bytes into `page` — the read counterpart of
/// [`materialize_frame`], used by `flush_page` to capture a dirty page's
/// contents before writing them back to disk.
fn read_frame_bytes(frame: &Frame, page: &mut Page4K) -> core::result::Result<(), Errno> {
    let ppn = frame.ppn();
    #[cfg(test)]
    {
        page_allocator::testing::read_frame_bytes_for_test(ppn, 0, page);
    }
    #[cfg(not(test))]
    {
        let src = match page_allocator::frame_kernel_addr(ppn) {
            Ok(ptr) => ptr,
            Err(_) => return Err(Errno::EIO),
        };
        // SAFETY: `src` is the kernel direct-map VA of the frame backing
        // `ppn`; `page` is `&mut [u8; BLOCK_SIZE]`. Both regions are valid
        // and disjoint for BLOCK_SIZE bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, page.as_mut_ptr(), BLOCK_SIZE);
        }
    }
    Ok(())
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
    fn filesystem_stats(&self, _guard: &Guard<'_>) -> StepOutcome<FilesystemStats, NoProgress> {
        match self.with_pager(|pager| pager.filesystem_stats()) {
            Ok(stats) => StepOutcome::done(FilesystemStats {
                block_size: stats.block_size,
                total_blocks: stats.total_blocks,
                free_blocks: stats.free_blocks,
                available_blocks: stats.available_blocks,
                total_inodes: stats.total_inodes,
                free_inodes: stats.free_inodes,
                max_name_len: stats.max_name_len,
            }),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

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

        match self.with_pager(|pager| {
            pager.read_page_checked(
                inode,
                fs_object_id.inode_generation(),
                file_page_index,
                &mut page,
            )
        }) {
            Ok(CheckedPageRead::Page(_)) => {}
            Ok(CheckedPageRead::Missing) => return StepOutcome::err(Errno::ENOENT.into()),
            Ok(CheckedPageRead::Stale) => return StepOutcome::err(Errno::ESTALE.into()),
            Err(err) => return StepOutcome::err(err.into()),
        }

        materialize_frame(&page)
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if !offset.is_multiple_of(BLOCK_SIZE as u64) {
            return StepOutcome::err(Errno::EINVAL.into());
        }
        let inode = match self.resolve_object(fs_object_id) {
            Ok((inode, _)) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let file_page_index = offset / BLOCK_SIZE as u64;
        let mut page: Page4K = [0; BLOCK_SIZE];
        if let Err(err) = read_frame_bytes(frame, &mut page) {
            return StepOutcome::err(err.into());
        }
        match self.with_pager_raw(|pager| pager.write_page(inode, file_page_index, &page)) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => {
                let stats = self.with_pager_raw(|pager| pager.filesystem_stats()).ok();
                crate::report_writeback_stats(inode.get(), file_page_index, err, stats);
                StepOutcome::err(crate::read_backend::map_format_error(err).into())
            }
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let inode = match self.resolve_object(fs_object_id) {
            Ok((inode, _)) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let now_sec = tx_subsystems::wall_clock::current_realtime_sec().min(u32::MAX as u64) as u32;
        match self.with_pager(|pager| pager.set_inode_size_and_times(inode, new_size, now_sec)) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn fsync_file(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        match self.resolve_object(fs_object_id) {
            Ok(_) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    // `fallocate` and `supports_reflink` inherit the trait defaults
    // (`done(())` and `false` respectively).
}
