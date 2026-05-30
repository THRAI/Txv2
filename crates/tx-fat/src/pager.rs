//! FsPageBacking implementation for tx-fat.
//!
//! Reads file content through the FAT cluster chain.

use crate::adapter::step_engine::{page_allocator, Guard, NoProgress, StepOutcome};
use crate::read_backend::{cluster_from_fs_id, FatFsInstance};
use page_allocator::ZeroPolicy;
use tx_fat_format::ondisk;
use tx_fat_format::pager::{BlockImage, FatFormatError};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{reserve_frame_with_reclaim, Frame, FsPageBacking};
use tx_subsystems::vfs::structure::FsObjectId;

// ====================================================================
// Factory
// ====================================================================

impl<I> FatFsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_page_backing_arc(
        self: alloc::sync::Arc<Self>,
    ) -> alloc::sync::Arc<dyn FsPageBacking> {
        self
    }
}

// ====================================================================
// FsPageBacking impl
// ====================================================================

impl<I> FsPageBacking for FatFsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        const PAGE_SIZE: u64 = 4096;
        if !offset.is_multiple_of(PAGE_SIZE) {
            return StepOutcome::err(Errno::EINVAL);
        }

        let cluster = cluster_from_fs_id(fs_object_id);

        // Walk the FAT chain, but only read clusters that intersect
        // the requested page instead of the whole file.
        let result = self.with_pager(|p| {
            let cluster_size = p.bpb.bytes_per_cluster() as u64;
            if cluster_size == 0 {
                return Err(FatFormatError::Corrupt);
            }

            // Compute the cluster range for this page.
            let page_start = offset;
            let page_end = offset + PAGE_SIZE;
            let _start_cluster_idx = (page_start / cluster_size) as usize;
            let end_cluster_idx = page_end.div_ceil(cluster_size) as usize;

            // Allocate a page-sized buffer (zeroed for partial reads / holes).
            let mut page_buf = [0u8; PAGE_SIZE as usize];
            let mut current_cluster = cluster;
            let mut idx: usize = 0;

            loop {
                // If we've passed the needed range, stop.
                if idx >= end_cluster_idx {
                    break;
                }

                // Read the cluster data first, then check what's next.
                // This ensures single-cluster files and the last cluster
                // of multi-cluster files are actually read before the
                // EOC check terminates the loop.
                let cluster_start = idx as u64 * cluster_size;
                let cluster_end = cluster_start + cluster_size;
                if cluster_start < page_end && cluster_end > page_start {
                    let copy_start = page_start.saturating_sub(cluster_start) as usize;
                    let copy_end =
                        ((page_end.min(cluster_end)).saturating_sub(cluster_start)) as usize;
                    let copy_len = copy_end - copy_start;
                    let dest_offset = (cluster_start.saturating_sub(page_start)) as usize;

                    let mut cluster_buf = alloc::vec![0u8; cluster_size as usize];
                    p.read_cluster(current_cluster, &mut cluster_buf)?;
                    page_buf[dest_offset..dest_offset + copy_len]
                        .copy_from_slice(&cluster_buf[copy_start..copy_start + copy_len]);
                }

                // Check for EOC or bad cluster AFTER reading the data.
                let next = p.read_fat_entry(current_cluster);
                let fat_type = p.bpb.fat_type;
                if ondisk::is_bad(next, fat_type) {
                    return Err(FatFormatError::Corrupt);
                }
                if ondisk::is_eoc(next, fat_type) {
                    break;
                }
                current_cluster = next;
                idx += 1;
            }

            Ok(page_buf)
        });

        match result {
            Ok(page_buf) => {
                let owned = match reserve_frame_with_reclaim(ZeroPolicy::Zeroed) {
                    Ok(reservation) => reservation.commit(),
                    Err(_) => return StepOutcome::err(Errno::EBUSY),
                };
                let ppn = owned.ppn();

                #[cfg(test)]
                {
                    page_allocator::testing::write_frame_bytes_for_test(ppn, 0, &page_buf);
                }
                #[cfg(not(test))]
                {
                    let dst = match page_allocator::frame_kernel_addr(ppn) {
                        Ok(ptr) => ptr,
                        Err(_) => return StepOutcome::err(Errno::EIO),
                    };
                    unsafe {
                        core::ptr::copy_nonoverlapping(page_buf.as_ptr(), dst, PAGE_SIZE as usize);
                    }
                }

                StepOutcome::done(Frame::from_owned(owned))
            }
            Err(err) => StepOutcome::err(err),
        }
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        const PAGE_SIZE: u64 = 4096;
        if !offset.is_multiple_of(PAGE_SIZE) {
            return StepOutcome::err(Errno::EINVAL);
        }

        let cluster = cluster_from_fs_id(fs_object_id);
        let ppn = frame.ppn();

        // Read the frame content into a local buffer.
        let page_data = {
            let ptr = match page_allocator::frame_kernel_addr(ppn) {
                Ok(p) => p,
                Err(_) => return StepOutcome::err(Errno::EIO),
            };
            let mut buf = [0u8; PAGE_SIZE as usize];
            // SAFETY: `ptr` points to a valid kernel mapping of the frame.
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), PAGE_SIZE as usize);
            }
            buf
        };

        // Walk the FAT chain and write the clusters that intersect
        // the page.
        let result: core::result::Result<(), Errno> = self.with_pager(|p| {
            let cluster_size = p.bpb.bytes_per_cluster() as u64;
            if cluster_size == 0 {
                return Err(FatFormatError::Corrupt);
            }

            let page_start = offset;
            let page_end = offset + PAGE_SIZE;
            let end_cluster_idx = page_end.div_ceil(cluster_size) as usize;

            let mut current_cluster = cluster;
            let mut idx: usize = 0;

            loop {
                if idx >= end_cluster_idx {
                    break;
                }

                let cluster_start = idx as u64 * cluster_size;
                let cluster_end = cluster_start + cluster_size;

                // If this cluster intersects the page, write the
                // appropriate portion.
                if cluster_start < page_end && cluster_end > page_start {
                    let src_start = cluster_start.saturating_sub(page_start) as usize;
                    let src_end = (page_end.min(cluster_end)).saturating_sub(page_start) as usize;
                    let copy_len = src_end - src_start;

                    // Read the existing cluster, overlay our changes,
                    // and write back (FAT clusters may be larger or
                    // smaller than a page).
                    let cluster_bytes = cluster_size as usize;
                    let mut cluster_buf = alloc::vec![0u8; cluster_bytes];

                    // Read-modify-write: read the current cluster content.
                    p.read_cluster(current_cluster, &mut cluster_buf)?;

                    // Dest offset within the cluster.
                    let dest_start = page_start.saturating_sub(cluster_start) as usize;
                    cluster_buf[dest_start..dest_start + copy_len]
                        .copy_from_slice(&page_data[src_start..src_start + copy_len]);

                    p.write_cluster(current_cluster, &cluster_buf)?;
                }

                // Check for EOC.
                let next = p.read_fat_entry(current_cluster);
                let fat_type = p.bpb.fat_type;
                if ondisk::is_bad(next, fat_type) {
                    return Err(FatFormatError::Corrupt);
                }
                if ondisk::is_eoc(next, fat_type) {
                    break;
                }
                current_cluster = next;
                idx += 1;
            }

            Ok(())
        });

        match result {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err),
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        use crate::read_backend::{is_fat_root, FAT_ROOT_CLUSTER_SENTINEL};

        // Root directory — nothing to truncate.
        if is_fat_root(fs_object_id) {
            return StepOutcome::done(());
        }

        // Look up cached dirent for parent info.
        let cached = match self.dirent_cache.lock().get(fs_object_id) {
            Some(c) => c,
            None => return StepOutcome::err(Errno::ENOENT),
        };

        // Preserve existing timestamp.
        let fat_date = cached.write_date;
        let fat_time = cached.write_time;
        let new_size_u32 = new_size as u32;

        let parent_cluster = cached.parent_cluster;
        let target_cluster = cached.first_cluster;

        let result = self.with_pager(|p| {
            if parent_cluster == FAT_ROOT_CLUSTER_SENTINEL {
                p.update_dirent_in_root(target_cluster, new_size_u32, fat_date, fat_time)
            } else {
                p.update_dirent_in_subdir(
                    parent_cluster,
                    target_cluster,
                    new_size_u32,
                    fat_date,
                    fat_time,
                )
            }
        });

        match result {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err),
        }
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
}
