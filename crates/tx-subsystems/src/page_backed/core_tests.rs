//! Inline tests for the core page_backed module — extracted to a
//! sibling file to keep page_backed.rs under the 1500-line
//! authored-Rust cap. RecordingFs/BlockingFs fixtures live here,
//! including their `FsOps` + `FsPageBacking` impls needed because
//! `MountPayload` carries `fs_ops` / `fs_page_backing` fields.

use super::*;
use crate::execution::Errno;
use crate::mount::{DevId, MountOptions, MountPayload, SourceLabel};
use crate::page_backed::adapter::step_engine::{
    self as step_engine, Errno as V3Errno, NoProgress, StepOutcome as V3Out,
};
use crate::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
    OpenFileFlags, RNode, RNodeBacking,
};
use alloc::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

fn setup_host_substrate() {
    tx_test_support::init_host();
    crate::zones::register_all().expect("kernel zones");
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked tests: {error:?}"),
    }
}

fn cached_frame_for_test() -> CachedFrame {
    setup_host_substrate();
    allocate_cached_frame().expect("cached frame")
}

struct RecordingFs {
    fetches: AtomicUsize,
    last_object: AtomicU64,
    last_offset: AtomicU64,
}

impl RecordingFs {
    fn new() -> Self {
        Self {
            fetches: AtomicUsize::new(0),
            last_object: AtomicU64::new(0),
            last_offset: AtomicU64::new(0),
        }
    }
}

// Trait impls so `RecordingFs` satisfies the `FsOps` /
// `FsPageBacking` fields on `MountPayload`.
impl crate::vfs::FsOps for RecordingFs {
    fn lookup(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Out<FsObjectId, NoProgress> {
        V3Out::err(V3Errno::ENOSYS)
    }

    fn load_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<InodeMeta, NoProgress> {
        V3Out::done(InodeMeta::new(InodeKind::Regular, 0o100644))
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> V3Out<Option<(DirEntry, DirCursor)>, NoProgress> {
        V3Out::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }
}

impl FsPageBacking for RecordingFs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &Guard<'_>,
    ) -> V3Out<Frame, NoProgress> {
        self.fetches.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_offset.store(offset, Ordering::Release);
        V3Out::done(Frame::new(
            page_allocator::zero_frame_ppn().expect("zero frame"),
        ))
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn fsync_file(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }
}

struct ReentrantFs {
    fetches: AtomicUsize,
    reentered: AtomicBool,
    outcome: ReentrantFetchOutcome,
    inner_done: AtomicUsize,
    inner_wait_source: AtomicU64,
    inner_wait_interests: AtomicU64,
    pc: std::sync::Mutex<Option<Weak<PageContainer>>>,
}

#[derive(Clone, Copy)]
enum ReentrantFetchOutcome {
    Done,
    Yield,
}

impl ReentrantFs {
    fn new() -> Self {
        Self::with_outcome(ReentrantFetchOutcome::Done)
    }

    fn blocking() -> Self {
        Self::with_outcome(ReentrantFetchOutcome::Yield)
    }

    fn with_outcome(outcome: ReentrantFetchOutcome) -> Self {
        Self {
            fetches: AtomicUsize::new(0),
            reentered: AtomicBool::new(false),
            outcome,
            inner_done: AtomicUsize::new(0),
            inner_wait_source: AtomicU64::new(0),
            inner_wait_interests: AtomicU64::new(0),
            pc: std::sync::Mutex::new(None),
        }
    }

    fn set_page_container(&self, pc: &Arc<PageContainer>) {
        *self.pc.lock().expect("reentrant fs pc lock") = Some(Arc::downgrade(pc));
    }
}

impl crate::vfs::FsOps for ReentrantFs {
    fn lookup(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Out<FsObjectId, NoProgress> {
        V3Out::err(V3Errno::ENOSYS)
    }

    fn load_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<InodeMeta, NoProgress> {
        V3Out::done(InodeMeta::new(InodeKind::Regular, 0o100644))
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> V3Out<Option<(DirEntry, DirCursor)>, NoProgress> {
        V3Out::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }
}

impl FsPageBacking for ReentrantFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        guard: &Guard<'_>,
    ) -> V3Out<Frame, NoProgress> {
        self.fetches.fetch_add(1, Ordering::AcqRel);
        if !self.reentered.swap(true, Ordering::AcqRel) {
            let pc = self
                .pc
                .lock()
                .expect("reentrant fs pc lock")
                .as_ref()
                .and_then(Weak::upgrade)
                .expect("reentrant page container");
            match pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, guard) {
                V3Out::Yield { shape, .. } => {
                    let Some((source, interests)) =
                        crate::page_backed::notification::wait_source_parts(&shape)
                    else {
                        panic!("expected reentrant file miss to yield on wait source");
                    };
                    self.inner_wait_source.store(source, Ordering::Release);
                    self.inner_wait_interests
                        .store(interests, Ordering::Release);
                }
                V3Out::Done(_) => {
                    self.inner_done.store(1, Ordering::Release);
                }
                other => panic!("unexpected reentrant materialize outcome: {other:?}"),
            }
        }
        match self.outcome {
            ReentrantFetchOutcome::Done => V3Out::done(Frame::new(
                page_allocator::zero_frame_ppn().expect("zero frame"),
            )),
            ReentrantFetchOutcome::Yield => V3Out::yield_on_wait_source(NoProgress, 9, 0x44),
        }
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn fsync_file(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }
}

struct BlockingFs;

// Trait impls so `BlockingFs` satisfies the `FsOps` /
// `FsPageBacking` fields on `MountPayload`. The interesting case
// is `fetch_page`: a `Blocked(token)` shape has no equivalent in
// `NoProgress` outcomes, so the closest analog `Err(EAGAIN)` is
// surfaced here — that gives walker-driven tests a non-Done
// deterministic result. The page-backed unit tests below all
// drive `BlockingFs` through `materialize_page` (which reads its
// own `fs_page_backing` route), so this trait body is
// dispatch-stub-only.
impl crate::vfs::FsOps for BlockingFs {
    fn lookup(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Out<FsObjectId, NoProgress> {
        V3Out::err(V3Errno::ENOSYS)
    }

    fn load_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<InodeMeta, NoProgress> {
        V3Out::err(V3Errno::ENOSYS)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Out<(FsObjectId, InodeMeta), NoProgress> {
        V3Out::err(V3Errno::EROFS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> V3Out<Option<(DirEntry, DirCursor)>, NoProgress> {
        V3Out::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }
}

impl FsPageBacking for BlockingFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> V3Out<Frame, NoProgress> {
        // Yield on `WaitToken(9, 0x44)` so production fns routing
        // through this trait (e.g. `materialize_file_page`)
        // observe a yield rather than `Err(EAGAIN)`.
        V3Out::yield_on_wait_source(NoProgress, 9, 0x44)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }

    fn fsync_file(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> V3Out<(), NoProgress> {
        V3Out::done(())
    }
}

fn file_page_container(
    fs_v3: Arc<dyn FsOps>,
    page_backing_v3: Arc<dyn FsPageBacking>,
    fs_object_id: FsObjectId,
    page_count: u64,
) -> PageContainer {
    let mount = MountPayload::new_cap(
        fs_v3,
        page_backing_v3,
        None,
        DevId::new(8),
        MountOptions::default(),
        "mockfs",
        SourceLabel::Static("mock"),
    )
    .expect("mount payload");
    PageContainer::new(
        PageContainerKind::File {
            mount: MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(mount)),
            fs_object_id,
        },
        page_count,
    )
}

fn open_file_for_pc(pc: &PageContainer) -> OpenFile {
    let pc = PageContainer::new_cap(pc.kind().clone(), pc.page_count())
        .expect("page container cap for open file");
    let rnode = RNode::new_cap(
        FsObjectId::new(700),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("rnode cap");
    OpenFile::new(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    )
}

#[test]
fn page_cache_index_install_if_absent_linearizes_sparse_offsets() {
    let mut index = PageCacheIndex::new();
    let page = PageIndex::new(7);
    let first = cached_frame_for_test();
    let first_ppn = first.ppn;
    let second = cached_frame_for_test();

    assert_eq!(index.lookup(page), None);
    assert_eq!(index.install_if_absent(page, first), Ok(()));
    assert_eq!(
        index.install_if_absent(page, second),
        Err(PageCacheError::AlreadyPresent { current: first_ppn })
    );
    assert_eq!(index.lookup(page), Some(first_ppn));
    assert_eq!(index.len(), 1);
}

#[test]
fn page_cache_index_install_if_match_replaces_or_withdraws_exact_frame() {
    let mut index = PageCacheIndex::new();
    let page = PageIndex::new(3);
    let first = cached_frame_for_test();
    let first_ppn = first.ppn;
    let wrong = cached_frame_for_test().ppn;
    let replacement = cached_frame_for_test();

    index
        .install_if_absent(page, first)
        .expect("initial insert");
    assert_eq!(
        index.install_if_match(page, wrong, Some(replacement)),
        Err(PageCacheError::MismatchedFrame { current: first_ppn })
    );
    let replacement = cached_frame_for_test();
    let replacement_ppn = replacement.ppn;
    assert_eq!(
        index.install_if_match(page, first_ppn, Some(replacement)),
        Ok(Some(first_ppn))
    );
    assert_eq!(index.lookup(page), Some(replacement_ppn));
    assert_eq!(
        index.install_if_match(page, replacement_ppn, None),
        Ok(Some(replacement_ppn))
    );
    assert_eq!(index.lookup(page), None);
}

#[test]
fn anon_page_container_materializes_once_and_tracks_dirty_writes() {
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    let page = PageIndex::new(2);

    let first = pc
        .materialize_anon(page, MaterializeAccess::Read)
        .expect("read materializes anon page");
    let second = pc
        .materialize_anon(page, MaterializeAccess::Write)
        .expect("write reuses anon page");

    assert!(first.newly_installed);
    assert!(!first.dirty);
    assert_eq!(second.ppn, first.ppn);
    assert!(!second.newly_installed);
    assert!(second.dirty);
    assert_eq!(pc.lookup(page), Some(first.ppn));
    assert_eq!(pc.resident_pages(), 1);
}

#[test]
fn anon_page_materialization_keeps_allocator_work_outside_state_lock() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    reset_page_container_lock_service_observations_for_test();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    let page = PageIndex::new(1);

    let first = pc
        .materialize_anon(page, MaterializeAccess::Read)
        .expect("cold anon materialization");
    let cold_observations = page_container_lock_service_observations_for_test();

    reset_page_container_lock_service_observations_for_test();
    let second = pc
        .materialize_anon(page, MaterializeAccess::Write)
        .expect("hot anon rematerialization");
    let hot_observations = page_container_lock_service_observations_for_test();

    assert!(first.newly_installed);
    assert_eq!(second.ppn, first.ppn);
    assert!(!second.newly_installed);
    assert_eq!(
        cold_observations,
        (0, 0),
        "cold materialization must not allocate frames or acquire map pins under PageContainer.state"
    );
    assert_eq!(
        hot_observations,
        (0, 0),
        "hot rematerialization must not acquire map pins under PageContainer.state"
    );
}

#[test]
fn page_container_cap_materializes_anon_pages() {
    setup_host_substrate();
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    )
    .expect("page container cap");
    assert_eq!(pc.page_count(), 4);

    let first = pc
        .materialize_anon(PageIndex::new(1), MaterializeAccess::Read)
        .expect("cap-backed materialization");
    let second = pc
        .materialize_anon(PageIndex::new(1), MaterializeAccess::Write)
        .expect("cap-backed rematerialization");

    assert_eq!(first.ppn, second.ppn);
    assert!(first.newly_installed);
    assert!(!second.newly_installed);
    assert!(second.dirty);
    assert_eq!(pc.resident_pages(), 1);
}

#[test]
fn page_container_materialize_page_dispatches_anon() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );

    let page = match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Write, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected materialize outcome: {other:?}"),
    };

    assert!(page.newly_installed);
    assert!(page.dirty);
    assert_eq!(pc.lookup(PageIndex::new(1)), Some(page.ppn));
}

#[test]
fn page_container_materialize_page_dispatches_file_fetch_once() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs.clone(), FsObjectId::new(55), 4);

    let first = match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected materialize outcome: {other:?}"),
    };
    let second = match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Write, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected rematerialize outcome: {other:?}"),
    };

    assert!(first.newly_installed);
    assert!(!first.dirty);
    assert_eq!(second.ppn, first.ppn);
    assert!(!second.newly_installed);
    assert!(second.dirty);
    assert_eq!(fs.fetches.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 55);
    assert_eq!(
        fs.last_offset.load(Ordering::Acquire),
        2 * crate::vm::USER_PAGE_SIZE as u64
    );
    assert_eq!(pc.resident_pages(), 1);
}

#[test]
fn file_page_miss_joins_reentrant_inflight_without_duplicate_fetch() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(ReentrantFs::new());
    let pc = Arc::new(file_page_container(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(57),
        4,
    ));
    fs.set_page_container(&pc);

    let first = match pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected materialize outcome: {other:?}"),
    };

    assert!(first.newly_installed);
    assert_eq!(pc.resident_pages(), 1);
    assert_eq!(
        fs.inner_done.load(Ordering::Acquire),
        0,
        "reentrant same-page miss must join the in-flight fetch rather than materialize recursively"
    );
    assert_ne!(
        fs.inner_wait_source.load(Ordering::Acquire),
        0,
        "reentrant same-page miss should receive a PageBacked-owned retry source"
    );
    assert_eq!(fs.inner_wait_interests.load(Ordering::Acquire), 0x1);
    assert_eq!(
        fs.fetches.load(Ordering::Acquire),
        1,
        "only the first caller should issue the backend fetch for a concurrently missing page"
    );
}

#[test]
fn file_page_miss_join_source_survives_blocked_owner_fetch() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(ReentrantFs::blocking());
    let pc = Arc::new(file_page_container(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(58),
        4,
    ));
    fs.set_page_container(&pc);

    match pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
        V3Out::Yield { shape, .. } => {
            let Some((source, interests)) =
                crate::page_backed::notification::wait_source_parts(&shape)
            else {
                panic!("expected backend file fetch wait source");
            };
            assert_eq!(source, 9);
            assert_eq!(interests, 0x44);
        }
        other => panic!("expected blocked owner fetch, got {other:?}"),
    }

    let inner_source = fs.inner_wait_source.load(Ordering::Acquire);
    assert_ne!(
        inner_source, 0,
        "reentrant joiner should receive a PageBacked-owned retry source"
    );
    assert_eq!(fs.inner_wait_interests.load(Ordering::Acquire), 0x1);
    assert_eq!(fs.inner_done.load(Ordering::Acquire), 0);
    assert_eq!(fs.fetches.load(Ordering::Acquire), 1);
    assert_eq!(pc.resident_pages(), 0);

    let state = pc.state.lock();
    assert!(
        !state.in_flight_file_pages.contains_key(&PageIndex::new(0)),
        "owner yield must clear the in-flight fetch slot so the next retry can become owner"
    );
    assert_eq!(
        state
            .file_page_waits
            .get(&PageIndex::new(0))
            .map(crate::page_backed::notification::page_ready_source_id),
        Some(inner_source),
        "the PageBacked retry source must survive owner yield for late waiter registration"
    );
}

#[test]
fn file_page_stale_owner_after_truncate_cannot_publish_page() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(59), 4);
    let page = PageIndex::new(0);
    let fetch_id = {
        let mut state = pc.state.lock();
        let fetch_id = state.allocate_file_fetch_id();
        let wait = crate::page_backed::notification::new_page_ready_wait();
        let source_id = crate::page_backed::notification::page_ready_source_id(&wait);
        let mut fetch = FilePageFetch::new(fetch_id);
        fetch.source_id = Some(source_id);
        fetch.joined = true;
        state.file_page_waits.insert(page, wait);
        state.in_flight_file_pages.insert(page, fetch);
        fetch_id
    };

    assert_eq!(step_truncate(&pc, 0, &guard), V3Out::Done(()));

    let stale = pc.install_fetched_file_page_from_owner(
        page,
        MaterializeAccess::Read,
        Frame::new(page_allocator::zero_frame_ppn().expect("zero frame")),
        fetch_id,
    );

    match stale {
        V3Out::Err(errno) => assert_eq!(errno, V3Errno::EAGAIN),
        other => panic!("expected stale owner publish to return EAGAIN, got {other:?}"),
    }
    assert_eq!(pc.resident_pages(), 0);
    assert_eq!(pc.lookup(page), None);
    assert!(
        pc.state.lock().file_page_waits.contains_key(&page),
        "truncate must keep the PageBacked retry source live for late waiter registration"
    );
}

#[test]
fn file_page_materialization_keeps_map_pin_outside_state_lock() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    reset_page_container_lock_service_observations_for_test();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(56), 4);

    let first = match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected materialize outcome: {other:?}"),
    };
    let cold_observations = page_container_lock_service_observations_for_test();

    reset_page_container_lock_service_observations_for_test();
    let second = match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Write, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected rematerialize outcome: {other:?}"),
    };
    let hot_observations = page_container_lock_service_observations_for_test();

    assert!(first.newly_installed);
    assert_eq!(second.ppn, first.ppn);
    assert!(!second.newly_installed);
    assert_eq!(
        cold_observations,
        (0, 0),
        "cold file materialization must not acquire map pins under PageContainer.state"
    );
    assert_eq!(
        hot_observations,
        (0, 0),
        "hot file rematerialization must not acquire map pins under PageContainer.state"
    );
}

#[test]
fn page_container_materialize_page_propagates_file_block() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(BlockingFs);
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(77), 4);

    match pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
        V3Out::Yield { shape, .. } => {
            let Some((carrier, interests)) =
                crate::page_backed::notification::wait_source_parts(&shape)
            else {
                panic!("expected wait-source file fetch, got {shape:?}");
            };
            assert_eq!(carrier, 9);
            assert_eq!(interests, 0x44);
        }
        other => panic!("expected blocked file fetch, got {other:?}"),
    }
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn page_container_materialize_page_wraps_device_ppns() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xfeed_0000),
            page_count: 2,
        },
        2,
    );

    let page = match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Write, &guard) {
        V3Out::Done(page) => page,
        other => panic!("unexpected materialize outcome: {other:?}"),
    };

    assert_eq!(page.ppn, Ppn(0xfeed_0001));
    assert!(page.newly_installed);
    assert!(!page.dirty);
    assert_eq!(pc.lookup(PageIndex::new(1)), Some(Ppn(0xfeed_0001)));
    match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Read, &guard) {
        V3Out::Err(errno) => {
            assert_eq!(errno, V3Errno::EINVAL);
        }
        other => panic!("expected out-of-bounds error, got {other:?}"),
    }
}

#[test]
fn pagebacked_step_read_materializes_pages_and_advances_offset() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        3,
    );
    let of = open_file_for_pc(&pc);
    of.set_offset((crate::vm::USER_PAGE_SIZE - 8) as u64);

    assert_eq!(step_read(&pc, &of, 32, &guard), V3Out::Done(32));

    assert_eq!(of.offset(), (crate::vm::USER_PAGE_SIZE - 8 + 32) as u64);
    assert_eq!(pc.resident_pages(), 2);
    assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(!pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
}

#[test]
fn pagebacked_step_read_eof_does_not_materialize() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let of = open_file_for_pc(&pc);
    of.set_offset(crate::vm::USER_PAGE_SIZE as u64);

    assert_eq!(step_read(&pc, &of, 16, &guard), V3Out::Done(0));
    assert_eq!(of.offset(), crate::vm::USER_PAGE_SIZE as u64);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_write_marks_dirty_and_advances_offset() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let of = open_file_for_pc(&pc);

    assert_eq!(
        step_write(&pc, &of, crate::vm::USER_PAGE_SIZE + 17, &guard),
        V3Out::Done(crate::vm::USER_PAGE_SIZE + 17)
    );

    assert_eq!(of.offset(), (crate::vm::USER_PAGE_SIZE + 17) as u64);
    assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
}

#[test]
fn pagebacked_step_read_returns_advanced_then_blocked_after_progress() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(BlockingFs);
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(88), 2);
    let of = open_file_for_pc(&pc);
    pc.state
        .lock()
        .pages
        .install_if_absent(PageIndex::new(0), cached_frame_for_test())
        .expect("seed cached page");

    assert_eq!(
        step_read(&pc, &of, crate::vm::USER_PAGE_SIZE + 1, &guard),
        V3Out::yield_on_wait_source(
            step_engine::ByteProgress::new(crate::vm::USER_PAGE_SIZE),
            9,
            0x44,
        )
    );
    assert_eq!(of.offset(), crate::vm::USER_PAGE_SIZE as u64);
}

#[test]
fn pagebacked_step_write_rejects_device_backing() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_0000),
            page_count: 1,
        },
        1,
    );
    let of = open_file_for_pc(&pc);

    assert_eq!(
        step_write(&pc, &of, 8, &guard),
        V3Out::Err(Errno::EINVAL.into())
    );
    assert_eq!(of.offset(), 0);
    assert_eq!(pc.resident_pages(), 0);
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 wave-3 smoke tests for `ReadOp`/`WriteOp` `StepOp` wraps.
    //!
    //! Each test builds the `*Op` adapter, drives it through a single
    //! `.step(&mut ctx)` call, and pins the outcome variant against the
    //! same expectation as the free-fn suite above. Compile-checks
    //! `impl StepOp` correctness; the heavy-lifting semantics tests
    //! live in the free-fn suite.
    use super::*;
    use crate::page_backed::{ReadOp, WriteOp};
    use step_engine::{PlaceholderProcessSubject, ScriptCtx, StepOp};

    #[test]
    fn read_op_advances_offset_through_step() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            3,
        );
        let of = open_file_for_pc(&pc);
        of.set_offset((crate::vm::USER_PAGE_SIZE - 8) as u64);
        let mut op = ReadOp {
            pc: &pc,
            of: &of,
            len: 32,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Done(32));
        assert_eq!(of.offset(), (crate::vm::USER_PAGE_SIZE - 8 + 32) as u64);
    }

    #[test]
    fn read_op_eof_returns_done_zero() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        let of = open_file_for_pc(&pc);
        of.set_offset(crate::vm::USER_PAGE_SIZE as u64);
        let mut op = ReadOp {
            pc: &pc,
            of: &of,
            len: 16,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Done(0));
        assert_eq!(pc.resident_pages(), 0);
    }

    #[test]
    fn write_op_marks_dirty_and_advances_offset() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            2,
        );
        let of = open_file_for_pc(&pc);
        let len = crate::vm::USER_PAGE_SIZE + 17;
        let mut op = WriteOp {
            pc: &pc,
            of: &of,
            len,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Done(len));
        assert_eq!(of.offset(), len as u64);
        assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
        assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
    }

    #[test]
    fn write_op_rejects_device_backing() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Device {
                base_ppn: Ppn(0xface_0000),
                page_count: 1,
            },
            1,
        );
        let of = open_file_for_pc(&pc);
        let mut op = WriteOp {
            pc: &pc,
            of: &of,
            len: 8,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Err(Errno::EINVAL.into()));
        assert_eq!(of.offset(), 0);
        assert_eq!(pc.resident_pages(), 0);
    }
}
