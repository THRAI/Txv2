//! Resident pmap shadow index.
//!
//! VM recipes are the authoritative address bindings; HAL PTEs are derived
//! materializations. This store is only the pmap-local shadow index that lets
//! VM enumerate and retain published materializations for lookup, mprotect,
//! munmap, fork, exec, and drop. The active VM spec requires those operations
//! to enumerate resident pages and preserve `MapPin` ownership, but it does not
//! prescribe this module's internal data structure.
//!
//! `PmapResidentStore` is the stable facade used by `VmPmap`. The v1 backend is
//! an address-sorted `Vec`, preserved for behavior while later work swaps in a
//! non-shifting teardown backend behind this boundary.

use alloc::vec::Vec;

use super::{PmapMapping, PmapMappingSnapshot};
use crate::vm::UserPage;

#[derive(Debug, Default)]
pub(super) struct PmapResidentStore {
    backend: VecPmapResidentStore,
}

impl PmapResidentStore {
    pub(super) fn new() -> Self {
        Self {
            backend: VecPmapResidentStore::new(),
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
struct VecPmapResidentStore {
    entries: Vec<(UserPage, PmapMapping)>,
}

impl VecPmapResidentStore {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

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

    fn search(&self, page: UserPage) -> Result<usize, usize> {
        self.entries
            .binary_search_by_key(&page, |(entry_page, _)| *entry_page)
    }
}
