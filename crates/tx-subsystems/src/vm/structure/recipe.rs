//! Authoritative recipe range index, published lock-free under EBR.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::vm::adapter::step_engine::{epoch_mod as epoch, SpinMutex};

use crate::execution::Guard;

use super::{
    AddressSpaceStats, MapPlacement, Prot, UfdRegistration, UserRange, UserVirtAddr, VmBacking,
    VmEntry, VmEntryError, VmEntryFlags, VmMapCommit, VmMapError, VmRemapPlacement, USER_PAGE_SIZE,
};

type RecipeTree = BTreeMap<UserVirtAddr, VmEntry>;

/// Authoritative recipe range index.
///
/// The published tree is owned by an `AtomicPtr<RecipeTree>` and reachable via
/// EBR (`adapter::step_engine::epoch_mod`): readers under a `Guard<'_>` perform a single
/// `Acquire` load and observe the immutable tree without holding a lock. A
/// separate writer `mutation` mutex serializes mutators so they can build a
/// replacement tree, atomically swap it in, and retire the old one through
/// `epoch::retire_raw`. Old trees are freed by EBR once every reader past has
/// drained, satisfying VM_v1_2 §1.2 publication rule with guard-scoped reader
/// lifetimes.
pub(in crate::vm) struct RecipeIndex {
    current: AtomicPtr<RecipeTree>,
    mutation: SpinMutex<()>,
}

impl Drop for RecipeIndex {
    fn drop(&mut self) {
        let raw = self.current.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !raw.is_null() {
            // SAFETY: `current` was last published from `Box::into_raw`.
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(in crate::vm) struct RecipeSnapshot {
    entries: RecipeTree,
}

#[cfg(test)]
impl RecipeSnapshot {
    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        lookup_in(&self.entries, addr)
    }

    pub(in crate::vm) fn snapshot(&self) -> Vec<VmEntry> {
        self.entries.values().cloned().collect()
    }
}

impl RecipeIndex {
    pub(in crate::vm) fn new() -> Self {
        let initial = Box::into_raw(Box::new(RecipeTree::new()));
        Self {
            current: AtomicPtr::new(initial),
            mutation: SpinMutex::new(()),
        }
    }

    /// Borrow the published tree under the caller-supplied epoch guard. The
    /// returned reference is valid for the guard's lifetime.
    fn pinned<'g>(&self, guard: &'g Guard<'_>) -> &'g RecipeTree {
        let _ = guard;
        let raw = self.current.load(Ordering::Acquire);
        // SAFETY: the guard pins this CPU at the publication epoch, so the
        // tree behind `raw` is not yet reclaimed. Writers retire the old
        // pointer only after swap, and EBR guarantees no pinned reader can be
        // observing a stale pointer issued before its guard.
        unsafe { &*raw }
    }

    #[cfg(test)]
    pub(in crate::vm) fn snapshot_reader(&self, guard: &Guard<'_>) -> RecipeSnapshot {
        RecipeSnapshot {
            entries: RecipeTree::clone(self.pinned(guard)),
        }
    }

    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr, guard: &Guard<'_>) -> Option<VmEntry> {
        lookup_in(self.pinned(guard), addr)
    }

    pub(in crate::vm) fn stats(&self, guard: &Guard<'_>) -> AddressSpaceStats {
        stats_for(self.pinned(guard))
    }

    pub(in crate::vm) fn find_free_range(
        &self,
        window: UserRange,
        page_count: usize,
        guard: &Guard<'_>,
    ) -> Option<UserRange> {
        find_gap_in(self.pinned(guard), window, page_count)
    }

    pub(in crate::vm) fn overlapping(&self, range: UserRange, guard: &Guard<'_>) -> Vec<VmEntry> {
        overlapping_in(self.pinned(guard), range)
    }

    pub(in crate::vm) fn snapshot(&self, guard: &Guard<'_>) -> Vec<VmEntry> {
        self.pinned(guard).values().cloned().collect()
    }

    pub(in crate::vm) fn validate_map(
        &self,
        entry: &VmEntry,
        placement: MapPlacement,
        guard: &Guard<'_>,
    ) -> Result<(), VmMapError> {
        let entries = self.pinned(guard);
        match placement {
            MapPlacement::RequireFree => validate_insert_free(entries, entry),
            MapPlacement::FixedReplace => rewrite_fixed(entries, entry).map(|_| ()),
        }
    }

    /// Replace the published tree with `next` and retire the old one through
    /// EBR. Requires the writer mutation lock to already be held.
    fn publish(&self, next: RecipeTree) {
        let new_ptr = Box::into_raw(Box::new(next));
        let old_ptr = self.current.swap(new_ptr, Ordering::AcqRel);
        if !old_ptr.is_null() {
            // SAFETY: every reader holds an epoch guard issued before its
            // load; EBR delays reclamation until those guards drop. Once
            // drained, `reclaim_recipe_tree` runs and frees the box.
            let _ = unsafe { epoch::retire_raw(old_ptr as *mut u8, reclaim_recipe_tree) };
        }
    }

    /// Borrow the published tree behind the writer mutation lock. The
    /// returned reference is valid until the lock is released.
    ///
    /// # Safety
    ///
    /// Must only be called while `self.mutation` is held by the caller; the
    /// caller is responsible for not retaining the borrow past the swap.
    unsafe fn under_writer_lock(&self) -> &RecipeTree {
        let raw = self.current.load(Ordering::Acquire);
        // SAFETY: the writer mutation lock prevents concurrent writers from
        // swapping or retiring; readers are EBR-protected separately.
        unsafe { &*raw }
    }

    pub(in crate::vm) fn commit_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        // SAFETY: writer lock held, so under_writer_lock's borrow is sound.
        let current = unsafe { self.under_writer_lock() };
        match placement {
            MapPlacement::RequireFree => {
                validate_insert_free(current, &entry)?;
                let changed_pages = entry.range.page_count();
                let mut rewritten = RecipeTree::clone(current);
                push_entry(&mut rewritten, entry);
                self.publish(rewritten);
                Ok(VmMapCommit { changed_pages })
            }
            MapPlacement::FixedReplace => {
                let (rewritten, changed_pages) = rewrite_fixed(current, &entry)?;
                self.publish(rewritten);
                Ok(VmMapCommit { changed_pages })
            }
        }
    }

    pub(in crate::vm) fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages) = rewrite_unmap(current, range)?;
        self.publish(rewritten);
        Ok(VmMapCommit { changed_pages })
    }

    pub(in crate::vm) fn protect(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages) = rewrite_protect(current, range, prot)?;
        self.publish(rewritten);
        Ok(VmMapCommit { changed_pages })
    }

    pub(in crate::vm) fn set_locked(
        &self,
        range: UserRange,
        locked: bool,
    ) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages) = rewrite_locked(current, range, locked)?;
        self.publish(rewritten);
        Ok(VmMapCommit { changed_pages })
    }

    pub(in crate::vm) fn remap(
        &self,
        old_range: UserRange,
        new_range: UserRange,
        placement: VmRemapPlacement,
        destination: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages) = match placement {
            VmRemapPlacement::Move => {
                rewrite_remap_disjoint(current, old_range, new_range, destination)?
            }
            VmRemapPlacement::InPlace => rewrite_remap_in_place(current, old_range, new_range)?,
        };
        self.publish(rewritten);
        Ok(VmMapCommit { changed_pages })
    }

    /// PR-10 phase 3: stamp `tag` on every VMA whose range is fully
    /// contained in `range`. The range must be fully covered by VMAs
    /// (every byte mapped) and each covering VMA must be fully contained
    /// in `range` — partial overlap returns
    /// [`VmMapError::MissingMapping`] so the ufd shim can map that to
    /// the Linux `-EINVAL` it returns for unaligned registrations.
    ///
    /// On success returns a [`VmMapCommit`] whose `changed_pages` is
    /// the page count of every VMA tagged. The recipe tree is
    /// published atomically under the writer mutation lock; readers
    /// observe either the pre-tag or post-tag state, never a torn
    /// view.
    ///
    /// Phase 3 contract: this is the **only** structural change the
    /// shim drives; the fault path (`fault_script`) does not yet read
    /// the tag — that branch lands in phase 4.
    pub(in crate::vm) fn tag_ufd_registration(
        &self,
        range: UserRange,
        tag: UfdRegistration,
    ) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages) = rewrite_tag_ufd_registration(current, range, tag)?;
        self.publish(rewritten);
        Ok(VmMapCommit { changed_pages })
    }
}

unsafe fn reclaim_recipe_tree(ptr: *mut u8) {
    // SAFETY: `ptr` was last published from `Box::into_raw(Box::new(_))`.
    let _ = unsafe { Box::from_raw(ptr as *mut RecipeTree) };
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

fn rewrite_locked(
    entries: &RecipeTree,
    range: UserRange,
    locked: bool,
) -> Result<(RecipeTree, usize), VmMapError> {
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }

    let mut rewritten = RecipeTree::new();
    let mut changed_pages = 0;

    for existing in entries.values().cloned() {
        if existing.range.overlaps(range) {
            changed_pages += existing.range.page_count();
            push_entry(&mut rewritten, existing.with_locked(locked));
        } else {
            push_entry(&mut rewritten, existing);
        }
    }
    Ok((rewritten, changed_pages))
}

fn rewrite_remap_disjoint(
    entries: &RecipeTree,
    old_range: UserRange,
    new_range: UserRange,
    destination: MapPlacement,
) -> Result<(RecipeTree, usize), VmMapError> {
    if old_range.overlaps(new_range) {
        return Err(VmMapError::InvalidRange);
    }
    if !range_is_fully_mapped(entries, old_range) {
        return Err(VmMapError::MissingMapping);
    }
    if destination == MapPlacement::RequireFree {
        validate_insert_free(
            entries,
            &VmEntry::new(
                new_range,
                Prot::NONE,
                VmEntryFlags::PRIVATE,
                VmBacking::None,
            ),
        )?;
    }

    let move_len = old_range.len().min(new_range.len());
    let move_source = UserRange::new_aligned(old_range.start(), move_len)
        .map_err(|_| VmMapError::InvalidRange)?;
    let mut rewritten = RecipeTree::new();
    let mut moved = Vec::new();

    for existing in entries.values().cloned() {
        let Some(moving_overlap) = range_intersection(existing.range, move_source) else {
            continue;
        };
        moved.push(rebased_entry_for_move(
            existing,
            moving_overlap,
            old_range,
            new_range,
        )?);
    }

    if new_range.len() > old_range.len() {
        let Some(last) = moved.pop() else {
            return Err(VmMapError::MissingMapping);
        };
        let tail_len = new_range.len() - old_range.len();
        let expanded_range = UserRange::new_aligned(
            last.range.start(),
            last.range
                .len()
                .checked_add(tail_len)
                .ok_or(VmMapError::InvalidRange)?,
        )
        .map_err(|_| VmMapError::InvalidRange)?;
        moved.push(entry_with_range(&last, expanded_range)?);
    }

    for existing in entries.values() {
        for entry in remove_remap_covered_ranges(existing, old_range, new_range)? {
            push_entry(&mut rewritten, entry);
        }
    }
    for entry in moved {
        push_entry(&mut rewritten, entry);
    }

    Ok((rewritten, old_range.page_count() + new_range.page_count()))
}

fn remove_remap_covered_ranges(
    entry: &VmEntry,
    old_range: UserRange,
    new_range: UserRange,
) -> Result<Vec<VmEntry>, VmMapError> {
    let mut pieces = alloc::vec![entry.clone()];
    for range in [old_range, new_range] {
        let mut next = Vec::new();
        for piece in pieces {
            let Some(overlap) = range_intersection(piece.range, range) else {
                next.push(piece);
                continue;
            };
            let rewrite = piece.split_for_unmap(overlap).map_err(vm_entry_error)?;
            if let Some(before) = rewrite.before {
                next.push(before);
            }
            if let Some(after) = rewrite.after {
                next.push(after);
            }
        }
        pieces = next;
    }
    Ok(pieces)
}

fn rebased_entry_for_move(
    existing: VmEntry,
    moving_overlap: UserRange,
    old_range: UserRange,
    new_range: UserRange,
) -> Result<VmEntry, VmMapError> {
    let moving = existing
        .split_for_protect(moving_overlap, existing.prot)
        .map_err(vm_entry_error)?
        .target
        .ok_or(VmMapError::MissingMapping)?;
    let delta = moving_overlap.start().as_usize() - old_range.start().as_usize();
    let target_start = UserVirtAddr(
        new_range
            .start()
            .as_usize()
            .checked_add(delta)
            .ok_or(VmMapError::InvalidRange)?,
    );
    let target_range = UserRange::new_aligned(target_start, moving_overlap.len())
        .map_err(|_| VmMapError::InvalidRange)?;
    // Preserve the moving VmEntry's private CoW set (mapping identity
    // follows the VmEntry across mremap-move per the plan).
    Ok(
        VmEntry::new(target_range, moving.prot, moving.flags, moving.backing)
            .with_ufd_registration(moving.ufd_registration)
            .with_private(moving.private),
    )
}

fn rewrite_remap_in_place(
    entries: &RecipeTree,
    old_range: UserRange,
    new_range: UserRange,
) -> Result<(RecipeTree, usize), VmMapError> {
    if old_range.start() != new_range.start() {
        return Err(VmMapError::InvalidRange);
    }
    if !range_is_fully_mapped(entries, old_range) {
        return Err(VmMapError::MissingMapping);
    }

    if new_range.len() == old_range.len() {
        return Ok((RecipeTree::clone(entries), 0));
    }

    if new_range.len() < old_range.len() {
        let shrink_start = UserVirtAddr(
            old_range
                .start()
                .as_usize()
                .checked_add(new_range.len())
                .ok_or(VmMapError::InvalidRange)?,
        );
        let shrink_range = UserRange::new_aligned(shrink_start, old_range.len() - new_range.len())
            .map_err(|_| VmMapError::InvalidRange)?;
        return rewrite_unmap(entries, shrink_range);
    }

    let grow_start = old_range.end();
    let grow_len = new_range.len() - old_range.len();
    let grow_range =
        UserRange::new_aligned(grow_start, grow_len).map_err(|_| VmMapError::InvalidRange)?;
    validate_insert_free(
        entries,
        &VmEntry::new(
            grow_range,
            Prot::NONE,
            VmEntryFlags::PRIVATE,
            VmBacking::None,
        ),
    )?;

    let mut rewritten = RecipeTree::new();
    let mut expanded = false;

    for existing in entries.values().cloned() {
        if existing.range == old_range && !expanded {
            push_entry(&mut rewritten, entry_with_range(&existing, new_range)?);
            expanded = true;
        } else {
            push_entry(&mut rewritten, existing);
        }
    }

    if !expanded {
        return Err(VmMapError::InvalidRange);
    }

    Ok((rewritten, old_range.page_count() + new_range.page_count()))
}

fn entry_with_range(entry: &VmEntry, range: UserRange) -> Result<VmEntry, VmMapError> {
    if entry.range.start() != range.start() || range.end().as_usize() < entry.range.end().as_usize()
    {
        return Err(VmMapError::InvalidRange);
    }
    Ok(
        VmEntry::new(range, entry.prot, entry.flags, entry.backing.clone())
            .with_ufd_registration(entry.ufd_registration)
            .with_private(entry.private.clone()),
    )
}

fn rewrite_tag_ufd_registration(
    entries: &RecipeTree,
    range: UserRange,
    tag: UfdRegistration,
) -> Result<(RecipeTree, usize), VmMapError> {
    // Phase 3 only handles whole-VMA registrations: every byte of
    // `range` must be mapped, and no VMA in `range` may straddle the
    // boundary (start before / end after the requested range). The
    // partial-overlap case maps to the Linux `-EINVAL` the shim
    // returns when `UFFDIO_REGISTER` is given an unaligned range.
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }

    let mut rewritten = RecipeTree::new();
    let mut tagged_pages = 0;
    for existing in entries.values().cloned() {
        if existing.range.overlaps(range) {
            if !range.contains_range(existing.range) {
                // Partial-VMA registration is out of scope for phase
                // 3. The shim is expected to pre-align the registered
                // range to VMA boundaries; we surface the misuse here
                // rather than silently re-tagging part of a VMA.
                return Err(VmMapError::MissingMapping);
            }
            tagged_pages += existing.range.page_count();
            push_entry(&mut rewritten, existing.with_ufd_registration(Some(tag)));
        } else {
            push_entry(&mut rewritten, existing);
        }
    }

    Ok((rewritten, tagged_pages))
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
        VmEntryError::Private(err) => VmMapError::Private(err),
    }
}
