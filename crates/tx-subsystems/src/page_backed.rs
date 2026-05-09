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
use crate::mount::MountPayloadPin;
use crate::sync::SpinMutex;
use crate::vfs::{FsObjectId, OpenFile};
use tx_hal::{Ppn, UserPtr};
use tx_substrate::{
    page_allocator::{
        self, AllocError, BitmapPageAllocator, CachePin, DeviceFrame, MapPin, ZeroPolicy,
    },
    zone::{self, Cap, Zone, ZoneAllocated, ZoneError},
};

mod cross_variant;
mod fs_page_backing;
mod lifecycle;
mod reflink;
mod targeted_read;
mod user_buffer;
pub use cross_variant::step_copy_file_range;
pub use fs_page_backing::FsPageBacking;
pub use lifecycle::{step_fallocate, step_fsync, step_truncate};
pub use reflink::{cow_replace_into_private, install_shared_page};
pub use targeted_read::read_exact_at;
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
        mount: &MountPayloadPin,
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
        // Routes through `FsPageBacking::fetch_page`. The step_v3
        // outcome variants collapse onto `execution::StepOutcome` as:
        // Done→Done, Continue→handled below (no frame to install,
        // surface EAGAIN), Yield{OnCarrier{c,i}}→Blocked(WaitToken(c,i)),
        // Yield{OnAgent}→Err(EIO), Err→Err. There is no
        // `AdvancedThenBlocked`-with-frame variant; the install side
        // is reached only via Done.
        use tx_substrate::step_v3::{StepOutcome as V3, YieldShape};
        match mount
            .payload()
            .fs_page_backing
            .fetch_page(fs_object_id, offset, guard)
        {
            V3::Done(frame) => self.install_fetched_file_page(page, access, frame, false),
            V3::Continue { progress: _ } => {
                // `Continue` with `NoProgress` means "fs is asking us
                // to retry"; there is no frame to install. Conservative
                // choice: surface `Err(EAGAIN)` so callers that expect
                // a frame don't observe a stale value.
                StepOutcome::Err(Errno::EAGAIN)
            }
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => StepOutcome::Blocked(crate::execution::WaitToken::new(
                carrier.raw(),
                interests.raw(),
            )),
            V3::Yield {
                shape: YieldShape::OnAgent { .. },
                ..
            } => StepOutcome::Err(Errno::EIO),
            V3::Err(v3_errno) => StepOutcome::Err(Errno::from(v3_errno)),
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
    of: &OpenFile,
    len: usize,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    if len == 0 {
        return V3::done(0);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    let start = of.offset();
    let valid_end = core::cmp::min(pc.size_bytes(), capacity);
    if start >= valid_end {
        return V3::done(0);
    }
    let effective_len = core::cmp::min(len as u64, valid_end - start) as usize;
    step_range(pc, of, effective_len, PageBackedIoKind::Read, guard)
}

pub fn step_write(
    pc: &PageContainer,
    of: &OpenFile,
    len: usize,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    if len == 0 {
        return V3::done(0);
    }
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return V3::err(Errno::EINVAL.into());
    }
    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    let Some(end) = of.offset().checked_add(len as u64) else {
        return V3::err(Errno::EINVAL.into());
    };
    if end > capacity {
        return V3::err(Errno::EINVAL.into());
    }
    let start = of.offset();
    let outcome = step_range(pc, of, len, PageBackedIoKind::Write, guard);
    let advanced_bytes = match &outcome {
        V3::Done(n) => *n,
        V3::Continue { progress } => progress.bytes(),
        V3::Yield { progress, .. } => progress.bytes(),
        V3::Err(_) => 0,
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
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::{ByteProgress, StepOutcome as V3};
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

        // `materialize_page` is still on v4 (`execution::StepOutcome<MaterializedPage>`);
        // its many other callers rely on that shape. Translate per
        // outcome variant to v3 here:
        // - v4 `Done` / `Advanced` → continue the loop, advancing
        //   `offset` and accumulating `advanced` bytes.
        // - v4 `AdvancedThenBlocked(_, token)` → after advancing, return
        //   v3 `Yield { progress: ByteProgress::new(advanced), shape:
        //   OnCarrier { carrier, interests } }`.
        // - v4 `Blocked(token)` with `advanced == 0` → v3 `Yield {
        //   progress: ByteProgress::EMPTY, ... }`. Otherwise return a
        //   v3 `Yield` carrying the accumulated bytes (matches the v4
        //   `AdvancedThenBlocked` semantic).
        // - v4 `Err(errno)` with `advanced == 0` → v3 `Err(errno.into())`.
        //   Otherwise return v3 `Done(advanced)` (partial-success;
        //   matches existing v4 semantics where errors after progress
        //   were swallowed into a successful partial step).
        match pc.materialize_page(page_index, access, guard) {
            StepOutcome::Done(_) | StepOutcome::Advanced(_) => {
                advanced += chunk;
                offset += chunk as u64;
            }
            StepOutcome::AdvancedThenBlocked(_, token) => {
                advanced += chunk;
                offset += chunk as u64;
                of.set_offset(offset);
                return V3::yield_on_carrier(
                    ByteProgress::new(advanced),
                    token.carrier(),
                    token.interest(),
                );
            }
            StepOutcome::Blocked(token) => {
                if advanced == 0 {
                    return V3::yield_on_carrier(
                        ByteProgress::EMPTY,
                        token.carrier(),
                        token.interest(),
                    );
                }
                of.set_offset(offset);
                return V3::yield_on_carrier(
                    ByteProgress::new(advanced),
                    token.carrier(),
                    token.interest(),
                );
            }
            StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return V3::err(errno.into());
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
        }
    }

    of.set_offset(offset);
    V3::done(advanced)
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
