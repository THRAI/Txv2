//! PageBacked structure and sparse page-cache publication core.
//!
//! This is the first PageBacked-owned seam toward `PAGE_BACKED_v1.md`.
//! Page cache entries now hold real page-substrate `CachePin` evidence, while
//! VM fault materialization returns `MapPin` evidence for pmap publication.
//! `Frame` is intentionally not a zone entity: frame liveness is represented by
//! typed page-substrate contributors.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;
pub mod notification;

use adapter::step_engine::{
    self as step_engine, AllocError, BitmapPageAllocator, ByteProgress, CachePin, Cap, DeviceFrame,
    MapPin, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity, Weak, ZeroPolicy, Zone,
    ZoneAllocated, ZoneError, page_allocator,
};

use crate::execution::{Errno, Guard};
use crate::fs_iface::{FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget};
use crate::io_manager::backend::{
    BlockPageCompletion, BlockPageRequestTracker, PageFrameRef, dispatch_backend_plan,
};
use crate::io_manager::block::{
    BlockCompletionSource, BlockDispatchExecutor, BlockQueue, BlockServiceDriver, BlockServiceNext,
    BlockTagTable,
};
use crate::io_manager::page::{
    PageContainerKey, PageGeneration, PageIoCompletionKind, PageIoFlags, PageIoOp, PageIoPriority,
    PageIoRange, PageIoRequest, PageIoRequestId, PageIoResult,
    service::{
        PageCompletionRoute, PageService, PageServiceBackendContext, PageServiceBackendDriven,
        PageServiceBackendOutcome, PageServiceBackendSubmitOutcome, PageServiceDrivenWork,
        PageServiceNext, PageServiceTaggedBlockCompletionError, PageServiceTurn, PageServiceWork,
        PageWaitInterest, PageWaiter,
    },
};
use crate::io_manager::runtime::{IoServiceKind, QueueDepth, ServiceBudget, ServiceKick};
use crate::mount::{MountPayloadBackendContext, MountPayloadPin};
use crate::sync::SpinMutex;
use crate::vfs::{FsObjectId, OpenFile};
use tx_hal::{Ppn, UserPtr};
use tx_substrate::page_allocator::OwnedFrame;

mod cross_variant;
mod fs_page_backing;
mod gift;
mod lifecycle;
mod range;
mod reflink;
mod slot;
mod sparse_index;
mod targeted_read;
mod user_buffer;
pub use cross_variant::step_copy_file_range;
pub use fs_page_backing::FsPageBacking;
pub use lifecycle::{FallocateOp, TruncateOp, step_fallocate, step_fsync, step_truncate};
pub use range::{
    PageRange, RangeReservation, RangeReservationError, RangeReservationId, RangeReservationKind,
    RangeReservationTable,
};
pub use reflink::{cow_replace_into_private, install_shared_page};
pub use slot::{
    PageSlot, PageSlotCompletionError, PageSlotFetch, PageSlotFsyncStatus, PageSlotSnapshot,
    PageSlotState,
};
use sparse_index::SparseIndex;
pub use targeted_read::read_exact_at;
pub use user_buffer::{
    ReadToUserOp, WriteFromUserOp, step_read_to_kernel, step_read_to_user, step_write_from_kernel,
    step_write_from_user,
};

#[cfg(test)]
use crate::test_support::EPOCH_TEST_LOCK;

static PAGE_CONTAINER_ZONE: Zone<PageContainer> = Zone::const_new();
static PAGE_CONTAINER_RECLAIM_REGISTRY: SpinMutex<alloc::vec::Vec<Weak<PageContainer>>> =
    SpinMutex::new(alloc::vec::Vec::new());

const PAGE_CACHE_RECLAIM_BATCH: usize = 256;
const PAGE_CACHE_RECLAIM_LOW_WATERMARK: usize = 1024;

unsafe impl ZoneAllocated for PageContainer {
    fn zone() -> &'static Zone<Self> {
        &PAGE_CONTAINER_ZONE
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageIndex(u64);

impl PageIndex {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame {
    ppn: Ppn,
    owned_handoff: bool,
}

impl Frame {
    pub const fn new(ppn: Ppn) -> Self {
        Self {
            ppn,
            owned_handoff: false,
        }
    }

    pub fn from_owned<A: page_allocator::PageAllocator>(owned: OwnedFrame<'_, A>) -> Self {
        let ppn = owned.ppn();
        core::mem::forget(owned);
        Self {
            ppn,
            owned_handoff: true,
        }
    }

    pub const fn ppn(self) -> Ppn {
        self.ppn
    }

    const fn owned_handoff(self) -> bool {
        self.owned_handoff
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageMarks {
    pub dirty: bool,
    pub writeback: bool,
    pub referenced: bool,
    pub no_reclaim: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageCacheMark {
    Dirty,
    Writeback,
    Referenced,
    NoReclaim,
}

pub(crate) struct PageCacheEntry {
    ppn: Ppn,
    pin: PageCachePin,
    marks: PageMarks,
}

impl PageCacheEntry {
    fn new(frame: CachedFrame) -> Self {
        Self {
            ppn: frame.ppn,
            pin: frame.pin,
            marks: PageMarks {
                referenced: true,
                ..PageMarks::new()
            },
        }
    }
}

impl PageCacheEntry {
    fn get_mark(&self, mark: PageCacheMark) -> bool {
        match mark {
            PageCacheMark::Dirty => self.marks.dirty,
            PageCacheMark::Writeback => self.marks.writeback,
            PageCacheMark::Referenced => self.marks.referenced,
            PageCacheMark::NoReclaim => self.marks.no_reclaim,
        }
    }

    fn set_mark(&mut self, mark: PageCacheMark, value: bool) {
        match mark {
            PageCacheMark::Dirty => self.marks.dirty = value,
            PageCacheMark::Writeback => self.marks.writeback = value,
            PageCacheMark::Referenced => self.marks.referenced = value,
            PageCacheMark::NoReclaim => self.marks.no_reclaim = value,
        }
    }
}

impl core::fmt::Debug for PageCacheEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheEntry")
            .field("ppn", &self.ppn)
            .field("pin", &self.pin)
            .field("marks", &self.marks)
            .finish()
    }
}

#[derive(Debug)]
struct CachedFrame {
    ppn: Ppn,
    pin: PageCachePin,
}

#[derive(Debug)]
enum PageCachePin {
    Allocated(CachePin<'static, BitmapPageAllocator<'static>>),
    Device(DeviceFrame),
}

impl PageMarks {
    pub const fn new() -> Self {
        Self {
            dirty: false,
            writeback: false,
            referenced: false,
            no_reclaim: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageCacheError {
    AlreadyPresent { current: Ppn },
    MissingPage,
    MismatchedFrame { current: Ppn },
    OutOfBounds,
    UnsupportedKind,
    Backend(Errno),
    Alloc(AllocError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoBusy {
    Dirty,
    Writeback,
    Fetching,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoAdmissionError {
    UnsupportedKind,
    OutOfBounds,
    Range(RangeReservationError),
    Busy {
        page: PageIndex,
        state: DirectIoBusy,
    },
}

impl From<RangeReservationError> for DirectIoAdmissionError {
    fn from(error: RangeReservationError) -> Self {
        Self::Range(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoCompletionError {
    UnknownReservation(RangeReservationId),
    WrongReservationKind {
        expected: RangeReservationKind,
        actual: RangeReservationKind,
    },
    Backend(Errno),
}

#[derive(Debug, Default)]
pub struct PageCacheIndex {
    pages: BTreeMap<PageIndex, PageCacheEntry>,
}

impl PageCacheIndex {
    pub const fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        <Self as SparseIndex>::len(self)
    }

    pub fn is_empty(&self) -> bool {
        <Self as SparseIndex>::is_empty(self)
    }

    pub fn lookup(&self, page: PageIndex) -> Option<Ppn> {
        self.load(page).map(|entry| entry.ppn)
    }

    pub fn marks(&self, page: PageIndex) -> Option<PageMarks> {
        self.load(page)?;
        Some(PageMarks {
            dirty: self.get_mark(page, PageCacheMark::Dirty),
            writeback: self.get_mark(page, PageCacheMark::Writeback),
            referenced: self.get_mark(page, PageCacheMark::Referenced),
            no_reclaim: self.get_mark(page, PageCacheMark::NoReclaim),
        })
    }

    fn install_if_absent(
        &mut self,
        page: PageIndex,
        frame: CachedFrame,
    ) -> Result<(), PageCacheError> {
        if let Some(entry) = self.load(page) {
            return Err(PageCacheError::AlreadyPresent { current: entry.ppn });
        }

        self.insert(page, PageCacheEntry::new(frame))
    }

    fn install_if_match(
        &mut self,
        page: PageIndex,
        expected: Ppn,
        replacement: Option<CachedFrame>,
    ) -> Result<Option<Ppn>, PageCacheError> {
        let current = self
            .load(page)
            .map(|entry| entry.ppn)
            .ok_or(PageCacheError::MissingPage)?;
        if current != expected {
            return Err(PageCacheError::MismatchedFrame { current });
        }
        let replacement = replacement.map(PageCacheEntry::new);
        self.compare_replace(page, |entry| entry.ppn == expected, replacement)?;
        Ok(Some(current))
    }

    fn mark_dirty(&mut self, page: PageIndex) -> Result<(), PageCacheError> {
        self.set_mark(page, PageCacheMark::Dirty)?;
        self.set_mark(page, PageCacheMark::Referenced)
    }

    fn reclaim_clean_pages(&mut self, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }

        let mut reclaimed = 0usize;
        self.pages.retain(|_, entry| {
            if reclaimed >= budget {
                return true;
            }
            let reclaimable = !entry.get_mark(PageCacheMark::Dirty)
                && !entry.get_mark(PageCacheMark::Writeback)
                && !entry.get_mark(PageCacheMark::NoReclaim);
            if reclaimable {
                reclaimed += 1;
            }
            !reclaimable
        });
        reclaimed
    }

    fn invalidate_clean_range(&mut self, range: PageRange) -> Vec<PageIndex> {
        let invalidated = self
            .pages
            .iter()
            .filter_map(|(page, entry)| {
                (range.contains(*page)
                    && !entry.get_mark(PageCacheMark::Dirty)
                    && !entry.get_mark(PageCacheMark::Writeback))
                .then_some(*page)
            })
            .collect::<Vec<_>>();
        for page in &invalidated {
            let _ = self.pages.remove(page);
        }
        invalidated
    }
}

impl SparseIndex for PageCacheIndex {
    type Key = PageIndex;
    type Entry = PageCacheEntry;
    type Error = PageCacheError;
    type Mark = PageCacheMark;

    fn len(&self) -> usize {
        self.pages.len()
    }

    fn load(&self, key: Self::Key) -> Option<&Self::Entry> {
        self.pages.get(&key)
    }

    fn load_mut(&mut self, key: Self::Key) -> Option<&mut Self::Entry> {
        self.pages.get_mut(&key)
    }

    fn insert(&mut self, key: Self::Key, entry: Self::Entry) -> Result<(), Self::Error> {
        if let Some(current) = self.pages.get(&key) {
            return Err(PageCacheError::AlreadyPresent {
                current: current.ppn,
            });
        }
        self.pages.insert(key, entry);
        Ok(())
    }

    fn compare_replace(
        &mut self,
        key: Self::Key,
        matches: impl FnOnce(&Self::Entry) -> bool,
        replacement: Option<Self::Entry>,
    ) -> Result<Option<Self::Entry>, Self::Error> {
        let Some(current) = self.pages.get(&key) else {
            return Err(PageCacheError::MissingPage);
        };
        if !matches(current) {
            return Err(PageCacheError::MismatchedFrame {
                current: current.ppn,
            });
        }
        Ok(match replacement {
            Some(entry) => self.pages.insert(key, entry),
            None => self.erase(key),
        })
    }

    fn erase(&mut self, key: Self::Key) -> Option<Self::Entry> {
        self.pages.remove(&key)
    }

    fn erase_from(&mut self, first: Self::Key) {
        drop(self.pages.split_off(&first));
    }

    fn get_mark(&self, key: Self::Key, mark: Self::Mark) -> bool {
        self.load(key)
            .map(|entry| entry.get_mark(mark))
            .unwrap_or(false)
    }

    fn set_mark(&mut self, key: Self::Key, mark: Self::Mark) -> Result<(), Self::Error> {
        let Some(entry) = self.load_mut(key) else {
            return Err(PageCacheError::MissingPage);
        };
        entry.set_mark(mark, true);
        Ok(())
    }

    fn clear_mark(&mut self, key: Self::Key, mark: Self::Mark) -> Result<(), Self::Error> {
        let Some(entry) = self.load_mut(key) else {
            return Err(PageCacheError::MissingPage);
        };
        entry.set_mark(mark, false);
        Ok(())
    }

    fn marked(&self, mark: Self::Mark) -> bool {
        self.pages.values().any(|entry| entry.get_mark(mark))
    }

    fn collect_marked(&self, mark: Self::Mark) -> Vec<(Self::Key, &Self::Entry)> {
        self.pages
            .iter()
            .filter_map(|(key, entry)| entry.get_mark(mark).then_some((*key, entry)))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnonSwapPolicy {
    Reclaimable,
    Persistent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageContainerKind {
    Anon {
        swap_policy: AnonSwapPolicy,
    },
    File {
        mount: MountPayloadPin,
        fs_object_id: FsObjectId,
    },
    Device {
        base_ppn: Ppn,
        page_count: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializeAccess {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageBackedIoKind {
    Read,
    Write,
}

#[derive(Debug)]
pub struct MaterializedPage {
    pub ppn: Ppn,
    pub map_pin: MaterializedPagePin,
    pub newly_installed: bool,
    pub dirty: bool,
}

#[derive(Debug)]
pub struct PageLease {
    ppn: Ppn,
    cache_pin: PageCachePin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileFsyncFrontier(Vec<(PageIndex, PageGeneration)>);

impl FileFsyncFrontier {
    pub fn pages(&self) -> &[(PageIndex, PageGeneration)] {
        &self.0
    }
}

/// One fsync invocation's immutable view of file-page generations.
///
/// The session deliberately captures the frontier once. Re-entering after a
/// yield therefore waits on the same writeback requests instead of treating
/// later writes as part of the original fsync operation.
pub struct FileFsyncSession<'a> {
    pc: &'a PageContainer,
    frontier: FileFsyncFrontier,
}

impl FileFsyncSession<'_> {
    pub fn frontier(&self) -> &FileFsyncFrontier {
        &self.frontier
    }

    /// Submit or observe writeback for the generations captured at entry.
    /// Callers may issue the filesystem durability fence only after `Complete`.
    pub fn advance(&self) -> FileFsyncFrontierAdvance {
        self.pc.advance_file_fsync_frontier(&self.frontier)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileFsyncFrontierAdvance {
    Complete,
    Submitted { pages: u32 },
    Waiting,
    Error(Errno),
}

// PageLease carries page-cache role evidence for an already-live frame.
// Like PageContainer's internal PageCacheEntry pins, the token is an owned
// liveness contribution; moving it between pipe descriptors across harts does
// not create shared mutable access to the frame metadata.
unsafe impl Send for PageLease {}
unsafe impl Sync for PageLease {}

impl PageLease {
    pub const fn ppn(&self) -> Ppn {
        self.ppn
    }

    pub fn retain(&self) -> Result<Self, PageCacheError> {
        let cache_pin =
            page_allocator::acquire_cache_pin(self.ppn).map_err(PageCacheError::Alloc)?;
        Ok(Self {
            ppn: self.ppn,
            cache_pin: PageCachePin::Allocated(cache_pin),
        })
    }

    pub fn confirm(&self) -> Result<(), PageCacheError> {
        match &self.cache_pin {
            PageCachePin::Allocated(pin) if pin.ppn() == self.ppn => Ok(()),
            PageCachePin::Allocated(_) => {
                Err(PageCacheError::MismatchedFrame { current: self.ppn })
            }
            PageCachePin::Device(_) => Err(PageCacheError::UnsupportedKind),
        }
    }
}

#[derive(Debug)]
pub enum MaterializedPagePin {
    Allocated(MapPin<'static, BitmapPageAllocator<'static>>),
    Device(DeviceFrame),
}

struct MaterializedPageSnapshot {
    ppn: Ppn,
    pin: MaterializedPageSnapshotPin,
    newly_installed: bool,
    dirty: bool,
}

enum MaterializedPageSnapshotPin {
    Allocated(CachePin<'static, BitmapPageAllocator<'static>>),
    Device(DeviceFrame),
}

impl MaterializedPageSnapshot {
    fn into_materialized(self) -> Result<MaterializedPage, PageCacheError> {
        let map_pin = match self.pin {
            MaterializedPageSnapshotPin::Allocated(cache_pin) => {
                debug_assert_eq!(cache_pin.ppn(), self.ppn);
                let map_pin = acquire_map_pin_for_materialization(self.ppn)?;
                drop(cache_pin);
                MaterializedPagePin::Allocated(map_pin)
            }
            MaterializedPageSnapshotPin::Device(device) => MaterializedPagePin::Device(device),
        };
        Ok(MaterializedPage {
            ppn: self.ppn,
            map_pin,
            newly_installed: self.newly_installed,
            dirty: self.dirty,
        })
    }
}

#[derive(Debug)]
pub struct PageContainer {
    kind: PageContainerKind,
    page_count: u64,
    size_bytes: AtomicU64,
    state: PageContainerStateCell,
}

// `PageCacheIndex` (inside `PageContainerState`) is a `BTreeMap<PageIndex,
// PageCacheEntry>` whose values carry an internal `*const ()` cache pin
// for fast page-table dereferences. The pointer is treated as borrow-style
// evidence covered by the surrounding `SpinMutex`. Like `AddressSpace`,
// `PageContainer` is a zone-allocated entity whose `Cap` is meant to be
// shareable across hart boundaries; the pointer-shaped internal state
// does not preclude that.
unsafe impl Send for PageContainer {}
unsafe impl Sync for PageContainer {}

#[derive(Debug)]
struct PageContainerState {
    pages: PageCacheIndex,
    file_page_slots: BTreeMap<PageIndex, PageSlot>,
    in_flight_file_pages: BTreeMap<PageIndex, FilePageFetch>,
    file_io_service: PageService,
    file_io_leases: BTreeMap<PageIoRequestId, PageLease>,
    file_io_read_targets: BTreeMap<PageIoRequestId, CachedFrame>,
    file_block_runtime: FileIoBlockRuntime,
    range_reservations: RangeReservationTable,
    // Page-scoped retry sources are retained so a task that already received
    // `Yield` can still register and consume a pending wake before it retries
    // and re-observes page state.
    file_page_waits: BTreeMap<PageIndex, notification::PageReadyWait>,
    next_file_fetch_id: u64,
}

#[derive(Debug)]
struct FileIoBlockRuntime {
    queue: BlockQueue,
    tracker: BlockPageRequestTracker,
    depth: QueueDepth,
    tags: BlockTagTable,
}

impl FileIoBlockRuntime {
    fn new(max_pending: usize, queue_depth: usize) -> Self {
        Self {
            queue: BlockQueue::new(max_pending),
            tracker: BlockPageRequestTracker::new(),
            depth: QueueDepth::new(queue_depth),
            tags: BlockTagTable::new(),
        }
    }
}

enum FileBlockSubmissionTarget<'a> {
    External {
        block_queue: &'a mut BlockQueue,
        tracker: Option<&'a mut BlockPageRequestTracker>,
    },
    Owned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileBlockServiceTurn {
    pub dispatched: usize,
    pub device_completions: usize,
    pub page_completions: usize,
    pub next: BlockServiceNext,
    pub kicks: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FilePageFetchId(u64);

#[derive(Debug)]
struct FilePageFetch {
    id: FilePageFetchId,
    generation: PageGeneration,
    request_id: Option<PageIoRequestId>,
    source_id: Option<u64>,
    joined: bool,
}

impl FilePageFetch {
    #[cfg(test)]
    const fn new(id: FilePageFetchId) -> Self {
        Self {
            id,
            generation: PageGeneration::new(id.0),
            request_id: None,
            source_id: None,
            joined: false,
        }
    }

    #[cfg(test)]
    const fn with_l4_request(id: FilePageFetchId, request_id: Option<PageIoRequestId>) -> Self {
        Self::with_l4_request_generation(id, PageGeneration::new(id.0), request_id)
    }

    const fn with_l4_request_generation(
        id: FilePageFetchId,
        generation: PageGeneration,
        request_id: Option<PageIoRequestId>,
    ) -> Self {
        Self {
            id,
            generation,
            request_id,
            source_id: None,
            joined: false,
        }
    }
}

enum FilePageFetchStart {
    Cached(Result<MaterializedPage, PageCacheError>),
    Joined(Arc<tx_substrate::wake::WaitSource>),
    Owner(FilePageFetchId),
}

impl PageContainerState {
    fn allocate_file_fetch_id(&mut self) -> FilePageFetchId {
        let id = self.next_file_fetch_id;
        self.next_file_fetch_id = self.next_file_fetch_id.wrapping_add(1).max(1);
        FilePageFetchId(id)
    }

    fn register_file_io_waiter(&mut self, request_id: Option<PageIoRequestId>, source_id: u64) {
        let Some(request_id) = request_id else {
            return;
        };
        let _ = self.file_io_service.wait_on(
            request_id,
            PageWaiter {
                source_id,
                interests: PageWaitInterest::READY,
            },
        );
    }
}

struct PageContainerStateCell {
    inner: SpinMutex<PageContainerState>,
}

impl PageContainerStateCell {
    const fn new(state: PageContainerState) -> Self {
        Self {
            inner: SpinMutex::new(state),
        }
    }

    fn lock(&self) -> PageContainerStateGuard<'_> {
        let inner = self.inner.lock();
        PageContainerStateGuard {
            #[cfg(test)]
            _mark: PageContainerStateLockMark::enter(),
            inner,
        }
    }
}

impl core::fmt::Debug for PageContainerStateCell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("PageContainerStateCell")
            .field(&*self.lock())
            .finish()
    }
}

struct PageContainerStateGuard<'a> {
    #[cfg(test)]
    _mark: PageContainerStateLockMark,
    inner: tx_substrate::SpinMutexGuard<'a, PageContainerState>,
}

impl core::ops::Deref for PageContainerStateGuard<'_> {
    type Target = PageContainerState;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for PageContainerStateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
std::thread_local! {
    static PAGE_CONTAINER_STATE_LOCK_DEPTH_FOR_TEST: core::cell::Cell<usize> =
        core::cell::Cell::new(0);
}

#[cfg(test)]
struct PageContainerStateLockMark;

#[cfg(test)]
impl PageContainerStateLockMark {
    fn enter() -> Self {
        PAGE_CONTAINER_STATE_LOCK_DEPTH_FOR_TEST.with(|depth| {
            depth.set(depth.get() + 1);
        });
        Self
    }
}

#[cfg(test)]
impl Drop for PageContainerStateLockMark {
    fn drop(&mut self) {
        PAGE_CONTAINER_STATE_LOCK_DEPTH_FOR_TEST.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

#[cfg(test)]
static FRAME_ALLOC_UNDER_STATE_LOCK_FOR_TEST: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static MAP_PIN_UNDER_STATE_LOCK_FOR_TEST: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn page_container_state_lock_held_for_test() -> bool {
    PAGE_CONTAINER_STATE_LOCK_DEPTH_FOR_TEST.with(|depth| depth.get() != 0)
}

#[cfg(test)]
fn reset_page_container_lock_service_observations_for_test() {
    FRAME_ALLOC_UNDER_STATE_LOCK_FOR_TEST.store(0, Ordering::Release);
    MAP_PIN_UNDER_STATE_LOCK_FOR_TEST.store(0, Ordering::Release);
}

#[cfg(test)]
fn page_container_lock_service_observations_for_test() -> (usize, usize) {
    (
        FRAME_ALLOC_UNDER_STATE_LOCK_FOR_TEST.load(Ordering::Acquire),
        MAP_PIN_UNDER_STATE_LOCK_FOR_TEST.load(Ordering::Acquire),
    )
}

#[cfg(test)]
fn record_frame_alloc_for_test() {
    if page_container_state_lock_held_for_test() {
        FRAME_ALLOC_UNDER_STATE_LOCK_FOR_TEST.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
fn record_map_pin_for_test() {
    if page_container_state_lock_held_for_test() {
        MAP_PIN_UNDER_STATE_LOCK_FOR_TEST.fetch_add(1, Ordering::AcqRel);
    }
}

impl PageContainer {
    pub fn new(kind: PageContainerKind, page_count: u64) -> Self {
        let capacity = page_count.saturating_mul(crate::vm::USER_PAGE_SIZE as u64);
        Self {
            kind,
            page_count,
            size_bytes: AtomicU64::new(capacity),
            state: PageContainerStateCell::new(PageContainerState {
                pages: PageCacheIndex::new(),
                file_page_slots: BTreeMap::new(),
                in_flight_file_pages: BTreeMap::new(),
                file_io_service: PageService::new(1024),
                file_io_leases: BTreeMap::new(),
                file_io_read_targets: BTreeMap::new(),
                file_block_runtime: FileIoBlockRuntime::new(1024, 16),
                range_reservations: RangeReservationTable::new(),
                file_page_waits: BTreeMap::new(),
                next_file_fetch_id: 1,
            }),
        }
    }

    pub fn new_cap(
        kind: PageContainerKind,
        page_count: u64,
    ) -> Result<Cap<PageContainer>, ZoneError> {
        step_engine::sign(Self::new(kind, page_count))
    }

    pub fn new_file_cap(
        mount: MountPayloadPin,
        fs_object_id: FsObjectId,
        size_bytes: u64,
    ) -> Result<Cap<PageContainer>, ZoneError> {
        let page_size = crate::vm::USER_PAGE_SIZE as u64;
        let page_count = if size_bytes == 0 {
            0
        } else {
            1 + (size_bytes - 1) / page_size
        };
        let container = Self::new_cap(
            PageContainerKind::File {
                mount,
                fs_object_id,
            },
            page_count,
        )?;
        container.set_size_bytes(size_bytes);
        PAGE_CONTAINER_RECLAIM_REGISTRY
            .lock()
            .push(container.downgrade());
        Ok(container)
    }

    pub const fn kind(&self) -> &PageContainerKind {
        &self.kind
    }

    pub const fn page_count(&self) -> u64 {
        self.page_count
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes.load(Ordering::Acquire)
    }

    pub fn resident_pages(&self) -> usize {
        self.state.lock().pages.len()
    }

    fn io_manager_key(&self) -> PageContainerKey {
        PageContainerKey::new(self as *const Self as usize as u64)
    }

    pub fn file_backend_context(&self) -> Option<MountPayloadBackendContext<'_>> {
        let PageContainerKind::File {
            mount,
            fs_object_id,
        } = &self.kind
        else {
            return None;
        };
        Some(MountPayloadBackendContext::new(
            mount.payload(),
            FsObjectKey::new(fs_object_id.as_u64()),
        ))
    }

    /// Reserve an idle range for a direct write. The owner keeps the returned
    /// token until the corresponding direct-I/O completion releases it.
    pub fn begin_file_direct_write(
        &self,
        range: PageRange,
    ) -> Result<RangeReservation, DirectIoAdmissionError> {
        self.begin_file_direct_io(range, RangeReservationKind::DirectWrite)
    }

    /// Reserve an idle range for a direct read. Direct reads preserve clean
    /// cached data after completion but still exclude overlapping buffered I/O.
    pub fn begin_file_direct_read(
        &self,
        range: PageRange,
    ) -> Result<RangeReservation, DirectIoAdmissionError> {
        self.begin_file_direct_io(range, RangeReservationKind::DirectRead)
    }

    /// Complete a direct write and conservatively invalidate overlapped clean
    /// page-cache entries before releasing the direct-write reservation.
    pub fn complete_file_direct_write(
        &self,
        reservation: RangeReservation,
        result: Result<(), Errno>,
    ) -> Result<usize, DirectIoCompletionError> {
        if reservation.kind() != RangeReservationKind::DirectWrite {
            return Err(DirectIoCompletionError::WrongReservationKind {
                expected: RangeReservationKind::DirectWrite,
                actual: reservation.kind(),
            });
        }
        let mut state = self.state.lock();
        if !state.range_reservations.release(reservation.id()) {
            return Err(DirectIoCompletionError::UnknownReservation(
                reservation.id(),
            ));
        }
        if let Err(errno) = result {
            return Err(DirectIoCompletionError::Backend(errno));
        }
        let invalidated = state.pages.invalidate_clean_range(reservation.range());
        for page in &invalidated {
            if let Some(slot) = state.file_page_slots.get(page) {
                slot.invalidate();
            }
        }
        Ok(invalidated.len())
    }

    /// Release a direct-read reservation after its device result. The default
    /// coherency policy preserves clean resident cache pages for direct reads.
    pub fn complete_file_direct_read(
        &self,
        reservation: RangeReservation,
        result: Result<(), Errno>,
    ) -> Result<(), DirectIoCompletionError> {
        if reservation.kind() != RangeReservationKind::DirectRead {
            return Err(DirectIoCompletionError::WrongReservationKind {
                expected: RangeReservationKind::DirectRead,
                actual: reservation.kind(),
            });
        }
        let mut state = self.state.lock();
        if !state.range_reservations.release(reservation.id()) {
            return Err(DirectIoCompletionError::UnknownReservation(
                reservation.id(),
            ));
        }
        result.map_err(DirectIoCompletionError::Backend)
    }

    fn begin_file_direct_io(
        &self,
        range: PageRange,
        kind: RangeReservationKind,
    ) -> Result<RangeReservation, DirectIoAdmissionError> {
        if !matches!(self.kind, PageContainerKind::File { .. }) {
            return Err(DirectIoAdmissionError::UnsupportedKind);
        }
        let Some(end) = range.end() else {
            return Err(DirectIoAdmissionError::Range(
                RangeReservationError::Overflow,
            ));
        };
        if end.as_u64() > self.page_count {
            return Err(DirectIoAdmissionError::OutOfBounds);
        }

        let mut state = self.state.lock();
        for (page, entry) in &state.pages.pages {
            if !range.contains(*page) {
                continue;
            }
            if entry.get_mark(PageCacheMark::Writeback) {
                return Err(DirectIoAdmissionError::Busy {
                    page: *page,
                    state: DirectIoBusy::Writeback,
                });
            }
            if entry.get_mark(PageCacheMark::Dirty) {
                return Err(DirectIoAdmissionError::Busy {
                    page: *page,
                    state: DirectIoBusy::Dirty,
                });
            }
        }
        for (page, slot) in &state.file_page_slots {
            if !range.contains(*page) {
                continue;
            }
            let busy = match slot.snapshot().state {
                PageSlotState::Dirty { .. } => Some(DirectIoBusy::Dirty),
                PageSlotState::Writeback { .. } => Some(DirectIoBusy::Writeback),
                PageSlotState::Fetching => Some(DirectIoBusy::Fetching),
                PageSlotState::Empty
                | PageSlotState::Resident { .. }
                | PageSlotState::Error { .. } => None,
            };
            if let Some(state) = busy {
                return Err(DirectIoAdmissionError::Busy { page: *page, state });
            }
        }
        if let Some(page) = state
            .in_flight_file_pages
            .keys()
            .copied()
            .find(|page| range.contains(*page))
        {
            return Err(DirectIoAdmissionError::Busy {
                page,
                state: DirectIoBusy::Fetching,
            });
        }
        state
            .range_reservations
            .try_reserve(range, kind)
            .map_err(Into::into)
    }

    /// Move one dirty file page into the L4 writeback queue.
    ///
    /// This is only admission: the backend planner and L6 executor own later
    /// submission and completion. A failed admission restores the slot to
    /// `Dirty`, so no request is left falsely in flight.
    pub fn queue_file_page_writeback(&self, page: PageIndex) -> Option<PageIoRequestId> {
        if !matches!(self.kind, PageContainerKind::File { .. }) {
            return None;
        }

        let mut state = self.state.lock();
        let writeback = state.file_page_slots.get(&page)?.begin_writeback().ok()?;
        if state
            .pages
            .set_mark(page, PageCacheMark::Writeback)
            .is_err()
        {
            if let Some(slot) = state.file_page_slots.get(&page) {
                let _ = slot.abort_writeback(writeback.generation);
            }
            return None;
        }
        match state.file_io_service.submit(
            self.io_manager_key(),
            PageIoRange::new(page.as_u64(), 1),
            PageIoOp::Writeback,
            PageIoPriority::BackgroundWriteback,
            PageIoFlags::WRITEBACK,
            Some(writeback.generation),
        ) {
            Ok(id) => Some(id),
            Err(_) => {
                let _ = state.pages.clear_mark(page, PageCacheMark::Writeback);
                if let Some(slot) = state.file_page_slots.get(&page) {
                    let _ = slot.abort_writeback(writeback.generation);
                }
                None
            }
        }
    }

    pub fn snapshot_file_fsync_frontier(&self) -> Option<FileFsyncFrontier> {
        if !matches!(self.kind, PageContainerKind::File { .. }) {
            return None;
        }
        let state = self.state.lock();
        let mut pages = Vec::new();
        for (page, slot) in &state.file_page_slots {
            let snapshot = slot.snapshot();
            if matches!(
                snapshot.state,
                PageSlotState::Dirty { .. } | PageSlotState::Writeback { .. }
            ) {
                pages.push((*page, snapshot.generation));
            }
        }
        Some(FileFsyncFrontier(pages))
    }

    /// Start an fsync session with a stable dirty/writeback generation frontier.
    pub fn begin_file_fsync_session(&self) -> Option<FileFsyncSession<'_>> {
        self.snapshot_file_fsync_frontier()
            .map(|frontier| FileFsyncSession { pc: self, frontier })
    }

    pub fn advance_file_fsync_frontier(
        &self,
        frontier: &FileFsyncFrontier,
    ) -> FileFsyncFrontierAdvance {
        let mut submitted = 0u32;
        let mut waiting = false;
        for &(page, generation) in frontier.pages() {
            let status = self
                .state
                .lock()
                .file_page_slots
                .get(&page)
                .map(|slot| slot.fsync_status(generation));
            match status {
                Some(PageSlotFsyncStatus::NeedsWriteback { .. }) => {
                    if self.queue_file_page_writeback(page).is_some() {
                        submitted = submitted.saturating_add(1);
                    } else {
                        waiting = true;
                    }
                }
                Some(
                    PageSlotFsyncStatus::WaitingForWriteback { .. }
                    | PageSlotFsyncStatus::WaitingForEarlierWriteback { .. },
                ) => waiting = true,
                Some(PageSlotFsyncStatus::Error { errno }) => {
                    return FileFsyncFrontierAdvance::Error(errno);
                }
                Some(PageSlotFsyncStatus::Clean) | None => {}
            }
        }
        if submitted != 0 {
            FileFsyncFrontierAdvance::Submitted { pages: submitted }
        } else if waiting {
            FileFsyncFrontierAdvance::Waiting
        } else {
            FileFsyncFrontierAdvance::Complete
        }
    }

    pub fn drive_file_io_service_once<F>(
        &self,
        budget: ServiceBudget,
        block_queue: &mut BlockQueue,
        kick: F,
    ) -> Option<PageServiceBackendDriven>
    where
        F: FnMut(ServiceKick) -> bool,
    {
        self.drive_file_io_service_once_inner(
            budget,
            FileBlockSubmissionTarget::External {
                block_queue,
                tracker: None,
            },
            kick,
        )
    }

    pub fn drive_file_io_service_once_with_tracker<F>(
        &self,
        budget: ServiceBudget,
        block_queue: &mut BlockQueue,
        tracker: &mut BlockPageRequestTracker,
        kick: F,
    ) -> Option<PageServiceBackendDriven>
    where
        F: FnMut(ServiceKick) -> bool,
    {
        self.drive_file_io_service_once_inner(
            budget,
            FileBlockSubmissionTarget::External {
                block_queue,
                tracker: Some(tracker),
            },
            kick,
        )
    }

    pub fn drive_file_io_service_once_owned<F>(
        &self,
        budget: ServiceBudget,
        kick: F,
    ) -> Option<PageServiceBackendDriven>
    where
        F: FnMut(ServiceKick) -> bool,
    {
        self.drive_file_io_service_once_inner(budget, FileBlockSubmissionTarget::Owned, kick)
    }

    fn drive_file_io_service_once_inner<F>(
        &self,
        budget: ServiceBudget,
        mut block_target: FileBlockSubmissionTarget<'_>,
        mut kick: F,
    ) -> Option<PageServiceBackendDriven>
    where
        F: FnMut(ServiceKick) -> bool,
    {
        let context = self.file_backend_context()?;
        let step = {
            let mut state = self.state.lock();
            state.file_io_service.drive_turn(budget)
        };
        let mut work = Vec::new();

        if let PageServiceTurn::Work(items) = step.turn {
            for item in items {
                match item {
                    PageServiceWork::Completion(route) => {
                        let _ = self.apply_file_io_completion_route(&route);
                        work.push(PageServiceDrivenWork::Completion(route));
                    }
                    PageServiceWork::Submission(request) => {
                        let rollback_request = request.clone();
                        let source = self.file_io_source_for_submission(&request);
                        let target = self.file_io_target_for_submission(&request);
                        let Some(plan) = context.plan_submission_with_source_and_target(
                            request.clone(),
                            source,
                            target,
                        ) else {
                            self.abort_file_writeback_submission(&request);
                            self.release_file_io_read_target(&request);
                            work.push(PageServiceDrivenWork::UnplannedSubmission(request));
                            continue;
                        };
                        let dispatch = dispatch_backend_plan(plan);
                        let outcome = {
                            let mut state = self.state.lock();
                            state.file_io_service.consume_backend_dispatch(dispatch)
                        };
                        let queued = match &mut block_target {
                            FileBlockSubmissionTarget::External {
                                block_queue,
                                tracker,
                            } => {
                                // The compatibility caller cannot route tagged completions into
                                // this PageContainer's private graph registry.
                                let queued = match outcome {
                                    PageServiceBackendOutcome::BlockGraph(_) => {
                                        Ok(PageServiceBackendSubmitOutcome::Err(Errno::ENOSYS))
                                    }
                                    outcome => {
                                        self.state.lock().file_io_service.queue_backend_outcome(
                                            outcome,
                                            block_queue,
                                            request.clone(),
                                        )
                                    }
                                };
                                if let (Ok(outcome), Some(tracker)) = (&queued, tracker.as_mut()) {
                                    record_file_service_block_submissions(tracker, outcome);
                                }
                                queued
                            }
                            FileBlockSubmissionTarget::Owned => {
                                let mut state = self.state.lock();
                                let PageContainerState {
                                    file_io_service,
                                    file_block_runtime,
                                    ..
                                } = &mut *state;
                                let queued = file_io_service.queue_backend_outcome(
                                    outcome,
                                    &mut file_block_runtime.queue,
                                    request.clone(),
                                );
                                if let Ok(outcome) = &queued {
                                    record_file_service_block_submissions(
                                        &mut file_block_runtime.tracker,
                                        outcome,
                                    );
                                }
                                queued
                            }
                        };
                        if let Ok(outcome) = &queued {
                            self.register_file_service_metadata(&request, outcome);
                        }
                        match queued {
                            Ok(outcome) => {
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome));
                            }
                            Err(error) => {
                                self.abort_file_writeback_submission(&rollback_request);
                                self.release_file_io_read_target(&rollback_request);
                                work.push(PageServiceDrivenWork::BackendSubmitError(error));
                            }
                        }
                    }
                    PageServiceWork::BackendResume {
                        page_request,
                        resume,
                    } => {
                        let Some(plan) = context.resume_submission(resume) else {
                            self.abort_file_writeback_submission(&page_request);
                            self.release_file_io_read_target(&page_request);
                            work.push(PageServiceDrivenWork::UnplannedSubmission(page_request));
                            continue;
                        };
                        let dispatch = dispatch_backend_plan(plan);
                        let outcome = {
                            let mut state = self.state.lock();
                            state.file_io_service.consume_backend_dispatch(dispatch)
                        };
                        let queued = match &mut block_target {
                            FileBlockSubmissionTarget::External {
                                block_queue,
                                tracker,
                            } => {
                                // The compatibility caller cannot route tagged completions into
                                // this PageContainer's private graph registry.
                                let queued = match outcome {
                                    PageServiceBackendOutcome::BlockGraph(_) => {
                                        Ok(PageServiceBackendSubmitOutcome::Err(Errno::ENOSYS))
                                    }
                                    outcome => {
                                        self.state.lock().file_io_service.queue_backend_outcome(
                                            outcome,
                                            block_queue,
                                            page_request.clone(),
                                        )
                                    }
                                };
                                if let (Ok(outcome), Some(tracker)) = (&queued, tracker.as_mut()) {
                                    record_file_service_block_submissions(tracker, outcome);
                                }
                                queued
                            }
                            FileBlockSubmissionTarget::Owned => {
                                let mut state = self.state.lock();
                                let PageContainerState {
                                    file_io_service,
                                    file_block_runtime,
                                    ..
                                } = &mut *state;
                                let queued = file_io_service.queue_backend_outcome(
                                    outcome,
                                    &mut file_block_runtime.queue,
                                    page_request.clone(),
                                );
                                if let Ok(outcome) = &queued {
                                    record_file_service_block_submissions(
                                        &mut file_block_runtime.tracker,
                                        outcome,
                                    );
                                }
                                queued
                            }
                        };
                        if let Ok(outcome) = &queued {
                            self.register_file_service_metadata(&page_request, outcome);
                        }
                        match queued {
                            Ok(outcome) => {
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome))
                            }
                            Err(error) => {
                                work.push(PageServiceDrivenWork::BackendSubmitError(error))
                            }
                        }
                    }
                }
            }
        }

        let next = {
            let mut state = self.state.lock();
            state.file_io_service.drive_turn(ServiceBudget::new(0)).next
        };
        let kicks = if next == PageServiceNext::Runnable {
            usize::from(kick(ServiceKick::new(IoServiceKind::Page)))
        } else {
            0
        };

        Some(PageServiceBackendDriven { work, next, kicks })
    }

    pub fn drive_file_block_io_service_once<E, R, F>(
        &self,
        budget: ServiceBudget,
        executor: &mut E,
        mut frame_for: R,
        mut kick: F,
    ) -> Result<FileBlockServiceTurn, PageServiceTaggedBlockCompletionError>
    where
        E: BlockDispatchExecutor + BlockCompletionSource + ?Sized,
        R: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
        F: FnMut(ServiceKick) -> bool,
    {
        let mut driver = BlockServiceDriver::new(budget);
        let driven = {
            let mut state = self.state.lock();
            let runtime = &mut state.file_block_runtime;
            driver.drive_once(
                &mut runtime.queue,
                &mut runtime.depth,
                &mut runtime.tags,
                &mut kick,
            )
        };

        for dispatch in &driven.step.dispatches {
            executor.submit(dispatch);
        }

        let mut device_completions = 0usize;
        let mut page_completions = 0usize;
        let mut kicks = driven.kicks;
        let mut next = driven.step.next;
        while let Some(completion) = executor.poll_completion() {
            device_completions += 1;
            let outcome = {
                let mut state = self.state.lock();
                let PageContainerState {
                    file_io_service,
                    file_block_runtime,
                    ..
                } = &mut *state;
                file_io_service.push_tagged_block_completion_with_graphs(
                    &mut file_block_runtime.tags,
                    &mut file_block_runtime.depth,
                    &mut file_block_runtime.tracker,
                    &mut file_block_runtime.queue,
                    completion.tag,
                    completion.result,
                    &mut frame_for,
                )?
            };
            page_completions += outcome.queued;
            if outcome.wake.is_some() {
                kicks += usize::from(kick(ServiceKick::new(IoServiceKind::Page)));
            }
            if outcome.block_submitted != 0 {
                next = BlockServiceNext::Runnable;
                kicks += usize::from(kick(ServiceKick::new(IoServiceKind::Block)));
            }
        }

        Ok(FileBlockServiceTurn {
            dispatched: driven.step.dispatches.len(),
            device_completions,
            page_completions,
            next,
            kicks,
        })
    }

    fn apply_file_io_completion_route(
        &self,
        route: &PageCompletionRoute,
    ) -> Option<Result<PageSlotSnapshot, PageSlotCompletionError>> {
        if route.completion.range.page_count() != 1 {
            return None;
        }
        let page = PageIndex::new(route.completion.range.start_page());
        if route.completion.kind == PageIoCompletionKind::WritebackFinished {
            let mut state = self.state.lock();
            let lease = state.file_io_leases.remove(&route.completion.id);
            let slot = state.file_page_slots.get(&page)?;
            let result = slot.complete_writeback(
                route.completion.generation,
                match route.completion.result {
                    PageIoResult::Done => Ok(()),
                    PageIoResult::Err(errno) => Err(errno),
                },
            );
            if let Ok(snapshot) = result {
                let _ = state.pages.clear_mark(page, PageCacheMark::Writeback);
                if matches!(snapshot.state, PageSlotState::Resident { .. }) {
                    let _ = state.pages.clear_mark(page, PageCacheMark::Dirty);
                }
            }
            drop(lease);
            return Some(result);
        }
        if route.completion.kind != PageIoCompletionKind::ReadInstalled {
            return None;
        }
        let target = self
            .state
            .lock()
            .file_io_read_targets
            .remove(&route.completion.id);
        let PageIoResult::Err(errno) = route.completion.result else {
            return match (route.frame, target) {
                (Some(frame), Some(target)) if target.ppn == frame.ppn() => {
                    let result = self.apply_file_io_cached_read_completion(
                        page,
                        route.completion.generation,
                        target,
                    );
                    if result.is_ok() {
                        self.finish_file_page_fetch_after_service_completion(
                            page,
                            route.completion.generation,
                            !route.waiters.is_empty(),
                        );
                    }
                    Some(result)
                }
                (Some(frame), target) => {
                    drop(target);
                    let result = self.apply_file_io_read_frame_completion(
                        page,
                        route.completion.generation,
                        frame,
                    );
                    if result.is_ok() {
                        self.finish_file_page_fetch_after_service_completion(
                            page,
                            route.completion.generation,
                            !route.waiters.is_empty(),
                        );
                    }
                    Some(result)
                }
                (None, Some(target)) => {
                    let result = self.apply_file_io_cached_read_completion(
                        page,
                        route.completion.generation,
                        target,
                    );
                    if result.is_ok() {
                        self.finish_file_page_fetch_after_service_completion(
                            page,
                            route.completion.generation,
                            !route.waiters.is_empty(),
                        );
                    }
                    Some(result)
                }
                (None, None) => None,
            };
        };
        let result = {
            let state = self.state.lock();
            let slot = state.file_page_slots.get(&page)?;
            slot.complete_fetch(route.completion.generation, Err(errno))
        };
        if result.is_ok() {
            self.finish_file_page_fetch_after_service_completion(
                page,
                route.completion.generation,
                !route.waiters.is_empty(),
            );
        }
        Some(result)
    }

    fn register_file_service_metadata(
        &self,
        page_request: &PageIoRequest,
        outcome: &PageServiceBackendSubmitOutcome,
    ) {
        let PageServiceBackendSubmitOutcome::MetadataFirstQueued {
            backend_request,
            submitted,
            resume,
            ..
        } = outcome
        else {
            return;
        };
        self.state
            .lock()
            .file_io_service
            .register_metadata_submission(
                page_request.clone(),
                backend_request.clone(),
                *resume,
                submitted,
            );
    }

    fn file_io_source_for_submission(&self, request: &PageIoRequest) -> IoDataSource {
        if request.op != PageIoOp::Writeback || request.range.page_count() != 1 {
            return IoDataSource::None;
        }
        let page = PageIndex::new(request.range.start_page());
        let Some(ppn) = self.state.lock().pages.load(page).map(|entry| entry.ppn) else {
            return IoDataSource::None;
        };
        let Ok(cache_pin) = page_allocator::acquire_cache_pin(ppn) else {
            return IoDataSource::None;
        };
        let lease = PageLease {
            ppn,
            cache_pin: PageCachePin::Allocated(cache_pin),
        };
        self.state.lock().file_io_leases.insert(request.id, lease);
        IoDataSource::page_cache(
            IoDataLeaseId::new(request.id.raw()),
            PageFrameRef::new(ppn),
            0,
            crate::vm::USER_PAGE_SIZE as u32,
        )
    }

    fn file_io_target_for_submission(&self, request: &PageIoRequest) -> IoDataTarget {
        if request.op != PageIoOp::Read || request.range.page_count() != 1 {
            return IoDataTarget::None;
        }
        if !self
            .state
            .lock()
            .in_flight_file_pages
            .values()
            .any(|fetch| fetch.request_id == Some(request.id))
        {
            return IoDataTarget::None;
        }
        let Ok(target) = allocate_cached_frame() else {
            return IoDataTarget::None;
        };
        let frame = PageFrameRef::new(target.ppn);
        let mut state = self.state.lock();
        if !state
            .in_flight_file_pages
            .values()
            .any(|fetch| fetch.request_id == Some(request.id))
        {
            return IoDataTarget::None;
        }
        state.file_io_read_targets.insert(request.id, target);
        IoDataTarget::page_cache(
            IoDataLeaseId::new(request.id.raw()),
            frame,
            0,
            crate::vm::USER_PAGE_SIZE as u32,
        )
    }

    fn release_file_io_read_target(&self, request: &PageIoRequest) {
        let _ = self.state.lock().file_io_read_targets.remove(&request.id);
    }

    fn abort_file_writeback_submission(&self, request: &PageIoRequest) {
        if request.op != PageIoOp::Writeback || request.range.page_count() != 1 {
            return;
        }
        let page = PageIndex::new(request.range.start_page());
        let mut state = self.state.lock();
        let lease = state.file_io_leases.remove(&request.id);
        let _ = state.pages.clear_mark(page, PageCacheMark::Writeback);
        if let (Some(slot), Some(generation)) =
            (state.file_page_slots.get(&page), request.generation_hint)
        {
            let _ = slot.abort_writeback(generation);
        }
        drop(lease);
    }

    fn apply_file_io_read_frame_completion(
        &self,
        page: PageIndex,
        generation: PageGeneration,
        frame: crate::fs_iface::PageFrameRef,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let cached = match cached_frame_from_frame(Frame::new(frame.ppn())) {
            Ok(frame) => frame,
            Err(error) => {
                let state = self.state.lock();
                let Some(slot) = state.file_page_slots.get(&page) else {
                    return Err(PageSlotCompletionError::NotFetching {
                        state: PageSlotState::Empty,
                        generation: PageGeneration::new(0),
                    });
                };
                return slot.complete_fetch(generation, Err(page_cache_error_to_errno(error)));
            }
        };
        self.apply_file_io_cached_read_completion(page, generation, cached)
    }

    fn apply_file_io_cached_read_completion(
        &self,
        page: PageIndex,
        generation: PageGeneration,
        cached: CachedFrame,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let ppn = cached.ppn;
        let mut state = self.state.lock();
        let installed_ppn = match state.pages.lookup(page) {
            Some(current) => current,
            None => match state.pages.install_if_absent(page, cached) {
                Ok(()) => ppn,
                Err(PageCacheError::AlreadyPresent { current }) => current,
                Err(error) => {
                    let Some(slot) = state.file_page_slots.get(&page) else {
                        return Err(PageSlotCompletionError::NotFetching {
                            state: PageSlotState::Empty,
                            generation: PageGeneration::new(0),
                        });
                    };
                    return slot.complete_fetch(generation, Err(page_cache_error_to_errno(error)));
                }
            },
        };
        let Some(slot) = state.file_page_slots.get(&page) else {
            return Err(PageSlotCompletionError::NotFetching {
                state: PageSlotState::Empty,
                generation: PageGeneration::new(0),
            });
        };
        slot.complete_fetch(generation, Ok(installed_ppn))
    }

    #[cfg(test)]
    fn file_io_request_count_for_test(&self) -> usize {
        self.state.lock().file_io_service.submission_len()
    }

    #[cfg(test)]
    fn file_io_lease_count_for_test(&self) -> usize {
        self.state.lock().file_io_leases.len()
    }

    #[cfg(test)]
    fn file_io_read_target_count_for_test(&self) -> usize {
        self.state.lock().file_io_read_targets.len()
    }

    #[cfg(test)]
    fn file_io_pending_request_for_test(&self, page: PageIndex) -> Option<PageIoRequest> {
        let state = self.state.lock();
        state
            .file_io_service
            .find_submission(
                self.io_manager_key(),
                PageIoRange::new(page.as_u64(), 1),
                PageIoOp::Read,
            )
            .cloned()
    }

    #[cfg(test)]
    fn file_io_waiter_count_for_test(&self, page: PageIndex) -> usize {
        let state = self.state.lock();
        let Some(fetch) = state.in_flight_file_pages.get(&page) else {
            return 0;
        };
        fetch
            .request_id
            .map(|id| state.file_io_service.waiter_count(id))
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn file_io_block_queue_len_for_test(&self) -> usize {
        self.state.lock().file_block_runtime.queue.len()
    }

    #[cfg(test)]
    fn file_io_block_tracker_len_for_test(&self) -> usize {
        self.state.lock().file_block_runtime.tracker.len()
    }

    #[cfg(test)]
    fn file_page_fetch_in_flight_for_test(&self, page: PageIndex) -> bool {
        self.state.lock().in_flight_file_pages.contains_key(&page)
    }

    #[cfg(test)]
    fn file_page_slot_snapshot_for_test(&self, page: PageIndex) -> Option<PageSlotSnapshot> {
        self.state
            .lock()
            .file_page_slots
            .get(&page)
            .map(PageSlot::snapshot)
    }

    pub fn lookup(&self, page: PageIndex) -> Option<Ppn> {
        self.state.lock().pages.lookup(page)
    }

    pub fn page_marks(&self, page: PageIndex) -> Option<PageMarks> {
        self.state.lock().pages.marks(page)
    }

    pub fn materialize_anon(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> Result<MaterializedPage, PageCacheError> {
        if !matches!(&self.kind, PageContainerKind::Anon { .. }) {
            return Err(PageCacheError::UnsupportedKind);
        }
        self.check_bounds(page)?;

        if self.state.lock().pages.lookup(page).is_some() {
            return self.materialize_existing_page(page, access, false);
        }

        reclaim_clean_file_pages_if_low();
        let frame = allocate_cached_frame()?;
        let ppn = frame.ppn;
        let map_pin = acquire_map_pin_for_materialization(ppn)?;

        let installed_dirty = {
            let mut state = self.state.lock();
            let installed = match state.pages.lookup(page) {
                Some(_) => false,
                None => {
                    state.pages.install_if_absent(page, frame)?;
                    true
                }
            };
            if access == MaterializeAccess::Write {
                state.pages.mark_dirty(page)?;
            }
            installed
                .then(|| state.pages.marks(page).ok_or(PageCacheError::MissingPage))
                .transpose()?
                .map(|marks| marks.dirty)
        };
        if let Some(dirty) = installed_dirty {
            return Ok(MaterializedPage {
                ppn,
                map_pin: MaterializedPagePin::Allocated(map_pin),
                newly_installed: true,
                dirty,
            });
        }
        drop(map_pin);
        self.materialize_existing_page(page, access, false)
    }

    pub fn materialize_page_for_fault(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> Result<MaterializedPage, PageCacheError> {
        let guard =
            adapter::step_engine::epoch::borrow_current_guard().unwrap_or_else(step_engine::guard);
        self.materialize_page_now(page, access, &guard)
    }

    /// Materialize a page for VM fault resolution while preserving
    /// wait-source yields from file-backed fetches.
    pub fn materialize_page_for_fault_step(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        guard: &Guard<'_>,
    ) -> StepOutcome<MaterializedPage, NoProgress> {
        emit_pagebacked_trace(
            b"debug.pagebacked.fault_step.enter",
            access_trace_id(access),
        );
        emit_pagebacked_trace(b"debug.pagebacked.fault_step.page", page.as_u64() as i64);
        match &self.kind {
            PageContainerKind::Anon { .. } => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.kind", 1);
                match self.materialize_anon(page, access) {
                    Ok(page) => {
                        emit_pagebacked_trace(b"debug.pagebacked.fault_step.done", 1);
                        StepOutcome::Done(page)
                    }
                    Err(error) => {
                        emit_pagebacked_trace(b"debug.pagebacked.fault_step.err", 1);
                        StepOutcome::Err(page_cache_error_to_errno(error).into())
                    }
                }
            }
            PageContainerKind::File {
                mount,
                fs_object_id,
            } => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.kind", 2);
                match self.materialize_file_page(page, access, mount, *fs_object_id, guard) {
                    StepOutcome::Done(page) => {
                        emit_pagebacked_trace(b"debug.pagebacked.fault_step.done", 2);
                        StepOutcome::Done(page)
                    }
                    StepOutcome::Yield { progress, shape } => {
                        emit_pagebacked_trace(b"debug.pagebacked.fault_step.yield", 2);
                        StepOutcome::Yield { progress, shape }
                    }
                    StepOutcome::Err(errno) => {
                        emit_pagebacked_trace(b"debug.pagebacked.fault_step.err", 2);
                        StepOutcome::Err(errno)
                    }
                    StepOutcome::Continue { progress } => {
                        emit_pagebacked_trace(b"debug.pagebacked.fault_step.continue", 2);
                        StepOutcome::Continue { progress }
                    }
                }
            }
            PageContainerKind::Device { .. } => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.kind", 3);
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.err", 3);
                StepOutcome::Err(page_cache_error_to_errno(PageCacheError::UnsupportedKind).into())
            }
        }
    }

    pub fn materialize_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        guard: &Guard<'_>,
    ) -> StepOutcome<MaterializedPage, NoProgress> {
        if let Err(error) = self.check_bounds(page) {
            return StepOutcome::Err(page_cache_error_to_errno(error).into());
        }

        match &self.kind {
            PageContainerKind::Anon { .. } => match self.materialize_anon(page, access) {
                Ok(page) => StepOutcome::Done(page),
                Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
            },
            PageContainerKind::File {
                mount,
                fs_object_id,
            } => self.materialize_file_page(page, access, mount, *fs_object_id, guard),
            PageContainerKind::Device {
                base_ppn,
                page_count,
            } => self.materialize_device_page(page, *base_ppn, *page_count),
        }
    }

    pub fn materialize_page_now(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        guard: &Guard<'_>,
    ) -> Result<MaterializedPage, PageCacheError> {
        use adapter::step_engine::StepOutcome as V3;
        match self.materialize_page(page, access, guard) {
            V3::Done(page) => Ok(page),
            V3::Err(errno) => Err(PageCacheError::Backend(errno.into())),
            V3::Continue { .. } => Err(PageCacheError::Backend(Errno::EAGAIN)),
            V3::Yield { shape, .. } if notification::is_wait_source(&shape) => {
                Err(PageCacheError::Backend(Errno::EAGAIN))
            }
            V3::Yield { .. } => Err(PageCacheError::Backend(Errno::EIO)),
        }
    }

    pub fn export_page_lease(
        &self,
        page: PageIndex,
        guard: &Guard<'_>,
    ) -> StepOutcome<PageLease, NoProgress> {
        if matches!(self.kind(), PageContainerKind::Device { .. }) {
            return StepOutcome::Err(
                page_cache_error_to_errno(PageCacheError::UnsupportedKind).into(),
            );
        }
        match self.materialize_page(page, MaterializeAccess::Read, guard) {
            StepOutcome::Done(materialized) => {
                let cache_pin = match page_allocator::acquire_cache_pin(materialized.ppn) {
                    Ok(pin) => pin,
                    Err(error) => {
                        return StepOutcome::Err(
                            page_cache_error_to_errno(PageCacheError::Alloc(error)).into(),
                        );
                    }
                };
                StepOutcome::Done(PageLease {
                    ppn: materialized.ppn,
                    cache_pin: PageCachePin::Allocated(cache_pin),
                })
            }
            StepOutcome::Continue { progress } => StepOutcome::Continue { progress },
            StepOutcome::Yield { progress, shape } => StepOutcome::Yield { progress, shape },
            StepOutcome::Err(errno) => StepOutcome::Err(errno),
        }
    }

    pub fn install_page_lease_or_copy(
        &self,
        page: PageIndex,
        lease: PageLease,
    ) -> Result<bool, PageCacheError> {
        lease.confirm()?;
        match install_shared_page(self, page, lease.ppn) {
            Ok(()) => Ok(true),
            Err(PageCacheError::AlreadyPresent { current }) => {
                page_allocator::copy_frame_contents(lease.ppn, current)
                    .map_err(PageCacheError::Alloc)?;
                let mut state = self.state.lock();
                state.pages.mark_dirty(page)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn materialize_file_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        mount: &MountPayloadPin,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<MaterializedPage, NoProgress> {
        use adapter::step_engine::Errno as V3Errno;
        if self.file_page_access_conflicts_with_reservation(page, access) {
            return StepOutcome::Err(V3Errno::EBUSY);
        }
        let fetch_id = match self.begin_file_page_fetch(page, access) {
            FilePageFetchStart::Cached(materialized) => {
                return match materialized {
                    Ok(page) => StepOutcome::Done(page),
                    Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
                };
            }
            FilePageFetchStart::Joined(endpoint) => {
                return notification::yield_on_page_ready_source(NoProgress, &endpoint);
            }
            FilePageFetchStart::Owner(fetch_id) => fetch_id,
        };

        reclaim_clean_file_pages_if_low();

        if let Some(planned) =
            self.try_materialize_file_page_from_backend_plan(page, access, fetch_id)
        {
            return planned;
        }

        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            self.finish_file_page_fetch_without_install(page, fetch_id);
            return StepOutcome::Err(V3Errno::EINVAL);
        };
        // Routes through `FsPageBacking::fetch_page`. v3 outcome:
        // Done→install + Done; Continue→ no frame, surface EAGAIN as
        // a conservative choice; Yield{OnWaitSource{c,i}}→pass through
        // with `NoProgress`; Yield{OnAgent}→Err(EIO); Err→Err.
        match mount
            .payload()
            .fs_page_backing
            .fetch_page(fs_object_id, offset, guard)
        {
            StepOutcome::Done(frame) => {
                self.install_fetched_file_page_from_owner(page, access, frame, fetch_id)
            }
            StepOutcome::Continue { progress: _ } => {
                // `Continue` with `NoProgress` means "fs is asking us
                // to retry"; there is no frame to install. Conservative
                // choice: surface `Err(EAGAIN)` so callers that expect
                // a frame don't observe a stale value.
                self.finish_file_page_fetch_without_install(page, fetch_id);
                StepOutcome::Err(V3Errno::EAGAIN)
            }
            StepOutcome::Yield { progress: _, shape } => {
                self.finish_file_page_fetch_without_install(page, fetch_id);
                if let Some((carrier, interests)) = notification::wait_source_parts(&shape) {
                    notification::yield_on_wait_source(NoProgress, carrier, interests)
                } else {
                    StepOutcome::Err(V3Errno::EIO)
                }
            }
            StepOutcome::Err(v3_errno) => {
                self.finish_file_page_fetch_without_install(page, fetch_id);
                StepOutcome::Err(v3_errno)
            }
        }
    }

    fn file_page_access_conflicts_with_reservation(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> bool {
        let kind = match access {
            MaterializeAccess::Read => RangeReservationKind::BufferedRead,
            MaterializeAccess::Write => RangeReservationKind::BufferedWrite,
        };
        self.state
            .lock()
            .range_reservations
            .conflicts(PageRange::new(page, 1), kind)
    }

    fn try_materialize_file_page_from_backend_plan(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        fetch_id: FilePageFetchId,
    ) -> Option<StepOutcome<MaterializedPage, NoProgress>> {
        let PageContainerKind::File { mount, .. } = &self.kind else {
            return None;
        };
        if mount.payload().backend_planner().is_none() {
            return None;
        }

        let first = self.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)?;
        let mut waits_for_async_completion =
            file_service_work_waits_for_async_completion(first.work.as_slice());

        if first.next == PageServiceNext::Runnable {
            if let Some(second) =
                self.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)
            {
                waits_for_async_completion |=
                    file_service_work_waits_for_async_completion(second.work.as_slice());
            }
        }

        if self.lookup(page).is_some() {
            self.finish_file_page_fetch_after_planned_install(page, fetch_id);
            return Some(match self.materialize_existing_page(page, access, true) {
                Ok(page) => StepOutcome::Done(page),
                Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
            });
        }

        if waits_for_async_completion {
            return self.yield_on_file_page_fetch(page, access, fetch_id);
        }

        None
    }

    fn yield_on_file_page_fetch(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        fetch_id: FilePageFetchId,
    ) -> Option<StepOutcome<MaterializedPage, NoProgress>> {
        let wait = notification::new_page_ready_wait();
        let new_source_id = notification::page_ready_source_id(&wait);
        let new_endpoint = Arc::clone(notification::page_ready_endpoint(&wait));

        let endpoint = {
            let mut state = self.state.lock();
            if state.pages.lookup(page).is_some() {
                return Some(match self.materialize_existing_page(page, access, false) {
                    Ok(page) => StepOutcome::Done(page),
                    Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
                });
            }
            let fetch = state.in_flight_file_pages.get(&page)?;
            if fetch.id != fetch_id {
                return None;
            }

            if let Some(existing) = state.file_page_waits.get(&page) {
                let source_id = notification::page_ready_source_id(existing);
                let endpoint = Arc::clone(notification::page_ready_endpoint(existing));
                let request_id = fetch.request_id;
                if let Some(fetch) = state.in_flight_file_pages.get_mut(&page) {
                    fetch.source_id = Some(source_id);
                    fetch.joined = true;
                }
                state.register_file_io_waiter(request_id, source_id);
                endpoint
            } else {
                let request_id = fetch.request_id;
                state.file_page_waits.insert(page, wait);
                if let Some(fetch) = state.in_flight_file_pages.get_mut(&page) {
                    fetch.source_id = Some(new_source_id);
                    fetch.joined = true;
                }
                state.register_file_io_waiter(request_id, new_source_id);
                new_endpoint
            }
        };

        Some(notification::yield_on_page_ready_source(
            NoProgress, &endpoint,
        ))
    }

    fn begin_file_page_fetch(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> FilePageFetchStart {
        loop {
            if let Some(materialized) = self.materialize_cached_page(page, access) {
                return FilePageFetchStart::Cached(materialized);
            }

            let mut state = self.state.lock();
            if state.pages.lookup(page).is_some() {
                drop(state);
                return FilePageFetchStart::Cached(
                    self.materialize_existing_page(page, access, false),
                );
            }

            let existing_source_id = state
                .file_page_waits
                .get(&page)
                .map(notification::page_ready_source_id);
            let existing_endpoint = state
                .file_page_waits
                .get(&page)
                .map(|wait| Arc::clone(notification::page_ready_endpoint(wait)));
            if let Some(fetch) = state.in_flight_file_pages.get_mut(&page) {
                if fetch.source_id.is_none() {
                    fetch.source_id = existing_source_id;
                }
                if let (Some(source_id), Some(endpoint)) = (fetch.source_id, existing_endpoint) {
                    fetch.joined = true;
                    let request_id = fetch.request_id;
                    state.register_file_io_waiter(request_id, source_id);
                    return FilePageFetchStart::Joined(endpoint);
                }
                drop(state);

                let wait = notification::new_page_ready_wait();
                let source_id = notification::page_ready_source_id(&wait);
                let endpoint = Arc::clone(notification::page_ready_endpoint(&wait));
                let mut state = self.state.lock();
                if state.pages.lookup(page).is_some() {
                    drop(state);
                    return FilePageFetchStart::Cached(
                        self.materialize_existing_page(page, access, false),
                    );
                }
                if let Some((existing_source_id, existing_endpoint)) =
                    state.file_page_waits.get(&page).map(|wait| {
                        (
                            notification::page_ready_source_id(wait),
                            Arc::clone(notification::page_ready_endpoint(wait)),
                        )
                    })
                {
                    if let Some(fetch) = state.in_flight_file_pages.get_mut(&page) {
                        fetch.source_id = Some(existing_source_id);
                        fetch.joined = true;
                        let request_id = fetch.request_id;
                        state.register_file_io_waiter(request_id, existing_source_id);
                        return FilePageFetchStart::Joined(existing_endpoint);
                    }
                    continue;
                }
                if state.in_flight_file_pages.contains_key(&page) {
                    state.file_page_waits.insert(page, wait);
                    let fetch = state
                        .in_flight_file_pages
                        .get_mut(&page)
                        .expect("file page fetch still present");
                    fetch.source_id = Some(source_id);
                    fetch.joined = true;
                    let request_id = fetch.request_id;
                    state.register_file_io_waiter(request_id, source_id);
                    return FilePageFetchStart::Joined(endpoint);
                }
                continue;
            }

            let fetch_id = state.allocate_file_fetch_id();
            let generation = match state.file_page_slots.entry(page).or_default().begin_fetch() {
                PageSlotFetch::Owner { generation }
                | PageSlotFetch::Joined { generation }
                | PageSlotFetch::Resident { generation, .. }
                | PageSlotFetch::Blocked { generation, .. } => generation,
            };
            let request_id = state
                .file_io_service
                .submit(
                    self.io_manager_key(),
                    PageIoRange::new(page.as_u64(), 1),
                    PageIoOp::Read,
                    PageIoPriority::Demand,
                    PageIoFlags::DEMAND,
                    Some(generation),
                )
                .ok();
            if request_id.is_none() {
                if let Some(slot) = state.file_page_slots.get(&page) {
                    if slot.generation() == generation {
                        slot.invalidate();
                    }
                }
            }
            state.in_flight_file_pages.insert(
                page,
                FilePageFetch::with_l4_request_generation(fetch_id, generation, request_id),
            );
            return FilePageFetchStart::Owner(fetch_id);
        }
    }

    fn retire_file_page_fetch_wait(
        state: &mut PageContainerState,
        page: PageIndex,
        fetch: FilePageFetch,
    ) -> Option<notification::PageReadyNotifier> {
        let routed_waiters = fetch
            .request_id
            .map(|request_id| state.file_io_service.retire_submission(request_id))
            .unwrap_or_default();
        (!routed_waiters.is_empty())
            .then(|| state.file_page_waits.get(&page).map(|wait| wait.notifier()))
            .flatten()
    }

    fn finish_file_page_fetch_without_install(&self, page: PageIndex, fetch_id: FilePageFetchId) {
        let notify_ready: Option<notification::PageReadyNotifier> = {
            let mut state = self.state.lock();
            let Some(fetch) = state.in_flight_file_pages.get(&page) else {
                return;
            };
            if fetch.id != fetch_id {
                return;
            }
            let fetch = state
                .in_flight_file_pages
                .remove(&page)
                .expect("matched file page fetch present");
            if let Some(slot) = state.file_page_slots.get(&page) {
                if slot.generation() == fetch.generation {
                    slot.invalidate();
                }
            }
            Self::retire_file_page_fetch_wait(&mut state, page, fetch)
        };
        if let Some(notifier) = notify_ready {
            notification::notify_page_ready_with_post(&notifier, |mailbox, event| {
                mailbox.post(event)
            });
        }
    }

    fn finish_file_page_fetch_after_planned_install(
        &self,
        page: PageIndex,
        fetch_id: FilePageFetchId,
    ) {
        let notify_ready: Option<notification::PageReadyNotifier> = {
            let mut state = self.state.lock();
            let Some(fetch) = state.in_flight_file_pages.get(&page) else {
                return;
            };
            if fetch.id != fetch_id {
                return;
            }
            let fetch = state
                .in_flight_file_pages
                .remove(&page)
                .expect("matched file page fetch present");
            Self::retire_file_page_fetch_wait(&mut state, page, fetch)
        };
        if let Some(notifier) = notify_ready {
            notification::notify_page_ready_with_post(&notifier, |mailbox, event| {
                mailbox.post(event)
            });
        }
    }

    fn finish_file_page_fetch_after_service_completion(
        &self,
        page: PageIndex,
        generation: PageGeneration,
        notify_waiters: bool,
    ) {
        let notify_ready: Option<notification::PageReadyNotifier> = {
            let mut state = self.state.lock();
            let Some(fetch) = state.in_flight_file_pages.get(&page) else {
                return;
            };
            if fetch.generation != generation {
                return;
            }
            let _fetch = state
                .in_flight_file_pages
                .remove(&page)
                .expect("matched file page fetch present");
            notify_waiters
                .then(|| state.file_page_waits.get(&page).map(|wait| wait.notifier()))
                .flatten()
        };
        if let Some(notifier) = notify_ready {
            notification::notify_page_ready_with_post(&notifier, |mailbox, event| {
                mailbox.post(event)
            });
        }
    }

    fn install_fetched_file_page_from_owner(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        frame: Frame,
        fetch_id: FilePageFetchId,
    ) -> StepOutcome<MaterializedPage, NoProgress> {
        use adapter::step_engine::Errno as V3Errno;

        let frame = match cached_frame_from_frame(frame) {
            Ok(frame) => frame,
            Err(error) => {
                self.finish_file_page_fetch_without_install(page, fetch_id);
                return StepOutcome::Err(page_cache_error_to_errno(error).into());
            }
        };
        let ppn = frame.ppn;
        let map_pin = match acquire_map_pin_for_materialization(ppn) {
            Ok(pin) => pin,
            Err(error) => {
                self.finish_file_page_fetch_without_install(page, fetch_id);
                return StepOutcome::Err(page_cache_error_to_errno(error).into());
            }
        };
        let (installed_dirty, notify_ready) = {
            let mut state = self.state.lock();
            let Some(fetch) = state.in_flight_file_pages.get(&page) else {
                return StepOutcome::Err(V3Errno::EAGAIN);
            };
            if fetch.id != fetch_id {
                return StepOutcome::Err(V3Errno::EAGAIN);
            }
            let fetch = state
                .in_flight_file_pages
                .remove(&page)
                .expect("matched file page fetch present");
            let fetch_generation = fetch.generation;
            let notify_ready = Self::retire_file_page_fetch_wait(&mut state, page, fetch);

            let installed_dirty = (|| {
                let installed = match state.pages.lookup(page) {
                    Some(_) => false,
                    None => match state.pages.install_if_absent(page, frame) {
                        Ok(()) => true,
                        Err(PageCacheError::AlreadyPresent { .. }) => false,
                        Err(error) => return Err(error),
                    },
                };
                if access == MaterializeAccess::Write {
                    state.pages.mark_dirty(page)?;
                }
                installed
                    .then(|| state.pages.marks(page).ok_or(PageCacheError::MissingPage))
                    .transpose()
                    .map(|marks| marks.map(|marks| marks.dirty))
            })();
            if matches!(installed_dirty, Ok(Some(_))) {
                if let Some(slot) = state.file_page_slots.get(&page) {
                    if slot.generation() == fetch_generation {
                        if slot.complete_fetch(fetch_generation, Ok(ppn)).is_ok()
                            && access == MaterializeAccess::Write
                        {
                            let _ = slot.mark_dirty();
                        }
                    }
                }
            }

            (installed_dirty, notify_ready)
        };
        if let Some(notifier) = notify_ready {
            notification::notify_page_ready_with_post(&notifier, |mailbox, event| {
                mailbox.post(event)
            });
        }
        match installed_dirty {
            Ok(Some(dirty)) => StepOutcome::Done(MaterializedPage {
                ppn,
                map_pin: MaterializedPagePin::Allocated(map_pin),
                newly_installed: true,
                dirty,
            }),
            Ok(None) => {
                drop(map_pin);
                match self.materialize_existing_page(page, access, false) {
                    Ok(page) => StepOutcome::Done(page),
                    Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
                }
            }
            Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
        }
    }

    fn materialize_existing_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        newly_installed: bool,
    ) -> Result<MaterializedPage, PageCacheError> {
        loop {
            let snapshot = {
                let mut state = self.state.lock();
                state
                    .pages
                    .lookup(page)
                    .ok_or(PageCacheError::MissingPage)?;
                if access == MaterializeAccess::Write
                    && !matches!(self.kind, PageContainerKind::Device { .. })
                {
                    state.pages.mark_dirty(page)?;
                    if let Some(slot) = state.file_page_slots.get(&page) {
                        let _ = slot.mark_dirty();
                    }
                }
                materialized_snapshot_from_state(&state, page, newly_installed)?
            };
            let mut materialized = match snapshot.into_materialized() {
                Ok(page) => page,
                Err(PageCacheError::Alloc(AllocError::InvalidRequest)) => {
                    if self.state.lock().pages.lookup(page).is_some() {
                        return Err(PageCacheError::Alloc(AllocError::InvalidRequest));
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Some(dirty) = ({
                let mut state = self.state.lock();
                match state.pages.load_mut(page) {
                    Some(entry) if entry.ppn == materialized.ppn => {
                        if access == MaterializeAccess::Write
                            && !matches!(self.kind, PageContainerKind::Device { .. })
                        {
                            entry.marks.dirty = true;
                            entry.marks.referenced = true;
                        }
                        Some(entry.marks.dirty)
                    }
                    _ => None,
                }
            }) else {
                drop(materialized);
                continue;
            };
            materialized.dirty = dirty;
            return Ok(materialized);
        }
    }

    fn materialize_device_page(
        &self,
        page: PageIndex,
        base_ppn: Ppn,
        page_count: u64,
    ) -> StepOutcome<MaterializedPage, NoProgress> {
        use adapter::step_engine::Errno as V3Errno;
        if page.as_u64() >= page_count {
            return StepOutcome::Err(V3Errno::EINVAL);
        }
        let Ok(delta) = usize::try_from(page.as_u64()) else {
            return StepOutcome::Err(V3Errno::EINVAL);
        };
        let Some(ppn) = base_ppn.0.checked_add(delta).map(Ppn) else {
            return StepOutcome::Err(V3Errno::EINVAL);
        };

        let snapshot = {
            let mut state = self.state.lock();
            let newly_installed = match state.pages.lookup(page) {
                Some(_) => false,
                None => {
                    let frame = CachedFrame {
                        ppn,
                        pin: PageCachePin::Device(DeviceFrame::new(ppn)),
                    };
                    match state.pages.install_if_absent(page, frame) {
                        Ok(()) => true,
                        Err(PageCacheError::AlreadyPresent { .. }) => false,
                        Err(error) => {
                            return StepOutcome::Err(page_cache_error_to_errno(error).into());
                        }
                    }
                }
            };
            materialized_snapshot_from_state(&state, page, newly_installed)
        };
        match snapshot {
            Ok(snapshot) => match snapshot.into_materialized() {
                Ok(page) => StepOutcome::Done(page),
                Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
            },
            Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
        }
    }

    fn materialize_cached_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> Option<Result<MaterializedPage, PageCacheError>> {
        self.state.lock().pages.lookup(page)?;
        Some(self.materialize_existing_page(page, access, false))
    }

    fn check_bounds(&self, page: PageIndex) -> Result<(), PageCacheError> {
        if page.as_u64() >= self.page_count {
            return Err(PageCacheError::OutOfBounds);
        }
        Ok(())
    }

    fn byte_capacity(&self) -> Option<u64> {
        self.page_count
            .checked_mul(crate::vm::USER_PAGE_SIZE as u64)
    }

    pub fn set_size_bytes(&self, size: u64) {
        self.size_bytes.store(size, Ordering::Release);
    }

    fn grow_size_to(&self, new_size: u64) {
        let mut observed = self.size_bytes();
        while new_size > observed {
            match self.size_bytes.compare_exchange_weak(
                observed,
                new_size,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => observed = current,
            }
        }
    }
}

pub fn step_read(
    pc: &PageContainer,
    of: &OpenFile,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if len == 0 {
        return StepOutcome::done(0);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::err(Errno::EINVAL.into());
    };
    let start = of.offset();
    let valid_end = core::cmp::min(pc.size_bytes(), capacity);
    if start >= valid_end {
        return StepOutcome::done(0);
    }
    let effective_len = core::cmp::min(len as u64, valid_end - start) as usize;
    step_range(pc, of, effective_len, PageBackedIoKind::Read, guard)
}

pub fn step_write(
    pc: &PageContainer,
    of: &OpenFile,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if len == 0 {
        return StepOutcome::done(0);
    }
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::err(Errno::EINVAL.into());
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::err(Errno::EINVAL.into());
    };
    let Some(end) = of.offset().checked_add(len as u64) else {
        return StepOutcome::err(Errno::EINVAL.into());
    };
    if end > capacity {
        return StepOutcome::err(Errno::EINVAL.into());
    }
    let start = of.offset();
    let outcome = step_range(pc, of, len, PageBackedIoKind::Write, guard);
    let advanced_bytes = match &outcome {
        StepOutcome::Done(n) => *n,
        StepOutcome::Continue { progress } => progress.bytes(),
        StepOutcome::Yield { progress, .. } => progress.bytes(),
        StepOutcome::Err(_) => 0,
    };
    if advanced_bytes > 0 {
        pc.grow_size_to(start + advanced_bytes as u64);
    }
    outcome
}

fn step_range(
    pc: &PageContainer,
    of: &OpenFile,
    len: usize,
    kind: PageBackedIoKind,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    let mut advanced = 0usize;
    let mut offset = of.offset();
    while advanced < len {
        let page_index = PageIndex::new(offset / crate::vm::USER_PAGE_SIZE as u64);
        let within_page = (offset % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(len - advanced, crate::vm::USER_PAGE_SIZE - within_page);
        let access = match kind {
            PageBackedIoKind::Read => MaterializeAccess::Read,
            PageBackedIoKind::Write => MaterializeAccess::Write,
        };

        // `materialize_page` returns v3
        // `StepOutcome<MaterializedPage, NoProgress>`. Map per variant:
        // - `Done` → continue the loop, advancing `offset` and
        //   accumulating `advanced` bytes.
        // - `Continue { .. }` (NoProgress wait source) — page-level retry
        //   without a frame. Treat as a no-op and continue, advancing
        //   the chunk; one-shot page allocation rarely emits this.
        // - `Yield { .. }` with `advanced == 0` → propagate yield with
        //   `ByteProgress::EMPTY`. Otherwise propagate yield with
        //   accumulated bytes (`ByteProgress::new(advanced)`).
        // - `Err(errno)` with `advanced == 0` → v3 `Err(errno)`.
        //   Otherwise return v3 `Done(advanced)` (partial-success;
        //   matches the prior semantics where errors after progress
        //   were swallowed into a successful partial step).
        use adapter::step_engine::Errno as V3Errno;
        match pc.materialize_page(page_index, access, guard) {
            StepOutcome::Done(_) | StepOutcome::Continue { .. } => {
                advanced += chunk;
                offset += chunk as u64;
            }
            StepOutcome::Yield { shape, .. } => {
                let Some((carrier, interests)) = notification::wait_source_parts(&shape) else {
                    if advanced == 0 {
                        return StepOutcome::err(V3Errno::EIO);
                    }
                    of.set_offset(offset);
                    return StepOutcome::done(advanced);
                };
                if advanced == 0 {
                    return notification::yield_on_wait_source(
                        ByteProgress::EMPTY,
                        carrier,
                        interests,
                    );
                }
                of.set_offset(offset);
                return notification::yield_on_wait_source(
                    ByteProgress::new(advanced),
                    carrier,
                    interests,
                );
            }
            StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return StepOutcome::err(errno);
                }
                of.set_offset(offset);
                return StepOutcome::done(advanced);
            }
        }
    }

    of.set_offset(offset);
    StepOutcome::done(advanced)
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 3)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs by reference under a single lifetime `'a` and
// delegates from `step()` to the corresponding free fn above — semantics are
// unchanged. The free fns remain the source of truth; callers can migrate to
// the `*Op` types incrementally.

/// `StepOp` wrap of [`step_read`].
pub struct ReadOp<'a> {
    pub pc: &'a PageContainer,
    pub of: &'a OpenFile,
    pub len: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_read(self.pc, self.of, self.len, &__guard)
    }
}

/// `StepOp` wrap of [`step_write`].
pub struct WriteOp<'a> {
    pub pc: &'a PageContainer,
    pub of: &'a OpenFile,
    pub len: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for WriteOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_write(self.pc, self.of, self.len, &__guard)
    }
}

fn allocate_cached_frame() -> Result<CachedFrame, PageCacheError> {
    #[cfg(test)]
    record_frame_alloc_for_test();

    let frame = page_allocator::reserve_frame(ZeroPolicy::Zeroed)
        .map_err(PageCacheError::Alloc)?
        .commit();
    let ppn = frame.ppn();
    let cache_pin = frame.try_cache_pin().map_err(PageCacheError::Alloc)?;
    drop(frame);
    Ok(CachedFrame {
        ppn,
        pin: PageCachePin::Allocated(cache_pin),
    })
}

fn acquire_map_pin_for_materialization(
    ppn: Ppn,
) -> Result<MapPin<'static, BitmapPageAllocator<'static>>, PageCacheError> {
    #[cfg(test)]
    record_map_pin_for_test();

    page_allocator::acquire_map_pin(ppn).map_err(PageCacheError::Alloc)
}

fn cached_frame_from_frame(frame: Frame) -> Result<CachedFrame, PageCacheError> {
    let ppn = frame.ppn();
    let cache_pin = match page_allocator::acquire_cache_pin(ppn) {
        Ok(pin) => pin,
        Err(error) => {
            if frame.owned_handoff() {
                let _ = page_allocator::release_owned_frame(ppn);
            }
            return Err(PageCacheError::Alloc(error));
        }
    };
    if frame.owned_handoff() {
        page_allocator::release_owned_frame(ppn).map_err(PageCacheError::Alloc)?;
    }
    Ok(CachedFrame {
        ppn,
        pin: PageCachePin::Allocated(cache_pin),
    })
}

pub fn reserve_frame_with_reclaim(
    policy: ZeroPolicy,
) -> Result<page_allocator::FrameReservation<'static, BitmapPageAllocator<'static>>, AllocError> {
    match page_allocator::reserve_frame(policy) {
        Ok(frame) => Ok(frame),
        Err(AllocError::Exhausted) => {
            reclaim_clean_file_pages(PAGE_CACHE_RECLAIM_BATCH);
            page_allocator::reserve_frame(policy)
        }
        Err(error) => Err(error),
    }
}

pub fn reclaim_clean_file_pages_if_low() -> usize {
    match page_allocator::free_count() {
        Ok(free) if free <= PAGE_CACHE_RECLAIM_LOW_WATERMARK => {
            reclaim_clean_file_pages(PAGE_CACHE_RECLAIM_BATCH)
        }
        _ => 0,
    }
}

pub fn reclaim_clean_file_pages(budget: usize) -> usize {
    if budget == 0 {
        return 0;
    }

    let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
    let mut reclaimed = 0usize;
    let mut registry = PAGE_CONTAINER_RECLAIM_REGISTRY.lock();
    registry.retain(|weak| {
        let Some(pc) = weak.upgrade(&guard) else {
            return false;
        };
        if reclaimed < budget {
            reclaimed += pc.reclaim_clean_file_pages(budget - reclaimed);
        }
        true
    });
    reclaimed
}

impl PageContainer {
    fn reclaim_clean_file_pages(&self, budget: usize) -> usize {
        if !matches!(self.kind(), PageContainerKind::File { .. }) {
            return 0;
        }
        self.state.lock().pages.reclaim_clean_pages(budget)
    }
}

fn materialized_snapshot_from_state(
    state: &PageContainerState,
    page: PageIndex,
    newly_installed: bool,
) -> Result<MaterializedPageSnapshot, PageCacheError> {
    let entry = state.pages.load(page).ok_or(PageCacheError::MissingPage)?;
    let pin = match &entry.pin {
        PageCachePin::Allocated(cache_pin) => {
            debug_assert_eq!(cache_pin.ppn(), entry.ppn);
            let cache_pin =
                page_allocator::acquire_cache_pin(entry.ppn).map_err(PageCacheError::Alloc)?;
            MaterializedPageSnapshotPin::Allocated(cache_pin)
        }
        PageCachePin::Device(device) => MaterializedPageSnapshotPin::Device(*device),
    };
    Ok(MaterializedPageSnapshot {
        ppn: entry.ppn,
        pin,
        newly_installed,
        dirty: entry.marks.dirty,
    })
}

fn record_file_service_block_submissions(
    tracker: &mut BlockPageRequestTracker,
    outcome: &PageServiceBackendSubmitOutcome,
) {
    match outcome {
        PageServiceBackendSubmitOutcome::BlockBiosQueued { request, submitted } => {
            tracker.record_submit_outcomes(request.clone(), submitted)
        }
        PageServiceBackendSubmitOutcome::BlockGraphQueued { .. }
        | PageServiceBackendSubmitOutcome::MetadataFirstQueued { .. } => {}
        PageServiceBackendSubmitOutcome::QueuedPageCompletions { .. }
        | PageServiceBackendSubmitOutcome::Yield(_)
        | PageServiceBackendSubmitOutcome::Err(_) => {}
    }
}

fn file_service_work_waits_for_async_completion(work: &[PageServiceDrivenWork]) -> bool {
    work.iter().any(|item| {
        matches!(
            item,
            PageServiceDrivenWork::BackendSubmission(
                PageServiceBackendSubmitOutcome::BlockBiosQueued { .. }
                    | PageServiceBackendSubmitOutcome::BlockGraphQueued { .. }
                    | PageServiceBackendSubmitOutcome::MetadataFirstQueued { .. }
            )
        )
    })
}

const fn page_cache_error_to_errno(error: PageCacheError) -> Errno {
    match error {
        PageCacheError::AlreadyPresent { .. }
        | PageCacheError::MissingPage
        | PageCacheError::MismatchedFrame { .. } => Errno::ESTALE,
        PageCacheError::OutOfBounds | PageCacheError::UnsupportedKind => Errno::EINVAL,
        PageCacheError::Backend(errno) => errno,
        PageCacheError::Alloc(_) => Errno::ENOMEM,
    }
}

fn emit_pagebacked_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}

const fn access_trace_id(access: MaterializeAccess) -> i64 {
    match access {
        MaterializeAccess::Read => 1,
        MaterializeAccess::Write => 2,
    }
}

#[cfg(test)]
mod core_tests;

#[cfg(test)]
mod cross_variant_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod range_tests;
#[cfg(test)]
mod reflink_tests;
#[cfg(test)]
mod size_tests;
#[cfg(test)]
mod slot_tests;
#[cfg(test)]
mod targeted_read_tests;
#[cfg(test)]
mod user_buffer_tests;
