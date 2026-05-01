//! PageBacked structure and sparse page-cache publication core.
//!
//! This is the first PageBacked-owned seam toward `PAGE_BACKED_v1.md`.
//! `PageFrame` is a host-testable staging token for the future `Cap<Frame>`;
//! the ownership and publication shape is intentionally `PageContainer` +
//! offset-keyed `PageCacheIndex`, not VM-local file-cache state.

use alloc::collections::BTreeMap;

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
pub struct PageFrame(u64);

impl PageFrame {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn id(self) -> u64 {
        self.0
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
struct PageCacheEntry {
    frame: PageFrame,
    marks: PageMarks,
}

impl PageCacheEntry {
    const fn new(frame: PageFrame) -> Self {
        Self {
            frame,
            marks: PageMarks {
                referenced: true,
                ..PageMarks::new()
            },
        }
    }
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
    AlreadyPresent { current: PageFrame },
    MissingPage,
    MismatchedFrame { current: PageFrame },
    OutOfBounds,
    UnsupportedKind,
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

    pub fn lookup(&self, page: PageIndex) -> Option<PageFrame> {
        self.pages.get(&page).map(|entry| entry.frame)
    }

    pub fn marks(&self, page: PageIndex) -> Option<PageMarks> {
        self.pages.get(&page).map(|entry| entry.marks)
    }

    pub fn install_if_absent(
        &mut self,
        page: PageIndex,
        frame: PageFrame,
    ) -> Result<(), PageCacheError> {
        if let Some(entry) = self.pages.get(&page) {
            return Err(PageCacheError::AlreadyPresent {
                current: entry.frame,
            });
        }

        self.pages.insert(page, PageCacheEntry::new(frame));
        Ok(())
    }

    pub fn install_if_match(
        &mut self,
        page: PageIndex,
        expected: PageFrame,
        replacement: Option<PageFrame>,
    ) -> Result<Option<PageFrame>, PageCacheError> {
        let Some(entry) = self.pages.get_mut(&page) else {
            return Err(PageCacheError::MissingPage);
        };
        if entry.frame != expected {
            return Err(PageCacheError::MismatchedFrame {
                current: entry.frame,
            });
        }

        let previous = entry.frame;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageContainerKind {
    Anon { swap_policy: AnonSwapPolicy },
    File { fs_object_id: u64 },
    Device { base_ppn: u64, page_count: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializeAccess {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedPage {
    pub frame: PageFrame,
    pub newly_installed: bool,
    pub dirty: bool,
}

#[derive(Debug)]
pub struct PageContainer {
    kind: PageContainerKind,
    page_count: u64,
    pages: PageCacheIndex,
    next_frame_id: u64,
}

impl PageContainer {
    pub fn new(kind: PageContainerKind, page_count: u64) -> Self {
        Self {
            kind,
            page_count,
            pages: PageCacheIndex::new(),
            next_frame_id: 1,
        }
    }

    pub const fn kind(&self) -> PageContainerKind {
        self.kind
    }

    pub const fn page_count(&self) -> u64 {
        self.page_count
    }

    pub fn resident_pages(&self) -> usize {
        self.pages.len()
    }

    pub fn lookup(&self, page: PageIndex) -> Option<PageFrame> {
        self.pages.lookup(page)
    }

    pub fn page_marks(&self, page: PageIndex) -> Option<PageMarks> {
        self.pages.marks(page)
    }

    pub fn materialize_anon(
        &mut self,
        page: PageIndex,
        access: MaterializeAccess,
    ) -> Result<MaterializedPage, PageCacheError> {
        if !matches!(self.kind, PageContainerKind::Anon { .. }) {
            return Err(PageCacheError::UnsupportedKind);
        }
        self.check_bounds(page)?;

        let newly_installed = match self.pages.lookup(page) {
            Some(_) => false,
            None => {
                let frame = self.allocate_staging_frame();
                self.pages.install_if_absent(page, frame)?;
                true
            }
        };

        if access == MaterializeAccess::Write {
            self.pages.mark_dirty(page)?;
        }

        let frame = self.pages.lookup(page).ok_or(PageCacheError::MissingPage)?;
        let marks = self.pages.marks(page).ok_or(PageCacheError::MissingPage)?;
        Ok(MaterializedPage {
            frame,
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

    fn allocate_staging_frame(&mut self) -> PageFrame {
        let frame = PageFrame::new(self.next_frame_id);
        self.next_frame_id += 1;
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_cache_index_install_if_absent_linearizes_sparse_offsets() {
        let mut index = PageCacheIndex::new();
        let page = PageIndex::new(7);
        let first = PageFrame::new(11);
        let second = PageFrame::new(12);

        assert_eq!(index.lookup(page), None);
        assert_eq!(index.install_if_absent(page, first), Ok(()));
        assert_eq!(
            index.install_if_absent(page, second),
            Err(PageCacheError::AlreadyPresent { current: first })
        );
        assert_eq!(index.lookup(page), Some(first));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn page_cache_index_install_if_match_replaces_or_withdraws_exact_frame() {
        let mut index = PageCacheIndex::new();
        let page = PageIndex::new(3);
        let first = PageFrame::new(21);
        let replacement = PageFrame::new(22);

        index
            .install_if_absent(page, first)
            .expect("initial insert");
        assert_eq!(
            index.install_if_match(page, PageFrame::new(99), Some(replacement)),
            Err(PageCacheError::MismatchedFrame { current: first })
        );
        assert_eq!(
            index.install_if_match(page, first, Some(replacement)),
            Ok(Some(first))
        );
        assert_eq!(index.lookup(page), Some(replacement));
        assert_eq!(
            index.install_if_match(page, replacement, None),
            Ok(Some(replacement))
        );
        assert_eq!(index.lookup(page), None);
    }

    #[test]
    fn anon_page_container_materializes_once_and_tracks_dirty_writes() {
        let mut pc = PageContainer::new(
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
        assert_eq!(second.frame, first.frame);
        assert!(!second.newly_installed);
        assert!(second.dirty);
        assert_eq!(pc.lookup(page), Some(first.frame));
        assert_eq!(pc.resident_pages(), 1);
    }
}
