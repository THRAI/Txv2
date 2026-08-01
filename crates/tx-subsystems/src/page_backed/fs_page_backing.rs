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

use crate::execution::Guard;
use crate::vfs::FsObjectId;

use super::{Frame, PageContainer};
use crate::page_backed::adapter::step_engine::{NoProgress, StepOutcome};

/// `FsPageBacking` trait emitting `step_v3` outcomes.
///
/// Each stepping method returns
/// `StepOutcome<T, NoProgress>`. `fallocate`
/// defaults to `Done(())` and `supports_reflink` defaults to `false`.
pub trait FsPageBacking: Send + Sync + 'static {
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
    /// page. The default preserves tmpfs/devfs/procfs-style backends that do
    /// not need allocation claims; journaling backends override this to fail
    /// closed or retain filesystem-owned growth state before PageSlot dirties.
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
