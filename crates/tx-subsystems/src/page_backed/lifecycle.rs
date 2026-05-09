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

///   "mostly a hint".

fn first_page_after_size(size: u64) -> Option<PageIndex> {
    let page_size = crate::vm::USER_PAGE_SIZE as u64;
    if size == 0 {
        return Some(PageIndex::new(0));
    }
    size.checked_add(page_size - 1)
        .map(|rounded| PageIndex::new(rounded / page_size))
}

pub fn step_fsync(
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
        match mount.payload().fs_page_backing.flush_page(
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

    match mount.payload().fs_page_backing.fsync(*fs_object_id, guard) {
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
/// `FsPageBacking::truncate` returns `T = ()`, so there is no per-step
/// page count to thread through; both yield/continue cases use
/// `PageProgress::EMPTY`. If interim page-step accounting for truncate
/// is ever needed, extend `FsPageBacking::truncate` to expose a
/// `pages` count or track it externally at call sites.
pub fn step_truncate(
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
            .fs_page_backing
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

pub fn step_fallocate(
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

    if new_size <= pc.size_bytes() {
        return V3::done(());
    }

    let fs_advanced = match pc.kind() {
        PageContainerKind::File {
            mount,
            fs_object_id,
        } => match mount
            .payload()
            .fs_page_backing
            .fallocate(*fs_object_id, new_size, guard)
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

    pc.set_size_bytes(new_size);

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
    use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, InodeKind, InodeMeta};
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



    // Trait impls so the inner-mod LifecycleFs satisfies the
    // `FsOps` / `FsPageBacking` fields on `MountPayload`. Anything
    // that would go through `Blocked(_)` upgrades to `Err(EAGAIN)`
    // (this surface has no `NoProgress`-Blocked variant).
    impl crate::vfs::FsOps for LifecycleFs {
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

    impl crate::page_backed::FsPageBacking for LifecycleFs {
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

    // ------- step_fsync -------------------------------------------------

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
        assert_eq!(step_fsync(&pc, &guard), V3Outcome::done(()));
    }

    #[test]
    fn fsync_v3_no_dirty_pages_done() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("v3 lifecycle test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(LifecycleFs::new());
        let pc = file_page_container(fs.clone(), FsObjectId::new(91));
        assert_eq!(step_fsync(&pc, &guard), V3Outcome::done(()));
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
        assert_eq!(step_fsync(&pc, &guard), V3Outcome::done(()));
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
            step_fsync(&pc, &guard),
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
            step_fsync(&pc, &guard),
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

    // ------- step_truncate ---------------------------------------------

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
            step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
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
            step_truncate(&device, 0, &guard),
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
            step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
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
            step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
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
            step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
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
            step_truncate(&pc, new_size, &guard),
            V3Outcome::continue_with(PageProgress::EMPTY)
        );
        // Post-fs work *did* run, so size is published.
        assert_eq!(pc.size_bytes(), new_size);
    }
}
