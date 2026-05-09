//! `FsPageBackingV3` — parallel trait emitting v3 step outcomes.
//!
//! Wave-9a sibling to [`crate::vfs::FsOpsV3`] in
//! `crates/tx-subsystems/src/vfs/execution.rs`. Per
//! `docs/progress/decisions/2026-05-09-fsops-v3-design.md` (wave-8
//! design doc), `FsPageBacking` ships its v3 mirror at the same time
//! as the first production `FsOpsV3` impl (`Tmpfs`) because
//! `MountOutput` / `MountPayload` holds both trait objects
//! (`Arc<dyn FsOps>`, `Arc<dyn FsPageBacking>`) and every backend
//! constructs them together. Shipping both v3 traits in the same wave
//! lets each backend dual-route in one step.
//!
//! Per-method progress-type choice: every method picks `NoProgress`.
//! The design doc's open question on `fetch_page` resolves to
//! `NoProgress`: the trait surface is "fetch one page" — the caller
//! asked for one specific page; partial progress within a single page
//! fetch is meaningless, and multi-page accumulation lives at the
//! *call-site* loop (`step_fsync_v3`, `step_truncate_v3`) where
//! `PageProgress` is tallied against the dirty-page snapshot, not at
//! the trait surface. Same reasoning for `flush_page`, `truncate`,
//! `fsync`, `fallocate`. If a later backend surfaces real per-call
//! partial progress (e.g. a `fetch_page` that streams sub-page chunks),
//! it grows a new method rather than re-typing the trait surface.
//!
//! `supports_reflink` mirrors the v4 trait shape (boolean predicate,
//! not StepOutcome-returning) so backends keep impling the same
//! signature on both during the migration.

use crate::execution::Guard;
use crate::vfs::FsObjectId;

use super::{Frame, PageContainer};

/// Parallel `FsPageBacking` trait emitting v3 step outcomes.
///
/// Mirrors the 5 stepping methods of [`super::FsPageBacking`] one-for-one
/// with every `StepOutcome<T>` replaced by
/// `tx_substrate::step_v3::StepOutcome<T, NoProgress>`. Defaults match
/// `FsPageBacking` exactly: `fallocate` defaults to `Done(())`,
/// `supports_reflink` defaults to `false`.
///
/// Wave-9a introduces this trait with first impls on `Tmpfs` and
/// `TestFs`; wave 9b fans out to the remaining five backends
/// (`Devfs`, `Ext4FsInstance`, `DevptsInstance`, `ExecTestFs`,
/// `ExecveTestFs`).
pub trait FsPageBackingV3: Send + Sync + 'static {
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

    /// Default returns `Done(())` (parity with
    /// [`super::FsPageBacking::fallocate`]).
    fn fallocate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::done(())
    }

    /// Reflink predicate — same shape as the v4 trait. Default `false`,
    /// mirroring [`super::FsPageBacking::supports_reflink`].
    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}
