use super::*;
use crate::execution::{Errno, StepOutcome, WaitToken};
use crate::mount::{DevId, MountOptions, MountPayload, SourceLabel};
use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta};
use alloc::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked lifecycle tests: {error:?}"),
    }
}

struct LifecycleFs {
    flushes: AtomicUsize,
    fsyncs: AtomicUsize,
    truncates: AtomicUsize,
    last_object: AtomicU64,
    last_offset: AtomicU64,
    last_truncate_size: AtomicU64,
    block_flush_after: Option<usize>,
}

impl LifecycleFs {
    fn new() -> Self {
        Self {
            flushes: AtomicUsize::new(0),
            fsyncs: AtomicUsize::new(0),
            truncates: AtomicUsize::new(0),
            last_object: AtomicU64::new(0),
            last_offset: AtomicU64::new(0),
            last_truncate_size: AtomicU64::new(0),
            block_flush_after: None,
        }
    }

    fn blocking_after(first_done_count: usize) -> Self {
        Self {
            block_flush_after: Some(first_done_count),
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
    ) -> StepOutcome<Frame> {
        StepOutcome::Done(Frame::new(
            page_allocator::zero_frame_ppn().expect("zero frame"),
        ))
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        let flush = self.flushes.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_offset.store(offset, Ordering::Release);
        if self.block_flush_after == Some(flush) {
            StepOutcome::Blocked(WaitToken::new(13, 0x55))
        } else {
            StepOutcome::Done(())
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        self.truncates.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_truncate_size.store(new_size, Ordering::Release);
        StepOutcome::Done(())
    }

    fn fsync(&self, fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        self.fsyncs.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        StepOutcome::Done(())
    }
}

impl FsOps for LifecycleFs {
    fn lookup(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn load_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta> {
        StepOutcome::Done(InodeMeta::new(InodeKind::Regular, 0o100644))
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Done(None)
    }

    fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Done(())
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
            mount,
            fs_object_id,
        },
        4,
    )
}

fn cached_frame_for_test() -> CachedFrame {
    setup_host_substrate();
    allocate_cached_frame().expect("cached frame")
}

#[test]
fn pagebacked_step_truncate_withdraws_pages_at_or_beyond_new_size() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
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
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64 + 1, &guard),
        StepOutcome::Done(())
    );

    assert!(pc.lookup(PageIndex::new(0)).is_some());
    assert!(pc.lookup(PageIndex::new(1)).is_some());
    assert_eq!(pc.lookup(PageIndex::new(2)), None);
    assert_eq!(pc.lookup(PageIndex::new(3)), None);
}

#[test]
fn pagebacked_step_truncate_asks_file_backing_before_withdrawal() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(44));
    pc.state
        .lock()
        .pages
        .install_if_absent(PageIndex::new(3), cached_frame_for_test())
        .expect("seed page");

    assert_eq!(
        step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Done(())
    );

    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 44);
    assert_eq!(
        fs.last_truncate_size.load(Ordering::Acquire),
        2 * crate::vm::USER_PAGE_SIZE as u64
    );
    assert_eq!(pc.lookup(PageIndex::new(3)), None);
}

#[test]
fn pagebacked_step_truncate_rejects_device_and_capacity_growth() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_1000),
            page_count: 1,
        },
        1,
    );
    let anon = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );

    assert_eq!(
        step_truncate(&device, 0, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
    assert_eq!(
        step_truncate(&anon, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
}

#[test]
fn pagebacked_step_fsync_flushes_dirty_file_pages_in_order_and_cleans_marks() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(51));
    for page in [2, 0] {
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

    assert_eq!(step_fsync(&pc, &guard), StepOutcome::Done(()));

    assert_eq!(fs.flushes.load(Ordering::Acquire), 2);
    assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 51);
    assert_eq!(
        fs.last_offset.load(Ordering::Acquire),
        2 * crate::vm::USER_PAGE_SIZE as u64
    );
    assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(!pc.page_marks(PageIndex::new(2)).expect("page 2").dirty);
}

#[test]
fn pagebacked_step_fsync_returns_advanced_then_blocked_after_flush_progress() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::blocking_after(1));
    let pc = file_page_container(fs.clone(), FsObjectId::new(52));
    for page in 0..2 {
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
        StepOutcome::AdvancedThenBlocked((), WaitToken::new(13, 0x55))
    );

    assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
    assert_eq!(fs.fsyncs.load(Ordering::Acquire), 0);
}

#[test]
fn pagebacked_step_fsync_is_noop_for_anon_and_device() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let anon = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_2000),
            page_count: 1,
        },
        1,
    );

    assert_eq!(step_fsync(&anon, &guard), StepOutcome::Done(()));
    assert_eq!(step_fsync(&device, &guard), StepOutcome::Done(()));
}
