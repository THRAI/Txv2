//! PageBacked structure and sparse page-cache publication core.
//!
//! This is the first PageBacked-owned seam toward `PAGE_BACKED_v1.md`.
//! Page cache entries now hold real page-substrate `CachePin` evidence, while
//! VM fault materialization returns `MapPin` evidence for pmap publication.
//! `Frame` is intentionally not a zone entity: frame liveness is represented by
//! typed page-substrate contributors.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::mount::MountPayload;
use crate::sync::SpinMutex;
use crate::vfs::{FsObjectId, OpenFile};
use tx_hal::{KernelPtr, Ppn, UserAccessIf, UserPtr};
use tx_substrate::{
    page_allocator::{
        self, AllocError, BitmapPageAllocator, CachePin, DeviceFrame, MapPin, ZeroPolicy,
    },
    zone::{self, Cap, Zone, ZoneAllocated, ZoneError},
};

mod cross_variant;
mod lifecycle;
mod reflink;
mod user_buffer;
pub use cross_variant::step_copy_file_range;
pub use lifecycle::{step_fallocate, step_fsync, step_truncate};
pub use reflink::{cow_replace_into_private, install_shared_page};
pub use user_buffer::{step_read_to_user, step_write_from_user};

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
        mount: Cap<MountPayload>,
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

pub trait FsPageBacking: Send + Sync + 'static {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame>;

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn fsync(&self, fs_object_id: FsObjectId, guard: &Guard<'_>) -> StepOutcome<()>;

    /// Reserve space for future writes up to `new_size`. The default
    /// implementation is `Done(())`: most filesystems can treat fallocate as
    /// a hint. Backends that pre-allocate on-disk blocks override this.
    fn fallocate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct MaterializedPage {
    pub ppn: Ppn,
    pub map_pin: MaterializedPagePin,
    pub newly_installed: bool,
    pub dirty: bool,
}

#[derive(Debug)]
pub enum MaterializedPagePin {
    Allocated(MapPin<'static, BitmapPageAllocator<'static>>),
    Device(DeviceFrame),
}

#[derive(Debug)]
pub struct PageContainer {
    kind: PageContainerKind,
    page_count: u64,
    size_bytes: AtomicU64,
    state: SpinMutex<PageContainerState>,
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

impl PageContainer {
    pub fn new(kind: PageContainerKind, page_count: u64) -> Self {
        let capacity = page_count.saturating_mul(crate::vm::USER_PAGE_SIZE as u64);
        Self {
            kind,
            page_count,
            size_bytes: AtomicU64::new(capacity),
            state: SpinMutex::new(PageContainerState {
                pages: PageCacheIndex::new(),
            }),
        }
    }

    pub fn new_cap(
        kind: PageContainerKind,
        page_count: u64,
    ) -> Result<Cap<PageContainer>, ZoneError> {
        let reservation = zone::reserve_for::<PageContainer>()?;
        Ok(zone::sign_for(reservation, Self::new(kind, page_count)))
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

        let mut state = self.state.lock();
        let newly_installed = match state.pages.lookup(page) {
            Some(_) => false,
            None => {
                let frame = allocate_cached_frame()?;
                state.pages.install_if_absent(page, frame)?;
                true
            }
        };

        if access == MaterializeAccess::Write {
            state.pages.mark_dirty(page)?;
        }

        let ppn = state
            .pages
            .lookup(page)
            .ok_or(PageCacheError::MissingPage)?;
        let map_pin = page_allocator::acquire_map_pin(ppn).map_err(PageCacheError::Alloc)?;
        let marks = state.pages.marks(page).ok_or(PageCacheError::MissingPage)?;
        Ok(MaterializedPage {
            ppn,
            map_pin: MaterializedPagePin::Allocated(map_pin),
            newly_installed,
            dirty: marks.dirty,
        })
    }

    pub fn materialize_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        guard: &Guard<'_>,
    ) -> StepOutcome<MaterializedPage> {
        if let Err(error) = self.check_bounds(page) {
            return StepOutcome::Err(page_cache_error_to_errno(error));
        }

        match &self.kind {
            PageContainerKind::Anon { .. } => match self.materialize_anon(page, access) {
                Ok(page) => StepOutcome::Done(page),
                Err(error) => StepOutcome::Err(page_cache_error_to_errno(error)),
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

    fn materialize_file_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        mount: &Cap<MountPayload>,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<MaterializedPage> {
        if let Some(materialized) = self.materialize_cached_page(page, access) {
            return match materialized {
                Ok(page) => StepOutcome::Done(page),
                Err(error) => StepOutcome::Err(page_cache_error_to_errno(error)),
            };
        }

        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        match mount
            .fs_page_backing
            .fetch_page(fs_object_id, offset, guard)
        {
            StepOutcome::Done(frame) => self.install_fetched_file_page(page, access, frame, false),
            StepOutcome::Advanced(frame) => {
                match self.install_fetched_file_page(page, access, frame, false) {
                    StepOutcome::Done(page) => StepOutcome::Advanced(page),
                    other => other,
                }
            }
            StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked(frame, token) => {
                match self.install_fetched_file_page(page, access, frame, false) {
                    StepOutcome::Done(page) => StepOutcome::AdvancedThenBlocked(page, token),
                    other => other,
                }
            }
            StepOutcome::Err(errno) => StepOutcome::Err(errno),
        }
    }

    fn install_fetched_file_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        frame: Frame,
        newly_installed: bool,
    ) -> StepOutcome<MaterializedPage> {
        let frame = match cached_frame_from_frame(frame) {
            Ok(frame) => frame,
            Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error)),
        };
        let mut state = self.state.lock();
        let installed = match state.pages.lookup(page) {
            Some(_) => false,
            None => match state.pages.install_if_absent(page, frame) {
                Ok(()) => true,
                Err(PageCacheError::AlreadyPresent { .. }) => false,
                Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error)),
            },
        };
        if access == MaterializeAccess::Write {
            if let Err(error) = state.pages.mark_dirty(page) {
                return StepOutcome::Err(page_cache_error_to_errno(error));
            }
        }
        match materialized_from_state(&state, page, newly_installed || installed) {
            Ok(page) => StepOutcome::Done(page),
            Err(error) => StepOutcome::Err(page_cache_error_to_errno(error)),
        }
    }

    fn materialize_device_page(
        &self,
        page: PageIndex,
        base_ppn: Ppn,
        page_count: u64,
    ) -> StepOutcome<MaterializedPage> {
        if page.as_u64() >= page_count {
            return StepOutcome::Err(Errno::EINVAL);
        }
        let Ok(delta) = usize::try_from(page.as_u64()) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        let Some(ppn) = base_ppn.0.checked_add(delta).map(Ppn) else {
            return StepOutcome::Err(Errno::EINVAL);
        };

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
                    Err(error) => return StepOutcome::Err(page_cache_error_to_errno(error)),
                }
            }
        };
        match materialized_from_state(&state, page, newly_installed) {
            Ok(page) => StepOutcome::Done(page),
            Err(error) => StepOutcome::Err(page_cache_error_to_errno(error)),
        }
    }

    fn materialize_cached_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> Option<Result<MaterializedPage, PageCacheError>> {
        let mut state = self.state.lock();
        state.pages.lookup(page)?;
        if access == MaterializeAccess::Write
            && !matches!(self.kind, PageContainerKind::Device { .. })
        {
            if let Err(error) = state.pages.mark_dirty(page) {
                return Some(Err(error));
            }
        }
        Some(materialized_from_state(&state, page, false))
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

    fn set_size_bytes(&self, size: u64) {
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
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }
}

pub fn step_read(
    pc: &PageContainer,
    of: &mut OpenFile,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    if len == 0 {
        return StepOutcome::Done(0);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let start = of.offset();
    let valid_end = core::cmp::min(pc.size_bytes(), capacity);
    if start >= valid_end {
        return StepOutcome::Done(0);
    }
    let effective_len = core::cmp::min(len as u64, valid_end - start) as usize;
    step_range(pc, of, effective_len, PageBackedIoKind::Read, guard)
}

pub fn step_write(
    pc: &PageContainer,
    of: &mut OpenFile,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    if len == 0 {
        return StepOutcome::Done(0);
    }
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let Some(end) = of.offset().checked_add(len as u64) else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if end > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let start = of.offset();
    let outcome = step_range(pc, of, len, PageBackedIoKind::Write, guard);
    match &outcome {
        StepOutcome::Done(advanced)
        | StepOutcome::Advanced(advanced)
        | StepOutcome::AdvancedThenBlocked(advanced, _)
            if *advanced > 0 =>
        {
            pc.grow_size_to(start + *advanced as u64);
        }
        StepOutcome::Done(_)
        | StepOutcome::Advanced(_)
        | StepOutcome::AdvancedThenBlocked(_, _)
        | StepOutcome::Blocked(_)
        | StepOutcome::Err(_) => {}
    }
    outcome
}

fn step_range(
    pc: &PageContainer,
    of: &mut OpenFile,
    len: usize,
    kind: PageBackedIoKind,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
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

        match pc.materialize_page(page_index, access, guard) {
            StepOutcome::Done(_) | StepOutcome::Advanced(_) => {
                advanced += chunk;
                offset += chunk as u64;
            }
            StepOutcome::AdvancedThenBlocked(_, token) => {
                advanced += chunk;
                offset += chunk as u64;
                of.set_offset(offset);
                return StepOutcome::AdvancedThenBlocked(advanced, token);
            }
            StepOutcome::Blocked(token) => {
                if advanced == 0 {
                    return StepOutcome::Blocked(token);
                }
                of.set_offset(offset);
                return StepOutcome::AdvancedThenBlocked(advanced, token);
            }
            StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return StepOutcome::Err(errno);
                }
                of.set_offset(offset);
                return StepOutcome::Done(advanced);
            }
        }
    }

    of.set_offset(offset);
    StepOutcome::Done(advanced)
}

fn allocate_cached_frame() -> Result<CachedFrame, PageCacheError> {
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

fn cached_frame_from_frame(frame: Frame) -> Result<CachedFrame, PageCacheError> {
    let ppn = frame.ppn();
    let cache_pin = page_allocator::acquire_cache_pin(ppn).map_err(PageCacheError::Alloc)?;
    Ok(CachedFrame {
        ppn,
        pin: PageCachePin::Allocated(cache_pin),
    })
}

fn materialized_from_state(
    state: &PageContainerState,
    page: PageIndex,
    newly_installed: bool,
) -> Result<MaterializedPage, PageCacheError> {
    let entry = state
        .pages
        .pages
        .get(&page)
        .ok_or(PageCacheError::MissingPage)?;
    let map_pin = match &entry.pin {
        PageCachePin::Allocated(cache_pin) => {
            debug_assert_eq!(cache_pin.ppn(), entry.ppn);
            MaterializedPagePin::Allocated(
                page_allocator::acquire_map_pin(entry.ppn).map_err(PageCacheError::Alloc)?,
            )
        }
        PageCachePin::Device(device) => MaterializedPagePin::Device(*device),
    };
    Ok(MaterializedPage {
        ppn: entry.ppn,
        map_pin,
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
        PageCacheError::Alloc(_) => Errno::ENOMEM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{Errno, StepOutcome, WaitToken};
    use crate::mount::{DevId, MountOptions, MountPayload, SourceLabel};
    use crate::vfs::{
        Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
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

    fn file_page_container(
        fs: Arc<dyn FsOps + Send + Sync>,
        page_backing: Arc<dyn FsPageBacking + Send + Sync>,
        fs_object_id: FsObjectId,
        page_count: u64,
    ) -> PageContainer {
        let mount = MountPayload::new_cap(
            fs,
            page_backing,
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
        let pc = file_page_container(fs.clone(), fs.clone(), FsObjectId::new(55), 4);

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
        let pc = file_page_container(fs.clone(), fs, FsObjectId::new(77), 4);

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
        let mut of = open_file_for_pc(&pc);
        of.set_offset((crate::vm::USER_PAGE_SIZE - 8) as u64);

        assert_eq!(step_read(&pc, &mut of, 32, &guard), StepOutcome::Done(32));

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
        let mut of = open_file_for_pc(&pc);
        of.set_offset(crate::vm::USER_PAGE_SIZE as u64);

        assert_eq!(step_read(&pc, &mut of, 16, &guard), StepOutcome::Done(0));
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
        let mut of = open_file_for_pc(&pc);

        assert_eq!(
            step_write(&pc, &mut of, crate::vm::USER_PAGE_SIZE + 17, &guard),
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
        let pc = file_page_container(fs.clone(), fs, FsObjectId::new(88), 2);
        let mut of = open_file_for_pc(&pc);
        pc.state
            .lock()
            .pages
            .install_if_absent(PageIndex::new(0), cached_frame_for_test())
            .expect("seed cached page");

        assert_eq!(
            step_read(&pc, &mut of, crate::vm::USER_PAGE_SIZE + 1, &guard),
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
        let mut of = open_file_for_pc(&pc);

        assert_eq!(
            step_write(&pc, &mut of, 8, &guard),
            StepOutcome::Err(Errno::EINVAL)
        );
        assert_eq!(of.offset(), 0);
        assert_eq!(pc.resident_pages(), 0);
    }
}

#[cfg(test)]
mod cross_variant_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod reflink_tests;
#[cfg(test)]
mod size_tests;
#[cfg(test)]
mod user_buffer_tests;
