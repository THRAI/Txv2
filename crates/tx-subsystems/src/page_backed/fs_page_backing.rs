//! `FsPageBacking` — page-backing trait emitting `step_v3` outcomes.
//!
//! Sibling to [`crate::vfs::FsOps`] in
//! `crates/tx-subsystems/src/vfs/execution.rs`. `MountOutput` /
//! `MountPayload` holds both trait objects (`Arc<dyn FsOps>`,
//! `Arc<dyn FsPageBacking>`); every backend constructs them together.
//!
//! Per-method progress-type choice: every method picks `NoProgress`.
//! `fetch_page` resolves to `NoProgress` because the trait surface is
//! "fetch one page" — the caller asked for one specific page; partial
//! progress within a single page fetch is meaningless, and multi-page
//! accumulation lives at the *call-site* loop (`step_fsync`,
//! `step_truncate`) where `PageProgress` is tallied against the
//! dirty-page snapshot, not at the trait surface. Same reasoning for
//! `flush_page`, `truncate`, `fsync`, `fallocate`. If a later backend
//! surfaces real per-call partial progress (e.g. a `fetch_page` that
//! streams sub-page chunks), it grows a new method rather than
//! re-typing the trait surface.
//!
//! `supports_reflink` is a boolean predicate (not `StepOutcome`-
//! returning).

use crate::execution::{Errno, Guard};
use crate::vfs::FsObjectId;

use super::{Frame, PageContainer};
use crate::page_backed::adapter::step_engine::{NoProgress, StepOutcome};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemStats {
    pub block_size: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub available_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub max_name_len: u64,
}

/// `FsPageBacking` trait emitting `step_v3` outcomes.
///
/// Each stepping method returns
/// `StepOutcome<T, NoProgress>`. `fallocate`
/// defaults to `Done(())` and `supports_reflink` defaults to `false`.
pub trait FsPageBacking: Send + Sync + 'static {
    /// Whether dirty pages are owned by the mount's asynchronous backend
    /// planner.  A filesystem may install a planner only for parallel reads
    /// while retaining synchronous compatibility writeback; planner presence
    /// alone is therefore not a writeback capability test.
    fn uses_async_writeback(&self) -> bool {
        false
    }

    /// Maximum contiguous page count accepted atomically by `flush_pages`.
    /// Backends inherit one-page progress semantics until they explicitly
    /// implement a range transaction.
    fn flush_batch_limit(&self) -> usize {
        1
    }

    fn filesystem_stats(&self, _guard: &Guard<'_>) -> StepOutcome<FilesystemStats, NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress>;

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    /// Flush one contiguous run of file pages.
    ///
    /// The compatibility implementation preserves existing backends by
    /// replaying `flush_page`; its matching `flush_batch_limit` is one.
    /// Filesystems with a range planner override both methods so allocation
    /// and metadata publication are admitted once for the whole run.
    fn flush_pages(
        &self,
        fs_object_id: FsObjectId,
        first_offset: u64,
        frames: &[Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let mut offset = first_offset;
        for frame in frames {
            match self.flush_page(fs_object_id, offset, frame, guard) {
                StepOutcome::Done(()) => {}
                StepOutcome::Continue { progress } => {
                    return StepOutcome::Continue { progress };
                }
                StepOutcome::Yield { progress, shape } => {
                    return StepOutcome::Yield { progress, shape };
                }
                StepOutcome::Err(errno) => return StepOutcome::Err(errno),
            }
            let Some(next) = offset.checked_add(crate::vm::USER_PAGE_SIZE as u64) else {
                return StepOutcome::err(Errno::EINVAL.into());
            };
            offset = next;
        }
        StepOutcome::done(())
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn fsync_file(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    /// Pre-admit a buffered write before PageBacked publishes a dirty file
    /// page. The default preserves filesystem backends that do not need
    /// allocation claims; journaling backends may retain growth state before
    /// PageSlot dirties.
    fn prepare_write_range(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _len: usize,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    /// Filesystem-wide flush, the storage backend for `syncfs(2)`.
    /// Default delegates to `fsync_file(ROOT)`; journaling filesystems
    /// override to issue a single barrier across all dirty inodes.
    /// See commit "refactor(fsync): split fsync into fsync_file +
    /// sync_filesystem" for the design split.
    fn sync_filesystem(&self, guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        self.fsync_file(super::super::vfs::structure::FsObjectId::ROOT, guard)
    }

    /// Default returns `Done(())`.
    fn fallocate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    /// Reflink predicate. Default `false`.
    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}
