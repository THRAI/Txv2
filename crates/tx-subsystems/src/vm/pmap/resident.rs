//! Resident pmap shadow index.
//!
//! VM recipes are the authoritative address bindings; HAL PTEs are derived
//! materializations. This store is only the pmap-local shadow index that lets
//! VM enumerate and retain published materializations for lookup, mprotect,
//! munmap, fork, exec, and drop. The active VM spec requires those operations
//! to enumerate resident pages and preserve `MapPin` ownership, but it does not
//! prescribe this module's internal data structure.
//!
//! `PmapResidentStore` is the production cfg-selected alias used by `VmPmap`.
//! `PmapResidentStoreWith<B>` is the backend-neutral facade for A/B tests and
//! future implementations. Production uses the chunked backend so a large
//! address space never has to grow one physically-contiguous resident array.
//! Tests retain the address-sorted `Vec` backend for behavior comparison.

use alloc::vec::Vec;
use core::fmt::Debug;

use super::{PmapMapping, PmapMappingSnapshot};
use crate::vm::UserPage;

type DefaultPmapResidentStoreImpl = ChunkedPmapResidentStore;

const CHUNK_CAPACITY: usize = 64;

pub(super) type PmapResidentStore = PmapResidentStoreWith<DefaultPmapResidentStoreImpl>;

#[derive(Debug, Default)]
pub(super) struct PmapResidentStoreWith<B: PmapResidentStoreImpl> {
    backend: B,
}

impl<B: PmapResidentStoreImpl> PmapResidentStoreWith<B> {
    pub(super) fn new() -> Self {
        Self {
            backend: B::default(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.backend.len()
    }

    pub(super) fn reserve_additional(&mut self, additional: usize) {
        self.backend.reserve_additional(additional);
    }

    pub(super) fn get(&self, page: &UserPage) -> Option<&PmapMapping> {
        self.backend.get(page)
    }

    pub(super) fn get_mut(&mut self, page: &UserPage) -> Option<&mut PmapMapping> {
        self.backend.get_mut(page)
    }

    pub(super) fn insert(&mut self, page: UserPage, mapping: PmapMapping) -> Option<PmapMapping> {
        self.backend.insert(page, mapping)
    }

    pub(super) fn remove(&mut self, page: &UserPage) -> Option<PmapMapping> {
        self.backend.remove(page)
    }

    pub(super) fn drain_range(&mut self, start: UserPage, end: UserPage) -> DrainedMappings {
        self.backend.drain_range(start, end)
    }

    pub(super) fn snapshots_in_range(
        &self,
        start: UserPage,
        end: UserPage,
    ) -> Vec<(UserPage, PmapMappingSnapshot)> {
        self.backend.snapshots_in_range(start, end)
    }

    pub(super) fn pages_in_range(&self, start: UserPage, end: UserPage) -> Vec<UserPage> {
        self.backend.pages_in_range(start, end)
    }
}

pub(super) trait PmapResidentStoreImpl: Debug + Default {
    fn len(&self) -> usize;

    fn reserve_additional(&mut self, additional: usize);

    fn get(&self, page: &UserPage) -> Option<&PmapMapping>;

    fn get_mut(&mut self, page: &UserPage) -> Option<&mut PmapMapping>;

    fn insert(&mut self, page: UserPage, mapping: PmapMapping) -> Option<PmapMapping>;

    fn remove(&mut self, page: &UserPage) -> Option<PmapMapping>;

    fn drain_range(&mut self, start: UserPage, end: UserPage) -> DrainedMappings;

    fn snapshots_in_range(
        &self,
        start: UserPage,
        end: UserPage,
    ) -> Vec<(UserPage, PmapMappingSnapshot)>;

    fn pages_in_range(&self, start: UserPage, end: UserPage) -> Vec<UserPage>;
}

#[derive(Debug, Default)]
pub(super) struct DrainedMappings {
    entries: Vec<(UserPage, PmapMapping)>,
    shifted_entries: usize,
}

impl DrainedMappings {
    fn new(entries: Vec<(UserPage, PmapMapping)>, shifted_entries: usize) -> Self {
        Self {
            entries,
            shifted_entries,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn shifted_entries(&self) -> usize {
        self.shifted_entries
    }
}

impl IntoIterator for DrainedMappings {
    type Item = (UserPage, PmapMapping);
    type IntoIter = alloc::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

#[derive(Debug, Default)]
#[cfg(test)]
pub(super) struct VecPmapResidentStore {
    entries: Vec<(UserPage, PmapMapping)>,
}

#[cfg(test)]
impl PmapResidentStoreImpl for VecPmapResidentStore {
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn reserve_additional(&mut self, additional: usize) {
        self.entries.reserve(additional);
    }

    fn get(&self, page: &UserPage) -> Option<&PmapMapping> {
        self.search(*page).ok().map(|index| &self.entries[index].1)
    }

    fn get_mut(&mut self, page: &UserPage) -> Option<&mut PmapMapping> {
        self.search(*page)
            .ok()
            .map(|index| &mut self.entries[index].1)
    }

    fn insert(&mut self, page: UserPage, mapping: PmapMapping) -> Option<PmapMapping> {
        match self.search(page) {
            Ok(index) => Some(core::mem::replace(&mut self.entries[index].1, mapping)),
            Err(index) if index == self.entries.len() => {
                self.entries.push((page, mapping));
                None
            }
            Err(index) => {
                self.entries.insert(index, (page, mapping));
                None
            }
        }
    }

    fn remove(&mut self, page: &UserPage) -> Option<PmapMapping> {
        self.search(*page)
            .ok()
            .map(|index| self.entries.remove(index).1)
    }

    fn drain_range(&mut self, start: UserPage, end: UserPage) -> DrainedMappings {
        let start_index = self.search(start).unwrap_or_else(|index| index);
        let end_index = self.search(end).unwrap_or_else(|index| index);
        if start_index >= end_index {
            return DrainedMappings::default();
        }

        let shifted_entries = self.entries.len().saturating_sub(end_index);
        let entries = self.entries.drain(start_index..end_index).collect();
        DrainedMappings::new(entries, shifted_entries)
    }

    fn snapshots_in_range(
        &self,
        start: UserPage,
        end: UserPage,
    ) -> Vec<(UserPage, PmapMappingSnapshot)> {
        let mut snapshots = Vec::new();
        let start_index = self.search(start).unwrap_or_else(|index| index);
        for (page, mapping) in self.entries[start_index..].iter() {
            if *page >= end {
                break;
            }
            snapshots.push((*page, mapping.snapshot()));
        }
        snapshots
    }

    fn pages_in_range(&self, start: UserPage, end: UserPage) -> Vec<UserPage> {
        let mut pages = Vec::new();
        let start_index = self.search(start).unwrap_or_else(|index| index);
        for (page, _) in self.entries[start_index..].iter() {
            if *page >= end {
                break;
            }
            pages.push(*page);
        }
        pages
    }
}

#[cfg(test)]
impl VecPmapResidentStore {
    fn search(&self, page: UserPage) -> Result<usize, usize> {
        self.entries
            .binary_search_by_key(&page, |(entry_page, _)| *entry_page)
    }
}

#[derive(Debug, Default)]
pub(super) struct ChunkedPmapResidentStore {
    chunks: Vec<ResidentChunk>,
    len: usize,
}

impl PmapResidentStoreImpl for ChunkedPmapResidentStore {
    fn len(&self) -> usize {
        self.len
    }

    fn reserve_additional(&mut self, additional: usize) {
        if additional == 0 {
            return;
        }
        let needed_chunks = additional.saturating_add(CHUNK_CAPACITY - 1) / CHUNK_CAPACITY;
        let spare_chunks = self.chunks.capacity().saturating_sub(self.chunks.len());
        if needed_chunks > spare_chunks {
            self.chunks.reserve(needed_chunks - spare_chunks);
        }
    }

    fn get(&self, page: &UserPage) -> Option<&PmapMapping> {
        let chunk_index = self.find_chunk_for_page(*page)?;
        self.chunks[chunk_index].get(page)
    }

    fn get_mut(&mut self, page: &UserPage) -> Option<&mut PmapMapping> {
        let chunk_index = self.find_chunk_for_page(*page)?;
        self.chunks[chunk_index].get_mut(page)
    }

    fn insert(&mut self, page: UserPage, mapping: PmapMapping) -> Option<PmapMapping> {
        if self.chunks.is_empty() {
            let mut chunk = ResidentChunk::new();
            chunk.push(page, mapping);
            self.chunks.push(chunk);
            self.len += 1;
            return None;
        }
        if self
            .chunks
            .last()
            .and_then(ResidentChunk::last_page)
            .is_some_and(|last_page| last_page < page)
        {
            let last = self.chunks.last_mut().expect("nonempty chunks");
            if last.len() >= CHUNK_CAPACITY {
                let mut chunk = ResidentChunk::new();
                chunk.push(page, mapping);
                self.chunks.push(chunk);
            } else {
                last.push(page, mapping);
            }
            self.len += 1;
            return None;
        }

        let chunk_index = match self.find_chunk_for_insert(page) {
            Ok(index) => index,
            Err(index) => {
                self.chunks.insert(index, ResidentChunk::new());
                index
            }
        };

        let replaced = self.chunks[chunk_index].insert(page, mapping);
        if replaced.is_none() {
            self.len += 1;
            if self.chunks[chunk_index].len() > CHUNK_CAPACITY {
                let split = self.chunks[chunk_index].split();
                self.chunks.insert(chunk_index + 1, split);
            }
        }
        replaced
    }

    fn remove(&mut self, page: &UserPage) -> Option<PmapMapping> {
        let chunk_index = self.find_chunk_for_page(*page)?;
        let removed = self.chunks[chunk_index].remove(page)?;
        self.len = self.len.saturating_sub(1);
        if self.chunks[chunk_index].is_empty() {
            self.chunks.remove(chunk_index);
        }
        Some(removed)
    }

    fn drain_range(&mut self, start: UserPage, end: UserPage) -> DrainedMappings {
        if start >= end || self.chunks.is_empty() {
            return DrainedMappings::default();
        }

        let mut entries = Vec::new();
        entries.reserve(end.0.saturating_sub(start.0).min(self.len));
        let mut shifted_entries = 0usize;
        let mut index = self
            .find_chunk_index_by_first(start)
            .unwrap_or_else(|index| index.saturating_sub(1));

        while index < self.chunks.len() {
            if self.chunks[index].first_page() >= Some(end) {
                break;
            }
            if self.chunks[index]
                .last_page()
                .is_some_and(|last| last < start)
            {
                index += 1;
                continue;
            }

            if self.chunks[index].covered_by(start, end) {
                let chunk = self.chunks.remove(index);
                self.len = self.len.saturating_sub(chunk.len());
                entries.extend(chunk.into_entries());
                continue;
            }

            let removed = self.chunks[index].drain_range(start, end);
            shifted_entries = shifted_entries.saturating_add(removed.shifted_entries());
            self.len = self.len.saturating_sub(removed.len());
            entries.extend(removed);
            if self.chunks[index].is_empty() {
                self.chunks.remove(index);
            } else {
                index += 1;
            }
        }

        DrainedMappings::new(entries, shifted_entries)
    }

    fn snapshots_in_range(
        &self,
        start: UserPage,
        end: UserPage,
    ) -> Vec<(UserPage, PmapMappingSnapshot)> {
        let mut snapshots = Vec::new();
        if start >= end {
            return snapshots;
        }
        let mut index = self
            .find_chunk_index_by_first(start)
            .unwrap_or_else(|index| index.saturating_sub(1));
        while index < self.chunks.len() {
            if self.chunks[index].first_page() >= Some(end) {
                break;
            }
            self.chunks[index].append_snapshots_in_range(start, end, &mut snapshots);
            index += 1;
        }
        snapshots
    }

    fn pages_in_range(&self, start: UserPage, end: UserPage) -> Vec<UserPage> {
        let mut pages = Vec::new();
        if start >= end {
            return pages;
        }
        let mut index = self
            .find_chunk_index_by_first(start)
            .unwrap_or_else(|index| index.saturating_sub(1));
        while index < self.chunks.len() {
            if self.chunks[index].first_page() >= Some(end) {
                break;
            }
            self.chunks[index].append_pages_in_range(start, end, &mut pages);
            index += 1;
        }
        pages
    }
}

impl ChunkedPmapResidentStore {
    fn find_chunk_for_page(&self, page: UserPage) -> Option<usize> {
        let index = self
            .find_chunk_index_by_first(page)
            .unwrap_or_else(|index| index.saturating_sub(1));
        self.chunks
            .get(index)
            .filter(|chunk| chunk.contains_page_range(page))
            .map(|_| index)
    }

    fn find_chunk_for_insert(&self, page: UserPage) -> Result<usize, usize> {
        if self.chunks.is_empty() {
            return Err(0);
        }
        match self.find_chunk_index_by_first(page) {
            Ok(index) => Ok(index),
            Err(index) => {
                if index > 0 && self.chunks[index - 1].accepts_insert(page) {
                    Ok(index - 1)
                } else {
                    Err(index)
                }
            }
        }
    }

    fn find_chunk_index_by_first(&self, page: UserPage) -> Result<usize, usize> {
        self.chunks
            .binary_search_by_key(&page, |chunk| chunk.first_page().expect("nonempty chunk"))
    }
}

#[derive(Debug, Default)]
struct ResidentChunk {
    entries: Vec<(UserPage, PmapMapping)>,
}

impl ResidentChunk {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(CHUNK_CAPACITY),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn first_page(&self) -> Option<UserPage> {
        self.entries.first().map(|(page, _)| *page)
    }

    fn last_page(&self) -> Option<UserPage> {
        self.entries.last().map(|(page, _)| *page)
    }

    fn covered_by(&self, start: UserPage, end: UserPage) -> bool {
        self.first_page().is_some_and(|first| first >= start)
            && self.last_page().is_some_and(|last| last < end)
    }

    fn contains_page_range(&self, page: UserPage) -> bool {
        self.first_page().is_some_and(|first| first <= page)
            && self.last_page().is_some_and(|last| page <= last)
    }

    fn accepts_insert(&self, page: UserPage) -> bool {
        self.last_page().is_some_and(|last| page >= last)
            || self.contains_page_range(page)
            || self.entries.len() < CHUNK_CAPACITY
    }

    fn get(&self, page: &UserPage) -> Option<&PmapMapping> {
        self.search(*page).ok().map(|index| &self.entries[index].1)
    }

    fn get_mut(&mut self, page: &UserPage) -> Option<&mut PmapMapping> {
        self.search(*page)
            .ok()
            .map(|index| &mut self.entries[index].1)
    }

    fn insert(&mut self, page: UserPage, mapping: PmapMapping) -> Option<PmapMapping> {
        match self.search(page) {
            Ok(index) => Some(core::mem::replace(&mut self.entries[index].1, mapping)),
            Err(index) => {
                self.entries.insert(index, (page, mapping));
                None
            }
        }
    }

    fn push(&mut self, page: UserPage, mapping: PmapMapping) {
        debug_assert!(self.entries.len() < CHUNK_CAPACITY);
        debug_assert!(self.last_page().is_none_or(|last| last < page));
        self.entries.push((page, mapping));
    }

    fn remove(&mut self, page: &UserPage) -> Option<PmapMapping> {
        self.search(*page)
            .ok()
            .map(|index| self.entries.remove(index).1)
    }

    fn drain_range(&mut self, start: UserPage, end: UserPage) -> DrainedMappings {
        let start_index = self.search(start).unwrap_or_else(|index| index);
        let end_index = self.search(end).unwrap_or_else(|index| index);
        if start_index >= end_index {
            return DrainedMappings::default();
        }
        let shifted_entries = self.entries.len().saturating_sub(end_index);
        let entries = self.entries.drain(start_index..end_index).collect();
        DrainedMappings::new(entries, shifted_entries)
    }

    fn split(&mut self) -> Self {
        let split_at = self.entries.len() / 2;
        Self {
            entries: self.entries.split_off(split_at),
        }
    }

    fn into_entries(self) -> Vec<(UserPage, PmapMapping)> {
        self.entries
    }

    fn append_snapshots_in_range(
        &self,
        start: UserPage,
        end: UserPage,
        out: &mut Vec<(UserPage, PmapMappingSnapshot)>,
    ) {
        let start_index = self.search(start).unwrap_or_else(|index| index);
        for (page, mapping) in self.entries[start_index..].iter() {
            if *page >= end {
                break;
            }
            out.push((*page, mapping.snapshot()));
        }
    }

    fn append_pages_in_range(&self, start: UserPage, end: UserPage, out: &mut Vec<UserPage>) {
        let start_index = self.search(start).unwrap_or_else(|index| index);
        for (page, _) in self.entries[start_index..].iter() {
            if *page >= end {
                break;
            }
            out.push(*page);
        }
    }

    fn search(&self, page: UserPage) -> Result<usize, usize> {
        self.entries
            .binary_search_by_key(&page, |(entry_page, _)| *entry_page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_backed::MaterializedPagePin;
    use crate::vm::Prot;
    use alloc::vec;
    use tx_hal::Ppn;
    use tx_substrate::page_allocator::DeviceFrame;

    fn mapping(page: usize) -> PmapMapping {
        let ppn = Ppn(0x1000 + page);
        PmapMapping::new(
            ppn,
            Prot::READ,
            MaterializedPagePin::Device(DeviceFrame::new(ppn)),
        )
    }

    fn page_values(entries: &[(UserPage, PmapMappingSnapshot)]) -> Vec<usize> {
        entries.iter().map(|(page, _)| page.0).collect()
    }

    fn drained_values(drained: DrainedMappings) -> (Vec<usize>, usize) {
        let shifted_entries = drained.shifted_entries();
        let pages = drained.into_iter().map(|(page, _)| page.0).collect();
        (pages, shifted_entries)
    }

    fn assert_ordered_backend<B: PmapResidentStoreImpl>() {
        let mut store = PmapResidentStoreWith::<B>::new();
        for page in [8, 1, 4, 2, 7, 3, 6, 5] {
            assert!(store.insert(UserPage(page), mapping(page)).is_none());
        }

        assert_eq!(store.len(), 8);
        assert_eq!(
            page_values(&store.snapshots_in_range(UserPage(0), UserPage(10))),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            store.pages_in_range(UserPage(3), UserPage(7)),
            vec![UserPage(3), UserPage(4), UserPage(5), UserPage(6)]
        );
        assert_eq!(
            store.get(&UserPage(4)).map(|mapping| mapping.ppn),
            Some(Ppn(0x1004))
        );
    }

    fn assert_replace_and_remove_backend<B: PmapResidentStoreImpl>() {
        let mut store = PmapResidentStoreWith::<B>::new();
        assert!(store.insert(UserPage(4), mapping(4)).is_none());
        let replacement = PmapMapping::new(
            Ppn(0xbeef),
            Prot::READ_WRITE,
            MaterializedPagePin::Device(DeviceFrame::new(Ppn(0xbeef))),
        );
        let previous = store
            .insert(UserPage(4), replacement)
            .expect("previous mapping");
        assert_eq!(previous.ppn, Ppn(0x1004));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(&UserPage(4)).map(|mapping| mapping.ppn),
            Some(Ppn(0xbeef))
        );

        let removed = store.remove(&UserPage(4)).expect("removed mapping");
        assert_eq!(removed.ppn, Ppn(0xbeef));
        assert_eq!(store.len(), 0);
        assert!(store.remove(&UserPage(4)).is_none());
    }

    fn assert_drain_range_backend<B: PmapResidentStoreImpl>() {
        let mut store = PmapResidentStoreWith::<B>::new();
        for page in 0..96 {
            store.insert(UserPage(page), mapping(page));
        }

        let (removed, shifted) = drained_values(store.drain_range(UserPage(16), UserPage(80)));
        assert_eq!(removed, (16..80).collect::<Vec<_>>());
        assert!(
            shifted <= 16,
            "chunked backend should not report global suffix shift"
        );
        assert_eq!(store.len(), 32);
        assert_eq!(
            page_values(&store.snapshots_in_range(UserPage(0), UserPage(96))),
            (0..16).chain(80..96).collect::<Vec<_>>()
        );
    }

    fn assert_partial_drain_keeps_boundary_chunks_searchable<B: PmapResidentStoreImpl>() {
        let mut store = PmapResidentStoreWith::<B>::new();
        for page in 0..96 {
            store.insert(UserPage(page), mapping(page));
        }

        let (removed, _) = drained_values(store.drain_range(UserPage(16), UserPage(80)));
        assert_eq!(removed, (16..80).collect::<Vec<_>>());
        assert!(store.get(&UserPage(15)).is_some());
        assert!(store.get(&UserPage(16)).is_none());
        assert!(store.get(&UserPage(79)).is_none());
        assert!(store.get(&UserPage(80)).is_some());

        assert!(store.insert(UserPage(24), mapping(24)).is_none());
        assert_eq!(
            page_values(&store.snapshots_in_range(UserPage(0), UserPage(96))),
            (0..16).chain(24..25).chain(80..96).collect::<Vec<_>>()
        );
    }

    #[test]
    fn vec_backend_preserves_order_and_lookup() {
        assert_ordered_backend::<VecPmapResidentStore>();
    }

    #[test]
    fn vec_backend_replaces_and_removes() {
        assert_replace_and_remove_backend::<VecPmapResidentStore>();
    }

    #[test]
    fn vec_backend_drains_range() {
        let mut store = PmapResidentStoreWith::<VecPmapResidentStore>::new();
        for page in 0..96 {
            store.insert(UserPage(page), mapping(page));
        }

        let (removed, shifted) = drained_values(store.drain_range(UserPage(16), UserPage(80)));
        assert_eq!(removed, (16..80).collect::<Vec<_>>());
        assert_eq!(shifted, 16);
    }

    #[test]
    fn chunked_backend_preserves_order_and_lookup() {
        assert_ordered_backend::<ChunkedPmapResidentStore>();
    }

    #[test]
    fn chunked_backend_replaces_and_removes() {
        assert_replace_and_remove_backend::<ChunkedPmapResidentStore>();
    }

    #[test]
    fn chunked_backend_drains_range_without_global_suffix_shift() {
        assert_drain_range_backend::<ChunkedPmapResidentStore>();
    }

    #[test]
    fn chunked_backend_keeps_boundary_chunks_searchable_after_partial_drain() {
        assert_partial_drain_keeps_boundary_chunks_searchable::<ChunkedPmapResidentStore>();
    }

    #[test]
    fn chunked_backend_drains_full_middle_chunks_without_entry_shift() {
        let mut store = PmapResidentStoreWith::<ChunkedPmapResidentStore>::new();
        for page in 0..192 {
            store.insert(UserPage(page), mapping(page));
        }

        let (removed, shifted) = drained_values(store.drain_range(UserPage(64), UserPage(128)));
        assert_eq!(removed, (64..128).collect::<Vec<_>>());
        assert_eq!(shifted, 0);
        assert_eq!(
            page_values(&store.snapshots_in_range(UserPage(0), UserPage(192))),
            (0..64).chain(128..192).collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunked_backend_matches_vec_for_mixed_sparse_operations() {
        let mut vec_store = PmapResidentStoreWith::<VecPmapResidentStore>::new();
        let mut chunked_store = PmapResidentStoreWith::<ChunkedPmapResidentStore>::new();
        let mut rng = 0x5eed_f00d_dead_beefu64;

        fn next(rng: &mut u64) -> usize {
            *rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (*rng >> 32) as usize
        }

        for step in 0..4096 {
            match next(&mut rng) % 5 {
                0 | 1 => {
                    let page = UserPage(next(&mut rng) % 512);
                    let vec_prev = vec_store.insert(page, mapping(page.0));
                    let chunk_prev = chunked_store.insert(page, mapping(page.0));
                    assert_eq!(vec_prev.map(|m| m.ppn), chunk_prev.map(|m| m.ppn));
                }
                2 => {
                    let page = UserPage(next(&mut rng) % 512);
                    let vec_removed = vec_store.remove(&page);
                    let chunk_removed = chunked_store.remove(&page);
                    assert_eq!(
                        vec_removed.map(|m| m.ppn),
                        chunk_removed.map(|m| m.ppn),
                        "remove diverged at step {step} for page {:?}",
                        page
                    );
                }
                3 => {
                    let a = next(&mut rng) % 512;
                    let b = next(&mut rng) % 512;
                    let start = UserPage(a.min(b));
                    let end = UserPage(a.max(b).saturating_add(1));
                    let (vec_removed, _) = drained_values(vec_store.drain_range(start, end));
                    let (chunk_removed, _) = drained_values(chunked_store.drain_range(start, end));
                    assert_eq!(
                        vec_removed, chunk_removed,
                        "drain diverged at step {step} for {:?}..{:?}",
                        start, end
                    );
                }
                _ => {
                    let a = next(&mut rng) % 512;
                    let b = next(&mut rng) % 512;
                    let start = UserPage(a.min(b));
                    let end = UserPage(a.max(b).saturating_add(1));
                    assert_eq!(
                        page_values(&vec_store.snapshots_in_range(start, end)),
                        page_values(&chunked_store.snapshots_in_range(start, end)),
                        "snapshot diverged at step {step} for {:?}..{:?}",
                        start,
                        end
                    );
                    assert_eq!(
                        vec_store.pages_in_range(start, end),
                        chunked_store.pages_in_range(start, end),
                        "page walk diverged at step {step} for {:?}..{:?}",
                        start,
                        end
                    );
                }
            }

            assert_eq!(
                vec_store.len(),
                chunked_store.len(),
                "len diverged at step {step}"
            );
            assert_eq!(
                page_values(&vec_store.snapshots_in_range(UserPage(0), UserPage(512))),
                page_values(&chunked_store.snapshots_in_range(UserPage(0), UserPage(512))),
                "full snapshot diverged at step {step}"
            );
        }
    }
}
