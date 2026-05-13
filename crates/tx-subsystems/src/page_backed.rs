//! PageBacked structure and sparse page-cache publication core.
//!
//! This is the first PageBacked-owned seam toward `PAGE_BACKED_v1.md`.
//! Page cache entries now hold real page-substrate `CachePin` evidence, while
//! VM fault materialization returns `MapPin` evidence for pmap publication.
//! `Frame` is intentionally not a zone entity: frame liveness is represented by
//! typed page-substrate contributors.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::execution::{Errno, Guard};
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
pub use user_buffer::{
    step_read_to_kernel, step_read_to_user, step_write_from_kernel, step_write_from_user,
    ReadToUserOp, WriteFromUserOp,
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

    /// Materialize a page for VM fault resolution. Handles both
    /// `Anon` and `File` PCs. `Anon` delegates to `materialize_anon`;
    /// `File` borrows the current epoch guard (or acquires a fresh one
    /// if none is held) and fetches via `FsPageBacking::fetch_page`.
    /// `Device` is rejected — device mappings install through the pmap
    /// directly and never fault.
    pub fn materialize_page_for_fault(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> Result<MaterializedPage, PageCacheError> {
        match &self.kind {
            PageContainerKind::Anon { .. } => self.materialize_anon(page, access),
            PageContainerKind::File { mount, fs_object_id } => {
                use tx_substrate::step_v3::StepOutcome as V3;
                let borrowed = tx_substrate::epoch::borrow_current_guard();
                let fresh;
                let guard: &Guard<'_> = match &borrowed {
                    Some(g) => g,
                    None => {
                        fresh = tx_substrate::epoch::guard();
                        &fresh
                    }
                };
                match self.materialize_file_page(page, access, mount, *fs_object_id, guard) {
                    V3::Done(materialized) => Ok(materialized),
                    V3::Err(errno) => {
                        if errno == tx_substrate::step_v3::Errno::ENOMEM {
                            Err(PageCacheError::Alloc(
                                tx_substrate::page_allocator::AllocError::Exhausted,
                            ))
                        } else {
                            Err(PageCacheError::MissingPage)
                        }
                    }
                    _ => Err(PageCacheError::MissingPage),
                }
            }
            PageContainerKind::Device { .. } => Err(PageCacheError::UnsupportedKind),
        }
    }

    pub fn materialize_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<MaterializedPage, tx_substrate::step_v3::NoProgress>
    {
        use tx_substrate::step_v3::StepOutcome as V3;
        if let Err(error) = self.check_bounds(page) {
            return V3::Err(page_cache_error_to_errno(error).into());
        }

        match &self.kind {
            PageContainerKind::Anon { .. } => match self.materialize_anon(page, access) {
                Ok(page) => V3::Done(page),
                Err(error) => V3::Err(page_cache_error_to_errno(error).into()),
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
    ) -> tx_substrate::step_v3::StepOutcome<MaterializedPage, tx_substrate::step_v3::NoProgress>
    {
        use tx_substrate::step_v3::{NoProgress, StepOutcome as V3, YieldShape};
        if let Some(materialized) = self.materialize_cached_page(page, access) {
            return match materialized {
                Ok(page) => V3::Done(page),
                Err(error) => V3::Err(page_cache_error_to_errno(error).into()),
            };
        }

        let Some(offset) = page.as_u64().checked_mul(crate::vm::USER_PAGE_SIZE as u64) else {
            return V3::Err(tx_substrate::step_v3::Errno::EINVAL);
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
            V3::Done(frame) => self.install_fetched_file_page(page, access, frame, false),
            V3::Continue { progress: _ } => {
                // `Continue` with `NoProgress` means "fs is asking us
                // to retry"; there is no frame to install. Conservative
                // choice: surface `Err(EAGAIN)` so callers that expect
                // a frame don't observe a stale value.
                V3::Err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            V3::Yield {
                progress: _,
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
            } => V3::yield_on_wait_source(NoProgress, carrier.raw(), interests.raw()),
            V3::Yield {
                shape: YieldShape::OnAgent { .. },
                ..
            } => V3::Err(tx_substrate::step_v3::Errno::EIO),
            V3::Yield {
                shape: YieldShape::OnTimer { .. },
                ..
            } => V3::Err(tx_substrate::step_v3::Errno::EIO),
            V3::Err(v3_errno) => V3::Err(v3_errno),
        }
    }

    fn install_fetched_file_page(
        &self,
        page: PageIndex,
        access: MaterializeAccess,
        frame: Frame,
        newly_installed: bool,
    ) -> tx_substrate::step_v3::StepOutcome<MaterializedPage, tx_substrate::step_v3::NoProgress>
    {
        use tx_substrate::step_v3::StepOutcome as V3;
        let frame = match cached_frame_from_frame(frame) {
            Ok(frame) => frame,
            Err(error) => return V3::Err(page_cache_error_to_errno(error).into()),
        };
        let mut state = self.state.lock();
        let installed = match state.pages.lookup(page) {
            Some(_) => false,
            None => match state.pages.install_if_absent(page, frame) {
                Ok(()) => true,
                Err(PageCacheError::AlreadyPresent { .. }) => false,
                Err(error) => return V3::Err(page_cache_error_to_errno(error).into()),
            },
        };
        if access == MaterializeAccess::Write {
            if let Err(error) = state.pages.mark_dirty(page) {
                return V3::Err(page_cache_error_to_errno(error).into());
            }
        }
        match materialized_from_state(&state, page, newly_installed || installed) {
            Ok(page) => V3::Done(page),
            Err(error) => V3::Err(page_cache_error_to_errno(error).into()),
        }
    }

    fn materialize_device_page(
        &self,
        page: PageIndex,
        base_ppn: Ppn,
        page_count: u64,
    ) -> tx_substrate::step_v3::StepOutcome<MaterializedPage, tx_substrate::step_v3::NoProgress>
    {
        use tx_substrate::step_v3::StepOutcome as V3;
        if page.as_u64() >= page_count {
            return V3::Err(tx_substrate::step_v3::Errno::EINVAL);
        }
        let Ok(delta) = usize::try_from(page.as_u64()) else {
            return V3::Err(tx_substrate::step_v3::Errno::EINVAL);
        };
        let Some(ppn) = base_ppn.0.checked_add(delta).map(Ppn) else {
            return V3::Err(tx_substrate::step_v3::Errno::EINVAL);
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
                    Err(error) => return V3::Err(page_cache_error_to_errno(error).into()),
                }
            }
        };
        match materialized_from_state(&state, page, newly_installed) {
            Ok(page) => V3::Done(page),
            Err(error) => V3::Err(page_cache_error_to_errno(error).into()),
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
        use tx_substrate::step_v3::YieldShape;
        match pc.materialize_page(page_index, access, guard) {
            tx_substrate::step_v3::StepOutcome::Done(_)
            | tx_substrate::step_v3::StepOutcome::Continue { .. } => {
                advanced += chunk;
                offset += chunk as u64;
            }
            tx_substrate::step_v3::StepOutcome::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => {
                if advanced == 0 {
                    return V3::yield_on_wait_source(
                        ByteProgress::EMPTY,
                        carrier.raw(),
                        interests.raw(),
                    );
                }
                of.set_offset(offset);
                return V3::yield_on_wait_source(
                    ByteProgress::new(advanced),
                    carrier.raw(),
                    interests.raw(),
                );
            }
            tx_substrate::step_v3::StepOutcome::Yield { .. } => {
                if advanced == 0 {
                    return V3::err(tx_substrate::step_v3::Errno::EIO);
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
            tx_substrate::step_v3::StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return V3::err(errno);
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
        }
    }

    of.set_offset(offset);
    V3::done(advanced)
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
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity> tx_substrate::step_v3::StepOp<I>
    for ReadOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_read(self.pc, self.of, self.len, self.guard)
    }
}

/// `StepOp` wrap of [`step_write`].
pub struct WriteOp<'a> {
    pub pc: &'a PageContainer,
    pub of: &'a OpenFile,
    pub len: usize,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity> tx_substrate::step_v3::StepOp<I>
    for WriteOp<'a>
{
    type Output = usize;
    type Progress = tx_substrate::step_v3::ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_write(self.pc, self.of, self.len, self.guard)
    }
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
