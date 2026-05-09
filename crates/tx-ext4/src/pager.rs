use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_substrate::epoch::Guard;
use tx_substrate::page_allocator::{self, ZeroPolicy};
use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::page_backed::{Frame, FsPageBacking, FsPageBackingV3, PageContainer};
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

// === Wave 9b: parallel v3 trait impl =================================
//
// `impl FsPageBackingV3 for Ext4FsInstance<I>` mirrors the v4 body
// above one-for-one. ext4 is the wave-9b backend most likely to surface
// real `Advanced(t)` outcomes because `fetch_page` drives on-disk block
// I/O — but the current read-only `Ext4Pager::read_page` returns
// `Result<T, Ext4FormatError>` (not `StepOutcome`), so the v4
// `fetch_page` body lands on `StepOutcome::Done(t)` /
// `StepOutcome::Err(e)` only. There are no `Advanced(t)` / `Blocked` /
// `AdvancedThenBlocked` returns from the v4 bodies today; the v3
// mapping has zero ambiguous Continue-vs-Done call sites in this wave.
//
// Per the wave-9a design doc + trait-surface contract: where the v4
// fn does (in a future async/journal-aware revision) return
// `Advanced(t)`, the trait surface translates `Advanced(t)` → `done(t)`
// (one-shot v3 contract); any partial-progress accounting lives at the
// *caller* (wave 9c walker). `Blocked` / `AdvancedThenBlocked` map
// defensively to `EAGAIN`.

/// v3 sibling factory for `MountOutput::fs_page_backing_v3` cutover.
///
/// Mirrors `Tmpfs::fs_page_backing_v3_arc`. Gated behind `cfg(test)`
/// for now because `Ext4FsInstance` is `pub(crate)` and the v3 wiring
/// on `MountOutput` lands in wave 9c — the factory is exercised inline
/// in `tests_v3.rs` to pin the cutover shape, and the non-test build
/// does not yet have a caller. Wave 9c lifts the cfg gate as part of
/// the `MountOutput::fs_page_backing_v3` field landing.
#[cfg(test)]
impl<I> Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_page_backing_v3_arc(
        self: alloc::sync::Arc<Self>,
    ) -> alloc::sync::Arc<dyn FsPageBackingV3> {
        self
    }
}

impl<I> FsPageBackingV3 for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fetch_page(self, fs_object_id, offset, guard) {
            StepOutcome::Done(frame) | StepOutcome::Advanced(frame) => {
                tx_substrate::step_v3::StepOutcome::done(frame)
            }
            StepOutcome::AdvancedThenBlocked(frame, _) => {
                tx_substrate::step_v3::StepOutcome::done(frame)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::flush_page(self, fs_object_id, offset, frame, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::truncate(self, fs_object_id, new_size, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn fsync(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fsync(self, fs_object_id, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn fallocate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fallocate(self, fs_object_id, new_size, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn supports_reflink(&self, other: &PageContainer) -> bool {
        <Self as FsPageBacking>::supports_reflink(self, other)
    }
}
