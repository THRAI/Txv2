//! Authoritative staged recipe range index.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::SpinMutex;

use super::{
    AddressSpaceStats, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntry,
    VmEntryError, VmEntryFlags, VmMapCommit, VmMapError, USER_PAGE_SIZE,
};

type RecipeTree = BTreeMap<UserVirtAddr, VmEntry>;

/// Authoritative recipe range index.
///
/// This wrapper is backed by `BTreeMap` keyed by mapping start address, so it
/// is no longer the artificial fixed-capacity array from the first slice.
/// Readers clone an owned tree snapshot; writers publish a whole replacement
/// tree after validating and rewriting the old one. This gives observers a
/// complete pre- or post-mutation recipe set while still stopping short of the
/// final VM_v1_2 persistent/epoch range index.
pub(in crate::vm) struct RecipeIndex {
    published: SpinMutex<RecipeTree>,
}

#[derive(Clone)]
pub(in crate::vm) struct RecipeSnapshot {
    entries: RecipeTree,
}

impl RecipeSnapshot {
    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        lookup_in(&self.entries, addr)
    }

    pub(in crate::vm) fn stats(&self) -> AddressSpaceStats {
        stats_for(&self.entries)
    }

    pub(in crate::vm) fn find_free_range(
        &self,
        window: UserRange,
        page_count: usize,
    ) -> Option<UserRange> {
        find_gap_in(&self.entries, window, page_count)
    }

    pub(in crate::vm) fn overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        overlapping_in(&self.entries, range)
    }

    pub(in crate::vm) fn snapshot(&self) -> Vec<VmEntry> {
        self.entries.values().cloned().collect()
    }
}

impl RecipeIndex {
    pub(in crate::vm) fn new() -> Self {
        Self {
            published: SpinMutex::new(BTreeMap::new()),
        }
    }

    pub(in crate::vm) fn snapshot_reader(&self) -> RecipeSnapshot {
        let published = self.published.lock();
        RecipeSnapshot {
            entries: RecipeTree::clone(&published),
        }
    }

    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        self.snapshot_reader().lookup(addr)
    }

    pub(in crate::vm) fn stats(&self) -> AddressSpaceStats {
        self.snapshot_reader().stats()
    }

    pub(in crate::vm) fn find_free_range(
        &self,
        window: UserRange,
        page_count: usize,
    ) -> Option<UserRange> {
        self.snapshot_reader().find_free_range(window, page_count)
    }

    pub(in crate::vm) fn overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        self.snapshot_reader().overlapping(range)
    }

    pub(in crate::vm) fn snapshot(&self) -> Vec<VmEntry> {
        self.snapshot_reader().snapshot()
    }

    pub(in crate::vm) fn validate_map(
        &self,
        entry: &VmEntry,
        placement: MapPlacement,
    ) -> Result<(), VmMapError> {
        let snapshot = self.snapshot_reader();
        match placement {
            MapPlacement::RequireFree => validate_insert_free(&snapshot.entries, entry),
            MapPlacement::FixedReplace => rewrite_fixed(&snapshot.entries, entry).map(|_| ()),
        }
    }

    pub(in crate::vm) fn commit_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let mut published = self.published.lock();
        match placement {
            MapPlacement::RequireFree => {
                validate_insert_free(&published, &entry)?;
                let changed_pages = entry.range.page_count();
                let mut rewritten = RecipeTree::clone(&published);
                push_entry(&mut rewritten, entry);
                *published = rewritten;
                Ok(VmMapCommit { changed_pages })
            }
            MapPlacement::FixedReplace => {
                let (rewritten, changed_pages) = rewrite_fixed(&published, &entry)?;
                *published = rewritten;
                Ok(VmMapCommit { changed_pages })
            }
        }
    }

    pub(in crate::vm) fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let mut published = self.published.lock();
        let (rewritten, changed_pages) = rewrite_unmap(&published, range)?;
        *published = rewritten;
        Ok(VmMapCommit { changed_pages })
    }

    pub(in crate::vm) fn protect(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        let mut published = self.published.lock();
        let (rewritten, changed_pages) = rewrite_protect(&published, range, prot)?;
        *published = rewritten;
        Ok(VmMapCommit { changed_pages })
    }

    pub(in crate::vm) fn remap_disjoint(
        &self,
        old_range: UserRange,
        new_range: UserRange,
    ) -> Result<VmMapCommit, VmMapError> {
        let mut published = self.published.lock();
        let (rewritten, changed_pages) = rewrite_remap_disjoint(&published, old_range, new_range)?;
        *published = rewritten;
        Ok(VmMapCommit { changed_pages })
    }
}

pub(in crate::vm) struct AddressSpaceStatsCell {
    recipe_count: AtomicUsize,
    vm_size: AtomicUsize,
}

impl AddressSpaceStatsCell {
    pub(in crate::vm) const fn new() -> Self {
        Self {
            recipe_count: AtomicUsize::new(0),
            vm_size: AtomicUsize::new(0),
        }
    }

    pub(in crate::vm) fn load(&self) -> AddressSpaceStats {
        AddressSpaceStats {
            recipe_count: self.recipe_count.load(Ordering::Acquire),
            vm_size: self.vm_size.load(Ordering::Acquire),
        }
    }

    pub(in crate::vm) fn store(&self, stats: AddressSpaceStats) {
        self.recipe_count
            .store(stats.recipe_count, Ordering::Release);
        self.vm_size.store(stats.vm_size, Ordering::Release);
    }
}

fn validate_insert_free(entries: &RecipeTree, entry: &VmEntry) -> Result<(), VmMapError> {
    if entries
        .values()
        .any(|existing| existing.range.overlaps(entry.range))
    {
        return Err(VmMapError::AlreadyMapped);
    }

    Ok(())
}

fn rewrite_fixed(
    entries: &RecipeTree,
    replacement: &VmEntry,
) -> Result<(RecipeTree, usize), VmMapError> {
    let mut rewritten = RecipeTree::new();
    let mut replaced_pages = 0;

    for existing in entries.values().cloned() {
        let Some(overlap) = range_intersection(existing.range, replacement.range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        replaced_pages += overlap.page_count();
        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }
    }

    push_entry(&mut rewritten, replacement.clone());
    Ok((rewritten, replaced_pages + replacement.range.page_count()))
}

fn rewrite_unmap(
    entries: &RecipeTree,
    range: UserRange,
) -> Result<(RecipeTree, usize), VmMapError> {
    let mut rewritten = RecipeTree::new();
    let mut changed_pages = 0;

    for existing in entries.values().cloned() {
        let Some(overlap) = range_intersection(existing.range, range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        changed_pages += overlap.page_count();
        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }
    }

    Ok((rewritten, changed_pages))
}

fn rewrite_protect(
    entries: &RecipeTree,
    range: UserRange,
    prot: Prot,
) -> Result<(RecipeTree, usize), VmMapError> {
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }

    let mut rewritten = RecipeTree::new();
    let mut changed_pages = 0;

    for existing in entries.values().cloned() {
        let Some(overlap) = range_intersection(existing.range, range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        changed_pages += overlap.page_count();
        let rewrite = existing
            .split_for_protect(overlap, prot)
            .map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(target) = rewrite.target {
            push_entry(&mut rewritten, target);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }
    }

    Ok((rewritten, changed_pages))
}

fn rewrite_remap_disjoint(
    entries: &RecipeTree,
    old_range: UserRange,
    new_range: UserRange,
) -> Result<(RecipeTree, usize), VmMapError> {
    if old_range.overlaps(new_range) || old_range.len() != new_range.len() {
        return Err(VmMapError::InvalidRange);
    }
    if !range_is_fully_mapped(entries, old_range) {
        return Err(VmMapError::MissingMapping);
    }
    validate_insert_free(
        entries,
        &VmEntry::new(
            new_range,
            Prot::NONE,
            VmEntryFlags::PRIVATE,
            VmBacking::None,
        ),
    )?;

    let mut rewritten = RecipeTree::new();
    let mut moved = Vec::new();

    for existing in entries.values().cloned() {
        let Some(overlap) = range_intersection(existing.range, old_range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }

        let moving = existing
            .split_for_protect(overlap, existing.prot)
            .map_err(vm_entry_error)?
            .target
            .ok_or(VmMapError::MissingMapping)?;
        let delta = overlap.start().as_usize() - old_range.start().as_usize();
        let target_start = UserVirtAddr(
            new_range
                .start()
                .as_usize()
                .checked_add(delta)
                .ok_or(VmMapError::InvalidRange)?,
        );
        let target_range = UserRange::new_aligned(target_start, overlap.len())
            .map_err(|_| VmMapError::InvalidRange)?;
        moved.push(VmEntry::new(
            target_range,
            moving.prot,
            moving.flags,
            moving.backing,
        ));
    }

    for entry in moved {
        push_entry(&mut rewritten, entry);
    }

    Ok((rewritten, old_range.page_count() + new_range.page_count()))
}

fn range_is_fully_mapped(entries: &RecipeTree, range: UserRange) -> bool {
    let mut cursor = range.start().as_usize();
    let end = range.end().as_usize();

    while cursor < end {
        let Some(entry) = lookup_in(entries, UserVirtAddr(cursor)) else {
            return false;
        };
        cursor = entry.range.end().as_usize().min(end);
    }

    true
}

fn push_entry(entries: &mut RecipeTree, entry: VmEntry) {
    entries.insert(entry.range.start(), entry);
}

fn lookup_in(entries: &RecipeTree, addr: UserVirtAddr) -> Option<VmEntry> {
    entries
        .range(..=addr)
        .next_back()
        .map(|(_, entry)| entry.clone())
        .filter(|entry| entry.range.contains_addr(addr))
}

fn stats_for(entries: &RecipeTree) -> AddressSpaceStats {
    let mut stats = AddressSpaceStats::default();
    for entry in entries.values() {
        stats.recipe_count += 1;
        stats.vm_size += entry.range.len();
    }
    stats
}

fn find_gap_in(entries: &RecipeTree, window: UserRange, page_count: usize) -> Option<UserRange> {
    let len = page_count.checked_mul(USER_PAGE_SIZE)?;
    if len == 0 || len > window.len() {
        return None;
    }

    let mut cursor = window.start().as_usize();
    let window_end = window.end().as_usize();

    for entry in entries.values() {
        if entry.range.end().as_usize() <= cursor {
            continue;
        }
        if entry.range.start().as_usize() >= window_end {
            break;
        }
        if !entry.range.overlaps(window) {
            continue;
        }

        let entry_start = entry
            .range
            .start()
            .as_usize()
            .max(window.start().as_usize());
        if cursor.checked_add(len)? <= entry_start {
            return UserRange::new_aligned(UserVirtAddr(cursor), len).ok();
        }
        cursor = cursor.max(entry.range.end().as_usize());
        if cursor >= window_end {
            return None;
        }
    }

    if cursor.checked_add(len)? <= window_end {
        UserRange::new_aligned(UserVirtAddr(cursor), len).ok()
    } else {
        None
    }
}

fn overlapping_in(entries: &RecipeTree, range: UserRange) -> Vec<VmEntry> {
    entries
        .values()
        .filter(|entry| entry.range.overlaps(range))
        .cloned()
        .collect()
}

fn range_intersection(a: UserRange, b: UserRange) -> Option<UserRange> {
    let start = a.start().as_usize().max(b.start().as_usize());
    let end = a.end().as_usize().min(b.end().as_usize());
    if start >= end {
        return None;
    }
    UserRange::new_aligned(UserVirtAddr(start), end - start).ok()
}

pub(in crate::vm) fn vm_entry_error(error: VmEntryError) -> VmMapError {
    match error {
        VmEntryError::BackingOffsetOverflow => VmMapError::BackingOffsetOverflow,
        VmEntryError::Range(_) | VmEntryError::RangeNotContained => VmMapError::AlreadyMapped,
    }
}
