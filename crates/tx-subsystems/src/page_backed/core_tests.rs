//! Inline tests for the core page_backed module — extracted to a sibling
//! file in wave 9d to keep page_backed.rs under the 1500-line authored-Rust
//! cap. RecordingFs/BlockingFs fixtures live here, including their wave-9d
//! v3 trait impls (FsOpsV3 + FsPageBackingV3) needed because MountPayload
//! now carries fs_ops_v3 / fs_page_backing_v3 fields.

    use super::*;
    use crate::execution::{Errno, StepOutcome, WaitToken};
    use crate::mount::{DevId, MountOptions, MountPayload, SourceLabel};
    use crate::vfs::{
        Credential, DirCursor, DirEntry, FsObjectId, FsOps, FsOpsV3, InodeKind, InodeMeta, OpenFile,
        OpenFileFlags, RNode, RNodeBacking,
    };
    use alloc::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    fn setup_host_substrate() {
        tx_substrate::testing::init_host_for_test_once();
        crate::zones::register_all().expect("kernel zones");
        match tx_substrate::page_allocator::claim_zero_frame() {
            Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
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

    impl FsOps for RecordingFs {
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

    impl FsPageBacking for RecordingFs {
        fn fetch_page(
            &self,
            fs_object_id: FsObjectId,
            offset: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Frame> {
            self.fetches.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_offset.store(offset, Ordering::Release);
            StepOutcome::Done(Frame::new(
                page_allocator::zero_frame_ppn().expect("zero frame"),
            ))
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }
    }

    // Wave 9d: v3 trait impls so `RecordingFs` satisfies the v3 fields
    // on `MountPayload`. Bodies mirror the v4 impls one-for-one.
    impl crate::vfs::FsOpsV3 for RecordingFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(InodeMeta::new(InodeKind::Regular, 0o100644))
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            (FsObjectId, InodeMeta),
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            (FsObjectId, InodeMeta),
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            (FsObjectId, InodeMeta),
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            Option<(DirEntry, DirCursor)>,
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::done(None)
        }

        fn destroy_inode(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }
    }

    impl FsPageBackingV3 for RecordingFs {
        fn fetch_page(
            &self,
            fs_object_id: FsObjectId,
            offset: u64,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
            self.fetches.fetch_add(1, Ordering::AcqRel);
            self.last_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            self.last_offset.store(offset, Ordering::Release);
            tx_substrate::step_v3::StepOutcome::done(Frame::new(
                page_allocator::zero_frame_ppn().expect("zero frame"),
            ))
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }

        fn fsync(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }
    }

    struct BlockingFs;

    impl FsPageBacking for BlockingFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Frame> {
            StepOutcome::Blocked(WaitToken::new(9, 0x44))
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }
    }

    impl FsOps for BlockingFs {
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
            StepOutcome::Err(Errno::ENOSYS)
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

    // Wave 9d: v3 trait impls so `BlockingFs` satisfies the v3 fields
    // on `MountPayload`. Mirror the v4 bodies, except `fetch_page` —
    // v4 returns `Blocked(token)`, which has no v3 equivalent in
    // `NoProgress` outcomes; the closest analog is `Err(EAGAIN)` so
    // tests that exercise the v3 walker get a non-Done deterministic
    // result. The page-backed unit tests below all drive `BlockingFs`
    // through the v4 surface (`materialize_page` reads `fs_page_backing`,
    // not the v3 sibling), so this v3 body is dispatch-stub-only.
    impl crate::vfs::FsOpsV3 for BlockingFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            (FsObjectId, InodeMeta),
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            (FsObjectId, InodeMeta),
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            (FsObjectId, InodeMeta),
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<
            Option<(DirEntry, DirCursor)>,
            tx_substrate::step_v3::NoProgress,
        > {
            tx_substrate::step_v3::StepOutcome::done(None)
        }

        fn destroy_inode(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }
    }

    impl FsPageBackingV3 for BlockingFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
            // Wave 9h-β: align v3 with v4's `Blocked(WaitToken(9, 0x44))`
            // so production fns routing through v3 (e.g. materialize_file_page)
            // observe a yield, not EAGAIN. Without this, the v4→v3 swap of
            // materialize_file_page would convert the test's Blocked
            // expectation into Err(EAGAIN).
            tx_substrate::step_v3::StepOutcome::yield_on_carrier(
                tx_substrate::step_v3::NoProgress,
                9,
                0x44,
            )
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }

        fn fsync(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
            tx_substrate::step_v3::StepOutcome::done(())
        }
    }

    fn file_page_container(
        fs_v3: Arc<dyn FsOpsV3>,
        page_backing_v3: Arc<dyn FsPageBackingV3>,
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
                mount: MountPayloadPin::acquire(&tx_substrate::zone::PayloadCap::from_cap(mount)),
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
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            4,
        );

        let page = match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Write, &guard) {
            StepOutcome::Done(page) => page,
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
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(RecordingFs::new());
        let pc = file_page_container(
            fs.clone(),
            fs.clone(),
            FsObjectId::new(55),
            4,
        );

        let first = match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Read, &guard) {
            StepOutcome::Done(page) => page,
            other => panic!("unexpected materialize outcome: {other:?}"),
        };
        let second = match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Write, &guard)
        {
            StepOutcome::Done(page) => page,
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
    fn page_container_materialize_page_propagates_file_block() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(BlockingFs);
        let pc = file_page_container(
            fs.clone(),
            fs,
            FsObjectId::new(77),
            4,
        );

        assert_eq!(
            match pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
                StepOutcome::Blocked(token) => StepOutcome::<()>::Blocked(token),
                StepOutcome::Done(_)
                | StepOutcome::Advanced(_)
                | StepOutcome::AdvancedThenBlocked(_, _)
                | StepOutcome::Err(_) => panic!("expected blocked file fetch"),
            },
            StepOutcome::Blocked(WaitToken::new(9, 0x44))
        );
        assert_eq!(pc.resident_pages(), 0);
    }

    #[test]
    fn page_container_materialize_page_wraps_device_ppns() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Device {
                base_ppn: Ppn(0xfeed_0000),
                page_count: 2,
            },
            2,
        );

        let page = match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Write, &guard) {
            StepOutcome::Done(page) => page,
            other => panic!("unexpected materialize outcome: {other:?}"),
        };

        assert_eq!(page.ppn, Ppn(0xfeed_0001));
        assert!(page.newly_installed);
        assert!(!page.dirty);
        assert_eq!(pc.lookup(PageIndex::new(1)), Some(Ppn(0xfeed_0001)));
        assert_eq!(
            match pc.materialize_page(PageIndex::new(2), MaterializeAccess::Read, &guard) {
                StepOutcome::Err(errno) => StepOutcome::<()>::Err(errno),
                StepOutcome::Done(_)
                | StepOutcome::Advanced(_)
                | StepOutcome::AdvancedThenBlocked(_, _)
                | StepOutcome::Blocked(_) => panic!("expected out-of-bounds error"),
            },
            StepOutcome::Err(Errno::EINVAL)
        );
    }

    #[test]
    fn pagebacked_step_read_materializes_pages_and_advances_offset() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            3,
        );
        let of = open_file_for_pc(&pc);
        of.set_offset((crate::vm::USER_PAGE_SIZE - 8) as u64);

        assert_eq!(step_read(&pc, &of, 32, &guard), StepOutcome::Done(32));

        assert_eq!(of.offset(), (crate::vm::USER_PAGE_SIZE - 8 + 32) as u64);
        assert_eq!(pc.resident_pages(), 2);
        assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
        assert!(!pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
    }

    #[test]
    fn pagebacked_step_read_eof_does_not_materialize() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        let of = open_file_for_pc(&pc);
        of.set_offset(crate::vm::USER_PAGE_SIZE as u64);

        assert_eq!(step_read(&pc, &of, 16, &guard), StepOutcome::Done(0));
        assert_eq!(of.offset(), crate::vm::USER_PAGE_SIZE as u64);
        assert_eq!(pc.resident_pages(), 0);
    }

    #[test]
    fn pagebacked_step_write_marks_dirty_and_advances_offset() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            2,
        );
        let of = open_file_for_pc(&pc);

        assert_eq!(
            step_write(&pc, &of, crate::vm::USER_PAGE_SIZE + 17, &guard),
            StepOutcome::Done(crate::vm::USER_PAGE_SIZE + 17)
        );

        assert_eq!(of.offset(), (crate::vm::USER_PAGE_SIZE + 17) as u64);
        assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
        assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
    }

    #[test]
    fn pagebacked_step_read_returns_advanced_then_blocked_after_progress() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
        let fs = Arc::new(BlockingFs);
        let pc = file_page_container(
            fs.clone(),
            fs,
            FsObjectId::new(88),
            2,
        );
        let of = open_file_for_pc(&pc);
        pc.state
            .lock()
            .pages
            .install_if_absent(PageIndex::new(0), cached_frame_for_test())
            .expect("seed cached page");

        assert_eq!(
            step_read(&pc, &of, crate::vm::USER_PAGE_SIZE + 1, &guard),
            StepOutcome::AdvancedThenBlocked(crate::vm::USER_PAGE_SIZE, WaitToken::new(9, 0x44))
        );
        assert_eq!(of.offset(), crate::vm::USER_PAGE_SIZE as u64);
    }

    #[test]
    fn pagebacked_step_write_rejects_device_backing() {
        let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
        setup_host_substrate();
        let guard = tx_substrate::epoch::guard();
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
            StepOutcome::Err(Errno::EINVAL)
        );
        assert_eq!(of.offset(), 0);
        assert_eq!(pc.resident_pages(), 0);
    }
