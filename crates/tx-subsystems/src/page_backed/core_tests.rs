//! Inline tests for the core page_backed module — extracted to a
//! sibling file to keep page_backed.rs under the 1500-line
//! authored-Rust cap. RecordingFs/BlockingFs fixtures live here,
//! including their `FsOps` + `FsPageBacking` impls needed because
//! `MountPayload` carries `fs_ops` / `fs_page_backing` fields.

use super::*;
use crate::device::{
    BlockDevice, BlockDeviceHandle, BlockDeviceOps, BlockDeviceRegistration, DevT,
    PhysicalBlockNumber,
};
use crate::execution::Errno;
use crate::fs_iface::{
    BackendPageRequest, BackendPlan, BackendPlanner, BioPlanList, IoDataLeaseId, IoDataSource,
    PageCompletion, PageCompletionList, PageFrameRef,
};
use crate::io_manager::backend::BlockPageRequestTracker;
use crate::io_manager::block::{
    BioPlan, BioVec, BlockCompletion, BlockCompletionSource, BlockDeviceCompletion, BlockDispatch,
    BlockDispatchExecutor, BlockFlags, BlockOp, BlockQueue, BlockRequestId, BlockServiceNext,
    BlockTag, DeviceKey, LbaRange,
};
use crate::io_manager::page::service::PageServiceBackendContext;
use crate::io_manager::page::service::{
    PageServiceBackendSubmitOutcome, PageServiceDrivenWork, PageServiceNext,
};
use crate::io_manager::page::{PageIoCompletion, PageIoCompletionKind, PageIoResult};
use crate::io_manager::runtime::{IoServiceKind, ServiceBudget, ServiceKick, ServiceWakeSource};
use crate::mount::{DevId, MountOptions, MountPayload, SourceLabel};
use crate::page_backed::adapter::step_engine::{
    self as step_engine, Errno as V3Errno, NoProgress, PlaceholderProcessSubject, ScriptCtx,
    StepOp, StepOutcome as V3Out,
};
use crate::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
    OpenFileFlags, RNode, RNodeBacking,
};
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

struct RecordingPlanner;

impl BackendPlanner for RecordingPlanner {
    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
        BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![
            PageCompletion::new(
                request.id,
                request.range,
                crate::io_manager::page::PageIoResult::Done,
                request
                    .generation_hint
                    .unwrap_or(crate::io_manager::page::PageGeneration::new(0)),
                crate::io_manager::page::PageIoCompletionKind::ReadInstalled,
            ),
        ]))
    }
}

struct SourceRecordingPlanner {
    source: SpinMutex<Option<IoDataSource>>,
}

impl SourceRecordingPlanner {
    const fn new() -> Self {
        Self {
            source: SpinMutex::new(None),
        }
    }
}

impl BackendPlanner for SourceRecordingPlanner {
    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
        *self.source.lock() = Some(request.source);
        BackendPlan::Complete(PageCompletionList::default())
    }
}

struct LockCheckingPlanner {
    calls: AtomicUsize,
    saw_page_container_lock: AtomicBool,
}

impl LockCheckingPlanner {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            saw_page_container_lock: AtomicBool::new(false),
        }
    }
}

impl BackendPlanner for LockCheckingPlanner {
    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if page_container_state_lock_held_for_test() {
            self.saw_page_container_lock.store(true, Ordering::Release);
        }
        BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![
            PageCompletion::new(
                request.id,
                request.range,
                crate::io_manager::page::PageIoResult::Done,
                request
                    .generation_hint
                    .unwrap_or(crate::io_manager::page::PageGeneration::new(0)),
                crate::io_manager::page::PageIoCompletionKind::ReadInstalled,
            ),
        ]))
    }
}

struct FrameReturningPlanner {
    ppn: Ppn,
    calls: AtomicUsize,
}

impl FrameReturningPlanner {
    fn new(ppn: Ppn) -> Self {
        Self {
            ppn,
            calls: AtomicUsize::new(0),
        }
    }
}

impl BackendPlanner for FrameReturningPlanner {
    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
        self.calls.fetch_add(1, Ordering::AcqRel);
        BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![
            PageCompletion::new(
                request.id,
                request.range,
                PageIoResult::Done,
                request
                    .generation_hint
                    .unwrap_or(crate::io_manager::page::PageGeneration::new(0)),
                PageIoCompletionKind::ReadInstalled,
            )
            .with_frame_ref(PageFrameRef::new(self.ppn)),
        ]))
    }
}

struct BioOnlyPlanner {
    calls: AtomicUsize,
}

impl BioOnlyPlanner {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

impl BackendPlanner for BioOnlyPlanner {
    fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
        self.calls.fetch_add(1, Ordering::AcqRel);
        BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
            DeviceKey::new(8),
            BlockOp::Read,
            LbaRange::new(64, 1),
            alloc::vec![BioVec::new(0xfeed, 0, crate::vm::USER_PAGE_SIZE as u32)],
            BlockFlags::EMPTY,
        )]))
    }
}

struct PageBackedServiceBlockDevice;

static PAGE_BACKED_SERVICE_BLOCK_DEVICE: PageBackedServiceBlockDevice =
    PageBackedServiceBlockDevice;
static PAGE_BACKED_SERVICE_LAST_READ: AtomicU64 = AtomicU64::new(u64::MAX);
static PAGE_BACKED_SERVICE_BLOCK_REG: BlockDeviceRegistration = BlockDeviceRegistration {
    devt: DevT::new(0, 8),
    name: "pagebacked-service-block",
    ops: &PAGE_BACKED_SERVICE_BLOCK_DEVICE,
};

impl BlockDeviceOps for PageBackedServiceBlockDevice {
    fn read_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        _target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        PAGE_BACKED_SERVICE_LAST_READ.store(block_id.as_u64(), Ordering::SeqCst);
        V3Out::Done(())
    }

    fn write_blocks(
        &self,
        _block_id: PhysicalBlockNumber,
        _source: &[Frame],
        _guard: &Guard<'_>,
    ) -> V3Out<(), NoProgress> {
        V3Out::Done(())
    }

    fn barrier(&self, _guard: &Guard<'_>) -> V3Out<(), NoProgress> {
        V3Out::Done(())
    }
}

impl BlockDevice for PageBackedServiceBlockDevice {
    fn total_blocks(&self) -> u64 {
        1024
    }

    fn block_size(&self) -> u32 {
        crate::vm::USER_PAGE_SIZE as u32
    }
}

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

fn user_page_gift_for_test() -> (crate::vm::UserPageGift, Ppn) {
    setup_host_substrate();
    let aspace = crate::vm::AddressSpace::new_cap().expect("gift source aspace");
    let range = crate::vm::UserRange::new_aligned(
        crate::vm::UserVirtAddr(0x10000),
        crate::vm::USER_PAGE_SIZE,
    )
    .expect("gift source range");
    let frame = page_allocator::reserve_frame(ZeroPolicy::Zeroed)
        .expect("gift source frame")
        .commit();
    let ppn = frame.ppn();
    let pin = frame.try_gift_pin().expect("gift pin for source frame");
    let gift = crate::vm::UserPageGift::new_for_vm(
        ppn,
        crate::vm::UserPageGiftSource::new(aspace, range),
        pin,
        crate::vm::UserPageGiftFreeze::DetachedPrivate,
    );
    drop(frame);
    (gift, ppn)
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
    l4_pending_before_reentry: AtomicUsize,
    l4_pending_after_reentry: AtomicUsize,
    l4_waiters_after_reentry: AtomicUsize,
    l4_generation_hint: AtomicU64,
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
            l4_pending_before_reentry: AtomicUsize::new(0),
            l4_pending_after_reentry: AtomicUsize::new(0),
            l4_waiters_after_reentry: AtomicUsize::new(0),
            l4_generation_hint: AtomicU64::new(0),
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
            self.l4_pending_before_reentry
                .store(pc.file_io_request_count_for_test(), Ordering::Release);
            if let Some(request) = pc.file_io_pending_request_for_test(PageIndex::new(0)) {
                self.l4_generation_hint.store(
                    request
                        .generation_hint
                        .map(|generation| generation.raw())
                        .unwrap_or(0),
                    Ordering::Release,
                );
            }
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
            self.l4_pending_after_reentry
                .store(pc.file_io_request_count_for_test(), Ordering::Release);
            self.l4_waiters_after_reentry.store(
                pc.file_io_waiter_count_for_test(PageIndex::new(0)),
                Ordering::Release,
            );
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

fn file_page_container_with_planner(
    fs_v3: Arc<dyn FsOps>,
    page_backing_v3: Arc<dyn FsPageBacking>,
    fs_object_id: FsObjectId,
    page_count: u64,
    planner: Arc<dyn BackendPlanner>,
) -> PageContainer {
    let mount = MountPayload::new_cap_with_backend_planner(
        fs_v3,
        page_backing_v3,
        None,
        DevId::new(8),
        MountOptions::default(),
        "mockfs",
        SourceLabel::Static("mock"),
        Some(planner),
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

fn file_page_container_cap_with_planner(
    fs_v3: Arc<dyn FsOps>,
    page_backing_v3: Arc<dyn FsPageBacking>,
    fs_object_id: FsObjectId,
    page_count: u64,
    planner: Arc<dyn BackendPlanner>,
) -> step_engine::Cap<PageContainer> {
    let mount = MountPayload::new_cap_with_backend_planner(
        fs_v3,
        page_backing_v3,
        None,
        DevId::new(8),
        MountOptions::default(),
        "mockfs",
        SourceLabel::Static("mock"),
        Some(planner),
    )
    .expect("mount payload");
    PageContainer::new_cap(
        PageContainerKind::File {
            mount: MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(mount)),
            fs_object_id,
        },
        page_count,
    )
    .expect("page container cap")
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
            packet: false,
        },
    )
}

#[test]
fn file_page_container_builds_mount_payload_backend_context() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container_with_planner(
        fs.clone(),
        fs,
        FsObjectId::new(77),
        4,
        Arc::new(RecordingPlanner),
    );

    let context = pc
        .file_backend_context()
        .expect("file page container backend context");

    assert_eq!(context.object().raw(), 77);
    let request = PageIoRequest::new(
        PageIoRequestId::new(9),
        pc.io_manager_key(),
        PageIoRange::new(2, 1),
        PageIoOp::Read,
        PageIoPriority::Demand,
        PageIoFlags::DEMAND,
        Some(PageGeneration::new(5)),
    );
    let plan = context
        .plan_submission(request)
        .expect("mount-hosted backend planner");
    match plan {
        BackendPlan::Complete(completions) => {
            assert_eq!(completions.as_slice().len(), 1);
            assert_eq!(completions.as_slice()[0].id, PageIoRequestId::new(9));
            assert_eq!(completions.as_slice()[0].range, PageIoRange::new(2, 1));
            assert_eq!(completions.as_slice()[0].generation, PageGeneration::new(5));
        }
        other => panic!("expected completion plan, got {other:?}"),
    }
}

#[test]
fn file_page_backend_context_preserves_explicit_data_source() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(SourceRecordingPlanner::new());
    let pc =
        file_page_container_with_planner(fs.clone(), fs, FsObjectId::new(78), 4, planner.clone());
    let source = IoDataSource::page_cache(
        IoDataLeaseId::new(17),
        PageFrameRef::new(Ppn(0x80)),
        0,
        crate::vm::USER_PAGE_SIZE as u32,
    );
    let request = PageIoRequest::new(
        PageIoRequestId::new(10),
        pc.io_manager_key(),
        PageIoRange::new(1, 1),
        PageIoOp::Writeback,
        PageIoPriority::BackgroundWriteback,
        PageIoFlags::WRITEBACK,
        Some(PageGeneration::new(6)),
    );

    pc.file_backend_context()
        .expect("file page container backend context")
        .plan_submission_with_source(request, source.clone())
        .expect("mount-hosted backend planner");

    assert_eq!(*planner.source.lock(), Some(source));
}

#[test]
fn non_file_page_container_has_no_backend_context() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );

    assert!(pc.file_backend_context().is_none());
}

#[test]
fn file_page_container_drives_service_submission_through_mount_planner_without_state_lock() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(LockCheckingPlanner::new());
    let pc =
        file_page_container_with_planner(fs.clone(), fs, FsObjectId::new(88), 4, planner.clone());
    {
        let mut state = pc.state.lock();
        state
            .file_io_service
            .submit(
                pc.io_manager_key(),
                PageIoRange::new(1, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(7)),
            )
            .expect("staged file service submission");
    }

    let mut block_queue = BlockQueue::new(4);
    let mut kicks = 0usize;
    let driven = pc
        .drive_file_io_service_once(ServiceBudget::new(1), &mut block_queue, |_| {
            kicks += 1;
            true
        })
        .expect("file service drive");

    assert_eq!(planner.calls.load(Ordering::Acquire), 1);
    assert!(
        !planner.saw_page_container_lock.load(Ordering::Acquire),
        "backend planning must run after releasing PageContainer state lock"
    );
    assert_eq!(block_queue.len(), 0);
    assert_eq!(
        driven.work,
        alloc::vec![PageServiceDrivenWork::BackendSubmission(
            PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                queued: 1,
                wake: Some(crate::io_manager::page::service::PageServiceWake::Wake),
            },
        )]
    );
    assert_eq!(driven.next, PageServiceNext::Runnable);
    assert_eq!(kicks, 1);
}

#[test]
fn file_page_service_completion_applies_matching_error_to_pageslot() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(89), 4);
    let page = PageIndex::new(2);
    let generation = {
        let mut state = pc.state.lock();
        let slot = state.file_page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("test slot should own fetch");
        };
        state.file_io_service.push_completion(PageIoCompletion::new(
            PageIoRequestId::new(11),
            PageIoRange::new(page.as_u64(), 1),
            PageIoResult::Err(Errno::EIO),
            generation,
            PageIoCompletionKind::ReadInstalled,
        ));
        generation
    };

    let mut block_queue = BlockQueue::new(4);
    let driven = pc
        .drive_file_io_service_once(ServiceBudget::new(1), &mut block_queue, |_| true)
        .expect("file service drive");

    assert!(matches!(
        driven.work.as_slice(),
        [PageServiceDrivenWork::Completion(_)]
    ));
    assert_eq!(
        pc.file_page_slot_snapshot_for_test(page),
        Some(PageSlotSnapshot {
            state: PageSlotState::Error { errno: Errno::EIO },
            generation,
        })
    );
}

#[test]
fn file_page_service_completion_rejects_stale_pageslot_generation() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(90), 4);
    let page = PageIndex::new(2);
    let stale_generation = {
        let mut state = pc.state.lock();
        let slot = state.file_page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("test slot should own fetch");
        };
        slot.invalidate();
        state.file_io_service.push_completion(PageIoCompletion::new(
            PageIoRequestId::new(12),
            PageIoRange::new(page.as_u64(), 1),
            PageIoResult::Err(Errno::EIO),
            generation,
            PageIoCompletionKind::ReadInstalled,
        ));
        generation
    };

    let mut block_queue = BlockQueue::new(4);
    pc.drive_file_io_service_once(ServiceBudget::new(1), &mut block_queue, |_| true)
        .expect("file service drive");

    let snapshot = pc
        .file_page_slot_snapshot_for_test(page)
        .expect("staged slot");
    assert_eq!(snapshot.state, PageSlotState::Empty);
    assert_ne!(snapshot.generation, stale_generation);
}

#[test]
fn file_page_service_completion_installs_planned_read_frame() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(91), 4);
    let page = PageIndex::new(1);
    let ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let generation = {
        let mut state = pc.state.lock();
        let slot = state.file_page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("test slot should own fetch");
        };
        state.file_io_service.push_page_completion(
            PageCompletion::new(
                PageIoRequestId::new(13),
                PageIoRange::new(page.as_u64(), 1),
                PageIoResult::Done,
                generation,
                PageIoCompletionKind::ReadInstalled,
            )
            .with_frame_ref(PageFrameRef::new(ppn)),
        );
        generation
    };

    let mut block_queue = BlockQueue::new(4);
    pc.drive_file_io_service_once(ServiceBudget::new(1), &mut block_queue, |_| true)
        .expect("file service drive");

    assert_eq!(pc.lookup(page), Some(ppn));
    assert_eq!(
        pc.file_page_slot_snapshot_for_test(page),
        Some(PageSlotSnapshot {
            state: PageSlotState::Resident { ppn },
            generation,
        })
    );
}

#[test]
fn file_page_writeback_admission_transitions_dirty_slot_and_queues_request() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(95), 4);
    let page = PageIndex::new(1);
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    {
        let mut state = pc.state.lock();
        state
            .pages
            .install_if_absent(page, frame)
            .expect("seed page");
        let slot = state.file_page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("slot fetch owner");
        };
        slot.complete_fetch(generation, Ok(ppn))
            .expect("resident slot");
        slot.mark_dirty().expect("dirty slot");
        state.pages.mark_dirty(page).expect("dirty page cache");
    }

    let id = pc
        .queue_file_page_writeback(page)
        .expect("writeback request");
    assert_eq!(
        pc.snapshot_file_fsync_frontier()
            .expect("file frontier")
            .pages(),
        &[(page, PageGeneration::new(2))]
    );
    assert_eq!(pc.file_io_request_count_for_test(), 1);
    assert_eq!(
        pc.file_page_slot_snapshot_for_test(page)
            .expect("slot")
            .state,
        PageSlotState::Writeback {
            ppn,
            submitted_generation: PageGeneration::new(2),
            redirtied: false,
        }
    );
    assert!(pc.page_marks(page).expect("marks").writeback);
    assert_ne!(id.raw(), 0);
}

#[test]
fn file_page_writeback_leases_source_until_completion() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(SourceRecordingPlanner::new());
    let pc =
        file_page_container_with_planner(fs.clone(), fs, FsObjectId::new(96), 4, planner.clone());
    let page = PageIndex::new(1);
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    let generation = {
        let mut state = pc.state.lock();
        state
            .pages
            .install_if_absent(page, frame)
            .expect("seed page");
        let slot = state.file_page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("fetch owner");
        };
        slot.complete_fetch(generation, Ok(ppn)).expect("resident");
        let dirty = slot.mark_dirty().expect("dirty");
        state.pages.mark_dirty(page).expect("dirty mark");
        dirty.generation
    };
    let id = pc
        .queue_file_page_writeback(page)
        .expect("writeback request");
    let mut block_queue = BlockQueue::new(4);
    pc.drive_file_io_service_once(ServiceBudget::new(1), &mut block_queue, |_| true)
        .expect("planner turn");
    assert!(
        matches!(*planner.source.lock(), Some(IoDataSource::PageCache { frame, .. }) if frame.ppn() == ppn)
    );
    assert_eq!(pc.file_io_lease_count_for_test(), 1);
    pc.state
        .lock()
        .file_io_service
        .push_completion(PageIoCompletion::new(
            id,
            PageIoRange::new(page.as_u64(), 1),
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::WritebackFinished,
        ));
    pc.drive_file_io_service_once(ServiceBudget::new(1), &mut block_queue, |_| true)
        .expect("completion turn");
    assert_eq!(pc.file_io_lease_count_for_test(), 0);
    assert_eq!(
        pc.file_page_slot_snapshot_for_test(page)
            .expect("slot")
            .state,
        PageSlotState::Resident { ppn }
    );
    assert!(!pc.page_marks(page).expect("marks").dirty);
}

#[test]
fn file_page_materialize_uses_frame_planner_before_compat_fetch() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(FrameReturningPlanner::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    ));
    let pc = file_page_container_with_planner(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(92),
        4,
        planner.clone(),
    );

    let materialized = match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Read, &guard)
    {
        V3Out::Done(page) => page,
        other => panic!("expected planned read materialization, got {other:?}"),
    };

    assert_eq!(materialized.ppn, planner.ppn);
    assert_eq!(planner.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fs.fetches.load(Ordering::Acquire),
        0,
        "frame-backed backend planner completion should bypass compat fetch"
    );
}

#[test]
fn file_page_planned_write_marks_pageslot_dirty() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(FrameReturningPlanner::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    ));
    let pc = file_page_container_with_planner(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(93),
        4,
        planner.clone(),
    );
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Write, &guard) {
        V3Out::Done(materialized) => assert_eq!(materialized.ppn, planner.ppn),
        other => panic!("expected planned write materialization, got {other:?}"),
    }

    let snapshot = pc
        .file_page_slot_snapshot_for_test(page)
        .expect("planned page slot");
    assert_eq!(snapshot.state, PageSlotState::Dirty { ppn: planner.ppn });
    assert!(pc.page_marks(page).expect("planned page marks").dirty);
}

#[test]
fn file_page_compat_write_marks_pageslot_dirty() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(94), 4);
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Write, &guard) {
        V3Out::Done(_) => {}
        other => panic!("expected compatibility write materialization, got {other:?}"),
    }

    let snapshot = pc
        .file_page_slot_snapshot_for_test(page)
        .expect("compatibility page slot");
    assert!(matches!(snapshot.state, PageSlotState::Dirty { .. }));
    assert!(pc.page_marks(page).expect("compatibility page marks").dirty);
}

#[test]
fn file_page_materialize_bio_only_plan_yields_without_compat_fetch() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(BioOnlyPlanner::new());
    let pc = file_page_container_with_planner(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(93),
        4,
        planner.clone(),
    );

    match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Read, &guard) {
        V3Out::Yield { shape, .. } => {
            let Some((_source, interests)) =
                crate::page_backed::notification::wait_source_parts(&shape)
            else {
                panic!("expected page-ready wait source after bio-only plan");
            };
            assert_eq!(interests, 0x1);
        }
        other => panic!("expected async yield after bio-only plan, got {other:?}"),
    }

    assert_eq!(planner.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fs.fetches.load(Ordering::Acquire),
        0,
        "Bio-only backend plans should stay on the owned L6 async path instead of falling back to compat fetch"
    );
    assert_eq!(pc.file_io_block_queue_len_for_test(), 1);
    assert_eq!(pc.file_io_block_tracker_len_for_test(), 1);
    assert!(pc.file_page_fetch_in_flight_for_test(PageIndex::new(1)));

    struct CompletingExecutor {
        completions: VecDeque<BlockDeviceCompletion>,
    }

    impl BlockDispatchExecutor for CompletingExecutor {
        fn submit(&mut self, dispatch: &BlockDispatch) {
            self.completions
                .push_back(BlockDeviceCompletion::new(dispatch.tag, Ok(())));
        }
    }

    impl BlockCompletionSource for CompletingExecutor {
        fn poll_completion(&mut self) -> Option<BlockDeviceCompletion> {
            self.completions.pop_front()
        }
    }

    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let mut executor = CompletingExecutor {
        completions: VecDeque::new(),
    };
    pc.drive_file_block_io_service_once(
        ServiceBudget::new(1),
        &mut executor,
        |_| Some(PageFrameRef::new(completion_ppn)),
        |_| true,
    )
    .expect("owned block service completion");
    pc.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)
        .expect("completion apply drive");

    assert!(!pc.file_page_fetch_in_flight_for_test(PageIndex::new(1)));
    match pc.materialize_page(PageIndex::new(1), MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => assert_eq!(page.ppn, completion_ppn),
        other => panic!("expected retry to observe installed async page, got {other:?}"),
    }
}

#[test]
fn file_page_bio_only_plan_drives_owned_l6_through_block_device_handle() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    struct DeviceBioPlanner {
        buffer_ppn: Ppn,
    }

    impl BackendPlanner for DeviceBioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(
                    self.buffer_ppn.0 as u64,
                    0,
                    crate::vm::USER_PAGE_SIZE as u32,
                )],
                BlockFlags::EMPTY,
            )]))
        }
    }

    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let planner = Arc::new(DeviceBioPlanner {
        buffer_ppn: completion_ppn,
    });
    let pc = file_page_container_with_planner(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(96),
        4,
        planner.clone(),
    );
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Yield { .. } => {}
        other => panic!("expected async yield after bio-only plan, got {other:?}"),
    }

    assert_eq!(pc.file_io_block_queue_len_for_test(), 1);
    assert_eq!(pc.file_io_block_tracker_len_for_test(), 1);
    PAGE_BACKED_SERVICE_LAST_READ.store(u64::MAX, Ordering::SeqCst);

    let turn = crate::device::drive_page_container_file_block_device_service_once(
        &pc,
        ServiceBudget::new(1),
        BlockDeviceHandle::whole(&PAGE_BACKED_SERVICE_BLOCK_REG),
        &guard,
        |_| true,
    )
    .expect("page container block-device service turn");

    assert_eq!(turn.dispatched, 1);
    assert_eq!(turn.device_completions, 1);
    assert_eq!(turn.page_completions, 1);
    assert_eq!(turn.next, BlockServiceNext::Sleeping);
    assert_eq!(PAGE_BACKED_SERVICE_LAST_READ.load(Ordering::SeqCst), 64);

    pc.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)
        .expect("completion apply drive");

    assert_eq!(pc.lookup(page), Some(completion_ppn));
    assert!(!pc.file_page_fetch_in_flight_for_test(page));
    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => assert_eq!(page.ppn, completion_ppn),
        other => panic!("expected retry to observe installed async page, got {other:?}"),
    }
}

#[test]
fn file_page_io_service_turn_plans_dispatches_and_applies_device_completion() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    struct DeviceBioPlanner {
        buffer_ppn: Ppn,
    }

    impl BackendPlanner for DeviceBioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(
                    self.buffer_ppn.0 as u64,
                    0,
                    crate::vm::USER_PAGE_SIZE as u32,
                )],
                BlockFlags::EMPTY,
            )]))
        }
    }

    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let planner = Arc::new(DeviceBioPlanner {
        buffer_ppn: completion_ppn,
    });
    let pc =
        file_page_container_with_planner(fs.clone(), fs.clone(), FsObjectId::new(97), 4, planner);
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Yield { .. } => {}
        other => panic!("expected async yield after bio-only plan, got {other:?}"),
    }

    PAGE_BACKED_SERVICE_LAST_READ.store(u64::MAX, Ordering::SeqCst);
    let turn = crate::device::drive_page_container_file_io_service_once(
        &pc,
        ServiceBudget::new(1),
        ServiceBudget::new(1),
        BlockDeviceHandle::whole(&PAGE_BACKED_SERVICE_BLOCK_REG),
        &guard,
        |_| true,
    )
    .expect("page container file I/O service turn");

    assert!(turn.page_before.is_some());
    assert_eq!(turn.block.dispatched, 1);
    assert_eq!(turn.block.device_completions, 1);
    assert_eq!(turn.block.page_completions, 1);
    assert!(turn.page_after.is_some());
    assert_eq!(
        turn.next,
        crate::device::PageContainerFileIoServiceNext::Sleeping
    );
    assert_eq!(PAGE_BACKED_SERVICE_LAST_READ.load(Ordering::SeqCst), 64);
    assert_eq!(pc.lookup(page), Some(completion_ppn));
    assert!(!pc.file_page_fetch_in_flight_for_test(page));
    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => assert_eq!(page.ppn, completion_ppn),
        other => panic!("expected retry to observe installed async page, got {other:?}"),
    }
}

#[test]
fn file_page_io_service_op_drives_aggregate_turn_as_step_op() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    struct DeviceBioPlanner {
        buffer_ppn: Ppn,
    }

    impl BackendPlanner for DeviceBioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(
                    self.buffer_ppn.0 as u64,
                    0,
                    crate::vm::USER_PAGE_SIZE as u32,
                )],
                BlockFlags::EMPTY,
            )]))
        }
    }

    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let planner = Arc::new(DeviceBioPlanner {
        buffer_ppn: completion_ppn,
    });
    let pc =
        file_page_container_with_planner(fs.clone(), fs.clone(), FsObjectId::new(98), 4, planner);
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Yield { .. } => {}
        other => panic!("expected async yield after bio-only plan, got {other:?}"),
    }
    drop(guard);

    PAGE_BACKED_SERVICE_LAST_READ.store(u64::MAX, Ordering::SeqCst);
    let mut op = crate::device::PageContainerFileIoServiceOp::new(
        &pc,
        ServiceBudget::new(1),
        ServiceBudget::new(1),
        BlockDeviceHandle::whole(&PAGE_BACKED_SERVICE_BLOCK_REG),
        |_| true,
    );
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);

    match outcome {
        V3Out::Done(turn) => {
            assert!(turn.page_before.is_some());
            assert_eq!(turn.block.dispatched, 1);
            assert_eq!(turn.block.device_completions, 1);
            assert_eq!(turn.block.page_completions, 1);
            assert!(turn.page_after.is_some());
            assert_eq!(
                turn.next,
                crate::device::PageContainerFileIoServiceNext::Sleeping
            );
        }
        other => panic!("expected service op Done(_), got {other:?}"),
    }
    assert_eq!(PAGE_BACKED_SERVICE_LAST_READ.load(Ordering::SeqCst), 64);
    assert_eq!(pc.lookup(page), Some(completion_ppn));
    assert!(!pc.file_page_fetch_in_flight_for_test(page));
    let guard = step_engine::guard();
    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => assert_eq!(page.ppn, completion_ppn),
        other => panic!("expected retry to observe installed async page, got {other:?}"),
    }
}

#[test]
fn file_page_io_service_task_loop_waits_for_service_kick_and_drives_turn() {
    fn block_on_ready<F: core::future::Future>(future: F) -> F::Output {
        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        let mut future = core::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            core::task::Poll::Ready(output) => output,
            core::task::Poll::Pending => panic!("expected service task loop to complete"),
        }
    }

    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    struct DeviceBioPlanner {
        buffer_ppn: Ppn,
    }

    impl BackendPlanner for DeviceBioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(
                    self.buffer_ppn.0 as u64,
                    0,
                    crate::vm::USER_PAGE_SIZE as u32,
                )],
                BlockFlags::EMPTY,
            )]))
        }
    }

    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let planner = Arc::new(DeviceBioPlanner {
        buffer_ppn: completion_ppn,
    });
    let pc =
        file_page_container_with_planner(fs.clone(), fs.clone(), FsObjectId::new(99), 4, planner);
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Yield { .. } => {}
        other => panic!("expected async yield after bio-only plan, got {other:?}"),
    }
    drop(guard);

    let wake_source = ServiceWakeSource::new(0x7100);
    let _ = wake_source.kick_with_post(ServiceKick::new(IoServiceKind::Page), |mailbox, event| {
        mailbox.post(event)
    });
    PAGE_BACKED_SERVICE_LAST_READ.store(u64::MAX, Ordering::SeqCst);

    let report = block_on_ready(crate::device::page_container_file_io_service_task_loop(
        &pc,
        BlockDeviceHandle::whole(&PAGE_BACKED_SERVICE_BLOCK_REG),
        &wake_source,
        crate::device::PageContainerFileIoServiceTaskConfig::run_turns(
            1,
            ServiceBudget::new(1),
            ServiceBudget::new(1),
        ),
    ));

    assert_eq!(report.waits_ready, 1);
    assert_eq!(report.ready_turns, 1);
    assert_eq!(report.waits_failed, 0);
    let turn = report.last_turn.expect("service turn");
    assert_eq!(turn.block.dispatched, 1);
    assert_eq!(turn.block.device_completions, 1);
    assert_eq!(turn.block.page_completions, 1);
    assert_eq!(
        turn.next,
        crate::device::PageContainerFileIoServiceNext::Sleeping
    );
    assert_eq!(PAGE_BACKED_SERVICE_LAST_READ.load(Ordering::SeqCst), 64);
    assert_eq!(pc.lookup(page), Some(completion_ppn));
}

#[test]
fn file_page_io_service_owned_task_loop_holds_runtime_for_static_submission() {
    fn block_on_ready<F: core::future::Future>(future: F) -> F::Output {
        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        let mut future = core::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            core::task::Poll::Ready(output) => output,
            core::task::Poll::Pending => panic!("expected owned service task loop to complete"),
        }
    }
    fn assert_send_static_future<F>(future: F) -> F
    where
        F: core::future::Future<Output = crate::device::PageContainerFileIoServiceTaskReport>
            + Send
            + 'static,
    {
        future
    }

    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(RecordingFs::new());
    struct DeviceBioPlanner {
        buffer_ppn: Ppn,
    }

    impl BackendPlanner for DeviceBioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(
                    self.buffer_ppn.0 as u64,
                    0,
                    crate::vm::USER_PAGE_SIZE as u32,
                )],
                BlockFlags::EMPTY,
            )]))
        }
    }

    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let planner = Arc::new(DeviceBioPlanner {
        buffer_ppn: completion_ppn,
    });
    let pc = file_page_container_cap_with_planner(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(100),
        4,
        planner,
    );
    let page = PageIndex::new(1);

    match pc.materialize_page(page, MaterializeAccess::Read, &guard) {
        V3Out::Yield { .. } => {}
        other => panic!("expected async yield after bio-only plan, got {other:?}"),
    }
    drop(guard);

    let runtime = crate::device::PageContainerFileIoServiceRuntime::new(
        pc.clone(),
        BlockDeviceHandle::whole(&PAGE_BACKED_SERVICE_BLOCK_REG),
        Arc::new(ServiceWakeSource::new(0x7101)),
    );
    let _ = runtime.kick(IoServiceKind::Page);
    PAGE_BACKED_SERVICE_LAST_READ.store(u64::MAX, Ordering::SeqCst);

    let report = block_on_ready(assert_send_static_future(
        crate::device::page_container_file_io_service_task_loop_owned(
            runtime,
            crate::device::PageContainerFileIoServiceTaskConfig::run_turns(
                1,
                ServiceBudget::new(1),
                ServiceBudget::new(1),
            ),
        ),
    ));

    assert_eq!(report.waits_ready, 1);
    assert_eq!(report.ready_turns, 1);
    assert_eq!(report.waits_failed, 0);
    let turn = report.last_turn.expect("service turn");
    assert_eq!(turn.block.dispatched, 1);
    assert_eq!(turn.block.device_completions, 1);
    assert_eq!(turn.block.page_completions, 1);
    assert_eq!(PAGE_BACKED_SERVICE_LAST_READ.load(Ordering::SeqCst), 64);
    assert_eq!(pc.lookup(page), Some(completion_ppn));
}

#[test]
fn file_page_service_records_l6_submit_outcomes_for_later_completion() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(BioOnlyPlanner::new());
    let pc =
        file_page_container_with_planner(fs.clone(), fs, FsObjectId::new(94), 4, planner.clone());
    let page = PageIndex::new(1);
    let request_id = {
        let mut state = pc.state.lock();
        state
            .file_io_service
            .submit(
                pc.io_manager_key(),
                PageIoRange::new(page.as_u64(), 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(19)),
            )
            .expect("staged file service submission")
    };

    let mut block_queue = BlockQueue::new(4);
    let mut tracker = BlockPageRequestTracker::new();
    let driven = pc
        .drive_file_io_service_once_with_tracker(
            ServiceBudget::new(1),
            &mut block_queue,
            &mut tracker,
            |_| true,
        )
        .expect("file service drive");

    assert_eq!(planner.calls.load(Ordering::Acquire), 1);
    assert_eq!(block_queue.len(), 1);
    assert_eq!(tracker.len(), 1);
    assert!(matches!(
        driven.work.as_slice(),
        [PageServiceDrivenWork::BackendSubmission(
            PageServiceBackendSubmitOutcome::BlockBiosQueued { .. }
        )]
    ));

    let block_completion = BlockCompletion {
        tag: BlockTag::new(1),
        id: BlockRequestId::new(1),
        plan: BioPlan::new(
            DeviceKey::new(8),
            BlockOp::Read,
            LbaRange::new(64, 1),
            alloc::vec![BioVec::new(0xfeed, 0, crate::vm::USER_PAGE_SIZE as u32)],
            BlockFlags::EMPTY,
        ),
        result: Ok(()),
    };
    let completions = tracker
        .complete(block_completion)
        .expect("tracked block request");

    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].request().id, request_id);
    assert_eq!(
        completions[0].request().generation_hint,
        Some(PageGeneration::new(19))
    );
}

#[test]
fn file_page_owned_l6_runtime_routes_bio_completion_into_page_slot() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let planner = Arc::new(BioOnlyPlanner::new());
    let pc =
        file_page_container_with_planner(fs.clone(), fs, FsObjectId::new(95), 4, planner.clone());
    let page = PageIndex::new(1);
    let completion_ppn = page_allocator::zero_frame_ppn().expect("zero frame");
    let (request_id, generation) = {
        let mut state = pc.state.lock();
        let slot = state.file_page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("test slot should own fetch");
        };
        let request_id = state
            .file_io_service
            .submit(
                pc.io_manager_key(),
                PageIoRange::new(page.as_u64(), 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(generation),
            )
            .expect("staged file service submission");
        (request_id, generation)
    };

    let page_driven = pc
        .drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)
        .expect("owned file service drive");

    assert_eq!(planner.calls.load(Ordering::Acquire), 1);
    assert_eq!(pc.file_io_block_queue_len_for_test(), 1);
    assert_eq!(pc.file_io_block_tracker_len_for_test(), 1);
    assert!(matches!(
        page_driven.work.as_slice(),
        [PageServiceDrivenWork::BackendSubmission(
            PageServiceBackendSubmitOutcome::BlockBiosQueued { .. }
        )]
    ));

    struct CompletingExecutor {
        completions: VecDeque<BlockDeviceCompletion>,
    }

    impl BlockDispatchExecutor for CompletingExecutor {
        fn submit(&mut self, dispatch: &BlockDispatch) {
            self.completions
                .push_back(BlockDeviceCompletion::new(dispatch.tag, Ok(())));
        }
    }

    impl BlockCompletionSource for CompletingExecutor {
        fn poll_completion(&mut self) -> Option<BlockDeviceCompletion> {
            self.completions.pop_front()
        }
    }

    let mut executor = CompletingExecutor {
        completions: VecDeque::new(),
    };
    let block_turn = pc
        .drive_file_block_io_service_once(
            ServiceBudget::new(1),
            &mut executor,
            |completion| {
                assert_eq!(completion.request().id, request_id);
                Some(PageFrameRef::new(completion_ppn))
            },
            |_| true,
        )
        .expect("owned block service drive");

    assert_eq!(block_turn.dispatched, 1);
    assert_eq!(block_turn.device_completions, 1);
    assert_eq!(block_turn.page_completions, 1);
    assert_eq!(block_turn.next, BlockServiceNext::Sleeping);
    assert_eq!(pc.file_io_block_tracker_len_for_test(), 0);

    pc.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)
        .expect("completion apply drive");

    assert_eq!(pc.lookup(page), Some(completion_ppn));
    assert_eq!(
        pc.file_page_slot_snapshot_for_test(page),
        Some(PageSlotSnapshot {
            state: PageSlotState::Resident {
                ppn: completion_ppn
            },
            generation,
        })
    );
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
fn owned_frame_handoff_releases_temporary_owner_on_cache_drop() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let owned = reserve_frame_with_reclaim(ZeroPolicy::Zeroed)
        .expect("reserve frame")
        .commit();
    let ppn = owned.ppn();

    let cached = cached_frame_from_frame(Frame::from_owned(owned)).expect("cache handoff");

    assert_eq!(cached.ppn, ppn);
    let map_pin = page_allocator::acquire_map_pin(ppn).expect("cache keeps frame live");
    drop(map_pin);
    drop(cached);
    assert!(page_allocator::acquire_map_pin(ppn).is_err());
}

#[test]
fn reclaim_clean_file_pages_drops_clean_cache_entries() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(56), 4);
    let cached = cached_frame_for_test();
    let ppn = cached.ppn;
    let page = PageIndex::new(1);

    pc.state
        .lock()
        .pages
        .install_if_absent(page, cached)
        .expect("install clean page");

    assert_eq!(pc.resident_pages(), 1);
    let map_pin = page_allocator::acquire_map_pin(ppn).expect("cache keeps frame live");
    drop(map_pin);
    assert_eq!(pc.reclaim_clean_file_pages(1), 1);
    assert_eq!(pc.resident_pages(), 0);
    assert!(page_allocator::acquire_map_pin(ppn).is_err());
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
fn file_page_miss_enters_l4_shadow_queue_before_compat_fetch() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(ReentrantFs::new());
    let pc = Arc::new(file_page_container(
        fs.clone(),
        fs.clone(),
        FsObjectId::new(60),
        4,
    ));
    fs.set_page_container(&pc);

    match pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard) {
        V3Out::Done(page) => assert!(page.newly_installed),
        other => panic!("unexpected materialize outcome: {other:?}"),
    }

    assert_eq!(
        fs.l4_pending_before_reentry.load(Ordering::Acquire),
        1,
        "owner miss must be admitted to the staged L4 queue before compat backend fetch"
    );
    assert_eq!(
        fs.l4_pending_after_reentry.load(Ordering::Acquire),
        1,
        "reentrant same-page miss must join the existing L4 request"
    );
    assert_eq!(
        fs.l4_waiters_after_reentry.load(Ordering::Acquire),
        1,
        "reentrant same-page miss must register its wait source with the staged L4 service"
    );
    assert_ne!(
        fs.l4_generation_hint.load(Ordering::Acquire),
        0,
        "staged L4 request must carry a generation hint"
    );
    assert_eq!(
        pc.file_io_request_count_for_test(),
        0,
        "compat executor must retire the staged L4 request after direct fetch completion"
    );
    assert_eq!(fs.fetches.load(Ordering::Acquire), 1);
}

#[test]
fn file_page_retire_notifies_only_when_l4_route_has_waiter() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    setup_host_substrate();
    let fs = Arc::new(RecordingFs::new());
    let pc = file_page_container(fs.clone(), fs, FsObjectId::new(61), 4);
    let page = PageIndex::new(0);

    let wait = crate::page_backed::notification::new_page_ready_wait();
    let source_id = crate::page_backed::notification::page_ready_source_id(&wait);
    let mut stale_legacy_fetch = FilePageFetch::new(FilePageFetchId(99));
    stale_legacy_fetch.source_id = Some(source_id);
    stale_legacy_fetch.joined = true;

    {
        let mut state = pc.state.lock();
        state.file_page_waits.insert(page, wait);
        assert!(
            PageContainer::retire_file_page_fetch_wait(&mut state, page, stale_legacy_fetch)
                .is_none(),
            "legacy joined/source_id alone must not bypass the L4 service route"
        );
    }

    let mut state = pc.state.lock();
    let request_id = state
        .file_io_service
        .submit(
            pc.io_manager_key(),
            PageIoRange::new(page.as_u64(), 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(1)),
        )
        .expect("staged L4 request");
    state.register_file_io_waiter(Some(request_id), source_id);
    let mut routed_fetch = FilePageFetch::with_l4_request(FilePageFetchId(100), Some(request_id));
    routed_fetch.source_id = Some(source_id);
    routed_fetch.joined = true;

    assert!(
        PageContainer::retire_file_page_fetch_wait(&mut state, page, routed_fetch).is_some(),
        "registered L4 waiter should drive the PageBacked notifier"
    );
    assert_eq!(state.file_io_service.submission_len(), 0);
    assert_eq!(state.file_io_service.waiter_count(request_id), 0);
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
fn pagebacked_user_gift_installs_absent_slot_without_copy() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    let (gift, source_ppn) = user_page_gift_for_test();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let page = PageIndex::new(1);

    assert_eq!(pc.install_user_gift_or_copy(page, gift), Ok(true));

    assert_eq!(pc.lookup(page), Some(source_ppn));
    assert_eq!(pc.resident_pages(), 1);
    assert!(
        pc.page_marks(page)
            .expect("gift-installed page marks")
            .dirty,
        "gifted user contents become dirty PageBacked contents"
    );
}

#[test]
fn pagebacked_user_gift_copies_to_present_slot_and_marks_dirty() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    let (gift, source_ppn) = user_page_gift_for_test();
    let pattern = [0x41, 0x52, 0x63, 0x74, 0x85, 0x96, 0xa7, 0xb8];
    page_allocator::testing::write_frame_bytes_for_test(source_ppn, 128, &pattern);
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let page = PageIndex::new(0);
    let existing = pc
        .materialize_anon(page, MaterializeAccess::Read)
        .expect("existing destination page");
    page_allocator::testing::write_frame_bytes_for_test(existing.ppn, 128, &[0u8; 8]);

    assert_eq!(pc.install_user_gift_or_copy(page, gift), Ok(false));

    let mut observed = [0u8; 8];
    page_allocator::testing::read_frame_bytes_for_test(existing.ppn, 128, &mut observed);
    assert_eq!(observed, pattern);
    assert_eq!(pc.lookup(page), Some(existing.ppn));
    assert!(pc.page_marks(page).expect("copied page marks").dirty);
}

#[test]
fn pagebacked_user_gift_rejects_device_destination() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed epoch test lock");
    let (gift, _source_ppn) = user_page_gift_for_test();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xfeed_0000),
            page_count: 1,
        },
        1,
    );

    assert_eq!(
        pc.install_user_gift_or_copy(PageIndex::new(0), gift),
        Err(PageCacheError::UnsupportedKind)
    );
    assert_eq!(pc.resident_pages(), 0);
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
