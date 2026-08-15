use alloc::vec::Vec;
use step_engine::Guard;
use tx_ext4_format::mutation::FsyncStamp;
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{
    reserve_frame_with_reclaim, FilesystemStats, Frame, FsPageBacking,
};
use tx_subsystems::vfs::structure::FsObjectId;

use crate::adapter::step_engine::{self as step_engine, page_allocator, NoProgress, StepOutcome};
use crate::read_backend::Ext4FsInstance;

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
    fn uses_async_writeback(&self) -> bool {
        !self.legacy_writeback_enabled()
    }

    fn flush_batch_limit(&self) -> usize {
        16
    }

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
        let inode = match self.resolve_object(fs_object_id) {
            Ok((inode, _)) => inode,
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
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        self.flush_pages(fs_object_id, offset, core::slice::from_ref(frame), guard)
    }

    fn flush_pages(
        &self,
        fs_object_id: FsObjectId,
        first_offset: u64,
        frames: &[Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if frames.is_empty() {
            return StepOutcome::done(());
        }
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        if !first_offset.is_multiple_of(BLOCK_SIZE as u64) {
            return StepOutcome::err(Errno::EINVAL.into());
        }
        let (inode, current) = match self.resolve_object(fs_object_id) {
            Ok(resolved) => resolved,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let first_file_page = first_offset / BLOCK_SIZE as u64;
        if first_file_page
            .checked_add(frames.len().saturating_sub(1) as u64)
            .is_none()
        {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        let mut pages = Vec::with_capacity(frames.len());
        for frame in frames {
            let mut page: Page4K = [0; BLOCK_SIZE];
            if let Err(err) = read_frame_bytes(frame, &mut page) {
                return StepOutcome::err(err.into());
            }
            pages.push(page);
        }

        let mutation = match self.with_pager(|pager| {
            if self.metadata_mutation_runtime().is_some() {
                // Journal/I/O-manager mounts fold the PageContainer's exact
                // EOF into the same transaction as ordered page data.
                pager.plan_write_pages_with_size(
                    inode,
                    first_file_page,
                    &pages,
                    self.file_page_container_size(fs_object_id),
                    FsyncStamp::new(current.ctime as u64),
                )
            } else {
                // Compatibility mounts execute `step_fsync` synchronously:
                // admit one bounded contiguous run, then let the final
                // truncate publish the byte-precise EOF. This avoids one
                // metadata plan and one admission cycle per dirty page.
                pager.plan_write_pages(
                    inode,
                    first_file_page,
                    &pages,
                    FsyncStamp::new(current.ctime as u64),
                )
            }
        }) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_inode_meta_for(fs_object_id);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let (inode, current) = match self.resolve_object(fs_object_id) {
            Ok(resolved) => resolved,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let mutation = match self.with_pager(|pager| {
            pager.plan_truncate_size(inode, new_size, FsyncStamp::new(current.ctime as u64))
        }) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_inode_meta_for(fs_object_id);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn prepare_write_range(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        len: usize,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if len == 0 {
            return StepOutcome::done(());
        }
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        if self.metadata_mutation_runtime().is_none() && !self.legacy_writeback_enabled() {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        let Some(end) = offset.checked_add(len as u64) else {
            return StepOutcome::err(Errno::EINVAL.into());
        };
        let file_page_index = offset / BLOCK_SIZE as u64;
        if end == 0 || (end - 1) / BLOCK_SIZE as u64 != file_page_index {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        if let Err(errno) = crate::read_backend::inode_no(fs_object_id) {
            return StepOutcome::err(errno.into());
        }
        // Buffered writes own only page-cache state. Hole allocation, ordered
        // data, and byte-precise i_size are admitted by the later bounded
        // writeback transaction. The generation-qualified PageContainer is
        // already the retained object identity; resolving its inode again on
        // every cached page write only serializes the data hot path.
        StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if !self.legacy_writeback_enabled() {
            return StepOutcome::err(Errno::ENOSYS.into());
        }
        StepOutcome::done(())
    }

    // `fallocate` and `supports_reflink` inherit the trait defaults
    // (`done(())` and `false` respectively).
}
