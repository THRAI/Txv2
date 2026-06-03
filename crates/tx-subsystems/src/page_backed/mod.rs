//! PageBacked structure and sparse page-cache publication core.
//!
//! This is the first PageBacked-owned seam toward `PAGE_BACKED_v1.md`.
//! Page cache entries now hold real page-substrate `CachePin` evidence, while
//! VM fault materialization returns `MapPin` evidence for pmap publication.
//! `Frame` is intentionally not a zone entity: frame liveness is represented by
//! typed page-substrate contributors.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;
pub mod notification;

use adapter::step_engine::{
    self as step_engine, AllocError, BitmapPageAllocator, ByteProgress, CachePin, Cap, DeviceFrame,
    MapPin, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity, ZeroPolicy, Zone,
    ZoneAllocated, ZoneError, page_allocator,
};

use crate::execution::{Errno, Guard};
use crate::mount::MountPayloadPin;
use crate::sync::SpinMutex;
use crate::vfs::{FsObjectId, OpenFile};
use tx_hal::{Ppn, UserPtr};

mod cross_variant;
mod fs_page_backing;
mod lifecycle;
mod reflink;
mod targeted_read;
mod user_buffer;
pub use cross_variant::step_copy_file_range;
pub use fs_page_backing::FsPageBacking;
pub use lifecycle::{FallocateOp, TruncateOp, step_fallocate, step_fsync, step_truncate};
pub use reflink::{cow_replace_into_private, install_shared_page};
pub use targeted_read::read_exact_at;
pub use user_buffer::{
    ReadToUserOp, WriteFromUserOp, step_read_to_kernel, step_read_to_user, step_write_from_kernel,
    step_write_from_user,
};

#[cfg(test)]
use crate::test_support::EPOCH_TEST_LOCK;

static PAGE_CONTAINER_ZONE: Zone<PageContainer> = Zone::const_new();

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
}

impl Frame {
    pub const fn new(ppn: Ppn) -> Self {
        Self { ppn }
    }

    pub const fn ppn(self) -> Ppn {
        self.ppn
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageMarks {
    pub dirty: bool,
    pub writeback: bool,
    pub referenced: bool,
    pub no_reclaim: bool,
}

struct PageCacheEntry {
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

impl core::fmt::Debug for PageCacheEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheEntry")
            .field("ppn", &self.ppn)
            .field("pin", &self.pin)
            .field("marks", &self.marks)
            .finish()
    }
}

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
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn lookup(&self, page: PageIndex) -> Option<Ppn> {
        self.pages.get(&page).map(|entry| entry.ppn)
    }

    pub fn marks(&self, page: PageIndex) -> Option<PageMarks> {
        self.pages.get(&page).map(|entry| entry.marks)
    }

    fn install_if_absent(
        &mut self,
        page: PageIndex,
        frame: CachedFrame,
    ) -> Result<(), PageCacheError> {
        if let Some(entry) = self.pages.get(&page) {
            return Err(PageCacheError::AlreadyPresent { current: entry.ppn });
        }

        self.pages.insert(page, PageCacheEntry::new(frame));
        Ok(())
    }

    fn install_if_match(
        &mut self,
        page: PageIndex,
        expected: Ppn,
        replacement: Option<CachedFrame>,
    ) -> Result<Option<Ppn>, PageCacheError> {
        let Some(entry) = self.pages.get_mut(&page) else {
            return Err(PageCacheError::MissingPage);
        };
        if entry.ppn != expected {
            return Err(PageCacheError::MismatchedFrame { current: entry.ppn });
        }

        let previous = entry.ppn;
        match replacement {
            Some(frame) => {
                *entry = PageCacheEntry::new(frame);
            }
            None => {
                self.pages.remove(&page);
            }
        }
        Ok(Some(previous))
    }

    fn mark_dirty(&mut self, page: PageIndex) -> Result<(), PageCacheError> {
        let Some(entry) = self.pages.get_mut(&page) else {
            return Err(PageCacheError::MissingPage);
        };
        entry.marks.dirty = true;
        entry.marks.referenced = true;
        Ok(())
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
        if let Some(materialized) = self.materialize_cached_page(page, access) {
            return match materialized {
                Ok(page) => StepOutcome::Done(page),
                Err(error) => StepOutcome::Err(page_cache_error_to_errno(error).into()),
            };
        }

        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
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
            StepOutcome::Done(frame) => self.install_fetched_file_page(page, access, frame, false),
            StepOutcome::Continue { progress: _ } => {
                // `Continue` with `NoProgress` means "fs is asking us
                // to retry"; there is no frame to install. Conservative
                // choice: surface `Err(EAGAIN)` so callers that expect
                // a frame don't observe a stale value.
                StepOutcome::Err(V3Errno::EAGAIN)
            }
            StepOutcome::Yield { progress: _, shape } => {
                if let Some((carrier, interests)) = notification::wait_source_parts(&shape) {
                    notification::yield_on_wait_source(NoProgress, carrier, interests)
                } else {
                    StepOutcome::Err(V3Errno::EIO)
                }
            }
            StepOutcome::Err(v3_errno) => StepOutcome::Err(v3_errno),
        }
    }

    fn install_fetched_file_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        frame: Frame,
        newly_installed: bool,
    ) -> StepOutcome<MaterializedPage, NoProgress> {
        let frame = match cached_frame_from_frame(frame) {
            Ok(frame) => frame,
            Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error).into()),
        };
        let ppn = frame.ppn;
        let map_pin = match acquire_map_pin_for_materialization(ppn) {
            Ok(pin) => pin,
            Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error).into()),
        };
        let installed_dirty = {
            let mut state = self.state.lock();
            let installed = match state.pages.lookup(page) {
                Some(_) => false,
                None => match state.pages.install_if_absent(page, frame) {
                    Ok(()) => true,
                    Err(PageCacheError::AlreadyPresent { .. }) => false,
                    Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error).into()),
                },
            };
            if access == MaterializeAccess::Write {
                if let Err(error) = state.pages.mark_dirty(page) {
                    return StepOutcome::Err(page_cache_error_to_errno(error).into());
                }
            }
            installed
                .then(|| state.pages.marks(page).ok_or(PageCacheError::MissingPage))
                .transpose()
                .map(|marks| marks.map(|marks| marks.dirty))
        };
        match installed_dirty {
            Ok(Some(dirty)) => StepOutcome::Done(MaterializedPage {
                ppn,
                map_pin: MaterializedPagePin::Allocated(map_pin),
                newly_installed: true,
                dirty,
            }),
            Ok(None) => {
                drop(map_pin);
                match self.materialize_existing_page(page, access, newly_installed) {
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
                match state.pages.pages.get_mut(&page) {
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
    let cache_pin = page_allocator::acquire_cache_pin(ppn).map_err(PageCacheError::Alloc)?;
    Ok(CachedFrame {
        ppn,
        pin: PageCachePin::Allocated(cache_pin),
    })
}

fn materialized_snapshot_from_state(
    state: &PageContainerState,
    page: PageIndex,
    newly_installed: bool,
) -> Result<MaterializedPageSnapshot, PageCacheError> {
    let entry = state
        .pages
        .pages
        .get(&page)
        .ok_or(PageCacheError::MissingPage)?;
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
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
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
mod reflink_tests;
#[cfg(test)]
mod size_tests;
#[cfg(test)]
mod targeted_read_tests;
#[cfg(test)]
mod user_buffer_tests;
