use super::*;
use crate::execution::{Errno, StepOutcome, WaitToken};
use crate::mount::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta};
use alloc::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    crate::zones::register_all().expect("kernel zones");
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked lifecycle tests: {error:?}"),
    }
}

struct LifecycleFs {
    flushes: AtomicUsize,
    fsyncs: AtomicUsize,
    truncates: AtomicUsize,
    fallocates: AtomicUsize,
    last_object: AtomicU64,
    last_offset: AtomicU64,
    last_truncate_size: AtomicU64,
    last_fallocate_size: AtomicU64,
    block_flush_after: Option<usize>,
    truncate_outcome: StepOutcome<()>,
    fallocate_outcome: StepOutcome<()>,
}

impl LifecycleFs {
    fn new() -> Self {
        Self {
            flushes: AtomicUsize::new(0),
            fsyncs: AtomicUsize::new(0),
            truncates: AtomicUsize::new(0),
            fallocates: AtomicUsize::new(0),
            last_object: AtomicU64::new(0),
            last_offset: AtomicU64::new(0),
            last_truncate_size: AtomicU64::new(0),
            last_fallocate_size: AtomicU64::new(0),
            block_flush_after: None,
            truncate_outcome: StepOutcome::Done(()),
            fallocate_outcome: StepOutcome::Done(()),
        }
    }

    fn failing_fallocate(errno: Errno) -> Self {
        Self {
            fallocate_outcome: StepOutcome::Err(errno),
            ..Self::new()
        }
    }

    fn blocking_after(first_done_count: usize) -> Self {
        Self {
            block_flush_after: Some(first_done_count),
            ..Self::new()
        }
    }

    fn failing_truncate(errno: Errno) -> Self {
        Self {
            truncate_outcome: StepOutcome::Err(errno),
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
        self.truncate_outcome.clone()
    }

    fn fsync(&self, fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        self.fsyncs.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        StepOutcome::Done(())
    }

    fn fallocate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        self.fallocates.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_fallocate_size.store(new_size, Ordering::Release);
        self.fallocate_outcome.clone()
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

    assert_eq!(pc.size_bytes(), crate::vm::USER_PAGE_SIZE as u64 + 1);
    assert!(pc.lookup(PageIndex::new(0)).is_some());
    assert!(pc.lookup(PageIndex::new(1)).is_some());
    assert_eq!(pc.lookup(PageIndex::new(2)), None);
    assert_eq!(pc.lookup(PageIndex::new(3)), None);
}

#[test]
fn pagebacked_step_truncate_can_grow_visible_size_without_materializing_pages() {
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
    assert_eq!(step_truncate(&pc, 8, &guard), StepOutcome::Done(()));

    assert_eq!(
        step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64 + 11, &guard),
        StepOutcome::Done(())
    );

    assert_eq!(pc.size_bytes(), 2 * crate::vm::USER_PAGE_SIZE as u64 + 11);
    assert_eq!(pc.resident_pages(), 0);
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
fn pagebacked_step_truncate_leaves_state_unchanged_when_file_backing_fails() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::failing_truncate(Errno::EROFS));
    let pc = file_page_container(fs.clone(), FsObjectId::new(45));
    pc.state
        .lock()
        .pages
        .install_if_absent(PageIndex::new(3), cached_frame_for_test())
        .expect("seed page");
    let original_size = pc.size_bytes();

    assert_eq!(
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Err(Errno::EROFS)
    );

    assert_eq!(pc.size_bytes(), original_size);
    assert!(pc.lookup(PageIndex::new(3)).is_some());
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
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

#[test]
fn pagebacked_step_truncate_zeros_partial_eof_tail_in_cached_page() {
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
    let ppn_page1 = pc.lookup(PageIndex::new(1)).expect("page 1 cached");

    let pattern: alloc::vec::Vec<u8> = (0..crate::vm::USER_PAGE_SIZE)
        .map(|i| ((i & 0xff) | 0x40) as u8)
        .collect();
    tx_substrate::page_allocator::testing::write_frame_bytes_for_test(ppn_page1, 0, &pattern);

    let new_size = crate::vm::USER_PAGE_SIZE as u64 + 4;
    assert_eq!(step_truncate(&pc, new_size, &guard), StepOutcome::Done(()));

    let mut head = [0u8; 4];
    tx_substrate::page_allocator::testing::read_frame_bytes_for_test(ppn_page1, 0, &mut head);
    assert_eq!(&head, &pattern[..4]);

    let mut tail = alloc::vec![0xCCu8; crate::vm::USER_PAGE_SIZE - 4];
    tx_substrate::page_allocator::testing::read_frame_bytes_for_test(ppn_page1, 4, &mut tail);
    assert!(
        tail.iter().all(|b| *b == 0),
        "tail bytes must be zeroed after truncate-shrink past mid-page"
    );
}

#[test]
fn pagebacked_step_truncate_does_not_touch_surviving_pages_at_page_aligned_shrink() {
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
    let ppn_page0 = pc.lookup(PageIndex::new(0)).expect("page 0 cached");
    let pattern = alloc::vec![0xAFu8; crate::vm::USER_PAGE_SIZE];
    tx_substrate::page_allocator::testing::write_frame_bytes_for_test(ppn_page0, 0, &pattern);

    assert_eq!(
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Done(())
    );

    let mut readback = alloc::vec![0u8; crate::vm::USER_PAGE_SIZE];
    tx_substrate::page_allocator::testing::read_frame_bytes_for_test(ppn_page0, 0, &mut readback);
    assert_eq!(readback, pattern);
}

#[test]
fn pagebacked_step_fallocate_grows_anon_visible_size_without_materializing_pages() {
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
    assert_eq!(step_truncate(&pc, 8, &guard), StepOutcome::Done(()));
    assert_eq!(pc.size_bytes(), 8);

    assert_eq!(
        step_fallocate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64 + 17, &guard),
        StepOutcome::Done(())
    );

    assert_eq!(pc.size_bytes(), 2 * crate::vm::USER_PAGE_SIZE as u64 + 17);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_fallocate_calls_file_backing_before_publishing_size() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(91));
    assert_eq!(step_truncate(&pc, 16, &guard), StepOutcome::Done(()));
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);

    let new_size = 3 * crate::vm::USER_PAGE_SIZE as u64;
    assert_eq!(step_fallocate(&pc, new_size, &guard), StepOutcome::Done(()));

    assert_eq!(fs.fallocates.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_fallocate_size.load(Ordering::Acquire), new_size);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 91);
    assert_eq!(pc.size_bytes(), new_size);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_fallocate_leaves_state_unchanged_when_file_backing_fails() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::failing_fallocate(Errno::EDQUOT));
    let pc = file_page_container(fs.clone(), FsObjectId::new(92));
    assert_eq!(step_truncate(&pc, 16, &guard), StepOutcome::Done(()));
    let baseline_size = pc.size_bytes();

    assert_eq!(
        step_fallocate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Err(Errno::EDQUOT)
    );

    assert_eq!(fs.fallocates.load(Ordering::Acquire), 1);
    assert_eq!(pc.size_bytes(), baseline_size);
}

#[test]
fn pagebacked_step_fallocate_rejects_device_and_capacity_growth() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
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
        step_fallocate(&device, 16, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );

    let anon = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let beyond = 3 * crate::vm::USER_PAGE_SIZE as u64;
    assert_eq!(
        step_fallocate(&anon, beyond, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
    assert_eq!(anon.size_bytes(), 2 * crate::vm::USER_PAGE_SIZE as u64);
}

#[test]
fn pagebacked_step_fallocate_is_noop_when_target_size_does_not_grow() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(93));
    assert_eq!(
        step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Done(())
    );
    let stable_size = pc.size_bytes();

    assert_eq!(
        step_fallocate(&pc, stable_size, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_fallocate(&pc, stable_size - 1, &guard),
        StepOutcome::Done(())
    );

    assert_eq!(fs.fallocates.load(Ordering::Acquire), 0);
    assert_eq!(pc.size_bytes(), stable_size);
}

// === FsOpsV3 prototype impl + tests ====================================
//
// Wave-8 design + prototype slice for the trait migration. Per
// `docs/progress/decisions/2026-05-09-fsops-v3-design.md`, `LifecycleFs`
// is the smallest test-only `FsOps` impl in the workspace — it lives
// next to wave-7's `step_truncate_v3`/`step_fsync_v3` work, so the
// `FsOpsV3` impl gets validated against the same fixture that already
// exercises the v3 sibling fns. Wave 9 fans out to the remaining
// seven impls (`Tmpfs`, `Devfs`, `Ext4FsInstance`, `DevptsInstance`,
// `TestFs`, `ExecTestFs`, `ExecveTestFs`).
//
// Every method picks `NoProgress` per the design doc's per-method
// progress-type table: the trait surface is one-shot identity-side
// queries / mutations, page accounting belongs to `FsPageBacking{,V3}`.

use crate::vfs::FsOpsV3;
use tx_substrate::step_v3::{
    Errno as V3Errno, NoProgress, StepOutcome as V3Outcome,
};

impl FsOpsV3 for LifecycleFs {
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

// Tests pin the v3 outcome shape end-to-end through the `LifecycleFs`
// impl. They are red until both the `FsOpsV3` trait declaration in
// `crates/tx-subsystems/src/vfs/execution.rs` and the `impl FsOpsV3
// for LifecycleFs` block above are present.

#[test]
fn fsopsv3_load_inode_meta_returns_done_with_default_meta() {
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOpsV3>::load_inode_meta(&fs, FsObjectId::new(7), &guard);
    let expected = InodeMeta::new(InodeKind::Regular, 0o100644);
    assert_eq!(outcome, V3Outcome::done(expected));
}

#[test]
fn fsopsv3_create_inode_returns_err_erofs_on_readonly_fixture() {
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOpsV3>::create_inode(
        &fs,
        FsObjectId::new(1),
        b"foo",
        0o100644,
        &Credential::root(),
        &guard,
    );
    assert_eq!(
        outcome,
        V3Outcome::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EROFS)
    );
}

#[test]
fn fsopsv3_readdir_done_none_for_empty_directory() {
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOpsV3>::readdir(
        &fs,
        FsObjectId::new(1),
        DirCursor::START,
        &guard,
    );
    assert_eq!(outcome, V3Outcome::done(None));
}

#[test]
fn fsopsv3_lookup_returns_err_enosys() {
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = LifecycleFs::new();
    let outcome =
        <LifecycleFs as FsOpsV3>::lookup(&fs, FsObjectId::new(1), b"missing", &guard);
    assert_eq!(
        outcome,
        V3Outcome::<FsObjectId, NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn fsopsv3_default_read_link_returns_enosys() {
    // Wave-8 design choice: defaults match `FsOps` exactly. `LifecycleFs`
    // does not override `read_link`, so the default `ENOSYS` answer
    // must round-trip through the v3 outcome shape.
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOpsV3>::read_link(&fs, FsObjectId::new(1), &guard);
    assert_eq!(
        outcome,
        V3Outcome::<alloc::boxed::Box<[u8]>, NoProgress>::err(V3Errno::ENOSYS)
    );
}
