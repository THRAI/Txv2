//! PageBacked structure and sparse page-cache publication core.
//!
//! This is the first PageBacked-owned seam toward `PAGE_BACKED_v1.md`.
//! Page cache entries now hold real page-substrate `CachePin` evidence, while
//! VM fault materialization returns `MapPin` evidence for pmap publication.
//! `Frame` is intentionally not a zone entity: frame liveness is represented by
//! typed page-substrate contributors.

use alloc::collections::BTreeMap;

use crate::execution::{Guard, StepOutcome};
use crate::mount::MountPayload;
use crate::sync::SpinMutex;
use crate::vfs::FsObjectId;
use tx_hal::Ppn;
use tx_substrate::{
    page_allocator::{self, AllocError, BitmapPageAllocator, CachePin, MapPin, ZeroPolicy},
    zone::{self, Cap, Zone, ZoneAllocated, ZoneError},
};

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
    cache_pin: CachePin<'static, BitmapPageAllocator<'static>>,
    marks: PageMarks,
}

impl PageCacheEntry {
    fn new(frame: CachedFrame) -> Self {
        Self {
            ppn: frame.ppn,
            cache_pin: frame.cache_pin,
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
            .field("cache_pin", &self.cache_pin)
            .field("marks", &self.marks)
            .finish()
    }
}

struct CachedFrame {
    ppn: Ppn,
    cache_pin: CachePin<'static, BitmapPageAllocator<'static>>,
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

    #[cfg(test)]
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

    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct MaterializedPage {
    pub ppn: Ppn,
    pub map_pin: MapPin<'static, BitmapPageAllocator<'static>>,
    pub newly_installed: bool,
    pub dirty: bool,
}

#[derive(Debug)]
pub struct PageContainer {
    kind: PageContainerKind,
    page_count: u64,
    state: SpinMutex<PageContainerState>,
}

#[derive(Debug)]
struct PageContainerState {
    pages: PageCacheIndex,
}

impl PageContainer {
    pub fn new(kind: PageContainerKind, page_count: u64) -> Self {
        Self {
            kind,
            page_count,
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
            map_pin,
            newly_installed,
            dirty: marks.dirty,
        })
    }

    fn check_bounds(&self, page: PageIndex) -> Result<(), PageCacheError> {
        if page.as_u64() >= self.page_count {
            return Err(PageCacheError::OutOfBounds);
        }
        Ok(())
    }
}

fn allocate_cached_frame() -> Result<CachedFrame, PageCacheError> {
    let frame = page_allocator::reserve_frame(ZeroPolicy::Zeroed)
        .map_err(PageCacheError::Alloc)?
        .commit();
    let ppn = frame.ppn();
    let cache_pin = frame.try_cache_pin().map_err(PageCacheError::Alloc)?;
    drop(frame);
    Ok(CachedFrame { ppn, cache_pin })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_host_substrate() {
        tx_substrate::testing::init_host_for_test_once();
    }

    fn cached_frame_for_test() -> CachedFrame {
        setup_host_substrate();
        allocate_cached_frame().expect("cached frame")
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
}
