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

/// `FsPageBacking` trait emitting `step_v3` outcomes.
///
/// Each stepping method returns
/// `tx_substrate::step_v3::StepOutcome<T, NoProgress>`. `fallocate`
/// defaults to `Done(())` and `supports_reflink` defaults to `false`.
pub trait FsPageBacking: Send + Sync + 'static {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress>;

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn fsync(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    /// Default returns `Done(())`.
    fn fallocate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::done(())
    }

    /// Reflink predicate. Default `false`.
    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}
