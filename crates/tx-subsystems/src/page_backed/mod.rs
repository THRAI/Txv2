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
    self as step_engine, page_allocator, AllocError, BitmapPageAllocator, ByteProgress, CachePin,
    Cap, DeviceFrame, MapPin, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity, Weak,
    ZeroPolicy, Zone, ZoneAllocated, ZoneError,
};

use crate::execution::{Errno, Guard};
use crate::fs_iface::{BackendPlan, FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget};
use crate::io_manager::backend::{
    dispatch_backend_plan, BlockPageCompletion, BlockPageRequestTracker, PageFrameRef,
};
use crate::io_manager::block::{
    BlockCompletionSource, BlockDispatchExecutor, BlockQueue, BlockServiceNext, QueueError,
};
use crate::io_manager::page::{
    service::{
        PageCompletionRoute, PageServiceBackendContext, PageServiceBackendDriven,
        PageServiceBackendOutcome, PageServiceBackendPrepared, PageServiceBackendSubmitError,
        PageServiceBackendSubmitOutcome, PageServiceDrivenWork, PageServiceNext,
        PageServiceTaggedBlockCompletionError, PageServiceTurn, PageServiceWork, PageWaitInterest,
        PageWaiter,
    },
    PageContainerKey, PageGeneration, PageIoCompletion, PageIoCompletionKind, PageIoFlags,
    PageIoOp, PageIoPriority, PageIoRange, PageIoRequest, PageIoRequestId, PageIoResult,
};
use crate::io_manager::runtime::{IoServiceKind, ServiceBudget, ServiceKick, ServiceWakeSource};
use crate::mount::{MountPayloadBackendContext, MountPayloadPin};
use crate::sync::SpinMutex;
use crate::vfs::{FsObjectId, OpenFile};
use tx_hal::{Ppn, UserPtr};
use tx_substrate::page_allocator::OwnedFrame;

mod block_runtime;
mod cross_variant;
mod direct_io;
mod error_seq;
mod fs_page_backing;
mod fsync_submission;
mod gift;
mod lifecycle;
mod range;
mod reflink;
mod resident;
mod slot;
mod sparse_index;
mod targeted_read;
mod user_buffer;
pub(crate) use crate::io_manager::page::PageIoSubmissionHandle;
pub(crate) use block_runtime::BlockSubmissionHandle;
pub use cross_variant::step_copy_file_range;
pub use direct_io::{
    DirectIoBuffer, DirectIoBufferError, DirectIoCompletion, DirectIoOperation, DirectIoSubmission,
    DirectIoWaitableSubmission,
};
pub use error_seq::{ErrorCursor, ErrorSeq};
pub use fs_page_backing::FsPageBacking;
use fsync_submission::FsyncSubmissionState;
pub(crate) use lifecycle::OwnedFileIoRequest;
pub use lifecycle::{
    step_fallocate, step_fsync, step_raw_block_fsync, step_truncate, FallocateOp, FsyncOp,
    TruncateOp,
};
use lifecycle::{FileIoPayload, FileIoTerminalResult, PageDataLease};
pub use range::{
    PageRange, RangeReservation, RangeReservationError, RangeReservationId, RangeReservationKind,
    RangeReservationTable,
};
pub use reflink::{cow_replace_into_private, install_shared_page};
use resident::{ResidentBindingPin, ResidentCell, ResidentHit, ResidentRoot};
pub use slot::{
    PageSlot, PageSlotCompletionError, PageSlotFetch, PageSlotFsyncStatus, PageSlotSnapshot,
    PageSlotState,
};
use sparse_index::SparseIndex;
pub use targeted_read::read_exact_at;
pub use user_buffer::{
    step_read_to_kernel, step_read_to_user, step_write_from_kernel, step_write_from_user,
    ReadToUserOp, WriteFromUserOp,
};

#[cfg(test)]
use crate::test_support::EPOCH_TEST_LOCK;
#[cfg(test)]
use core::sync::atomic::AtomicBool;

static PAGE_CONTAINER_ZONE: Zone<PageContainer> = Zone::const_new();
static PAGE_CONTAINER_RECLAIM_REGISTRY: SpinMutex<alloc::vec::Vec<Weak<PageContainer>>> =
    SpinMutex::new(alloc::vec::Vec::new());
static FILE_PAGE_CONTAINER_IDENTITIES: SpinMutex<alloc::vec::Vec<FilePageContainerIdentity>> =
    SpinMutex::new(alloc::vec::Vec::new());

const PAGE_CACHE_RECLAIM_BATCH: usize = 256;
const PAGE_CACHE_RECLAIM_LOW_WATERMARK: usize = 1024;
const MAX_FILE_WRITEBACK_BATCH_PAGES: usize = 64;
const RESIDENT_ROOT_RETIRE_MAINTENANCE_BUDGET: usize = 64;
const RESIDENT_ROOT_RETIRE_MAINTENANCE_ATTEMPTS: usize = 4;

#[cfg(test)]
static FORCE_RESIDENT_ROOT_RETIRE_BACKPRESSURE_FOR_TEST: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct FilePageContainerIdentity {
    mount_trace_id: u64,
    fs_object_id: FsObjectId,
    container: Weak<PageContainer>,
}

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
    Referenced,
    NoReclaim,
}

pub(crate) struct PageCacheEntry {
    cell: Arc<ResidentCell>,
    marks: PageMarks,
}

impl PageCacheEntry {
    fn new(frame: CachedFrame, slot: Arc<PageSlot>) -> Self {
        Self {
            cell: Arc::new(ResidentCell::from_cached_frame(frame, slot)),
            marks: PageMarks {
                referenced: true,
                ..PageMarks::new()
            },
        }
    }

    fn ppn(&self) -> Ppn {
        self.cell.ppn()
    }
}

impl PageCacheEntry {
    fn get_mark(&self, mark: PageCacheMark) -> bool {
        match mark {
            PageCacheMark::Referenced => self.marks.referenced,
            PageCacheMark::NoReclaim => self.marks.no_reclaim,
        }
    }

    fn set_mark(&mut self, mark: PageCacheMark, value: bool) {
        match mark {
            PageCacheMark::Referenced => self.marks.referenced = value,
            PageCacheMark::NoReclaim => self.marks.no_reclaim = value,
        }
    }
}

impl core::fmt::Debug for PageCacheEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheEntry")
            .field("ppn", &self.ppn())
            .field("cell", &self.cell)
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
    DuplicateLease(IoDataLeaseId),
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
    UnknownLease(IoDataLeaseId),
    UnknownReservation(RangeReservationId),
    WrongReservationKind {
        expected: RangeReservationKind,
        actual: RangeReservationKind,
    },
    Backend(Errno),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectIoSubmissionError {
    UnknownLease(IoDataLeaseId),
    OperationMismatch,
    Busy(IoDataLeaseId),
    MissingBackendPlanner,
    Backend(Errno),
    UnsupportedBackendPlan,
    Queue(Errno),
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
        self.load(page).map(PageCacheEntry::ppn)
    }

    pub fn marks(&self, page: PageIndex) -> Option<PageMarks> {
        self.load(page)?;
        Some(PageMarks {
            dirty: false,
            writeback: false,
            referenced: self.get_mark(page, PageCacheMark::Referenced),
            no_reclaim: self.get_mark(page, PageCacheMark::NoReclaim),
        })
    }

    #[cfg(test)]
    fn install_if_absent(
        &mut self,
        page: PageIndex,
        frame: CachedFrame,
    ) -> Result<(), PageCacheError> {
        self.install_if_absent_with_slot(page, frame, Arc::new(PageSlot::default()))
    }

    fn install_if_absent_with_slot(
        &mut self,
        page: PageIndex,
        frame: CachedFrame,
        slot: Arc<PageSlot>,
    ) -> Result<(), PageCacheError> {
        if let Some(entry) = self.load(page) {
            return Err(PageCacheError::AlreadyPresent {
                current: entry.ppn(),
            });
        }

        self.insert(page, PageCacheEntry::new(frame, slot))
    }

    #[cfg(test)]
    fn install_if_match(
        &mut self,
        page: PageIndex,
        expected: Ppn,
        replacement: Option<CachedFrame>,
    ) -> Result<Option<Ppn>, PageCacheError> {
        let replacement = replacement.map(|frame| {
            let slot = self
                .load(page)
                .map(|entry| Arc::clone(entry.cell.slot()))
                .unwrap_or_else(|| Arc::new(PageSlot::default()));
            (frame, slot)
        });
        self.install_if_match_with_slot(page, expected, replacement)
    }

    fn install_if_match_with_slot(
        &mut self,
        page: PageIndex,
        expected: Ppn,
        replacement: Option<(CachedFrame, Arc<PageSlot>)>,
    ) -> Result<Option<Ppn>, PageCacheError> {
        let current = self
            .load(page)
            .map(PageCacheEntry::ppn)
            .ok_or(PageCacheError::MissingPage)?;
        if current != expected {
            return Err(PageCacheError::MismatchedFrame { current });
        }
        let replacement = replacement.map(|(frame, slot)| PageCacheEntry::new(frame, slot));
        self.compare_replace(page, |entry| entry.ppn() == expected, replacement)?;
        Ok(Some(current))
    }

    fn clean_pages(&self, budget: usize) -> Vec<(PageIndex, Ppn)> {
        if budget == 0 {
            return Vec::new();
        }

        self.pages
            .iter()
            .filter_map(|(page, entry)| {
                let reclaimable = !entry.get_mark(PageCacheMark::NoReclaim);
                reclaimable.then_some((*page, entry.ppn()))
            })
            .take(budget)
            .collect()
    }

    fn clean_pages_in_range(&self, range: PageRange) -> Vec<(PageIndex, Ppn)> {
        self.pages
            .iter()
            .filter_map(|(page, entry)| range.contains(*page).then_some((*page, entry.ppn())))
            .collect()
    }

    fn resident_root(&self) -> ResidentRoot {
        self.pages
            .iter()
            .fold(ResidentRoot::empty(), |root, (page, entry)| {
                root.with_cell(*page, Arc::clone(&entry.cell))
            })
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
                current: current.ppn(),
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
                current: current.ppn(),
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
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    pub fn pages(&self) -> &[(PageIndex, PageGeneration)] {
        &self.0
    }

    #[cfg(test)]
    pub fn from_pages_for_test(pages: Vec<(PageIndex, PageGeneration)>) -> Self {
        Self(pages)
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

/// Owned fsync state that may cross reactor yields without retaining a guard
/// or a borrow of the PageContainer.
#[derive(Default)]
pub struct FileFsyncState {
    frontier: Option<FileFsyncFrontier>,
    request: Option<PageIoRequestId>,
    backend_result: Option<Result<(), Errno>>,
}

impl FileFsyncState {
    pub const fn new() -> Self {
        Self {
            frontier: None,
            request: None,
            backend_result: None,
        }
    }

    pub fn from_frontier(frontier: FileFsyncFrontier) -> Self {
        Self {
            frontier: Some(frontier),
            request: None,
            backend_result: None,
        }
    }

    pub fn frontier(&self) -> Option<&FileFsyncFrontier> {
        self.frontier.as_ref()
    }

    pub fn advance(&mut self, pc: &PageContainer) -> Result<FileFsyncFrontierAdvance, Errno> {
        if let Some(result) = self.backend_result {
            return result.map(|()| FileFsyncFrontierAdvance::Complete);
        }
        let frontier = self.frontier.get_or_insert_with(|| {
            pc.snapshot_file_fsync_frontier()
                .expect("file PageContainer has an fsync frontier")
        });
        match pc.advance_file_fsync_frontier(frontier) {
            advance @ (FileFsyncFrontierAdvance::Submitted { .. }
            | FileFsyncFrontierAdvance::Waiting) => Ok(advance),
            FileFsyncFrontierAdvance::Error(errno) => Err(errno),
            FileFsyncFrontierAdvance::Complete => {
                let Some(request) = self.request else {
                    return Ok(FileFsyncFrontierAdvance::Complete);
                };
                match pc.file_fsync_submission_state(request) {
                    Some(FsyncSubmissionState::Queued) => Ok(FileFsyncFrontierAdvance::Waiting),
                    Some(FsyncSubmissionState::Complete(_)) => {
                        let result = pc.take_file_fsync_submission(request).ok_or(Errno::EIO)?;
                        self.request = None;
                        self.backend_result = Some(result);
                        result.map(|()| FileFsyncFrontierAdvance::Complete)
                    }
                    Some(FsyncSubmissionState::Consumed) | None => Err(Errno::EIO),
                }
            }
        }
    }

    pub(crate) fn submit_backend_fsync(&mut self, pc: &PageContainer) -> Result<(), Errno> {
        if self.request.is_none() && self.backend_result.is_none() {
            self.request = Some(pc.submit_file_fsync().ok_or(Errno::EIO)?);
        }
        Ok(())
    }

    pub(crate) const fn backend_finished(&self) -> bool {
        self.backend_result.is_some()
    }
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

pub struct PageContainer {
    kind: PageContainerKind,
    page_count: u64,
    size_bytes: AtomicU64,
    resident: tx_substrate::Published<ResidentRoot>,
    resident_sequence: AtomicU64,
    direct_io_active: AtomicU64,
    page_submission: PageIoSubmissionHandle,
    block_submission: BlockSubmissionHandle,
    state: PageContainerStateCell,
}

/// Serializes derivation of immutable resident roots without entering the
/// PageContainer state domain. Even values are stable snapshots; an odd value
/// is an admitted writer preparing or publishing its replacement root.
struct ResidentMutationClaim<'a> {
    sequence: &'a AtomicU64,
    base: u64,
    committed: bool,
}

impl<'a> ResidentMutationClaim<'a> {
    fn try_acquire(sequence: &'a AtomicU64) -> Option<Self> {
        let base = sequence.load(Ordering::Acquire);
        if !base.is_multiple_of(2) {
            return None;
        }
        sequence
            .compare_exchange(
                base,
                base.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| Self {
                sequence,
                base,
                committed: false,
            })
    }

    fn commit(mut self) {
        self.sequence
            .store(self.base.wrapping_add(2).max(2), Ordering::Release);
        self.committed = true;
    }
}

impl Drop for ResidentMutationClaim<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.sequence.store(self.base, Ordering::Release);
        }
    }
}

struct PreparedResidentRoot<'a> {
    publication: tx_substrate::DetachedPublication<ResidentRoot>,
    retire: tx_substrate::epoch::LocalRetireReservation,
    claim: ResidentMutationClaim<'a>,
}

struct ResidentWithdrawal {
    page: PageIndex,
    ppn: Ppn,
    cell: Arc<ResidentCell>,
    slot: Arc<PageSlot>,
    generation: PageGeneration,
}

impl PreparedResidentRoot<'_> {
    fn commit(self, resident: &tx_substrate::Published<ResidentRoot>) {
        resident
            .commit_reserved(self.publication, self.retire)
            .expect("admitted resident-root publication invariant");
        self.claim.commit();
    }
}

impl core::fmt::Debug for PageContainer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageContainer")
            .field("kind", &self.kind)
            .field("page_count", &self.page_count)
            .field("size_bytes", &self.size_bytes)
            .field("state", &self.state)
            .finish()
    }
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
    page_slots: BTreeMap<PageIndex, Arc<PageSlot>>,
    in_flight_file_pages: BTreeMap<PageIndex, FilePageFetch>,
    fsync_submissions: BTreeMap<PageIoRequestId, fsync_submission::FsyncSubmission>,
    #[cfg(test)]
    // Test-only alias for the manager-owned L4 state. It must not become a
    // second service instance or production PageContainer ownership.
    file_io_service: PageIoSubmissionHandle,
    range_reservations: RangeReservationTable,
    direct_io_in_flight: BTreeMap<IoDataLeaseId, direct_io::DirectIoInFlight>,
    direct_io_completed: BTreeMap<IoDataLeaseId, direct_io::DirectIoCompleted>,
    // Page-scoped retry sources are retained so a task that already received
    // `Yield` can still register and consume a pending wake before it retries
    // and re-observes page state.
    file_page_waits: BTreeMap<PageIndex, notification::PageReadyWait>,
    next_file_fetch_id: u64,
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
    fn resident_slot(&mut self, page: PageIndex) -> Arc<PageSlot> {
        Arc::clone(self.page_slots.entry(page).or_default())
    }

    #[cfg(test)]
    fn install_resident_if_absent(
        &mut self,
        page: PageIndex,
        frame: CachedFrame,
    ) -> Result<(), PageCacheError> {
        let slot = self.resident_slot(page);
        self.pages.install_if_absent_with_slot(page, frame, slot)
    }

    #[cfg(test)]
    fn replace_resident_if_match(
        &mut self,
        page: PageIndex,
        expected: Ppn,
        replacement: Option<CachedFrame>,
    ) -> Result<Option<Ppn>, PageCacheError> {
        let slot = self.resident_slot(page);
        let current = self.pages.load(page).ok_or(PageCacheError::MissingPage)?;
        if current.ppn() != expected {
            return Err(PageCacheError::MismatchedFrame {
                current: current.ppn(),
            });
        }
        debug_assert!(Arc::ptr_eq(current.cell.slot(), &slot));
        current.cell.mark_withdrawn();
        self.pages.install_if_match_with_slot(
            page,
            expected,
            replacement.map(|frame| (frame, slot)),
        )
    }

    #[cfg(test)]
    fn withdraw_resident_entry_if_match(
        &mut self,
        page: PageIndex,
        expected: Ppn,
    ) -> Result<Option<Ppn>, PageCacheError> {
        self.replace_resident_if_match(page, expected, None)
    }

    fn allocate_file_fetch_id(&mut self) -> FilePageFetchId {
        let id = self.next_file_fetch_id;
        self.next_file_fetch_id = self.next_file_fetch_id.wrapping_add(1).max(1);
        FilePageFetchId(id)
    }

    // The fetch state owns only the endpoint lifetime. L4 owns the waiter
    // record itself through the typed submission handle.
    fn register_file_io_waiter(
        &mut self,
        page_submission: &PageIoSubmissionHandle,
        request_id: Option<PageIoRequestId>,
        source_id: u64,
    ) {
        if let Some(request_id) = request_id {
            page_submission.register_waiter(
                request_id,
                PageWaiter {
                    source_id,
                    interests: PageWaitInterest::READY,
                },
            );
        }
    }

    fn ensure_resident_page_slot(
        &mut self,
        page: PageIndex,
        ppn: Ppn,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let slot = self.resident_slot(page);
        let snapshot = slot.snapshot();
        let current = match snapshot.state {
            PageSlotState::Resident { ppn }
            | PageSlotState::Dirty { ppn }
            | PageSlotState::Writeback { ppn, .. } => Some(ppn),
            PageSlotState::Empty | PageSlotState::Error { .. } => None,
            PageSlotState::Fetching => {
                return Err(PageSlotCompletionError::NotFetching {
                    state: snapshot.state,
                    generation: snapshot.generation,
                });
            }
        };
        match current {
            Some(current) if current == ppn => Ok(snapshot),
            Some(current) => Err(PageSlotCompletionError::MismatchedFrame {
                expected: ppn,
                current,
            }),
            None => match slot.begin_fetch() {
                PageSlotFetch::Owner { generation } => slot.complete_fetch(generation, Ok(ppn)),
                _ => {
                    let snapshot = slot.snapshot();
                    Err(PageSlotCompletionError::NotFetching {
                        state: snapshot.state,
                        generation: snapshot.generation,
                    })
                }
            },
        }
    }

    #[cfg(test)]
    fn withdraw_resident_page_slot(
        &mut self,
        page: PageIndex,
        ppn: Ppn,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let snapshot = self.ensure_resident_page_slot(page, ppn)?;
        let slot = self
            .page_slots
            .get(&page)
            .expect("resident slot remains installed while PageContainer state is locked");
        slot.withdraw_if_matches(snapshot.generation, ppn)
    }

    #[cfg(test)]
    fn withdraw_clean_resident_page_slot(
        &mut self,
        page: PageIndex,
        ppn: Ppn,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let snapshot = self.ensure_resident_page_slot(page, ppn)?;
        if snapshot.state != (PageSlotState::Resident { ppn }) {
            return Err(PageSlotCompletionError::NotFetching {
                state: snapshot.state,
                generation: snapshot.generation,
            });
        }
        let slot = self
            .page_slots
            .get(&page)
            .expect("resident slot remains installed while PageContainer state is locked");
        slot.withdraw_if_matches(snapshot.generation, ppn)
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
        PAGE_CONTAINER_STATE_LOCK_ACQUISITIONS_FOR_TEST.fetch_add(1, Ordering::AcqRel);
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
static PAGE_CONTAINER_STATE_LOCK_ACQUISITIONS_FOR_TEST: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn page_container_state_lock_held_for_test() -> bool {
    PAGE_CONTAINER_STATE_LOCK_DEPTH_FOR_TEST.with(|depth| depth.get() != 0)
}

#[cfg(test)]
fn reset_page_container_lock_service_observations_for_test() {
    FRAME_ALLOC_UNDER_STATE_LOCK_FOR_TEST.store(0, Ordering::Release);
    MAP_PIN_UNDER_STATE_LOCK_FOR_TEST.store(0, Ordering::Release);
    PAGE_CONTAINER_STATE_LOCK_ACQUISITIONS_FOR_TEST.store(0, Ordering::Release);
}

#[cfg(test)]
fn page_container_lock_service_observations_for_test() -> (usize, usize) {
    (
        FRAME_ALLOC_UNDER_STATE_LOCK_FOR_TEST.load(Ordering::Acquire),
        MAP_PIN_UNDER_STATE_LOCK_FOR_TEST.load(Ordering::Acquire),
    )
}

#[cfg(test)]
fn page_container_state_lock_acquisitions_for_test() -> usize {
    PAGE_CONTAINER_STATE_LOCK_ACQUISITIONS_FOR_TEST.load(Ordering::Acquire)
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
        let page_submission = PageIoSubmissionHandle::new(1024);
        Self {
            kind,
            page_count,
            size_bytes: AtomicU64::new(capacity),
            resident: tx_substrate::Published::try_new(ResidentRoot::empty())
                .expect("initial PageContainer resident-root publication allocation"),
            resident_sequence: AtomicU64::new(0),
            direct_io_active: AtomicU64::new(0),
            page_submission: page_submission.clone(),
            block_submission: BlockSubmissionHandle::new(1024, 16),
            state: PageContainerStateCell::new(PageContainerState {
                pages: PageCacheIndex::new(),
                page_slots: BTreeMap::new(),
                in_flight_file_pages: BTreeMap::new(),
                fsync_submissions: BTreeMap::new(),
                #[cfg(test)]
                file_io_service: page_submission.clone(),
                range_reservations: RangeReservationTable::new(),
                direct_io_in_flight: BTreeMap::new(),
                direct_io_completed: BTreeMap::new(),
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
        Self::new_file_cap_with_page_count(mount, fs_object_id, size_bytes, page_count)
    }

    fn new_file_cap_with_page_count(
        mount: MountPayloadPin,
        fs_object_id: FsObjectId,
        size_bytes: u64,
        page_count: u64,
    ) -> Result<Cap<PageContainer>, ZoneError> {
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

    /// Returns the mount-scoped canonical File PageContainer for one backing
    /// object. The registry retains only weak evidence, so it cannot form a
    /// MountPayload <-> PageContainer ownership cycle.
    pub fn find_or_create_file_cap(
        mount: MountPayloadPin,
        fs_object_id: FsObjectId,
        size_bytes: u64,
        minimum_page_count: u64,
        guard: &Guard<'_>,
    ) -> Result<(Cap<PageContainer>, bool), ZoneError> {
        let mount_trace_id = mount.payload().trace_id();
        let mut identities = FILE_PAGE_CONTAINER_IDENTITIES.lock();
        identities.retain(|entry| entry.container.upgrade(guard).is_some());

        if let Some(existing) = identities
            .iter()
            .find(|entry| {
                entry.mount_trace_id == mount_trace_id && entry.fs_object_id == fs_object_id
            })
            .and_then(|entry| entry.container.upgrade(guard))
        {
            return Ok((existing, false));
        }

        let page_size = crate::vm::USER_PAGE_SIZE as u64;
        let page_count = if size_bytes == 0 {
            0
        } else {
            1 + (size_bytes - 1) / page_size
        }
        .max(minimum_page_count);
        let container =
            Self::new_file_cap_with_page_count(mount, fs_object_id, size_bytes, page_count)?;
        identities.push(FilePageContainerIdentity {
            mount_trace_id,
            fs_object_id,
            container: container.downgrade(),
        });
        Ok((container, true))
    }

    /// Removes exactly the current mount/object binding. This is intentionally
    /// separate from the later L4/L6 runtime retirement path: a runtime may
    /// still retain the PageContainer until the manager-owning phase retires it.
    pub fn retire_file_identity(
        mount: &MountPayloadPin,
        fs_object_id: FsObjectId,
        container: &Cap<PageContainer>,
        guard: &Guard<'_>,
    ) -> bool {
        let mount_trace_id = mount.payload().trace_id();
        let mut identities = FILE_PAGE_CONTAINER_IDENTITIES.lock();
        let Some(index) = identities.iter().position(|entry| {
            entry.mount_trace_id == mount_trace_id && entry.fs_object_id == fs_object_id
        }) else {
            return false;
        };
        let entry = identities[index];
        match entry.container.upgrade(guard) {
            Some(current) if current.key() == container.key() => {
                identities.remove(index);
                true
            }
            Some(_) => false,
            None => {
                identities.remove(index);
                false
            }
        }
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

    #[cfg(tx_vm_pmap_boot_diag)]
    pub fn file_io_boot_diag_counts(&self) -> (usize, usize, usize, usize, usize, bool) {
        let state = self.state.lock();
        (
            state.in_flight_file_pages.len(),
            self.page_submission
                .with_service(|service| service.submission_len()),
            self.page_submission.admitted_file_request_count(),
            self.block_submission.queue_len_for_test(),
            self.block_submission.tracker_len_for_test(),
            self.page_submission.has_wake_source(),
        )
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

    /// Bind the page container to the service wake endpoint owned by its
    /// registered L6 executor. The endpoint is a neutral I/O-manager runtime
    /// handle, not a concrete device reference.
    pub fn attach_file_io_wake_source(&self, wake_source: Arc<ServiceWakeSource>) -> bool {
        self.page_submission.attach_wake_source(wake_source)
    }

    /// Clone the typed service handles held by this PageContainer for a
    /// reactor-owned file-I/O runtime claim. The claim never retains the
    /// PageContainer itself; it upgrades its weak liveness endpoint per turn.
    pub(crate) fn file_io_runtime_handles(
        &self,
    ) -> (PageIoSubmissionHandle, BlockSubmissionHandle) {
        (self.page_submission.clone(), self.block_submission.clone())
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

    /// Admit a direct read and retain the user-page DMA pins until its terminal
    /// completion. Callers pass the returned neutral descriptor to the
    /// filesystem planner only after this method has returned.
    pub fn submit_file_direct_read(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
    ) -> Result<DirectIoSubmission, DirectIoAdmissionError> {
        self.submit_file_direct_io(range, buffer, DirectIoOperation::Read)
    }

    /// Admit a direct read with a terminal completion wait endpoint.
    pub fn submit_file_direct_read_waitable(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
    ) -> Result<DirectIoWaitableSubmission, DirectIoAdmissionError> {
        self.submit_file_direct_io_waitable(range, buffer, DirectIoOperation::Read)
    }

    /// Admit a direct write and retain the user-page DMA pins until its
    /// terminal completion.
    pub fn submit_file_direct_write(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
    ) -> Result<DirectIoSubmission, DirectIoAdmissionError> {
        self.submit_file_direct_io(range, buffer, DirectIoOperation::Write)
    }

    /// Admit a direct write with a terminal completion wait endpoint.
    pub fn submit_file_direct_write_waitable(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
    ) -> Result<DirectIoWaitableSubmission, DirectIoAdmissionError> {
        self.submit_file_direct_io_waitable(range, buffer, DirectIoOperation::Write)
    }

    /// Consume the terminal result associated with a waitable direct-I/O submission.
    pub fn take_file_direct_submission_result(
        &self,
        submission: &DirectIoWaitableSubmission,
    ) -> Option<Result<DirectIoCompletion, DirectIoCompletionError>> {
        let mut state = self.state.lock();
        let completed = state.direct_io_completed.remove(&submission.lease_id())?;
        debug_assert_eq!(
            notification::page_ready_source_id(&completed.wait),
            submission.wait_source_id()
        );
        Some(completed.result)
    }

    /// Apply a terminal direct-I/O result and release the associated DMA lease.
    ///
    /// A root-retirement retry keeps the in-flight entry, DMA pin, range
    /// reservation, and device result together until cache coherency commits.
    pub fn complete_file_direct_submission(
        &self,
        lease: IoDataLeaseId,
        result: Result<(), Errno>,
    ) -> Result<DirectIoCompletion, DirectIoCompletionError> {
        let mut in_flight = {
            let mut state = self.state.lock();
            state
                .direct_io_in_flight
                .remove(&lease)
                .ok_or(DirectIoCompletionError::UnknownLease(lease))?
        };
        let completion_result = match in_flight.state {
            direct_io::DirectIoInFlightState::CompletionPending { result } => result,
            direct_io::DirectIoInFlightState::Admitted
            | direct_io::DirectIoInFlightState::Planning
            | direct_io::DirectIoInFlightState::Queued => result,
        };
        let completion = match in_flight.operation {
            DirectIoOperation::Read => self
                .complete_file_direct_read(in_flight.reservation, completion_result)
                .map(|()| DirectIoCompletion::Read),
            DirectIoOperation::Write => self
                .complete_file_direct_write(in_flight.reservation, completion_result)
                .map(|invalidated| DirectIoCompletion::Write { invalidated }),
        };
        if completion_result.is_ok()
            && matches!(
                completion,
                Err(DirectIoCompletionError::Backend(Errno::EAGAIN))
            )
        {
            in_flight.state = direct_io::DirectIoInFlightState::CompletionPending {
                result: completion_result,
            };
            let replaced = self
                .state
                .lock()
                .direct_io_in_flight
                .insert(lease, in_flight);
            debug_assert!(replaced.is_none());
            self.kick_file_io_service(IoServiceKind::Block);
            return completion;
        }

        let completion_wait = in_flight.completion_wait.take();
        drop(in_flight);
        let notifier = completion_wait.map(|wait| {
            let notifier = wait.notifier();
            let replaced = self.state.lock().direct_io_completed.insert(
                lease,
                direct_io::DirectIoCompleted {
                    result: completion,
                    wait,
                },
            );
            debug_assert!(replaced.is_none());
            notifier
        });
        if let Some(notifier) = notifier {
            notification::notify_page_ready_with_post(&notifier, |mailbox, event| {
                mailbox.post(event)
            });
        }
        completion
    }

    /// Plan a previously admitted direct-I/O lease and put its mapped data bio
    /// on the owned L6 queue. The lease remains PageContainer-owned until the
    /// matching tagged device completion reaches `complete_file_direct_submission`.
    pub fn enqueue_file_direct_submission(
        &self,
        submission: &DirectIoSubmission,
    ) -> Result<(), DirectIoSubmissionError> {
        let lease = submission.lease_id();
        {
            let mut state = self.state.lock();
            let Some(in_flight) = state.direct_io_in_flight.get_mut(&lease) else {
                return Err(DirectIoSubmissionError::UnknownLease(lease));
            };
            if in_flight.operation != submission.operation() {
                return Err(DirectIoSubmissionError::OperationMismatch);
            }
            if !matches!(in_flight.state, direct_io::DirectIoInFlightState::Admitted) {
                return Err(DirectIoSubmissionError::Busy(lease));
            }
            in_flight.state = direct_io::DirectIoInFlightState::Planning;
        }

        let Some(context) = self.file_backend_context() else {
            self.fail_file_direct_submission(lease, Errno::ENOSYS);
            return Err(DirectIoSubmissionError::MissingBackendPlanner);
        };
        let (op, priority, flags) = match submission.operation() {
            DirectIoOperation::Read => {
                (PageIoOp::Read, PageIoPriority::Demand, PageIoFlags::DEMAND)
            }
            DirectIoOperation::Write => (
                PageIoOp::Writeback,
                PageIoPriority::ForegroundWrite,
                PageIoFlags::WRITEBACK,
            ),
        };
        let request = PageIoRequest::new(
            PageIoRequestId::new(lease.raw()),
            self.io_manager_key(),
            PageIoRange::new(
                submission.range().start().as_u64(),
                submission.range().page_count(),
            ),
            op,
            priority,
            flags,
            Some(PageGeneration::new(lease.raw())),
        );
        let Some(plan) = context
            .payload()
            .plan_backend_page_request_with_source_and_target(
                context.object(),
                request,
                submission.source().cloned().unwrap_or(IoDataSource::None),
                submission.target().cloned().unwrap_or(IoDataTarget::None),
            )
        else {
            self.fail_file_direct_submission(lease, Errno::ENOSYS);
            return Err(DirectIoSubmissionError::MissingBackendPlanner);
        };
        let bio = match plan {
            BackendPlan::SubmitBios(bios) if bios.as_slice().len() == 1 => bios
                .into_vec()
                .into_iter()
                .next()
                .expect("checked one direct bio"),
            BackendPlan::Err(errno) => {
                self.fail_file_direct_submission(lease, errno);
                return Err(DirectIoSubmissionError::Backend(errno));
            }
            _ => {
                self.fail_file_direct_submission(lease, Errno::ENOSYS);
                return Err(DirectIoSubmissionError::UnsupportedBackendPlan);
            }
        };

        let queued = {
            let mut state = self.state.lock();
            let Some(in_flight) = state.direct_io_in_flight.get(&lease) else {
                return Err(DirectIoSubmissionError::UnknownLease(lease));
            };
            if !matches!(in_flight.state, direct_io::DirectIoInFlightState::Planning) {
                return Err(DirectIoSubmissionError::Busy(lease));
            }
            match self.block_submission.submit_direct(lease, bio) {
                Ok(_) => {
                    state
                        .direct_io_in_flight
                        .get_mut(&lease)
                        .expect("direct lease stayed registered through planning")
                        .state = direct_io::DirectIoInFlightState::Queued;
                    Ok(())
                }
                Err(error) => {
                    state
                        .direct_io_in_flight
                        .get_mut(&lease)
                        .expect("direct lease stayed registered through planning")
                        .state = direct_io::DirectIoInFlightState::Admitted;
                    Err(error)
                }
            }
        };
        match queued {
            Ok(()) => {
                self.kick_file_io_service(IoServiceKind::Block);
                Ok(())
            }
            Err(error) => {
                let errno = direct_queue_errno(error);
                self.fail_file_direct_submission(lease, errno);
                Err(DirectIoSubmissionError::Queue(errno))
            }
        }
    }

    #[cfg(test)]
    pub fn direct_io_in_flight_count_for_test(&self) -> usize {
        self.state.lock().direct_io_in_flight.len()
    }

    #[cfg(test)]
    pub fn direct_io_completed_count_for_test(&self) -> usize {
        self.state.lock().direct_io_completed.len()
    }

    #[cfg(test)]
    pub fn direct_io_block_tracker_len_for_test(&self) -> usize {
        self.block_submission.direct_tracker_len_for_test()
    }

    fn retry_pending_direct_io_completions(&self) -> Result<bool, DirectIoCompletionError> {
        let pending: Vec<(IoDataLeaseId, Result<(), Errno>)> = {
            let state = self.state.lock();
            state
                .direct_io_in_flight
                .iter()
                .filter_map(|(lease, in_flight)| match in_flight.state {
                    direct_io::DirectIoInFlightState::CompletionPending { result } => {
                        Some((*lease, result))
                    }
                    direct_io::DirectIoInFlightState::Admitted
                    | direct_io::DirectIoInFlightState::Planning
                    | direct_io::DirectIoInFlightState::Queued => None,
                })
                .collect()
        };
        let mut still_pending = false;
        for (lease, result) in pending {
            match self.complete_file_direct_submission(lease, result) {
                Ok(_) => {}
                Err(DirectIoCompletionError::Backend(Errno::EAGAIN)) => still_pending = true,
                Err(error) => return Err(error),
            }
        }
        Ok(still_pending)
    }

    fn direct_io_completion_is_pending(&self, lease: IoDataLeaseId) -> bool {
        self.state
            .lock()
            .direct_io_in_flight
            .get(&lease)
            .is_some_and(|in_flight| {
                matches!(
                    in_flight.state,
                    direct_io::DirectIoInFlightState::CompletionPending { .. }
                )
            })
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
        if !state.range_reservations.contains(reservation.id()) {
            return Err(DirectIoCompletionError::UnknownReservation(
                reservation.id(),
            ));
        }
        if let Err(errno) = result {
            let released = state.range_reservations.release(reservation.id());
            debug_assert!(released, "validated direct-write reservation remains live");
            self.direct_io_active.fetch_sub(1, Ordering::AcqRel);
            return Err(DirectIoCompletionError::Backend(errno));
        }
        let candidates = state.pages.clean_pages_in_range(reservation.range());
        drop(state);
        let invalidated = match self.withdraw_resident_batch_published(&candidates, true) {
            Ok(invalidated) => invalidated,
            // The live reservation stays in the table. Callers that own it
            // can retry, while submission completion retains its in-flight
            // record in CompletionPending.
            Err(PageCacheError::Backend(Errno::EAGAIN)) => {
                return Err(DirectIoCompletionError::Backend(Errno::EAGAIN));
            }
            Err(_) => return Err(DirectIoCompletionError::Backend(Errno::ESTALE)),
        };
        let mut state = self.state.lock();
        let released = state.range_reservations.release(reservation.id());
        debug_assert!(
            released,
            "direct-write reservation remains live through invalidation"
        );
        self.direct_io_active.fetch_sub(1, Ordering::AcqRel);
        Ok(invalidated)
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
        self.direct_io_active.fetch_sub(1, Ordering::AcqRel);
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
        for (page, slot) in &state.page_slots {
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
        let reservation = state
            .range_reservations
            .try_reserve(range, kind)
            .map_err(DirectIoAdmissionError::from)?;
        drop(state);
        self.direct_io_active.fetch_add(1, Ordering::AcqRel);
        Ok(reservation)
    }

    fn submit_file_direct_io(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
        operation: DirectIoOperation,
    ) -> Result<DirectIoSubmission, DirectIoAdmissionError> {
        self.submit_file_direct_io_with_wait(range, buffer, operation, None)
    }

    fn submit_file_direct_io_waitable(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
        operation: DirectIoOperation,
    ) -> Result<DirectIoWaitableSubmission, DirectIoAdmissionError> {
        let wait = notification::new_page_ready_wait();
        let wait_source_id = notification::page_ready_source_id(&wait);
        let wait_endpoint = Arc::clone(notification::page_ready_endpoint(&wait));
        let submission =
            self.submit_file_direct_io_with_wait(range, buffer, operation, Some(wait))?;
        Ok(DirectIoWaitableSubmission::new(
            submission,
            wait_source_id,
            wait_endpoint,
        ))
    }

    fn submit_file_direct_io_with_wait(
        &self,
        range: PageRange,
        buffer: DirectIoBuffer,
        operation: DirectIoOperation,
        completion_wait: Option<notification::PageReadyWait>,
    ) -> Result<DirectIoSubmission, DirectIoAdmissionError> {
        let reservation = match operation {
            DirectIoOperation::Read => self.begin_file_direct_read(range)?,
            DirectIoOperation::Write => self.begin_file_direct_write(range)?,
        };
        let submission = match operation {
            DirectIoOperation::Read => DirectIoSubmission::read(range, &buffer),
            DirectIoOperation::Write => DirectIoSubmission::write(range, &buffer),
        };
        let lease = buffer.lease_id();
        let duplicate = {
            let mut state = self.state.lock();
            if state.direct_io_in_flight.contains_key(&lease) {
                true
            } else {
                state.direct_io_in_flight.insert(
                    lease,
                    direct_io::DirectIoInFlight {
                        reservation,
                        buffer,
                        operation,
                        state: direct_io::DirectIoInFlightState::Admitted,
                        completion_wait,
                    },
                );
                false
            }
        };
        if !duplicate {
            return Ok(submission);
        }

        // Lease ids are globally allocated, but do not strand a range if an
        // internal collision is ever introduced by a future buffer provider.
        let release = match operation {
            DirectIoOperation::Read => self.complete_file_direct_read(reservation, Ok(())),
            DirectIoOperation::Write => self
                .complete_file_direct_write(reservation, Ok(()))
                .map(|_| ()),
        };
        debug_assert!(release.is_ok());
        Err(DirectIoAdmissionError::DuplicateLease(lease))
    }

    fn fail_file_direct_submission(&self, lease: IoDataLeaseId, errno: Errno) {
        let completion = self.complete_file_direct_submission(lease, Err(errno));
        debug_assert!(
            matches!(completion, Err(DirectIoCompletionError::Backend(found)) if found == errno)
        );
    }

    fn kick_file_io_service(&self, service: IoServiceKind) {
        self.page_submission.kick(service);
    }

    /// Move one dirty file page into the L4 writeback queue.
    ///
    /// This is only admission: the backend planner and L6 executor own later
    /// submission and completion. A failed admission restores the slot to
    /// `Dirty`, so no request is left falsely in flight.
    pub fn queue_file_page_writeback(&self, page: PageIndex) -> Option<PageIoRequestId> {
        self.queue_file_writeback_batch(core::slice::from_ref(&page))
    }

    /// Atomically admit one bounded contiguous dirty-page range to L4.
    ///
    /// Every PageSlot transition and cache pin is acquired before the request
    /// becomes visible. Any failure restores all generations that this attempt
    /// moved into writeback and drops every acquired pin.
    fn queue_file_writeback_batch(&self, pages: &[PageIndex]) -> Option<PageIoRequestId> {
        if !matches!(self.kind, PageContainerKind::File { .. }) {
            return None;
        }
        if pages.is_empty() || pages.len() > MAX_FILE_WRITEBACK_BATCH_PAGES {
            return None;
        }
        if pages
            .windows(2)
            .any(|pair| pair[0].as_u64().checked_add(1) != Some(pair[1].as_u64()))
        {
            return None;
        }

        let state = self.state.lock();
        let mut segments = Vec::with_capacity(pages.len());
        let mut generations = Vec::with_capacity(pages.len());
        for &page in pages {
            let Some(slot) = state.page_slots.get(&page) else {
                rollback_file_writeback_slots(&state, &generations);
                return None;
            };
            let Ok(writeback) = slot.begin_writeback() else {
                rollback_file_writeback_slots(&state, &generations);
                return None;
            };
            generations.push((page, writeback.generation));
            let Some(ppn) = state.pages.load(page).map(PageCacheEntry::ppn) else {
                rollback_file_writeback_slots(&state, &generations);
                return None;
            };
            let Ok(cache_pin) = page_allocator::acquire_cache_pin(ppn) else {
                rollback_file_writeback_slots(&state, &generations);
                return None;
            };
            segments.push((
                page,
                writeback.generation,
                PageLease {
                    ppn,
                    cache_pin: PageCachePin::Allocated(cache_pin),
                },
            ));
        }
        drop(state);

        let first = pages[0];
        let first_generation = generations[0].1;
        let lease = match PageDataLease::from_segments(
            IoDataLeaseId::new(0),
            MAX_FILE_WRITEBACK_BATCH_PAGES,
            segments.into_boxed_slice(),
        ) {
            Ok(lease) => lease,
            Err(_) => {
                let state = self.state.lock();
                rollback_file_writeback_slots(&state, &generations);
                return None;
            }
        };
        match self.page_submission.submit_owned_file_request(
            self.io_manager_key(),
            PageIoRange::new(first.as_u64(), pages.len() as u64),
            PageIoOp::Writeback,
            PageIoPriority::BackgroundWriteback,
            PageIoFlags::WRITEBACK,
            Some(first_generation),
            move |request| {
                let lease_id = IoDataLeaseId::new(request.id.raw());
                OwnedFileIoRequest::writeback(request, lease.with_id(lease_id))
            },
        ) {
            Some(id) => Some(id),
            None => {
                let state = self.state.lock();
                rollback_file_writeback_slots(&state, &generations);
                None
            }
        }
    }

    /// Admit all currently dirty file pages to background L4 writeback.
    ///
    /// This is intentionally nonblocking: close may request background
    /// visibility writeback, but only an explicit fsync owns a durability
    /// frontier and waits for its journal commit.
    pub fn queue_dirty_file_writeback(&self) -> usize {
        let Some(frontier) = self.snapshot_file_fsync_frontier() else {
            return 0;
        };

        let candidates = {
            let state = self.state.lock();
            frontier
                .pages()
                .iter()
                .filter_map(|&(page, _)| {
                    state
                        .page_slots
                        .get(&page)
                        .filter(|slot| matches!(slot.snapshot().state, PageSlotState::Dirty { .. }))
                        .map(|_| page)
                })
                .collect::<Vec<_>>()
        };
        let mut admitted = 0usize;
        let mut start = 0usize;
        while start < candidates.len() {
            let mut end = start + 1;
            while end < candidates.len()
                && end - start < MAX_FILE_WRITEBACK_BATCH_PAGES
                && candidates[end].as_u64() == candidates[end - 1].as_u64().saturating_add(1)
            {
                end += 1;
            }
            let batch = &candidates[start..end];
            if self.queue_file_writeback_batch(batch).is_some() {
                admitted = admitted.saturating_add(batch.len());
            } else {
                for &page in batch {
                    admitted = admitted.saturating_add(usize::from(
                        self.queue_file_page_writeback(page).is_some(),
                    ));
                }
            }
            start = end;
        }
        if admitted != 0 {
            self.kick_file_io_service(IoServiceKind::Page);
        }
        admitted
    }

    pub fn snapshot_file_fsync_frontier(&self) -> Option<FileFsyncFrontier> {
        if !matches!(self.kind, PageContainerKind::File { .. }) {
            return None;
        }
        let state = self.state.lock();
        let mut pages = Vec::new();
        for (page, slot) in &state.page_slots {
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

    fn submit_file_fsync(&self) -> Option<PageIoRequestId> {
        if !matches!(self.kind, PageContainerKind::File { .. }) {
            return None;
        }
        let mut state = self.state.lock();
        let id = self
            .page_submission
            .with_service(|service| {
                service.submit(
                    self.io_manager_key(),
                    PageIoRange::new(0, self.page_count.max(1)),
                    PageIoOp::Fsync,
                    PageIoPriority::Fsync,
                    PageIoFlags::BARRIER,
                    None,
                )
            })
            .ok()?;
        let previous = state
            .fsync_submissions
            .insert(id, fsync_submission::FsyncSubmission::new(id));
        debug_assert!(previous.is_none(), "L4 request identifiers are unique");
        Some(id)
    }

    fn file_fsync_submission_state(&self, id: PageIoRequestId) -> Option<FsyncSubmissionState> {
        self.state
            .lock()
            .fsync_submissions
            .get(&id)
            .map(|row| row.state())
    }

    fn take_file_fsync_submission(&self, id: PageIoRequestId) -> Option<Result<(), Errno>> {
        let mut state = self.state.lock();
        let result = state.fsync_submissions.get_mut(&id)?.take()?;
        state.fsync_submissions.remove(&id);
        Some(result)
    }

    pub fn advance_file_fsync_frontier(
        &self,
        frontier: &FileFsyncFrontier,
    ) -> FileFsyncFrontierAdvance {
        let mut submitted = 0u32;
        let mut waiting = false;
        let mut candidates = Vec::new();
        for &(page, generation) in frontier.pages() {
            let status = self
                .state
                .lock()
                .page_slots
                .get(&page)
                .map(|slot| slot.fsync_status(generation));
            match status {
                Some(PageSlotFsyncStatus::NeedsWriteback { .. }) => {
                    candidates.push(page);
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
        let mut start = 0usize;
        while start < candidates.len() {
            let mut end = start + 1;
            while end < candidates.len()
                && end - start < MAX_FILE_WRITEBACK_BATCH_PAGES
                && candidates[end].as_u64() == candidates[end - 1].as_u64().saturating_add(1)
            {
                end += 1;
            }
            let batch = &candidates[start..end];
            if self.queue_file_writeback_batch(batch).is_some() {
                submitted = submitted.saturating_add(batch.len() as u32);
            } else {
                for &page in batch {
                    if self.queue_file_page_writeback(page).is_some() {
                        submitted = submitted.saturating_add(1);
                    } else {
                        waiting = true;
                    }
                }
            }
            start = end;
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
        let step = self
            .page_submission
            .with_service(|service| service.drive_turn(budget));
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
                        let (source, target) = self.prepare_owned_file_io_request(&request);
                        let guard =
                            step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
                        let plan = match context.prepare_submission_with_source_and_target(
                            &request, &source, &target, &guard,
                        ) {
                            Ok(()) => context.plan_submission_with_source_and_target(
                                request.clone(),
                                source,
                                target,
                            ),
                            Err(errno) => Some(BackendPlan::Err(errno)),
                        };
                        let Some(plan) = plan else {
                            self.fail_unsubmitted_file_io_request(&request, Errno::ENOSYS);
                            work.push(PageServiceDrivenWork::UnplannedSubmission(request));
                            continue;
                        };
                        let dispatch = dispatch_backend_plan(plan);
                        let outcome = self
                            .page_submission
                            .with_service(|service| service.consume_backend_dispatch(dispatch));
                        let queued = match &mut block_target {
                            FileBlockSubmissionTarget::External {
                                block_queue,
                                tracker,
                            } => {
                                // The compatibility caller cannot route tagged completions into
                                // this PageContainer's private graph registry.
                                let queued = match outcome {
                                    PageServiceBackendOutcome::BlockGraph(_) => {
                                        Ok(PageServiceBackendSubmitOutcome::Err {
                                            request: request.clone(),
                                            errno: Errno::ENOSYS,
                                        })
                                    }
                                    outcome => self.page_submission.with_service(|service| {
                                        service.queue_backend_outcome(
                                            outcome,
                                            block_queue,
                                            request.clone(),
                                        )
                                    }),
                                };
                                if let (Ok(outcome), Some(tracker)) = (&queued, tracker.as_mut()) {
                                    record_file_service_block_submissions(tracker, outcome);
                                }
                                queued
                            }
                            FileBlockSubmissionTarget::Owned => {
                                self.submit_owned_file_backend_outcome(outcome, request.clone())
                            }
                        };
                        match queued {
                            Ok(outcome @ PageServiceBackendSubmitOutcome::Err { errno, .. }) => {
                                self.fail_unsubmitted_file_io_request(&rollback_request, errno);
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome));
                            }
                            Ok(outcome) => {
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome));
                            }
                            Err(error) => {
                                self.fail_unsubmitted_file_io_request(
                                    &rollback_request,
                                    backend_submit_errno(error),
                                );
                                work.push(PageServiceDrivenWork::BackendSubmitError(error));
                            }
                        }
                    }
                    PageServiceWork::BackendResume {
                        page_request,
                        resume,
                    } => {
                        let Some(plan) = context.resume_submission(resume) else {
                            self.fail_unsubmitted_file_io_request(&page_request, Errno::ENOSYS);
                            work.push(PageServiceDrivenWork::UnplannedSubmission(page_request));
                            continue;
                        };
                        let dispatch = dispatch_backend_plan(plan);
                        let outcome = self
                            .page_submission
                            .with_service(|service| service.consume_backend_dispatch(dispatch));
                        let queued = match &mut block_target {
                            FileBlockSubmissionTarget::External {
                                block_queue,
                                tracker,
                            } => {
                                // The compatibility caller cannot route tagged completions into
                                // this PageContainer's private graph registry.
                                let queued = match outcome {
                                    PageServiceBackendOutcome::BlockGraph(_) => {
                                        Ok(PageServiceBackendSubmitOutcome::Err {
                                            request: page_request.clone(),
                                            errno: Errno::ENOSYS,
                                        })
                                    }
                                    outcome => self.page_submission.with_service(|service| {
                                        service.queue_backend_outcome(
                                            outcome,
                                            block_queue,
                                            page_request.clone(),
                                        )
                                    }),
                                };
                                if let (Ok(outcome), Some(tracker)) = (&queued, tracker.as_mut()) {
                                    record_file_service_block_submissions(tracker, outcome);
                                }
                                queued
                            }
                            FileBlockSubmissionTarget::Owned => self
                                .submit_owned_file_backend_outcome(outcome, page_request.clone()),
                        };
                        match queued {
                            Ok(outcome @ PageServiceBackendSubmitOutcome::Err { errno, .. }) => {
                                self.fail_unsubmitted_file_io_request(&page_request, errno);
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome));
                            }
                            Ok(outcome) => {
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome))
                            }
                            Err(error) => {
                                self.fail_unsubmitted_file_io_request(
                                    &page_request,
                                    backend_submit_errno(error),
                                );
                                work.push(PageServiceDrivenWork::BackendSubmitError(error))
                            }
                        }
                    }
                }
            }
        }

        let next = self
            .page_submission
            .with_service(|service| service.drive_turn(ServiceBudget::new(0)).next);
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
        let mut pending_direct_completion = self
            .retry_pending_direct_io_completions()
            .map_err(|_| PageServiceTaggedBlockCompletionError::ExternalCompletion)?;
        let driven = self.block_submission.drive(budget, &mut kick);

        for dispatch in &driven.dispatches {
            executor.submit(dispatch);
        }

        let mut device_completions = 0usize;
        let mut page_completions = 0usize;
        let mut kicks = driven.kicks;
        let mut next = driven.next;
        while let Some(completion) = executor.poll_completion() {
            device_completions += 1;
            let receipt = self.block_submission.complete_receipt(completion)?;
            let mut prepared = self.page_submission.prepare_block_completion_routes(
                receipt.block,
                receipt.page,
                !receipt.direct.is_empty(),
                &mut frame_for,
            )?;
            for action in prepared.actions.drain(..) {
                let receipt = self.block_submission.submit_page_action(action);
                let submitted = receipt.submitted.len();
                let applied = self
                    .page_submission
                    .apply_l6_receipt(receipt)
                    .map_err(|_| PageServiceTaggedBlockCompletionError::ExternalCompletion)?;
                self.block_submission.record_page_outcome(&applied.outcome);
                prepared.outcome.block_submitted += submitted;
                if let PageServiceBackendSubmitOutcome::QueuedPageCompletions { queued, wake } =
                    applied.outcome
                {
                    prepared.outcome.queued += queued;
                    prepared.outcome.wake = prepared.outcome.wake.or(wake);
                }
            }
            let outcome = prepared.outcome;
            let direct_completions = receipt.direct;
            for (lease, result) in direct_completions {
                match self.complete_file_direct_submission(lease, result) {
                    Ok(_) => {}
                    Err(DirectIoCompletionError::Backend(Errno::EAGAIN))
                        if self.direct_io_completion_is_pending(lease) =>
                    {
                        pending_direct_completion = true;
                    }
                    Err(_) => {
                        return Err(PageServiceTaggedBlockCompletionError::ExternalCompletion);
                    }
                }
            }
            page_completions += outcome.queued;
            if outcome.wake.is_some() {
                kicks += usize::from(kick(ServiceKick::new(IoServiceKind::Page)));
            }
            if outcome.block_submitted != 0 {
                next = BlockServiceNext::Runnable;
                kicks += usize::from(kick(ServiceKick::new(IoServiceKind::Block)));
            }
        }

        if pending_direct_completion {
            next = BlockServiceNext::Runnable;
            kicks += usize::from(kick(ServiceKick::new(IoServiceKind::Block)));
        }

        Ok(FileBlockServiceTurn {
            dispatched: driven.dispatches.len(),
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
        if route.completion.kind == PageIoCompletionKind::Noop {
            if self
                .page_submission
                .take_background_graph(route.completion.id)
            {
                self.notify_file_background_completion(&route.completion);
                return None;
            }
            if self.terminalize_file_fsync_submission(&route.completion) {
                self.notify_file_backend_completion(&route.completion);
            }
            return None;
        }
        if !matches!(
            route.completion.kind,
            PageIoCompletionKind::WritebackFinished | PageIoCompletionKind::ReadInstalled
        ) {
            return None;
        }
        let request = self.page_submission.file_request(route.completion.id)?;
        if route.completion.range != request.range
            || (request.op == PageIoOp::Read && request.range.page_count() != 1)
        {
            return self
                .finish_owned_file_io_request(&request, FileIoTerminalResult::SubmitFailure);
        }
        let kind_matches = matches!(
            (request.op, route.completion.kind),
            (
                PageIoOp::Read | PageIoOp::Readahead,
                PageIoCompletionKind::ReadInstalled
            ) | (PageIoOp::Writeback, PageIoCompletionKind::WritebackFinished)
                | (
                    PageIoOp::Fsync | PageIoOp::Checkpoint,
                    PageIoCompletionKind::Noop
                )
        );
        if !kind_matches {
            return self
                .finish_owned_file_io_request(&request, FileIoTerminalResult::SubmitFailure);
        }
        self.finish_owned_file_io_request(
            &request,
            FileIoTerminalResult::Completion {
                result: route.completion.result,
                kind: route.completion.kind,
                generation: route.completion.generation,
                frame: route.frame,
                notify_waiters: !route.waiters.is_empty(),
            },
        )
    }

    fn queue_file_background_graph(
        &self,
        planner: &dyn crate::fs_iface::BackendPlanner,
        object: FsObjectKey,
        graph: crate::fs_iface::BackendBioGraph,
    ) {
        let queued = {
            let request = self.page_submission.with_service(|service| {
                service.reserve_background_request(
                    self.io_manager_key(),
                    PageIoRange::new(0, self.page_count),
                )
            });
            match request {
                Ok(request) => match self.submit_owned_file_backend_outcome(
                    PageServiceBackendOutcome::BlockGraph(graph),
                    request.clone(),
                ) {
                    Ok(PageServiceBackendSubmitOutcome::Err { errno, .. }) => Err(errno),
                    Ok(_) => {
                        self.page_submission.mark_background_graph(request.id);
                        Ok(())
                    }
                    Err(_) => Err(Errno::EIO),
                },
                Err(_) => Err(Errno::EBUSY),
            }
        };
        if let Err(errno) = queued {
            planner.complete_background_graph(object, Err(errno));
        }
    }

    /// Cross the manager boundary as values: L4 prepares semantic state, L6
    /// owns queue mutation, then L4 accepts the exact receipt. No manager lock
    /// is held while entering the other manager.
    fn submit_owned_file_backend_outcome(
        &self,
        outcome: PageServiceBackendOutcome,
        request: PageIoRequest,
    ) -> Result<PageServiceBackendSubmitOutcome, PageServiceBackendSubmitError> {
        match self
            .page_submission
            .prepare_backend_outcome(outcome, request)
        {
            Ok(PageServiceBackendPrepared::Local(outcome)) => Ok(outcome),
            Ok(PageServiceBackendPrepared::Submit(action)) => {
                let receipt = self.block_submission.submit_page_action(action);
                let applied = self.page_submission.apply_l6_receipt(receipt)?;
                self.block_submission.record_page_outcome(&applied.outcome);
                Ok(applied.outcome)
            }
            Err(error) => Err(error),
        }
    }

    fn notify_file_background_completion(
        &self,
        completion: &crate::io_manager::page::PageIoCompletion,
    ) {
        let PageContainerKind::File {
            mount,
            fs_object_id,
        } = &self.kind
        else {
            return;
        };
        let result = match completion.result {
            PageIoResult::Done => Ok(()),
            PageIoResult::Err(errno) => Err(errno),
        };
        let Some(planner) = mount.payload().backend_planner() else {
            return;
        };
        planner.complete_background_graph(FsObjectKey::new(fs_object_id.as_u64()), result);
    }

    fn terminalize_file_fsync_submission(
        &self,
        completion: &crate::io_manager::page::PageIoCompletion,
    ) -> bool {
        let result = match completion.result {
            PageIoResult::Done => Ok(()),
            PageIoResult::Err(errno) => Err(errno),
        };
        self.state
            .lock()
            .fsync_submissions
            .get_mut(&completion.id)
            .is_some_and(|submission| submission.complete(result))
    }

    fn notify_file_backend_completion(
        &self,
        completion: &crate::io_manager::page::PageIoCompletion,
    ) {
        let PageContainerKind::File {
            mount,
            fs_object_id,
        } = &self.kind
        else {
            return;
        };
        let Some(planner) = mount.payload().backend_planner() else {
            return;
        };
        planner.complete_page_io(crate::fs_iface::BackendPageCompletion::new(
            FsObjectKey::new(fs_object_id.as_u64()),
            completion.id,
            PageIoOp::Fsync,
            completion.result,
        ));
        if completion.result != PageIoResult::Done {
            return;
        }
        let object = FsObjectKey::new(fs_object_id.as_u64());
        match planner.take_background_graph(object) {
            Ok(Some(graph)) => self.queue_file_background_graph(planner, object, graph),
            Ok(None) | Err(_) => {}
        }
    }

    fn prepare_owned_file_io_request(
        &self,
        request: &PageIoRequest,
    ) -> (IoDataSource, IoDataTarget) {
        self.page_submission.file_request_data(request.id)
    }

    /// Resolve a completed buffered read back to the target retained by its
    /// L4 owner.
    ///
    /// The block queue may merge adjacent BIOs from different page requests.
    /// In that case the completed BIO contains several vectors, so selecting
    /// `plan.vecs.first()` would install the first request's frame for every
    /// merged page.  The admitted request remains the authoritative mapping
    /// from page request to destination frame until terminal completion.
    pub(crate) fn file_io_read_target_for_completion(
        &self,
        completion: &BlockPageCompletion,
    ) -> Option<PageFrameRef> {
        if completion.block_completion().result.is_err()
            || !matches!(completion.request().op, PageIoOp::Read | PageIoOp::Readahead)
        {
            return None;
        }
        match self
            .page_submission
            .file_request_data(completion.request().id)
            .1
        {
            IoDataTarget::PageCache {
                frame,
                offset: 0,
                len,
                ..
            } if len != 0 => Some(frame),
            _ => None,
        }
    }

    fn fail_unsubmitted_file_io_request(&self, request: &PageIoRequest, errno: Errno) {
        self.finish_owned_file_io_request(request, FileIoTerminalResult::SubmitFailure);
        if request.op != PageIoOp::Fsync {
            return;
        }
        self.page_submission.with_service(|service| {
            service.push_completion(PageIoCompletion::new(
                request.id,
                request.range,
                PageIoResult::Err(errno),
                request.generation_hint.unwrap_or(PageGeneration::new(0)),
                PageIoCompletionKind::Noop,
            ));
        });
    }

    /// Consume the authoritative request owner and perform its one terminal
    /// PageSlot transition. Every submission failure and L4 completion reaches
    /// this method; stale terminal routes still release their retained bundle.
    fn finish_owned_file_io_request(
        &self,
        request: &PageIoRequest,
        terminal: FileIoTerminalResult,
    ) -> Option<Result<PageSlotSnapshot, PageSlotCompletionError>> {
        let (owner, _) = self.page_submission.take_file_request(request.id);
        debug_assert!(owner
            .as_ref()
            .is_none_or(|owner| owner.request().id == request.id));
        // PageService deliberately supports a completion racing ahead of the
        // submission turn. For PageBacked-owned I/O, this is the sole terminal
        // route, so retire the matching L4 row only after consuming its owner.
        let completion_requires_payload =
            matches!(terminal, FileIoTerminalResult::Completion { .. });
        let Some(owner_ref) = owner.as_ref() else {
            return None;
        };
        if completion_requires_payload && !owner_ref.completes(request.op) {
            // This is an owner-poison terminal route. It must consume the bad
            // record and L4 row, but cannot independently settle PageSlot.
            drop(owner);
            return None;
        }

        match request.op {
            PageIoOp::Writeback => {
                let lease = owner.and_then(|owner| match owner.take_payload() {
                    FileIoPayload::Writeback { lease } => Some(lease),
                    FileIoPayload::Read { .. } | FileIoPayload::Control => None,
                })?;
                let state = self.state.lock();
                let first_result = match terminal {
                    FileIoTerminalResult::SubmitFailure => {
                        let mut first_snapshot = None;
                        let mut first_error = None;
                        for (page, generation) in lease.generations() {
                            let result = state.page_slots.get(&page).map_or_else(
                                || {
                                    Err(PageSlotCompletionError::NotFetching {
                                        state: PageSlotState::Empty,
                                        generation: PageGeneration::new(0),
                                    })
                                },
                                |slot| slot.abort_writeback(generation),
                            );
                            match result {
                                Ok(snapshot) if first_snapshot.is_none() => {
                                    first_snapshot = Some(snapshot)
                                }
                                Err(error) if first_error.is_none() => first_error = Some(error),
                                Ok(_) | Err(_) => {}
                            }
                        }
                        first_error.map(Err).or_else(|| first_snapshot.map(Ok))
                    }
                    FileIoTerminalResult::Completion {
                        result,
                        kind: PageIoCompletionKind::WritebackFinished,
                        generation,
                        ..
                    } => {
                        if Some(generation) != request.generation_hint {
                            let current = lease
                                .generations()
                                .next()
                                .and_then(|(page, _)| state.page_slots.get(&page))
                                .map(|slot| slot.generation())
                                .unwrap_or(PageGeneration::new(0));
                            Some(Err(PageSlotCompletionError::GenerationMismatch {
                                current,
                                completed: generation,
                            }))
                        } else {
                            let io_result = match result {
                                PageIoResult::Done => Ok(()),
                                PageIoResult::Err(errno) => Err(errno),
                            };
                            let mut first_snapshot = None;
                            let mut first_error = None;
                            for (page, generation) in lease.generations() {
                                let result = state.page_slots.get(&page).map_or_else(
                                    || {
                                        Err(PageSlotCompletionError::NotFetching {
                                            state: PageSlotState::Empty,
                                            generation: PageGeneration::new(0),
                                        })
                                    },
                                    |slot| slot.complete_writeback(generation, io_result),
                                );
                                match result {
                                    Ok(snapshot) if first_snapshot.is_none() => {
                                        first_snapshot = Some(snapshot)
                                    }
                                    Err(error) if first_error.is_none() => {
                                        first_error = Some(error)
                                    }
                                    Ok(_) | Err(_) => {}
                                }
                            }
                            first_error.map(Err).or_else(|| first_snapshot.map(Ok))
                        }
                    }
                    FileIoTerminalResult::Completion { .. } => None,
                };
                drop(lease);
                first_result
            }
            PageIoOp::Read => match terminal {
                FileIoTerminalResult::SubmitFailure => {
                    let page = PageIndex::new(request.range.start_page());
                    let fetch_id = self
                        .state
                        .lock()
                        .in_flight_file_pages
                        .get(&page)
                        .filter(|fetch| fetch.request_id == Some(request.id))
                        .map(|fetch| fetch.id);
                    drop(owner);
                    if let Some(fetch_id) = fetch_id {
                        self.finish_file_page_fetch_without_install(page, fetch_id);
                    }
                    None
                }
                FileIoTerminalResult::Completion {
                    result,
                    kind: PageIoCompletionKind::ReadInstalled,
                    generation,
                    frame,
                    notify_waiters,
                } => {
                    let page = PageIndex::new(request.range.start_page());
                    let target = owner.and_then(|owner| match owner.take_payload() {
                        FileIoPayload::Read { target } => Some(target),
                        FileIoPayload::Writeback { .. } | FileIoPayload::Control => None,
                    });
                    let result = match result {
                        PageIoResult::Err(errno) => {
                            let state = self.state.lock();
                            let slot = state.page_slots.get(&page)?;
                            slot.complete_fetch(generation, Err(errno))
                        }
                        PageIoResult::Done => match (frame, target) {
                            (Some(frame), Some(target)) if target.ppn == frame.ppn() => {
                                self.apply_file_io_cached_read_completion(page, generation, target)
                            }
                            (Some(frame), target) => {
                                drop(target);
                                self.apply_file_io_read_frame_completion(page, generation, frame)
                            }
                            (None, Some(target)) => {
                                self.apply_file_io_cached_read_completion(page, generation, target)
                            }
                            (None, None) => {
                                let state = self.state.lock();
                                let slot = state.page_slots.get(&page)?;
                                slot.complete_fetch(generation, Err(Errno::EIO))
                            }
                        },
                    };
                    if result.is_ok() {
                        self.finish_file_page_fetch_after_service_completion(
                            page,
                            generation,
                            notify_waiters,
                        );
                    }
                    Some(result)
                }
                FileIoTerminalResult::Completion { .. } => None,
            },
            _ => None,
        }
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
                let Some(slot) = state.page_slots.get(&page) else {
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
        let existing = self.state.lock().pages.lookup(page);
        if let Some(ppn) = existing {
            let state = self.state.lock();
            let Some(slot) = state.page_slots.get(&page) else {
                return Err(PageSlotCompletionError::NotFetching {
                    state: PageSlotState::Empty,
                    generation: PageGeneration::new(0),
                });
            };
            return slot.complete_fetch(generation, Ok(ppn));
        }

        match self.install_fetched_resident_if_absent_published(page, generation, cached) {
            Ok(true) => Ok(self
                .state
                .lock()
                .page_slots
                .get(&page)
                .expect("published fetched page retains its PageSlot")
                .snapshot()),
            Ok(false) => {
                let state = self.state.lock();
                let ppn = state
                    .pages
                    .lookup(page)
                    .ok_or(PageSlotCompletionError::NotFetching {
                        state: PageSlotState::Empty,
                        generation: PageGeneration::new(0),
                    })?;
                state
                    .page_slots
                    .get(&page)
                    .ok_or(PageSlotCompletionError::NotFetching {
                        state: PageSlotState::Empty,
                        generation: PageGeneration::new(0),
                    })?
                    .complete_fetch(generation, Ok(ppn))
            }
            Err(error) => Err(PageSlotCompletionError::Backend(page_cache_error_to_errno(
                error,
            ))),
        }
    }

    #[cfg(test)]
    fn file_io_request_count_for_test(&self) -> usize {
        self.page_submission
            .with_service(|service| service.submission_len())
    }

    #[cfg(test)]
    fn file_io_owner_count_for_test(&self) -> usize {
        self.page_submission.admitted_file_request_count()
    }

    #[cfg(test)]
    fn file_io_lease_count_for_test(&self) -> usize {
        self.page_submission.admitted_file_writeback_count()
    }

    #[cfg(test)]
    fn file_io_read_target_count_for_test(&self) -> usize {
        self.page_submission.admitted_file_read_count()
    }

    #[cfg(test)]
    fn file_io_pending_request_for_test(&self, page: PageIndex) -> Option<PageIoRequest> {
        self.page_submission.with_service(|service| {
            service
                .find_submission(
                    self.io_manager_key(),
                    PageIoRange::new(page.as_u64(), 1),
                    PageIoOp::Read,
                )
                .cloned()
        })
    }

    #[cfg(test)]
    fn file_io_waiter_count_for_test(&self, page: PageIndex) -> usize {
        let state = self.state.lock();
        let Some(fetch) = state.in_flight_file_pages.get(&page) else {
            return 0;
        };
        fetch
            .request_id
            .map(|id| {
                self.page_submission
                    .with_service(|service| service.waiter_count(id))
            })
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn file_io_block_queue_len_for_test(&self) -> usize {
        self.block_submission.queue_len_for_test()
    }

    #[cfg(test)]
    fn file_io_block_tracker_len_for_test(&self) -> usize {
        self.block_submission.tracker_len_for_test()
    }

    #[cfg(test)]
    fn file_page_fetch_in_flight_for_test(&self, page: PageIndex) -> bool {
        self.state.lock().in_flight_file_pages.contains_key(&page)
    }

    #[cfg(test)]
    fn page_slot_snapshot_for_test(&self, page: PageIndex) -> Option<PageSlotSnapshot> {
        self.state
            .lock()
            .page_slots
            .get(&page)
            .map(|slot| slot.snapshot())
    }

    #[cfg(test)]
    fn resident_binding_pin_count_for_test(&self, page: PageIndex) -> usize {
        self.state
            .lock()
            .pages
            .load(page)
            .map(|entry| entry.cell.binding_evidence_count())
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn resident_binding_is_device_for_test(&self, page: PageIndex) -> bool {
        self.state
            .lock()
            .pages
            .load(page)
            .is_some_and(|entry| entry.cell.binding_is_device())
    }

    pub fn lookup(&self, page: PageIndex) -> Option<Ppn> {
        self.state.lock().pages.lookup(page)
    }

    /// Borrow one resident binding from the immutable root under the caller's
    /// epoch guard. This never takes the PageContainer or I/O-manager lock.
    fn lookup_resident_with_guard<'g>(
        &'g self,
        guard: &'g Guard<'_>,
        page: PageIndex,
    ) -> Option<ResidentHit<'g>> {
        ResidentHit::from_root(self.resident.read(guard), guard, page)
    }

    fn materialize_published_read(
        &self,
        page: PageIndex,
        guard: &Guard<'_>,
    ) -> Option<Result<MaterializedPage, PageCacheError>> {
        let hit = self.lookup_resident_with_guard(guard, page)?;
        let materialized = match hit.try_materialize() {
            Ok(Some(materialized)) => materialized,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        Some(Ok(MaterializedPage {
            ppn: hit.ppn(),
            map_pin: materialized.0,
            newly_installed: false,
            dirty: materialized.1,
        }))
    }

    fn prepare_resident_root_mutation(
        &self,
        update: impl FnOnce(ResidentRoot) -> Result<ResidentRoot, PageCacheError>,
    ) -> Result<PreparedResidentRoot<'_>, PageCacheError> {
        #[cfg(test)]
        if FORCE_RESIDENT_ROOT_RETIRE_BACKPRESSURE_FOR_TEST.swap(false, Ordering::AcqRel) {
            return Err(PageCacheError::Backend(Errno::EAGAIN));
        }
        let claim = ResidentMutationClaim::try_acquire(&self.resident_sequence)
            .ok_or(PageCacheError::Backend(Errno::EAGAIN))?;
        let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
        let next = update(self.resident.read(&guard).clone())?;
        let publication = self
            .resident
            .prepare_detached(next)
            .map_err(|_| PageCacheError::Backend(Errno::EAGAIN))?;
        let retire = (0..RESIDENT_ROOT_RETIRE_MAINTENANCE_ATTEMPTS)
            .find_map(
                |attempt| match tx_substrate::epoch::try_reserve_local_retire() {
                    Ok(reservation) => Some(Ok(reservation)),
                    Err(tx_substrate::epoch::EpochError::LocalRetireExhausted)
                        if attempt + 1 < RESIDENT_ROOT_RETIRE_MAINTENANCE_ATTEMPTS =>
                    {
                        let _ = tx_substrate::epoch::drain_with_budget(
                            RESIDENT_ROOT_RETIRE_MAINTENANCE_BUDGET,
                        );
                        None
                    }
                    Err(_) => Some(Err(PageCacheError::Backend(Errno::EAGAIN))),
                },
            )
            .unwrap_or(Err(PageCacheError::Backend(Errno::EAGAIN)))?;
        Ok(PreparedResidentRoot {
            publication,
            retire,
            claim,
        })
    }

    /// Exercise the allocation path before an external file operation. This
    /// deliberately owns neither the resident writer claim nor a retire credit,
    /// so it is safe to drop before a filesystem call can yield.
    fn preflight_resident_batch_withdrawal(
        &self,
        candidates: &[(PageIndex, Ppn)],
    ) -> Result<(), PageCacheError> {
        let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
        let mut next = self.resident.read(&guard).clone();
        for (page, _) in candidates {
            next = next.without_cell(*page);
        }
        let _ = self
            .resident
            .prepare_detached(next)
            .map_err(|_| PageCacheError::Backend(Errno::EAGAIN))?;
        Ok(())
    }

    fn service_resident_root_retire_maintenance(&self) {
        // A successful root replacement has consumed a CPU-local retire
        // credit. Service one bounded drain after releasing PageContainerState
        // so consecutive fault/install turns advance epochs and replenish the
        // pool without making an admitted publication fallible.
        let _ = tx_substrate::epoch::drain_with_budget(RESIDENT_ROOT_RETIRE_MAINTENANCE_BUDGET);
    }

    fn install_resident_if_absent_published(
        &self,
        page: PageIndex,
        frame: CachedFrame,
    ) -> Result<bool, PageCacheError> {
        self.install_resident_if_absent_published_with(page, frame, |_| Ok(()))
    }

    fn install_resident_if_absent_published_with(
        &self,
        page: PageIndex,
        frame: CachedFrame,
        before_publish: impl FnOnce(&PageSlot) -> Result<(), PageCacheError>,
    ) -> Result<bool, PageCacheError> {
        let ppn = frame.ppn;
        let slot = {
            let mut state = self.state.lock();
            if state.pages.lookup(page).is_some() {
                return Ok(false);
            }
            state.resident_slot(page)
        };
        let cell = Arc::new(ResidentCell::from_cached_frame(frame, slot));
        let prepared = self
            .prepare_resident_root_mutation(|root| Ok(root.with_cell(page, Arc::clone(&cell))))?;

        let mut state = self.state.lock();
        if state.pages.lookup(page).is_some() {
            return Ok(false);
        }
        state
            .ensure_resident_page_slot(page, ppn)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        let slot = state
            .page_slots
            .get(&page)
            .expect("published resident page retains its PageSlot");
        before_publish(slot)?;
        state.pages.insert(
            page,
            PageCacheEntry {
                cell,
                marks: PageMarks {
                    referenced: true,
                    ..PageMarks::new()
                },
            },
        )?;
        prepared.commit(&self.resident);
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(true)
    }

    fn replace_resident_if_match_published(
        &self,
        page: PageIndex,
        expected_ppn: Ppn,
        expected_generation: PageGeneration,
        frame: CachedFrame,
    ) -> Result<Ppn, PageCacheError> {
        let replacement_ppn = frame.ppn;
        let (expected_cell, slot, marks) = {
            let state = self.state.lock();
            let current = state.pages.load(page).ok_or(PageCacheError::MissingPage)?;
            if current.ppn() != expected_ppn {
                return Err(PageCacheError::MismatchedFrame {
                    current: current.ppn(),
                });
            }
            let slot = Arc::clone(current.cell.slot());
            if slot.snapshot().generation != expected_generation {
                return Err(PageCacheError::MismatchedFrame {
                    current: current.ppn(),
                });
            }
            (Arc::clone(&current.cell), slot, current.marks)
        };
        let replacement = Arc::new(ResidentCell::from_cached_frame(frame, Arc::clone(&slot)));
        let prepared = self.prepare_resident_root_mutation(|root| {
            Ok(root.with_cell(page, Arc::clone(&replacement)))
        })?;

        let mut state = self.state.lock();
        let current = state.pages.load(page).ok_or(PageCacheError::MissingPage)?;
        if !Arc::ptr_eq(&current.cell, &expected_cell) {
            return Err(PageCacheError::MismatchedFrame {
                current: current.ppn(),
            });
        }
        slot.replace_if_matches(expected_generation, expected_ppn, replacement_ppn)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        current.cell.mark_withdrawn();
        state
            .pages
            .compare_replace(
                page,
                |entry| Arc::ptr_eq(&entry.cell, &expected_cell),
                Some(PageCacheEntry {
                    cell: replacement,
                    marks,
                }),
            )
            .expect("validated resident replacement invariant");
        prepared.commit(&self.resident);
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(replacement_ppn)
    }

    /// Remove a stable set of resident bindings with one root publication.
    /// Callers must retain a range reservation covering every candidate.
    fn withdraw_resident_batch_published(
        &self,
        candidates: &[(PageIndex, Ppn)],
        require_clean: bool,
    ) -> Result<usize, PageCacheError> {
        let expected: Vec<ResidentWithdrawal> = {
            let state = self.state.lock();
            candidates
                .iter()
                .filter_map(|(page, ppn)| {
                    let entry = state.pages.load(*page)?;
                    if entry.ppn() != *ppn {
                        return None;
                    }
                    let slot = Arc::clone(entry.cell.slot());
                    let snapshot = slot.snapshot();
                    let matches_ppn = matches!(
                        snapshot.state,
                        PageSlotState::Resident { ppn: found }
                            | PageSlotState::Dirty { ppn: found }
                            | PageSlotState::Writeback { ppn: found, .. }
                            if found == *ppn
                    );
                    if !matches_ppn
                        || (require_clean
                            && snapshot.state != PageSlotState::Resident { ppn: *ppn })
                    {
                        return None;
                    }
                    Some(ResidentWithdrawal {
                        page: *page,
                        ppn: *ppn,
                        cell: Arc::clone(&entry.cell),
                        slot,
                        generation: snapshot.generation,
                    })
                })
                .collect()
        };
        if expected.is_empty() {
            return Ok(0);
        }

        let prepared = self.prepare_resident_root_mutation(|mut root| {
            for withdrawal in &expected {
                let Some(root_cell) = root.lookup(withdrawal.page) else {
                    return Err(PageCacheError::MissingPage);
                };
                if !core::ptr::eq(root_cell, Arc::as_ptr(&withdrawal.cell)) {
                    return Err(PageCacheError::MismatchedFrame {
                        current: root_cell.ppn(),
                    });
                }
                root = root.without_cell(withdrawal.page);
            }
            Ok(root)
        })?;

        let mut state = self.state.lock();
        for withdrawal in &expected {
            let current = state
                .pages
                .load(withdrawal.page)
                .ok_or(PageCacheError::MissingPage)?;
            let snapshot = withdrawal.slot.snapshot();
            let matches_ppn = matches!(
                snapshot.state,
                PageSlotState::Resident { ppn }
                    | PageSlotState::Dirty { ppn }
                    | PageSlotState::Writeback { ppn, .. }
                    if ppn == withdrawal.ppn
            );
            if !Arc::ptr_eq(&current.cell, &withdrawal.cell)
                || current.ppn() != withdrawal.ppn
                || snapshot.generation != withdrawal.generation
                || !matches_ppn
                || (require_clean
                    && snapshot.state
                        != PageSlotState::Resident {
                            ppn: withdrawal.ppn,
                        })
            {
                return Err(PageCacheError::Backend(Errno::ESTALE));
            }
        }

        // The range claim and exact revalidation above make these transitions
        // infallible. Do not introduce a fallible edge after the first slot.
        for withdrawal in &expected {
            withdrawal
                .slot
                .withdraw_if_matches(withdrawal.generation, withdrawal.ppn)
                .expect("validated batch withdrawal slot invariant");
        }
        for withdrawal in &expected {
            withdrawal.cell.mark_withdrawn();
        }
        prepared.commit(&self.resident);
        for withdrawal in &expected {
            state
                .pages
                .compare_replace(
                    withdrawal.page,
                    |entry| Arc::ptr_eq(&entry.cell, &withdrawal.cell),
                    None,
                )
                .expect("published batch withdrawal invariant");
        }
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(expected.len())
    }

    /// Withdraw one resident binding through the immutable-root transaction.
    ///
    /// The caller first identifies a candidate while holding whichever range
    /// reservation protects its operation. This helper then reserves root
    /// retirement before changing the slot. Once the slot changes, publishing
    /// removal is infallible and precedes erasing the compatibility index row.
    fn withdraw_resident_if_match_published(
        &self,
        page: PageIndex,
        expected_ppn: Ppn,
        require_clean: bool,
    ) -> Result<bool, PageCacheError> {
        let (expected_cell, slot, expected_generation) = {
            let state = self.state.lock();
            let Some(entry) = state.pages.load(page) else {
                return Ok(false);
            };
            if entry.ppn() != expected_ppn {
                return Ok(false);
            }
            let slot = Arc::clone(entry.cell.slot());
            let snapshot = slot.snapshot();
            let slot_matches = matches!(
                snapshot.state,
                PageSlotState::Resident { ppn }
                    | PageSlotState::Dirty { ppn }
                    | PageSlotState::Writeback { ppn, .. }
                    if ppn == expected_ppn
            );
            if !slot_matches
                || (require_clean
                    && snapshot.state != PageSlotState::Resident { ppn: expected_ppn })
            {
                return Ok(false);
            }
            (Arc::clone(&entry.cell), slot, snapshot.generation)
        };

        let prepared = match self.prepare_resident_root_mutation(|root| {
            let Some(root_cell) = root.lookup(page) else {
                return Err(PageCacheError::MissingPage);
            };
            if !core::ptr::eq(root_cell, Arc::as_ptr(&expected_cell)) {
                return Err(PageCacheError::MismatchedFrame {
                    current: root_cell.ppn(),
                });
            }
            Ok(root.without_cell(page))
        }) {
            Ok(prepared) => prepared,
            Err(PageCacheError::MissingPage | PageCacheError::MismatchedFrame { .. }) => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };

        let mut state = self.state.lock();
        let Some(current) = state.pages.load(page) else {
            return Ok(false);
        };
        if !Arc::ptr_eq(&current.cell, &expected_cell) || current.ppn() != expected_ppn {
            return Ok(false);
        }
        let snapshot = slot.snapshot();
        let slot_matches = matches!(
            snapshot.state,
            PageSlotState::Resident { ppn }
                | PageSlotState::Dirty { ppn }
                | PageSlotState::Writeback { ppn, .. }
                if ppn == expected_ppn
        );
        if snapshot.generation != expected_generation
            || !slot_matches
            || (require_clean && snapshot.state != PageSlotState::Resident { ppn: expected_ppn })
        {
            return Ok(false);
        }

        slot.withdraw_if_matches(expected_generation, expected_ppn)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        expected_cell.mark_withdrawn();
        prepared.commit(&self.resident);
        state
            .pages
            .compare_replace(page, |entry| Arc::ptr_eq(&entry.cell, &expected_cell), None)
            .expect("published resident withdrawal invariant");
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(true)
    }

    fn install_fetched_resident_if_absent_published(
        &self,
        page: PageIndex,
        generation: PageGeneration,
        frame: CachedFrame,
    ) -> Result<bool, PageCacheError> {
        let ppn = frame.ppn;
        let slot = {
            let mut state = self.state.lock();
            if state.pages.lookup(page).is_some() {
                return Ok(false);
            }
            state.resident_slot(page)
        };
        let cell = Arc::new(ResidentCell::from_cached_frame(frame, slot));
        let prepared = self
            .prepare_resident_root_mutation(|root| Ok(root.with_cell(page, Arc::clone(&cell))))?;

        let mut state = self.state.lock();
        if state.pages.lookup(page).is_some() {
            return Ok(false);
        }
        state
            .page_slots
            .get(&page)
            .expect("fetched resident page has a PageSlot")
            .complete_fetch(generation, Ok(ppn))
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        state.pages.insert(
            page,
            PageCacheEntry {
                cell,
                marks: PageMarks {
                    referenced: true,
                    ..PageMarks::new()
                },
            },
        )?;
        prepared.commit(&self.resident);
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(true)
    }

    #[cfg(test)]
    fn lookup_resident_with_guard_for_test<'g>(
        &'g self,
        guard: &'g Guard<'_>,
        page: PageIndex,
    ) -> Option<ResidentHit<'g>> {
        self.lookup_resident_with_guard(guard, page)
    }

    #[cfg(test)]
    fn publish_device_resident_for_test(
        &self,
        page: PageIndex,
        ppn: Ppn,
    ) -> Result<(), PageCacheError> {
        let cell = {
            let mut state = self.state.lock();
            if let Some(entry) = state.pages.load(page) {
                return Err(PageCacheError::AlreadyPresent {
                    current: entry.ppn(),
                });
            }
            let slot = state.resident_slot(page);
            let cell = Arc::new(ResidentCell::from_cached_frame(
                CachedFrame {
                    ppn,
                    pin: PageCachePin::Device(DeviceFrame::new(ppn)),
                },
                slot,
            ));
            cell
        };
        let prepared = self
            .prepare_resident_root_mutation(|root| Ok(root.with_cell(page, Arc::clone(&cell))))?;
        let mut state = self.state.lock();
        if let Some(entry) = state.pages.load(page) {
            return Err(PageCacheError::AlreadyPresent {
                current: entry.ppn(),
            });
        }
        state
            .ensure_resident_page_slot(page, ppn)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        state.pages.insert(
            page,
            PageCacheEntry {
                cell,
                marks: PageMarks {
                    referenced: true,
                    ..PageMarks::new()
                },
            },
        )?;
        prepared.commit(&self.resident);
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(())
    }

    #[cfg(test)]
    fn force_resident_retire_backpressure_for_test() {
        FORCE_RESIDENT_ROOT_RETIRE_BACKPRESSURE_FOR_TEST.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn try_replace_resident_for_test(
        &self,
        page: PageIndex,
        replacement_ppn: Ppn,
    ) -> Result<(), PageCacheError> {
        let (expected_ppn, expected_generation, slot) = {
            let state = self.state.lock();
            let entry = state.pages.load(page).ok_or(PageCacheError::MissingPage)?;
            let slot = Arc::clone(entry.cell.slot());
            let snapshot = slot.snapshot();
            (entry.ppn(), snapshot.generation, slot)
        };
        let replacement = Arc::new(ResidentCell::from_cached_frame(
            CachedFrame {
                ppn: replacement_ppn,
                pin: PageCachePin::Device(DeviceFrame::new(replacement_ppn)),
            },
            Arc::clone(&slot),
        ));
        let prepared = self.prepare_resident_root_mutation(|root| {
            Ok(root.with_cell(page, Arc::clone(&replacement)))
        })?;

        let mut state = self.state.lock();
        let current = state.pages.load(page).ok_or(PageCacheError::MissingPage)?;
        if current.ppn() != expected_ppn || !Arc::ptr_eq(current.cell.slot(), &slot) {
            return Err(PageCacheError::MismatchedFrame {
                current: current.ppn(),
            });
        }
        let marks = current.marks;
        slot.replace_if_matches(expected_generation, expected_ppn, replacement_ppn)
            .map_err(page_slot_completion_error_to_page_cache_error)?;
        current.cell.mark_withdrawn();
        state.pages.compare_replace(
            page,
            |entry| entry.ppn() == expected_ppn,
            Some(PageCacheEntry {
                cell: replacement,
                marks,
            }),
        )?;
        prepared.commit(&self.resident);
        drop(state);
        self.service_resident_root_retire_maintenance();
        Ok(())
    }

    pub fn page_marks(&self, page: PageIndex) -> Option<PageMarks> {
        let state = self.state.lock();
        let mut marks = state.pages.marks(page)?;
        if let Some(slot) = state.page_slots.get(&page) {
            match slot.snapshot().state {
                PageSlotState::Dirty { .. } => marks.dirty = true,
                PageSlotState::Writeback { .. } => {
                    marks.dirty = true;
                    marks.writeback = true;
                }
                PageSlotState::Empty
                | PageSlotState::Fetching
                | PageSlotState::Resident { .. }
                | PageSlotState::Error { .. } => {}
            }
        }
        Some(marks)
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

        let installed = self.install_resident_if_absent_published(page, frame)?;
        let installed_dirty = {
            let mut state = self.state.lock();
            if access == MaterializeAccess::Write {
                state.pages.set_mark(page, PageCacheMark::Referenced)?;
                state
                    .page_slots
                    .get(&page)
                    .expect("anon resident page has a PageSlot")
                    .mark_dirty()
                    .map_err(page_slot_completion_error_to_page_cache_error)?;
            }
            installed
                .then(|| {
                    state
                        .page_slots
                        .get(&page)
                        .map(|slot| slot.snapshot())
                        .map(page_slot_is_dirty)
                        .ok_or(PageCacheError::MissingPage)
                })
                .transpose()?
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
        let guard = adapter::step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
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
        let kind = match &self.kind {
            PageContainerKind::Anon { .. } => 1,
            PageContainerKind::File { .. } => 2,
            PageContainerKind::Device { .. } => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.kind", 3);
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.err", 3);
                return StepOutcome::Err(
                    page_cache_error_to_errno(PageCacheError::UnsupportedKind).into(),
                );
            }
        };
        emit_pagebacked_trace(b"debug.pagebacked.fault_step.kind", kind);
        match self.materialize_page(page, access, guard) {
            StepOutcome::Done(page) => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.done", kind);
                StepOutcome::Done(page)
            }
            StepOutcome::Yield { progress, shape } => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.yield", kind);
                StepOutcome::Yield { progress, shape }
            }
            StepOutcome::Err(errno) => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.err", kind);
                StepOutcome::Err(errno)
            }
            StepOutcome::Continue { progress } => {
                emit_pagebacked_trace(b"debug.pagebacked.fault_step.continue", kind);
                StepOutcome::Continue { progress }
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

        if matches!(self.kind, PageContainerKind::File { .. })
            && self.direct_io_active.load(Ordering::Acquire) != 0
        {
            let kind = match access {
                MaterializeAccess::Read => RangeReservationKind::BufferedRead,
                MaterializeAccess::Write => RangeReservationKind::BufferedWrite,
            };
            if self
                .state
                .lock()
                .range_reservations
                .conflicts(PageRange::new(page, 1), kind)
            {
                return StepOutcome::Err(Errno::EBUSY.into());
            }
        }

        if access == MaterializeAccess::Read {
            if let Some(materialized) = self.materialize_published_read(page, guard) {
                return match materialized {
                    Ok(page) => StepOutcome::Done(page),
                    Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
                };
            }
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
        // Persistent anonymous pages are used as private staging storage by
        // the ext4 journal pool.  Once resident, their binding remains valid
        // independently of the transient PageSlot state left by the previous
        // journal I/O.  Re-entering `materialize_page(Read)` here could turn a
        // perfectly reusable resident frame into EAGAIN while that slot is
        // completing.  The caller's epoch guard keeps the published binding
        // alive until the additional cache pin has been acquired.
        if matches!(
            self.kind(),
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Persistent
            }
        ) {
            if let Some(hit) = self.lookup_resident_with_guard(guard, page) {
                let ppn = hit.ppn();
                let cache_pin = match page_allocator::acquire_cache_pin(ppn) {
                    Ok(pin) => pin,
                    Err(error) => {
                        return StepOutcome::Err(
                            page_cache_error_to_errno(PageCacheError::Alloc(error)).into(),
                        );
                    }
                };
                return StepOutcome::Done(PageLease {
                    ppn,
                    cache_pin: PageCachePin::Allocated(cache_pin),
                });
            }
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
                state.pages.set_mark(page, PageCacheMark::Referenced)?;
                state
                    .ensure_resident_page_slot(page, current)
                    .map_err(page_slot_completion_error_to_page_cache_error)?;
                state
                    .page_slots
                    .get(&page)
                    .expect("shared page has a PageSlot")
                    .mark_dirty()
                    .map_err(page_slot_completion_error_to_page_cache_error)?;
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
        if access == MaterializeAccess::Write {
            let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
                return StepOutcome::Err(V3Errno::EINVAL);
            };
            match mount.payload().fs_page_backing.prepare_write_range(
                fs_object_id,
                offset,
                crate::vm::USER_PAGE_SIZE,
                guard,
            ) {
                StepOutcome::Done(()) => {}
                StepOutcome::Continue { .. } => return StepOutcome::Err(V3Errno::EAGAIN),
                StepOutcome::Yield { shape, .. } => {
                    if let Some((carrier, interests)) = notification::wait_source_parts(&shape) {
                        return notification::yield_on_wait_source(NoProgress, carrier, interests);
                    }
                    return StepOutcome::Err(V3Errno::EIO);
                }
                StepOutcome::Err(errno) => return StepOutcome::Err(errno),
            }
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

        let request_id = self
            .state
            .lock()
            .in_flight_file_pages
            .get(&page)
            .and_then(|fetch| fetch.request_id);
        let first = self.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)?;
        if let Some(errno) = file_service_terminal_error_for_request(&first.work, request_id) {
            return Some(StepOutcome::Err(errno.into()));
        }
        let mut waits_for_async_completion =
            file_service_work_waits_for_async_completion(first.work.as_slice());

        if first.next == PageServiceNext::Runnable {
            if let Some(second) =
                self.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true)
            {
                if let Some(errno) =
                    file_service_terminal_error_for_request(&second.work, request_id)
                {
                    return Some(StepOutcome::Err(errno.into()));
                }
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
            // This inline L4 turn has moved demand-read work into the owned
            // L6 block queue. Its fake kick callback cannot wake the runtime
            // task, so publish the real block-service wake before parking on
            // the page-ready endpoint.
            self.kick_file_io_service(IoServiceKind::Block);
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
                state.register_file_io_waiter(&self.page_submission, request_id, source_id);
                endpoint
            } else {
                let request_id = fetch.request_id;
                state.file_page_waits.insert(page, wait);
                if let Some(fetch) = state.in_flight_file_pages.get_mut(&page) {
                    fetch.source_id = Some(new_source_id);
                    fetch.joined = true;
                }
                state.register_file_io_waiter(&self.page_submission, request_id, new_source_id);
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
        // A planner-owned L4 read must retain its destination before admission.
        // Allocate it outside `state`: PageContainer's lock never covers frame
        // allocation, and a direct-pager fetch has no L4 payload to retain.
        let planner_present = matches!(
            &self.kind,
            PageContainerKind::File { mount, .. } if mount.payload().backend_planner().is_some()
        );
        let mut planned_target = if planner_present {
            allocate_cached_frame().ok()
        } else {
            None
        };
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
                    state.register_file_io_waiter(&self.page_submission, request_id, source_id);
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
                        state.register_file_io_waiter(
                            &self.page_submission,
                            request_id,
                            existing_source_id,
                        );
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
                    state.register_file_io_waiter(&self.page_submission, request_id, source_id);
                    return FilePageFetchStart::Joined(endpoint);
                }
                continue;
            }

            let fetch_id = state.allocate_file_fetch_id();
            let generation = match state.page_slots.entry(page).or_default().begin_fetch() {
                PageSlotFetch::Owner { generation }
                | PageSlotFetch::Joined { generation }
                | PageSlotFetch::Resident { generation, .. }
                | PageSlotFetch::Blocked { generation, .. } => generation,
            };
            let request_id = if planner_present {
                planned_target.take().and_then(|target| {
                    self.page_submission.submit_owned_file_request(
                        self.io_manager_key(),
                        PageIoRange::new(page.as_u64(), 1),
                        PageIoOp::Read,
                        PageIoPriority::Demand,
                        PageIoFlags::DEMAND,
                        Some(generation),
                        |request| OwnedFileIoRequest::read(request, target),
                    )
                })
            } else {
                self.page_submission.with_service(|service| {
                    service
                        .submit(
                            self.io_manager_key(),
                            PageIoRange::new(page.as_u64(), 1),
                            PageIoOp::Read,
                            PageIoPriority::Demand,
                            PageIoFlags::DEMAND,
                            Some(generation),
                        )
                        .ok()
                })
            };
            if request_id.is_none() {
                if let Some(slot) = state.page_slots.get(&page) {
                    let _ = slot.invalidate_if_generation(generation);
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
        page_submission: &PageIoSubmissionHandle,
        page: PageIndex,
        fetch: FilePageFetch,
    ) -> Option<notification::PageReadyNotifier> {
        let routed_waiters = fetch.request_id.map_or_else(Vec::new, |request_id| {
            let (owner, routed_waiters) = page_submission.take_file_request(request_id);
            debug_assert!(owner
                .as_ref()
                .is_none_or(|owner| owner.request().id == request_id));
            routed_waiters
        });
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
            if let Some(slot) = state.page_slots.get(&page) {
                let _ = slot.invalidate_if_generation(fetch.generation);
            }
            Self::retire_file_page_fetch_wait(&mut state, &self.page_submission, page, fetch)
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
            Self::retire_file_page_fetch_wait(&mut state, &self.page_submission, page, fetch)
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
        let (fetch_generation, notify_ready) = {
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
            let notify_ready =
                Self::retire_file_page_fetch_wait(&mut state, &self.page_submission, page, fetch);
            (fetch_generation, notify_ready)
        };
        let installed_dirty = match self.install_fetched_resident_if_absent_published(
            page,
            fetch_generation,
            frame,
        ) {
            Ok(true) => (|| {
                let mut state = self.state.lock();
                if access == MaterializeAccess::Write {
                    state.pages.set_mark(page, PageCacheMark::Referenced)?;
                }
                let slot = state
                    .page_slots
                    .get(&page)
                    .ok_or(PageCacheError::Backend(Errno::ESTALE))?;
                let snapshot = if access == MaterializeAccess::Write {
                    slot.mark_dirty()
                        .map_err(page_slot_completion_error_to_page_cache_error)?
                } else {
                    slot.snapshot()
                };
                Ok(Some(page_slot_is_dirty(snapshot)))
            })(),
            Ok(false) => (|| {
                if access == MaterializeAccess::Write {
                    self.state
                        .lock()
                        .pages
                        .set_mark(page, PageCacheMark::Referenced)?;
                }
                Ok(None)
            })(),
            Err(error) => Err(error),
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
            // Resident-root publication can be temporarily backpressured by
            // the CPU-local retire budget. Keep the fault owner retryable;
            // the next drive turn re-enters with a fresh epoch guard.
            Err(PageCacheError::Backend(Errno::EAGAIN)) => StepOutcome::Continue {
                progress: NoProgress,
            },
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
                let ppn = state
                    .pages
                    .lookup(page)
                    .ok_or(PageCacheError::MissingPage)?;
                state
                    .ensure_resident_page_slot(page, ppn)
                    .map_err(page_slot_completion_error_to_page_cache_error)?;
                if access == MaterializeAccess::Write
                    && !matches!(self.kind, PageContainerKind::Device { .. })
                {
                    state.pages.set_mark(page, PageCacheMark::Referenced)?;
                    state
                        .page_slots
                        .get(&page)
                        .expect("resident page has a PageSlot")
                        .mark_dirty()
                        .map_err(page_slot_completion_error_to_page_cache_error)?;
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
                let dirty = state
                    .page_slots
                    .get(&page)
                    .map(|slot| slot.snapshot())
                    .map(page_slot_is_dirty);
                match state.pages.load_mut(page) {
                    Some(entry) if entry.ppn() == materialized.ppn => {
                        if access == MaterializeAccess::Write
                            && !matches!(self.kind, PageContainerKind::Device { .. })
                        {
                            entry.marks.referenced = true;
                        }
                        dirty
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

        let frame = CachedFrame {
            ppn,
            pin: PageCachePin::Device(DeviceFrame::new(ppn)),
        };
        let newly_installed = match self.install_resident_if_absent_published(page, frame) {
            Ok(installed) => installed,
            Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error).into()),
        };
        let snapshot = {
            let state = self.state.lock();
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

/// Allocate one frame and retain it as a transferable page-cache lease.
///
/// This is the non-indexed counterpart of installing a frame into a
/// `PageContainer`: private staging pools that do not need lookup, dirty, or
/// writeback state can own the returned lease directly and call `retain()` for
/// each in-flight I/O user.  Avoiding a synthetic PageContainer also avoids an
/// RCU-root publication for every private staging frame.
pub fn reserve_page_lease_with_reclaim(policy: ZeroPolicy) -> Result<PageLease, PageCacheError> {
    let owned = reserve_frame_with_reclaim(policy)
        .map_err(PageCacheError::Alloc)?
        .commit();
    let ppn = owned.ppn();
    let cache_pin = owned.try_cache_pin().map_err(PageCacheError::Alloc)?;
    drop(owned);
    Ok(PageLease {
        ppn,
        cache_pin: PageCachePin::Allocated(cache_pin),
    })
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
        let mut reclaimed = 0usize;
        let candidates = self.state.lock().pages.clean_pages(budget);
        for (page, ppn) in candidates {
            let reservation = {
                let mut state = self.state.lock();
                if state.pages.lookup(page) != Some(ppn) {
                    None
                } else {
                    state
                        .range_reservations
                        .try_reserve(PageRange::new(page, 1), RangeReservationKind::Reclaim)
                        .ok()
                }
            };
            let Some(reservation) = reservation else {
                continue;
            };
            let result = self.withdraw_resident_if_match_published(page, ppn, true);
            let released = self
                .state
                .lock()
                .range_reservations
                .release(reservation.id());
            debug_assert!(
                released,
                "reclaim reservation remains live through withdrawal"
            );
            match result {
                Ok(true) => reclaimed = reclaimed.saturating_add(1),
                Ok(false) => {}
                Err(PageCacheError::Backend(Errno::EAGAIN)) => break,
                Err(_) => {}
            }
        }
        reclaimed
    }
}

fn materialized_snapshot_from_state(
    state: &PageContainerState,
    page: PageIndex,
    newly_installed: bool,
) -> Result<MaterializedPageSnapshot, PageCacheError> {
    let entry = state.pages.load(page).ok_or(PageCacheError::MissingPage)?;
    let slot = state
        .page_slots
        .get(&page)
        .ok_or(PageCacheError::MissingPage)?;
    if !Arc::ptr_eq(entry.cell.slot(), slot) {
        return Err(PageCacheError::Backend(Errno::ESTALE));
    }
    let slot_snapshot = slot.snapshot();
    let slot_ppn = match slot_snapshot.state {
        PageSlotState::Resident { ppn }
        | PageSlotState::Dirty { ppn }
        | PageSlotState::Writeback { ppn, .. } => ppn,
        PageSlotState::Empty | PageSlotState::Fetching | PageSlotState::Error { .. } => {
            return Err(PageCacheError::Backend(Errno::ESTALE));
        }
    };
    if slot_ppn != entry.ppn() {
        return Err(PageCacheError::Backend(Errno::ESTALE));
    }
    let pin = match entry.cell.binding() {
        ResidentBindingPin::Allocated(cache_pin) => {
            debug_assert_eq!(cache_pin.ppn(), entry.ppn());
            let cache_pin =
                page_allocator::acquire_cache_pin(entry.ppn()).map_err(PageCacheError::Alloc)?;
            MaterializedPageSnapshotPin::Allocated(cache_pin)
        }
        ResidentBindingPin::Device(device) => MaterializedPageSnapshotPin::Device(*device),
    };
    Ok(MaterializedPageSnapshot {
        ppn: entry.ppn(),
        pin,
        newly_installed,
        dirty: page_slot_is_dirty(slot_snapshot),
    })
}

const fn page_slot_is_dirty(snapshot: PageSlotSnapshot) -> bool {
    matches!(
        snapshot.state,
        PageSlotState::Dirty { .. } | PageSlotState::Writeback { .. }
    )
}

const fn page_slot_completion_error_to_page_cache_error(
    error: PageSlotCompletionError,
) -> PageCacheError {
    match error {
        PageSlotCompletionError::Backend(errno) => PageCacheError::Backend(errno),
        PageSlotCompletionError::GenerationMismatch { .. }
        | PageSlotCompletionError::MismatchedFrame { .. }
        | PageSlotCompletionError::NotFetching { .. } => PageCacheError::Backend(Errno::ESTALE),
    }
}

const fn direct_queue_errno(error: QueueError) -> Errno {
    match error {
        QueueError::Full | QueueError::DispatchDepthFull => Errno::EAGAIN,
        QueueError::EmptyRange => Errno::EINVAL,
    }
}

const fn backend_submit_errno(error: PageServiceBackendSubmitError) -> Errno {
    match error {
        PageServiceBackendSubmitError::BlockQueue(error) => direct_queue_errno(error),
        PageServiceBackendSubmitError::Graph(_)
        | PageServiceBackendSubmitError::DuplicateGraph(_)
        | PageServiceBackendSubmitError::UnknownL6Action(_) => Errno::EIO,
    }
}

fn rollback_file_writeback_slots(
    state: &PageContainerState,
    generations: &[(PageIndex, PageGeneration)],
) {
    for &(page, generation) in generations {
        if let Some(slot) = state.page_slots.get(&page) {
            let _ = slot.abort_writeback(generation);
        }
    }
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
        | PageServiceBackendSubmitOutcome::Err { .. } => {}
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

fn file_service_terminal_error_for_request(
    work: &[PageServiceDrivenWork],
    request_id: Option<PageIoRequestId>,
) -> Option<Errno> {
    work.iter().find_map(|item| match item {
        PageServiceDrivenWork::BackendSubmission(PageServiceBackendSubmitOutcome::Err {
            request,
            errno,
        }) if Some(request.id) == request_id => Some(*errno),
        _ => None,
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
mod block_runtime_tests;
#[cfg(test)]
mod core_tests;

#[cfg(test)]
mod resident_tests;

#[cfg(test)]
mod cross_variant_tests;
#[cfg(test)]
mod direct_io_tests;
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
