use super::*;
use alloc::vec::Vec;

impl PageCacheIndex {
    fn withdraw_from(&mut self, first: PageIndex) {
        drop(self.pages.split_off(&first));
    }

    fn dirty_pages(&self) -> Vec<(PageIndex, Ppn)> {
        self.pages
            .iter()
            .filter_map(|(page, entry)| entry.marks.dirty.then_some((*page, entry.ppn)))
            .collect()
    }

    fn clear_dirty_if_match(&mut self, page: PageIndex, ppn: Ppn) {
        let Some(entry) = self.pages.get_mut(&page) else {
            return;
        };
        if entry.ppn == ppn {
            entry.marks.dirty = false;
            entry.marks.writeback = false;
        }
    }
}

impl PageContainer {
    fn withdraw_cached_pages_from(&self, first: PageIndex) {
        self.state.lock().pages.withdraw_from(first);
    }

    fn dirty_pages_snapshot(&self) -> Vec<(PageIndex, Ppn)> {
        self.state.lock().pages.dirty_pages()
    }

    fn clear_dirty_if_match(&self, page: PageIndex, ppn: Ppn) {
        self.state.lock().pages.clear_dirty_if_match(page, ppn);
    }
}

pub fn step_truncate(pc: &PageContainer, new_size: u64, guard: &Guard<'_>) -> StepOutcome<()> {
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if new_size > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }

    // Wave 9g-f: migrated to consume the v3 `FsPageBackingV3` trait.
    // The v4 fn signature is preserved so the many existing callers
    // (tx-shims syscalls, tx-fs/tmpfs, vm/execution, tests across the
    // workspace) keep their v4 outcome shape until later waves migrate
    // them. The 4-variant v3 algebra maps back to v4 as:
    //
    //   v3 Done(())                    -> v4 Done(())
    //   v3 Continue { NoProgress }     -> v4 Advanced(())
    //   v3 Yield { OnCarrier { c, i } }-> v4 Blocked(WaitToken::new(c.raw(), i.raw()))
    //   v3 Yield { OnAgent { .. } }    -> v4 Err(EIO)  (no v4 representation)
    //   v3 Err(v3errno)                -> v4 Err(Errno::from(v3errno))
    use tx_substrate::step_v3::{StepOutcome as V3, YieldShape};

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .payload()
            .fs_page_backing_v3
            .truncate(*fs_object_id, new_size, guard)
        {
            V3::Done(()) => false,
            V3::Continue { progress: _ } => true,
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => {
                return StepOutcome::Blocked(crate::execution::WaitToken::new(
                    carrier.raw(),
                    interests.raw(),
                ));
            }
            V3::Yield { shape: YieldShape::OnAgent { .. }, .. } => {
                return StepOutcome::Err(Errno::EIO);
            }
            V3::Err(errno) => return StepOutcome::Err(Errno::from(errno)),
        },
        PageContainerKind::Anon { .. } => false,
        PageContainerKind::Device { .. } => unreachable!(),
    };

    let old_size = pc.size_bytes();
    pc.set_size_bytes(new_size);

    if new_size < old_size {
        let Some(first_drop) = first_page_after_size(new_size) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        pc.withdraw_cached_pages_from(first_drop);
        zero_partial_eof_tail(pc, new_size);
    }

    if fs_advanced {
        StepOutcome::Advanced(())
    } else {
        StepOutcome::Done(())
    }
}

/// Zero the bytes in the cached page containing the new EOF, from the
/// in-page byte offset of `new_size` up to the page end. After
/// truncate-shrink past a non-aligned size, the partial last page must not
/// expose stale post-EOF bytes when later grown back into. No-op when
/// `new_size` falls on a page boundary, when the EOF page is not currently
/// cached, or when the substrate kernel-address hook is not installed.
fn zero_partial_eof_tail(pc: &PageContainer, new_size: u64) {
    let page_size = crate::vm::USER_PAGE_SIZE as u64;
    let within_page = (new_size % page_size) as usize;
    if within_page == 0 || new_size == 0 {
        return;
    }
    let page_index = PageIndex::new(new_size / page_size);
    let Some(ppn) = pc.lookup(page_index) else {
        return;
    };
    let Ok(frame_base) = page_allocator::frame_kernel_addr(ppn) else {
        return;
    };
    let tail_len = (page_size as usize) - within_page;
    unsafe {
        core::ptr::write_bytes(frame_base.add(within_page), 0, tail_len);
    }
}

pub fn step_fsync(pc: &PageContainer, guard: &Guard<'_>) -> StepOutcome<()> {
    // Wave 9g-f: migrated to consume the v3 `FsPageBackingV3` trait.
    // See `step_truncate` for the v3->v4 outcome conversion table; the
    // v3 `Continue { NoProgress }` cases here map to the v4 "Advanced
    // (loop iter counted as progress)" path by toggling `progressed`.
    use tx_substrate::step_v3::{StepOutcome as V3, YieldShape};

    let PageContainerKind::File {
        mount,
        fs_object_id,
    } = pc.kind()
    else {
        return StepOutcome::Done(());
    };

    let mut progressed = false;
    for (page, ppn) in pc.dirty_pages_snapshot() {
        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        match mount.payload().fs_page_backing_v3.flush_page(
            *fs_object_id,
            offset,
            &Frame::new(ppn),
            guard,
        ) {
            V3::Done(()) | V3::Continue { progress: _ } => {
                pc.clear_dirty_if_match(page, ppn);
                progressed = true;
            }
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => {
                let token = crate::execution::WaitToken::new(carrier.raw(), interests.raw());
                return if progressed {
                    StepOutcome::AdvancedThenBlocked((), token)
                } else {
                    StepOutcome::Blocked(token)
                };
            }
            V3::Yield { shape: YieldShape::OnAgent { .. }, .. } => {
                return StepOutcome::Err(Errno::EIO);
            }
            V3::Err(errno) => return StepOutcome::Err(Errno::from(errno)),
        }
    }

    match mount.payload().fs_page_backing_v3.fsync(*fs_object_id, guard) {
        // Even with prior page-loop progress, the v4 contract for "loop
        // progress + fsync clean" is a single `Done(())`, not
        // `Advanced` — match the original v4 body's `other => other`
        // passthrough behavior.
        V3::Done(()) => StepOutcome::Done(()),
        V3::Continue { progress: _ } => StepOutcome::Advanced(()),
        V3::Yield {
            progress: _,
            shape: YieldShape::OnCarrier { carrier, interests },
        } => {
            let token = crate::execution::WaitToken::new(carrier.raw(), interests.raw());
            if progressed {
                StepOutcome::AdvancedThenBlocked((), token)
            } else {
                StepOutcome::Blocked(token)
            }
        }
        V3::Yield { shape: YieldShape::OnAgent { .. }, .. } => StepOutcome::Err(Errno::EIO),
        V3::Err(errno) => StepOutcome::Err(Errno::from(errno)),
    }
}

/// Reserve space up to `new_size` for future writes per PAGE_BACKED §5.5.
///
/// - Device backings reject with `EINVAL` (the device aperture is fixed).
/// - `new_size` beyond the fixed `page_count` capacity rejects with
///   `EINVAL`; the capacity bound is preserved.
/// - `new_size <= pc.size_bytes()` is a `Done(())` no-op (fallocate cannot
///   shrink; that is the truncate path).
/// - File backings call `FsPageBacking::fallocate` first; on backing
///   success, `pc.size_bytes` is published. Pages are not materialized.
/// - Anon backings simply publish the new visible size; per spec this is
///   "mostly a hint".
pub fn step_fallocate(pc: &PageContainer, new_size: u64, guard: &Guard<'_>) -> StepOutcome<()> {
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if new_size > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }

    if new_size <= pc.size_bytes() {
        return StepOutcome::Done(());
    }

    use tx_substrate::step_v3::{StepOutcome as V3, YieldShape};
    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .payload()
            .fs_page_backing_v3
            .fallocate(*fs_object_id, new_size, guard)
        {
            V3::Done(()) => false,
            V3::Continue { progress: _ } => true,
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => {
                return StepOutcome::Blocked(crate::execution::WaitToken::new(
                    carrier.raw(),
                    interests.raw(),
                ));
            }
            V3::Yield {
                shape: YieldShape::OnAgent { .. },
                ..
            } => return StepOutcome::Err(Errno::EIO),
            V3::Err(v3_errno) => return StepOutcome::Err(Errno::from(v3_errno)),
        },
        PageContainerKind::Anon { .. } => false,
        PageContainerKind::Device { .. } => unreachable!(),
    };

    pc.set_size_bytes(new_size);

    if fs_advanced {
        StepOutcome::Advanced(())
    } else {
        StepOutcome::Done(())
    }
}

fn first_page_after_size(size: u64) -> Option<PageIndex> {
    let page_size = crate::vm::USER_PAGE_SIZE as u64;
    if size == 0 {
        return Some(PageIndex::new(0));
    }
    size.checked_add(page_size - 1)
        .map(|rounded| PageIndex::new(rounded / page_size))
}

// -- v3 cascade probe (wave 7) -----------------------------------------------
//
// Sibling `*_v3` fns matching the same logic as the v4 fns above but
// emitting v3 step-algebra outcomes (`tx_substrate::step_v3::StepOutcome`)
// over `PageProgress` (page-shaped unit of work per
// `docs/Txv3/03_STEP_MODEL_v2.md` §STEP-V2-PROGRESS-TYPED-1).
//
// Existing callers stay on the v4 fns; later waves switch them over and
// delete the v4 fns. We re-run the body inline rather than delegating, so
// the v3 path is independently testable and there's no conversion-shim
// layer.
//
// Per-call-site `Advanced(())` mapping decisions (the load-bearing TDD
// signal for the wave-7 probe):
//
// * `step_fsync_v3`: each successful page flush is one unit of page work.
//   We track `pages_so_far: u32` across the loop and surface it through
//   `PageProgress::new(pages_so_far)` whenever the v4 path would have
//   yielded `AdvancedThenBlocked((), token)`. The v4 final-fsync `Blocked`
//   case (post-loop) similarly carries the page count of dirty pages
//   already flushed. The v4 final-fsync `Done(())` / non-blocked passthrough
//   maps to `done(())`.
// * `step_truncate_v3`: the fs `Advanced(())` is fs-implementation-internal
//   partial progress, NOT a page count — the v4 `T = ()` carries no page
//   delta to plumb through. We map `Advanced(())` to
//   `continue_with(PageProgress::EMPTY)` ("made progress, retry; no
//   page-count to expose"), and `AdvancedThenBlocked((), token)` to
//   `yield_on_carrier(PageProgress::EMPTY, c, i)` for the same reason.
//   The simpler-path choice flagged in the wave-7 worker spec; an inherent
//   `PageProgress::EMPTY` shadow (parallel to `ByteProgress::EMPTY`) would
//   shave an import line at these call sites — see report.

/// `step_fsync` — v3 outcome shape over `PageProgress`.
///
/// Same body and semantics as [`step_fsync`], translated to a v3
/// [`tx_substrate::step_v3::StepOutcome`]:
///
/// - `Anon` / `Device` / no dirty pages → `Done(())`
/// - blocking mid-loop with prior progress → `Yield { progress:
///   PageProgress::new(pages_so_far), shape: OnCarrier { … } }`
/// - blocking with no prior progress → `Yield { progress:
///   PageProgress::EMPTY, shape: OnCarrier { … } }`
/// - underlying flush/fsync errno → `Err(errno)`
/// - all dirty pages flushed and final fsync clean → `Done(())`
///
/// Per-call-site `Advanced(())` decision: counted as one page of progress
/// (we just cleared a dirty mark), accumulated into `pages_so_far` and
/// surfaced through `PageProgress::new(pages_so_far)` when the next call
/// blocks. See module-level comment.
pub fn step_fsync_v3(
    pc: &PageContainer,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::PageProgress> {
    use tx_substrate::step_v3::{PageProgress, StepOutcome as V3, YieldShape};

    let PageContainerKind::File {
        mount,
        fs_object_id,
    } = pc.kind()
    else {
        return V3::done(());
    };

    let mut pages_so_far: u32 = 0;
    for (page, ppn) in pc.dirty_pages_snapshot() {
        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            return V3::err(Errno::EINVAL.into());
        };
        match mount.payload().fs_page_backing_v3.flush_page(
            *fs_object_id,
            offset,
            &Frame::new(ppn),
            guard,
        ) {
            V3::Done(()) => {
                pc.clear_dirty_if_match(page, ppn);
                pages_so_far = pages_so_far.saturating_add(1);
            }
            V3::Continue { progress: _ } => {
                let progress = if pages_so_far == 0 {
                    PageProgress::EMPTY
                } else {
                    PageProgress::new(pages_so_far)
                };
                return V3::continue_with(progress);
            }
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => {
                let progress = if pages_so_far == 0 {
                    PageProgress::EMPTY
                } else {
                    PageProgress::new(pages_so_far)
                };
                return V3::yield_on_carrier(progress, carrier.raw(), interests.raw());
            }
            V3::Yield {
                shape: YieldShape::OnAgent { .. },
                ..
            } => {
                return V3::err(tx_substrate::step_v3::Errno::EIO);
            }
            V3::Err(v3_errno) => return V3::err(v3_errno),
        }
    }

    match mount.payload().fs_page_backing_v3.fsync(*fs_object_id, guard) {
        V3::Done(()) => V3::done(()),
        V3::Continue { progress: _ } => {
            let progress = if pages_so_far == 0 {
                PageProgress::EMPTY
            } else {
                PageProgress::new(pages_so_far)
            };
            V3::continue_with(progress)
        }
        V3::Yield {
            progress: _,
            shape: YieldShape::OnCarrier { carrier, interests },
        } => {
            let progress = if pages_so_far == 0 {
                PageProgress::EMPTY
            } else {
                PageProgress::new(pages_so_far)
            };
            V3::yield_on_carrier(progress, carrier.raw(), interests.raw())
        }
        V3::Yield {
            shape: YieldShape::OnAgent { .. },
            ..
        } => V3::err(tx_substrate::step_v3::Errno::EIO),
        V3::Err(v3_errno) => V3::err(v3_errno),
    }
}

/// `step_truncate` — v3 outcome shape over `PageProgress`.
///
/// Same body and semantics as [`step_truncate`], translated to a v3
/// [`tx_substrate::step_v3::StepOutcome`]:
///
/// - `Device` / new_size > capacity → `Err(EINVAL)`
/// - fs `Done(())` then post-fs work → `Done(())`
/// - fs `Advanced(())` then post-fs work → `Continue { progress:
///   PageProgress::EMPTY }` (rerun, no page count to expose — see note)
/// - fs `Blocked(token)` → `Yield { progress: PageProgress::EMPTY, … }`
/// - fs `AdvancedThenBlocked((), token)` → `Yield { progress:
///   PageProgress::EMPTY, … }` (see note)
/// - fs `Err(e)` → `Err(e)`
///
/// Per-call-site `Advanced(())` / `AdvancedThenBlocked` decision: the v4
/// fs `truncate` returns `T = ()`, so there is no per-step page count to
/// thread through. We pick the simpler probe path of `PageProgress::EMPTY`
/// in both yield/continue cases. If a later wave needs interim page-step
/// accounting for truncate it must extend `FsPageBacking::truncate` to
/// expose a `pages` count or have v3 callers track it externally.
pub fn step_truncate_v3(
    pc: &PageContainer,
    new_size: u64,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::PageProgress> {
    use tx_substrate::step_v3::{PageProgress, StepOutcome as V3, YieldShape};

    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return V3::err(Errno::EINVAL.into());
    }

    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    if new_size > capacity {
        return V3::err(Errno::EINVAL.into());
    }

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .payload()
            .fs_page_backing_v3
            .truncate(*fs_object_id, new_size, guard)
        {
            V3::Done(()) => false,
            V3::Continue { progress: _ } => true,
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => {
                return V3::yield_on_carrier(
                    PageProgress::EMPTY,
                    carrier.raw(),
                    interests.raw(),
                );
            }
            V3::Yield {
                shape: YieldShape::OnAgent { .. },
                ..
            } => return V3::err(tx_substrate::step_v3::Errno::EIO),
            V3::Err(v3_errno) => return V3::err(v3_errno),
        },
        PageContainerKind::Anon { .. } => false,
        PageContainerKind::Device { .. } => unreachable!(),
    };

    let old_size = pc.size_bytes();
    pc.set_size_bytes(new_size);

    if new_size < old_size {
        let Some(first_drop) = first_page_after_size(new_size) else {
            return V3::err(Errno::EINVAL.into());
        };
        pc.withdraw_cached_pages_from(first_drop);
        zero_partial_eof_tail(pc, new_size);
    }

    if fs_advanced {
        V3::continue_with(PageProgress::EMPTY)
    } else {
        V3::done(())
    }
}

#[cfg(test)]
mod v3_tests {
    use super::*;
    use crate::execution::{Errno as V4Errno, StepOutcome as V4Outcome, WaitToken};
    use crate::mount::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
    use crate::page_backed::{
        AnonSwapPolicy, CachedFrame, PageContainer, PageContainerKind, PageIndex,
        allocate_cached_frame,
    };
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta};
    use alloc::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tx_substrate::page_allocator;
    use tx_substrate::step_v3::{
        Errno as V3Errno, InterestConditions, NoProgress, PageProgress, StepOutcome as V3Outcome,
        WakeCarrier, YieldShape,
    };

    fn setup_host_substrate() {
        tx_substrate::testing::init_host_for_test_once();
        crate::zones::register_all().expect("kernel zones");
        match tx_substrate::page_allocator::claim_zero_frame() {
            Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for v3 lifecycle tests: {error:?}"),
        }
    }

    struct LifecycleFs {
        flushes: AtomicUsize,
        fsyncs: AtomicUsize,
        last_object: AtomicU64,
        last_offset: AtomicU64,
        last_truncate_size: AtomicU64,
        block_flush_after: Option<usize>,
        truncate_outcome: V4Outcome<()>,
    }

    impl LifecycleFs {
        fn new() -> Self {
            Self {
                flushes: AtomicUsize::new(0),
                fsyncs: AtomicUsize::new(0),
                last_object: AtomicU64::new(0),
                last_offset: AtomicU64::new(0),
                last_truncate_size: AtomicU64::new(0),
                block_flush_after: None,
                truncate_outcome: V4Outcome::Done(()),
            }
        }

        fn blocking_after(first_done_count: usize) -> Self {
            Self {
                block_flush_after: Some(first_done_count),
                ..Self::new()
            }
        }

        fn failing_truncate(errno: V4Errno) -> Self {
            Self {
                truncate_outcome: V4Outcome::Err(errno),
                ..Self::new()
            }
        }

        fn blocking_truncate(token: WaitToken) -> Self {
            Self {
                truncate_outcome: V4Outcome::Blocked(token),
                ..Self::new()
            }
        }

        fn advancing_truncate() -> Self {
            Self {
                truncate_outcome: V4Outcome::Advanced(()),
                ..Self::new()
            }
        }
    }

    impl FsPageBacking for LifecycleFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> V4Outcome<Frame> {
            V4Outcome::Done(Frame::new(
                page_allocator::zero_frame_ppn().expect("zero frame"),
            ))
        }

        fn flush_page(
            &self,
            fs_object_id: FsObjectId,
            offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            let flush = self.flushes.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_offset.store(offset, Ordering::Release);
            if self.block_flush_after == Some(flush) {
                V4Outcome::Blocked(WaitToken::new(13, 0x55))
            } else {
                V4Outcome::Done(())
            }
        }

        fn truncate(
            &self,
            fs_object_id: FsObjectId,
            new_size: u64,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_truncate_size.store(new_size, Ordering::Release);
            self.truncate_outcome.clone()
        }

        fn fsync(&self, fs_object_id: FsObjectId, _guard: &Guard<'_>) -> V4Outcome<()> {
            self.fsyncs.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            V4Outcome::Done(())
        }

        fn fallocate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            V4Outcome::Done(())
        }
    }

    impl FsOps for LifecycleFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &Guard<'_>,
        ) -> V4Outcome<FsObjectId> {
            V4Outcome::Err(V4Errno::ENOSYS)
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V4Outcome<InodeMeta> {
            V4Outcome::Done(InodeMeta::new(InodeKind::Regular, 0o100644))
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            V4Outcome::Done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V4Outcome<(FsObjectId, InodeMeta)> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V4Outcome<(FsObjectId, InodeMeta)> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V4Outcome<()> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V4Outcome<(FsObjectId, InodeMeta)> {
            V4Outcome::Err(V4Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> V4Outcome<Option<(DirEntry, DirCursor)>> {
            V4Outcome::Done(None)
        }

        fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> V4Outcome<()> {
            V4Outcome::Done(())
        }
    }

    // Wave 9d: v3 trait impls so the inner-mod LifecycleFs satisfies
    // the v3 fields on `MountPayload`. Bodies mirror the v4 impls;
    // anything that goes through `Blocked(_)` upgrades to
    // `Err(EAGAIN)` (the v3 surface has no `NoProgress`-Blocked
    // variant).
    impl crate::vfs::FsOpsV3 for LifecycleFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &Guard<'_>,
        ) -> V3Outcome<FsObjectId, NoProgress> {
            V3Outcome::err(V3Errno::ENOSYS)
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<InodeMeta, NoProgress> {
            V3Outcome::done(InodeMeta::new(InodeKind::Regular, 0o100644))
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
            V3Outcome::err(V3Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> V3Outcome<Option<(DirEntry, DirCursor)>, NoProgress> {
            V3Outcome::done(None)
        }

        fn destroy_inode(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            V3Outcome::done(())
        }
    }

    impl crate::page_backed::FsPageBackingV3 for LifecycleFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> V3Outcome<Frame, NoProgress> {
            V3Outcome::done(Frame::new(
                page_allocator::zero_frame_ppn().expect("zero frame"),
            ))
        }

        fn flush_page(
            &self,
            fs_object_id: FsObjectId,
            offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            let flush = self.flushes.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_offset.store(offset, Ordering::Release);
            if self.block_flush_after == Some(flush) {
                V3Outcome::yield_on_carrier(NoProgress, 13, 0x55)
            } else {
                V3Outcome::done(())
            }
        }

        fn truncate(
            &self,
            fs_object_id: FsObjectId,
            new_size: u64,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_truncate_size.store(new_size, Ordering::Release);
            match self.truncate_outcome.clone() {
                V4Outcome::Done(()) => V3Outcome::done(()),
                V4Outcome::Advanced(()) => V3Outcome::continue_with(NoProgress),
                V4Outcome::AdvancedThenBlocked((), token) => V3Outcome::yield_on_carrier(
                    NoProgress,
                    token.carrier(),
                    token.interest(),
                ),
                V4Outcome::Blocked(token) => V3Outcome::yield_on_carrier(
                    NoProgress,
                    token.carrier(),
                    token.interest(),
                ),
                V4Outcome::Err(errno) => V3Outcome::err(errno.into()),
            }
        }

        fn fsync(
            &self,
            fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> V3Outcome<(), NoProgress> {
            self.fsyncs.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            V3Outcome::done(())
        }
    }

    fn file_page_container(fs: Arc<LifecycleFs>, fs_object_id: FsObjectId) -> PageContainer {
        let mount = MountPayload::new_cap(
            fs.clone(),
            fs,
            None,
            DevId::new(8),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");
        PageContainer::new(
            PageContainerKind::File {
                mount: MountPayloadPin::acquire(&tx_substrate::zone::PayloadCap::from_cap(mount)),
                fs_object_id,
            },
            4,
        )
    }

    fn cached_frame_for_test() -> CachedFrame {
        setup_host_substrate();
        allocate_cached_frame().expect("cached frame")
    }

    // ------- step_fsync_v3 -------------------------------------------------

    #[test]
    fn fsync_v3_anon_returns_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        assert_eq!(step_fsync_v3(&pc, &guard), V3Outcome::done(()));
    }

    #[test]
    fn fsync_v3_no_dirty_pages_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::new());
        let pc = file_page_container(fs.clone(), FsObjectId::new(91));
        assert_eq!(step_fsync_v3(&pc, &guard), V3Outcome::done(()));
        assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
        assert_eq!(fs.flushes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn fsync_v3_flushes_clean_pages_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::new());
        let pc = file_page_container(fs.clone(), FsObjectId::new(92));
        for page in [0u64, 1, 2] {
            let mut state = pc.state.lock();
            state
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
            state
                .pages
                .mark_dirty(PageIndex::new(page))
                .expect("mark dirty");
        }
        assert_eq!(step_fsync_v3(&pc, &guard), V3Outcome::done(()));
        assert_eq!(fs.flushes.load(Ordering::Acquire), 3);
        assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
    }

    #[test]
    fn fsync_v3_blocked_first_page_yields_with_empty_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::blocking_after(0));
        let pc = file_page_container(fs.clone(), FsObjectId::new(93));
        for page in [0u64, 1] {
            let mut state = pc.state.lock();
            state
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
            state
                .pages
                .mark_dirty(PageIndex::new(page))
                .expect("mark dirty");
        }
        assert_eq!(
            step_fsync_v3(&pc, &guard),
            V3Outcome::Yield {
                progress: PageProgress::EMPTY,
                shape: YieldShape::OnCarrier {
                    carrier: WakeCarrier::new(13),
                    interests: InterestConditions::new(0x55),
                },
            }
        );
        // No clear-dirty happened on the blocked page.
        assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    }

    #[test]
    fn fsync_v3_blocked_after_progress_yields_with_pages_so_far() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::blocking_after(1));
        let pc = file_page_container(fs.clone(), FsObjectId::new(94));
        for page in [0u64, 1] {
            let mut state = pc.state.lock();
            state
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
            state
                .pages
                .mark_dirty(PageIndex::new(page))
                .expect("mark dirty");
        }
        assert_eq!(
            step_fsync_v3(&pc, &guard),
            V3Outcome::Yield {
                progress: PageProgress::new(1),
                shape: YieldShape::OnCarrier {
                    carrier: WakeCarrier::new(13),
                    interests: InterestConditions::new(0x55),
                },
            }
        );
        // The first flush did clear-dirty; the second blocked one didn't.
        assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
        assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
        assert_eq!(fs.fsyncs.load(Ordering::Acquire), 0);
    }

    // ------- step_truncate_v3 ---------------------------------------------

    #[test]
    fn truncate_v3_anon_shrink_done_and_withdraws_pages() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            4,
        );
        for page in 0..4 {
            pc.state
                .lock()
                .pages
                .install_if_absent(PageIndex::new(page), cached_frame_for_test())
                .expect("seed page");
        }
        assert_eq!(
            step_truncate_v3(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::done(())
        );
        assert!(pc.lookup(PageIndex::new(0)).is_some());
        assert_eq!(pc.lookup(PageIndex::new(1)), None);
    }

    #[test]
    fn truncate_v3_device_returns_einval() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let device = PageContainer::new(
            PageContainerKind::Device {
                base_ppn: Ppn(0xface_3000),
                page_count: 1,
            },
            1,
        );
        assert_eq!(
            step_truncate_v3(&device, 0, &guard),
            V3Outcome::err(V3Errno::EINVAL)
        );
    }

    #[test]
    fn truncate_v3_grow_past_capacity_returns_einval() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        assert_eq!(
            step_truncate_v3(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::err(V3Errno::EINVAL)
        );
    }

    #[test]
    fn truncate_v3_fs_err_propagates_unchanged_state() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::failing_truncate(V4Errno::EROFS));
        let pc = file_page_container(fs.clone(), FsObjectId::new(95));
        let original_size = pc.size_bytes();
        assert_eq!(
            step_truncate_v3(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::err(V3Errno::EROFS)
        );
        assert_eq!(pc.size_bytes(), original_size);
    }

    #[test]
    fn truncate_v3_fs_blocked_yields_empty_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::blocking_truncate(WaitToken::new(7, 0x11)));
        let pc = file_page_container(fs.clone(), FsObjectId::new(96));
        let original_size = pc.size_bytes();
        assert_eq!(
            step_truncate_v3(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
            V3Outcome::Yield {
                progress: PageProgress::EMPTY,
                shape: YieldShape::OnCarrier {
                    carrier: WakeCarrier::new(7),
                    interests: InterestConditions::new(0x11),
                },
            }
        );
        // Size stays unpublished on a yield (post-fs work didn't run).
        assert_eq!(pc.size_bytes(), original_size);
    }

    #[test]
    fn truncate_v3_fs_advanced_returns_continue_with_empty_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::advancing_truncate());
        let pc = file_page_container(fs.clone(), FsObjectId::new(97));
        // Pick a shrink so the post-fs work runs (withdraw + zero-tail).
        pc.set_size_bytes(2 * crate::vm::USER_PAGE_SIZE as u64);
        let new_size = crate::vm::USER_PAGE_SIZE as u64;
        assert_eq!(
            step_truncate_v3(&pc, new_size, &guard),
            V3Outcome::continue_with(PageProgress::EMPTY)
        );
        // Post-fs work *did* run, so size is published.
        assert_eq!(pc.size_bytes(), new_size);
    }
}
