//! Authoritative recipe range index, published lock-free under EBR.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::vm::adapter::step_engine::{epoch_mod as epoch, SpinMutex};

use crate::execution::Guard;

use super::{
    AddressSpaceStats, AddressSpaceStatsDelta, MapPlacement, Prot, UfdRegistration, UserRange,
    UserVirtAddr, VmBacking, VmEntry, VmEntryError, VmEntryFlags, VmMapCommit, VmMapError,
    VmRemapPlacement, USER_PAGE_SIZE,
};

#[derive(Clone, Default)]
struct RecipeTree {
    root: Option<Arc<RecipeNode>>,
    len: usize,
    vm_size: usize,
}

struct RecipeNode {
    key: UserVirtAddr,
    priority: u64,
    entry: VmEntry,
    left: Option<Arc<RecipeNode>>,
    right: Option<Arc<RecipeNode>>,
    subtree_len: usize,
    subtree_vm_size: usize,
}

impl RecipeTree {
    fn new() -> Self {
        Self::default()
    }

    fn values_vec(&self) -> Vec<VmEntry> {
        let mut out = Vec::new();
        collect_values(&self.root, &mut out);
        out
    }

    fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            if node.key.as_usize() <= addr.as_usize() {
                candidate = Some(node.entry.clone());
                cursor = node.right.as_deref();
            } else {
                cursor = node.left.as_deref();
            }
        }
        candidate.filter(|entry| entry.range.contains_addr(addr))
    }

    fn predecessor_entry(&self, key: UserVirtAddr) -> Option<VmEntry> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            if node.key.as_usize() <= key.as_usize() {
                candidate = Some(node.entry.clone());
                cursor = node.right.as_deref();
            } else {
                cursor = node.left.as_deref();
            }
        }
        candidate
    }

    fn predecessor_entry_before(&self, key: UserVirtAddr) -> Option<VmEntry> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            if node.key.as_usize() < key.as_usize() {
                candidate = Some(node.entry.clone());
                cursor = node.right.as_deref();
            } else {
                cursor = node.left.as_deref();
            }
        }
        candidate
    }

    fn successor_entry(&self, key: UserVirtAddr) -> Option<VmEntry> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            if node.key.as_usize() >= key.as_usize() {
                candidate = Some(node.entry.clone());
                cursor = node.left.as_deref();
            } else {
                cursor = node.right.as_deref();
            }
        }
        candidate
    }

    fn overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        let mut out = Vec::new();
        if let Some(entry) = self.predecessor_entry(range.start()) {
            if entry.range.start().as_usize() < range.start().as_usize()
                && entry.range.overlaps(range)
            {
                out.push(entry);
            }
        }
        collect_starting_in(&self.root, range, &mut out);
        out
    }

    fn insert_entry(&self, entry: VmEntry) -> (Self, usize) {
        let mut touched = 0usize;
        let root = insert_node(self.root.clone(), entry.clone(), &mut touched);
        (
            Self {
                root: Some(root),
                len: self.len + 1,
                vm_size: self.vm_size + entry.range.len(),
            },
            touched,
        )
    }

    fn remove_exact(&self, key: UserVirtAddr) -> (Self, Option<VmEntry>, usize) {
        let mut touched = 0usize;
        let mut removed = None;
        let root = remove_node(self.root.clone(), key, &mut removed, &mut touched);
        let (len, vm_size) = match &removed {
            Some(entry) => (self.len - 1, self.vm_size - entry.range.len()),
            None => (self.len, self.vm_size),
        };
        (Self { root, len, vm_size }, removed, touched)
    }

    fn replace_exact(&self, entry: VmEntry) -> Result<(Self, usize), VmMapError> {
        let (without_existing, removed, mut touched) = self.remove_exact(entry.range.start());
        let Some(removed) = removed else {
            return Err(VmMapError::MissingMapping);
        };
        if removed.range != entry.range {
            return Err(VmMapError::InvalidRange);
        }
        let (rewritten, insert_touched) = without_existing.insert_entry(entry);
        touched += insert_touched;
        Ok((rewritten, touched))
    }
}

fn collect_values(root: &Option<Arc<RecipeNode>>, out: &mut Vec<VmEntry>) {
    let Some(node) = root else {
        return;
    };
    collect_values(&node.left, out);
    out.push(node.entry.clone());
    collect_values(&node.right, out);
}

fn collect_starting_in(root: &Option<Arc<RecipeNode>>, range: UserRange, out: &mut Vec<VmEntry>) {
    let Some(node) = root else {
        return;
    };
    if node.key.as_usize() >= range.start().as_usize() {
        collect_starting_in(&node.left, range, out);
    }
    if node.key.as_usize() >= range.start().as_usize()
        && node.key.as_usize() < range.end().as_usize()
        && node.entry.range.overlaps(range)
    {
        out.push(node.entry.clone());
    }
    if node.key.as_usize() < range.end().as_usize() {
        collect_starting_in(&node.right, range, out);
    }
}

fn node_len(root: &Option<Arc<RecipeNode>>) -> usize {
    root.as_ref().map_or(0, |node| node.subtree_len)
}

fn node_vm_size(root: &Option<Arc<RecipeNode>>) -> usize {
    root.as_ref().map_or(0, |node| node.subtree_vm_size)
}

fn build_node(
    key: UserVirtAddr,
    priority: u64,
    entry: VmEntry,
    left: Option<Arc<RecipeNode>>,
    right: Option<Arc<RecipeNode>>,
) -> Arc<RecipeNode> {
    let subtree_len = 1 + node_len(&left) + node_len(&right);
    let subtree_vm_size = entry.range.len() + node_vm_size(&left) + node_vm_size(&right);
    Arc::new(RecipeNode {
        key,
        priority,
        entry,
        left,
        right,
        subtree_len,
        subtree_vm_size,
    })
}

fn insert_node(
    root: Option<Arc<RecipeNode>>,
    entry: VmEntry,
    touched: &mut usize,
) -> Arc<RecipeNode> {
    let Some(node) = root else {
        *touched += 1;
        return build_node(
            entry.range.start(),
            recipe_priority(entry.range.start()),
            entry,
            None,
            None,
        );
    };

    *touched += 1;
    if entry.range.start().as_usize() < node.key.as_usize() {
        let left = Some(insert_node(node.left.clone(), entry, touched));
        let rebuilt = build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            left,
            node.right.clone(),
        );
        rotate_right_if_needed(rebuilt)
    } else {
        let right = Some(insert_node(node.right.clone(), entry, touched));
        let rebuilt = build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            node.left.clone(),
            right,
        );
        rotate_left_if_needed(rebuilt)
    }
}

fn remove_node(
    root: Option<Arc<RecipeNode>>,
    key: UserVirtAddr,
    removed: &mut Option<VmEntry>,
    touched: &mut usize,
) -> Option<Arc<RecipeNode>> {
    let node = root?;
    *touched += 1;
    if key.as_usize() < node.key.as_usize() {
        return Some(build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            remove_node(node.left.clone(), key, removed, touched),
            node.right.clone(),
        ));
    }
    if key.as_usize() > node.key.as_usize() {
        return Some(build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            node.left.clone(),
            remove_node(node.right.clone(), key, removed, touched),
        ));
    }
    *removed = Some(node.entry.clone());
    merge_nodes(node.left.clone(), node.right.clone(), touched)
}

fn split_root_before(
    root: Option<Arc<RecipeNode>>,
    key: UserVirtAddr,
    touched: &mut usize,
) -> (Option<Arc<RecipeNode>>, Option<Arc<RecipeNode>>) {
    let Some(node) = root else {
        return (None, None);
    };

    *touched += 1;
    if node.key.as_usize() < key.as_usize() {
        let (right_left, right) = split_root_before(node.right.clone(), key, touched);
        let left = build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            node.left.clone(),
            right_left,
        );
        (Some(left), right)
    } else {
        let (left, left_right) = split_root_before(node.left.clone(), key, touched);
        let right = build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            left_right,
            node.right.clone(),
        );
        (left, Some(right))
    }
}

fn merge_nodes(
    left: Option<Arc<RecipeNode>>,
    right: Option<Arc<RecipeNode>>,
    touched: &mut usize,
) -> Option<Arc<RecipeNode>> {
    match (left, right) {
        (None, None) => None,
        (Some(node), None) | (None, Some(node)) => Some(node),
        (Some(left), Some(right)) if left.priority <= right.priority => {
            *touched += 1;
            Some(build_node(
                left.key,
                left.priority,
                left.entry.clone(),
                left.left.clone(),
                merge_nodes(left.right.clone(), Some(right), touched),
            ))
        }
        (Some(left), Some(right)) => {
            *touched += 1;
            Some(build_node(
                right.key,
                right.priority,
                right.entry.clone(),
                merge_nodes(Some(left), right.left.clone(), touched),
                right.right.clone(),
            ))
        }
    }
}

fn rotate_right_if_needed(node: Arc<RecipeNode>) -> Arc<RecipeNode> {
    let Some(left) = &node.left else {
        return node;
    };
    if left.priority > node.priority {
        return node;
    }
    build_node(
        left.key,
        left.priority,
        left.entry.clone(),
        left.left.clone(),
        Some(build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            left.right.clone(),
            node.right.clone(),
        )),
    )
}

fn rotate_left_if_needed(node: Arc<RecipeNode>) -> Arc<RecipeNode> {
    let Some(right) = &node.right else {
        return node;
    };
    if right.priority > node.priority {
        return node;
    }
    build_node(
        right.key,
        right.priority,
        right.entry.clone(),
        Some(build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            node.left.clone(),
            right.left.clone(),
        )),
        right.right.clone(),
    )
}

fn recipe_priority(key: UserVirtAddr) -> u64 {
    let mut x = key.as_usize() as u64;
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

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
    #[cfg(test)]
    last_publish_touched_entries: AtomicUsize,
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
        self.entries.lookup(addr)
    }

    pub(in crate::vm) fn snapshot(&self) -> Vec<VmEntry> {
        self.entries.values_vec()
    }
}

impl RecipeIndex {
    pub(in crate::vm) fn new() -> Self {
        let initial = Box::into_raw(Box::new(RecipeTree::new()));
        Self {
            current: AtomicPtr::new(initial),
            mutation: SpinMutex::new(()),
            #[cfg(test)]
            last_publish_touched_entries: AtomicUsize::new(0),
        }
    }

    pub(in crate::vm) fn clone_shared(&self, guard: &Guard<'_>) -> Self {
        let initial = Box::into_raw(Box::new(self.pinned(guard).clone()));
        Self {
            current: AtomicPtr::new(initial),
            mutation: SpinMutex::new(()),
            #[cfg(test)]
            last_publish_touched_entries: AtomicUsize::new(0),
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
            entries: self.pinned(guard).clone(),
        }
    }

    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr, guard: &Guard<'_>) -> Option<VmEntry> {
        self.pinned(guard).lookup(addr)
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
        self.pinned(guard).overlapping(range)
    }

    pub(in crate::vm) fn snapshot(&self, guard: &Guard<'_>) -> Vec<VmEntry> {
        self.pinned(guard).values_vec()
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
    fn publish(&self, next: RecipeTree, touched_entries: usize) {
        #[cfg(test)]
        self.last_publish_touched_entries
            .store(touched_entries, Ordering::Release);
        #[cfg(not(test))]
        let _ = touched_entries;
        emit_vm_recipe_trace(
            b"debug.vm.recipe.publish.touched_entries",
            touched_entries as i64,
        );
        let new_ptr = Box::into_raw(Box::new(next));
        let old_ptr = self.current.swap(new_ptr, Ordering::AcqRel);
        if !old_ptr.is_null() {
            // SAFETY: every reader holds an epoch guard issued before its
            // load; EBR delays reclamation until those guards drop. Once
            // drained, `reclaim_recipe_tree` runs and frees the box.
            emit_vm_recipe_trace(b"debug.vm.recipe.publish.retire_old", 1);
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

    #[cfg(test)]
    pub(in crate::vm) fn debug_last_publish_touched_entries(&self) -> usize {
        self.last_publish_touched_entries.load(Ordering::Acquire)
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
                let (rewritten, stats_delta, touched_entries) =
                    insert_coalescing_adjacent(current, entry)?;
                self.publish(rewritten, touched_entries);
                Ok(VmMapCommit {
                    changed_pages,
                    stats_delta,
                })
            }
            MapPlacement::FixedReplace => {
                let (rewritten, changed_pages, stats_delta, touched_entries) =
                    rewrite_fixed(current, &entry)?;
                self.publish(rewritten, touched_entries);
                Ok(VmMapCommit {
                    changed_pages,
                    stats_delta,
                })
            }
        }
    }

    pub(in crate::vm) fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages, stats_delta, touched_entries) =
            rewrite_unmap(current, range)?;
        self.publish(rewritten, touched_entries);
        Ok(VmMapCommit {
            changed_pages,
            stats_delta,
        })
    }

    pub(in crate::vm) fn protect(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 0);
        let _writer = self.mutation.lock();
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 1);
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages, stats_delta, touched_entries) =
            rewrite_protect(current, range, prot)?;
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 2);
        emit_vm_recipe_trace(
            b"debug.vm.recipe.protect.changed_pages",
            changed_pages as i64,
        );
        emit_vm_recipe_trace(
            b"debug.vm.recipe.protect.touched_entries",
            touched_entries as i64,
        );
        self.publish(rewritten, touched_entries);
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 3);
        Ok(VmMapCommit {
            changed_pages,
            stats_delta,
        })
    }

    pub(in crate::vm) fn set_locked(
        &self,
        range: UserRange,
        locked: bool,
    ) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, changed_pages, stats_delta, touched_entries) =
            rewrite_locked(current, range, locked)?;
        self.publish(rewritten, touched_entries);
        Ok(VmMapCommit {
            changed_pages,
            stats_delta,
        })
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
        let (rewritten, changed_pages, stats_delta, touched_entries) = match placement {
            VmRemapPlacement::Move => {
                rewrite_remap_disjoint(current, old_range, new_range, destination)?
            }
            VmRemapPlacement::InPlace => rewrite_remap_in_place(current, old_range, new_range)?,
        };
        self.publish(rewritten, touched_entries);
        Ok(VmMapCommit {
            changed_pages,
            stats_delta,
        })
    }

    pub(in crate::vm) fn replace_entry(&self, entry: VmEntry) -> Result<VmMapCommit, VmMapError> {
        let _writer = self.mutation.lock();
        let current = unsafe { self.under_writer_lock() };
        let (rewritten, touched_entries) = current.replace_exact(entry)?;
        let stats_delta = stats_delta_between(current, &rewritten);
        self.publish(rewritten, touched_entries);
        Ok(VmMapCommit {
            changed_pages: 0,
            stats_delta,
        })
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
        let (rewritten, changed_pages, stats_delta, touched_entries) =
            rewrite_tag_ufd_registration(current, range, tag)?;
        self.publish(rewritten, touched_entries);
        Ok(VmMapCommit {
            changed_pages,
            stats_delta,
        })
    }
}

unsafe fn reclaim_recipe_tree(ptr: *mut u8) {
    emit_vm_recipe_trace(b"debug.vm.recipe.reclaim_tree.begin", 1);
    // SAFETY: `ptr` was last published from `Box::into_raw(Box::new(_))`.
    let _ = unsafe { Box::from_raw(ptr as *mut RecipeTree) };
    emit_vm_recipe_trace(b"debug.vm.recipe.reclaim_tree.end", 1);
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

    pub(in crate::vm) fn apply_delta(&self, delta: AddressSpaceStatsDelta) {
        apply_signed_delta(&self.recipe_count, delta.recipe_count);
        apply_signed_delta(&self.vm_size, delta.vm_size);
    }
}

fn apply_signed_delta(cell: &AtomicUsize, delta: isize) {
    if delta >= 0 {
        cell.fetch_add(delta as usize, Ordering::AcqRel);
    } else {
        cell.fetch_sub(delta.unsigned_abs(), Ordering::AcqRel);
    }
}

fn validate_insert_free(entries: &RecipeTree, entry: &VmEntry) -> Result<(), VmMapError> {
    if entries
        .predecessor_entry(entry.range.start())
        .is_some_and(|existing| existing.range.overlaps(entry.range))
        || entries
            .successor_entry(entry.range.start())
            .is_some_and(|existing| existing.range.overlaps(entry.range))
    {
        return Err(VmMapError::AlreadyMapped);
    }

    Ok(())
}

fn tree_from_entries(entries: Vec<VmEntry>) -> (RecipeTree, usize) {
    let mut tree = RecipeTree::new();
    let mut touched_entries = 0usize;
    for entry in entries {
        let (next, touched) = tree.insert_entry(entry);
        tree = next;
        touched_entries += touched;
    }
    (tree, touched_entries)
}

fn insert_coalescing_adjacent(
    entries: &RecipeTree,
    entry: VmEntry,
) -> Result<(RecipeTree, AddressSpaceStatsDelta, usize), VmMapError> {
    let mut rewritten = entries.clone();
    let mut merged = entry;
    let mut removed_entries = 0isize;
    let mut removed_vm_size = 0isize;
    let mut touched_entries = 0usize;

    if let Some(left) = entries.predecessor_entry(merged.range.start()) {
        if let Some(joined) = try_merge_adjacent_entries(&left, &merged)? {
            let (next, removed, touched) = rewritten.remove_exact(left.range.start());
            debug_assert!(removed.is_some());
            rewritten = next;
            touched_entries += touched;
            removed_entries += 1;
            removed_vm_size += left.range.len() as isize;
            merged = joined;
        }
    }

    if let Some(right) = entries.successor_entry(merged.range.end()) {
        if let Some(joined) = try_merge_adjacent_entries(&merged, &right)? {
            let (next, removed, touched) = rewritten.remove_exact(right.range.start());
            debug_assert!(removed.is_some());
            rewritten = next;
            touched_entries += touched;
            removed_entries += 1;
            removed_vm_size += right.range.len() as isize;
            merged = joined;
        }
    }

    let inserted_len = merged.range.len() as isize;
    let (rewritten, touched) = rewritten.insert_entry(merged);
    touched_entries += touched;
    Ok((
        rewritten,
        AddressSpaceStatsDelta {
            recipe_count: 1 - removed_entries,
            vm_size: inserted_len - removed_vm_size,
        },
        touched_entries,
    ))
}

fn try_merge_adjacent_entries(
    left: &VmEntry,
    right: &VmEntry,
) -> Result<Option<VmEntry>, VmMapError> {
    if left.range.end() != right.range.start()
        || left.prot != right.prot
        || left.flags != right.flags
        || left.backing != right.backing
        || left.ufd_registration != right.ufd_registration
    {
        return Ok(None);
    }

    let private = match (&left.private, &right.private) {
        (Some(left_set), Some(right_set)) if right_set.is_empty() => Some(left_set.clone()),
        (Some(left_set), None) => Some(left_set.clone()),
        (None, Some(right_set)) if right_set.is_empty() => Some(right_set.clone()),
        (None, None) => None,
        _ => return Ok(None),
    };
    let range = UserRange::new_aligned(
        left.range.start(),
        right.range.end().as_usize() - left.range.start().as_usize(),
    )
    .map_err(|_| VmMapError::InvalidRange)?;
    Ok(Some(
        VmEntry::new(range, left.prot, left.flags, left.backing.clone())
            .with_ufd_registration(left.ufd_registration)
            .with_private(private),
    ))
}

fn rewrite_fixed(
    entries: &RecipeTree,
    replacement: &VmEntry,
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    let mut rewritten = entries.clone();
    let mut replaced_pages = 0;
    let mut stats_delta = stats_delta_for_insert(replacement);
    let mut touched_entries = 0usize;

    for existing in entries.overlapping(replacement.range) {
        let Some(overlap) = range_intersection(existing.range, replacement.range) else {
            continue;
        };

        replaced_pages += overlap.page_count();
        subtract_entry_stats(&mut stats_delta, &existing);
        let (without_existing, removed, touched) = rewritten.remove_exact(existing.range.start());
        debug_assert!(removed.is_some());
        rewritten = without_existing;
        touched_entries += touched;
        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            add_entry_stats(&mut stats_delta, &before);
            let (next, touched) = rewritten.insert_entry(before);
            rewritten = next;
            touched_entries += touched;
        }
        if let Some(after) = rewrite.after {
            add_entry_stats(&mut stats_delta, &after);
            let (next, touched) = rewritten.insert_entry(after);
            rewritten = next;
            touched_entries += touched;
        }
    }

    let (rewritten, touched) = rewritten.insert_entry(replacement.clone());
    touched_entries += touched;
    Ok((
        rewritten,
        replaced_pages + replacement.range.page_count(),
        stats_delta,
        touched_entries,
    ))
}

fn rewrite_unmap(
    entries: &RecipeTree,
    range: UserRange,
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    let mut survivors = Vec::new();
    let mut removed_entries = 0usize;
    let mut removed_vm_size = 0usize;
    let mut touched_entries = 0usize;

    let (left, at_or_after_start) =
        split_root_before(entries.root.clone(), range.start(), &mut touched_entries);

    let head = entries
        .predecessor_entry(range.start())
        .filter(|entry| entry.range.start().as_usize() < range.start().as_usize())
        .filter(|entry| entry.range.overlaps(range));
    let mut left = if let Some(head) = head {
        let mut removed = None;
        let without_head =
            remove_node(left, head.range.start(), &mut removed, &mut touched_entries);
        debug_assert!(removed.is_some());
        removed_entries += 1;
        removed_vm_size += head.range.len();
        push_unmap_survivors(&mut survivors, &head, range)?;
        without_head
    } else {
        left
    };

    let (discard, right) = split_root_before(at_or_after_start, range.end(), &mut touched_entries);
    removed_entries += node_len(&discard);
    removed_vm_size += node_vm_size(&discard);

    let tail = entries
        .predecessor_entry_before(range.end())
        .filter(|entry| entry.range.start().as_usize() >= range.start().as_usize())
        .filter(|entry| entry.range.end().as_usize() > range.end().as_usize())
        .filter(|entry| entry.range.overlaps(range));
    if let Some(tail) = tail {
        push_unmap_survivors(&mut survivors, &tail, range)?;
    }

    if removed_entries == 0 {
        return Ok((entries.clone(), 0, AddressSpaceStatsDelta::default(), 0));
    }

    let survivor_vm_size = survivors
        .iter()
        .map(|entry| entry.range.len())
        .sum::<usize>();
    let changed_pages = (removed_vm_size - survivor_vm_size) / USER_PAGE_SIZE;
    let stats_delta = AddressSpaceStatsDelta {
        recipe_count: survivors.len() as isize - removed_entries as isize,
        vm_size: survivor_vm_size as isize - removed_vm_size as isize,
    };

    let root = merge_nodes(left.take(), right, &mut touched_entries);
    let mut rewritten = RecipeTree {
        root,
        len: entries.len - removed_entries,
        vm_size: entries.vm_size - removed_vm_size,
    };

    for survivor in survivors {
        let (next, touched) = rewritten.insert_entry(survivor);
        rewritten = next;
        touched_entries += touched;
    }

    Ok((rewritten, changed_pages, stats_delta, touched_entries))
}

fn push_unmap_survivors(
    survivors: &mut Vec<VmEntry>,
    existing: &VmEntry,
    range: UserRange,
) -> Result<(), VmMapError> {
    let Some(overlap) = range_intersection(existing.range, range) else {
        return Ok(());
    };
    let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
    if let Some(before) = rewrite.before {
        survivors.push(before);
    }
    if let Some(after) = rewrite.after {
        survivors.push(after);
    }
    Ok(())
}

fn rewrite_protect(
    entries: &RecipeTree,
    range: UserRange,
    prot: Prot,
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 0);
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }
    emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 1);

    let mut rewritten = entries.clone();
    let mut changed_pages = 0;
    let mut stats_delta = AddressSpaceStatsDelta::default();
    let mut touched_entries = 0usize;
    let overlapping = entries.overlapping(range);
    emit_vm_recipe_trace(
        b"debug.vm.recipe.rewrite_protect.overlap_count",
        overlapping.len() as i64,
    );
    emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 2);

    for existing in overlapping {
        let Some(overlap) = range_intersection(existing.range, range) else {
            continue;
        };

        changed_pages += overlap.page_count();
        subtract_entry_stats(&mut stats_delta, &existing);
        let (without_existing, removed, touched) = rewritten.remove_exact(existing.range.start());
        debug_assert!(removed.is_some());
        rewritten = without_existing;
        touched_entries += touched;
        let rewrite = existing
            .split_for_protect(overlap, prot)
            .map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            add_entry_stats(&mut stats_delta, &before);
            let (next, touched) = rewritten.insert_entry(before);
            rewritten = next;
            touched_entries += touched;
        }
        if let Some(target) = rewrite.target {
            add_entry_stats(&mut stats_delta, &target);
            let (next, touched) = rewritten.insert_entry(target);
            rewritten = next;
            touched_entries += touched;
        }
        if let Some(after) = rewrite.after {
            add_entry_stats(&mut stats_delta, &after);
            let (next, touched) = rewritten.insert_entry(after);
            rewritten = next;
            touched_entries += touched;
        }
    }
    emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 3);

    Ok((rewritten, changed_pages, stats_delta, touched_entries))
}

fn rewrite_locked(
    entries: &RecipeTree,
    range: UserRange,
    locked: bool,
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }

    let mut rewritten = entries.clone();
    let mut changed_pages = 0;
    let mut touched_entries = 0usize;

    for existing in entries.overlapping(range) {
        if existing.range.overlaps(range) {
            changed_pages += existing.range.page_count();
            let (without_existing, removed, touched) =
                rewritten.remove_exact(existing.range.start());
            debug_assert!(removed.is_some());
            rewritten = without_existing;
            touched_entries += touched;
            let (next, touched) = rewritten.insert_entry(existing.with_locked(locked));
            rewritten = next;
            touched_entries += touched;
        }
    }
    Ok((
        rewritten,
        changed_pages,
        AddressSpaceStatsDelta::default(),
        touched_entries,
    ))
}

fn rewrite_remap_disjoint(
    entries: &RecipeTree,
    old_range: UserRange,
    new_range: UserRange,
    destination: MapPlacement,
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
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
    let mut kept = Vec::new();
    let mut moved = Vec::new();

    for existing in entries.overlapping(move_source) {
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

    for existing in entries.values_vec() {
        for entry in remove_remap_covered_ranges(&existing, old_range, new_range)? {
            kept.push(entry);
        }
    }
    for entry in moved {
        kept.push(entry);
    }

    let (rewritten, touched_entries) = tree_from_entries(kept);
    let stats_delta = stats_delta_between(entries, &rewritten);
    Ok((
        rewritten,
        old_range.page_count() + new_range.page_count(),
        stats_delta,
        touched_entries,
    ))
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
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    if old_range.start() != new_range.start() {
        return Err(VmMapError::InvalidRange);
    }
    if !range_is_fully_mapped(entries, old_range) {
        return Err(VmMapError::MissingMapping);
    }

    if new_range.len() == old_range.len() {
        return Ok((entries.clone(), 0, AddressSpaceStatsDelta::default(), 0));
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

    let existing = entries
        .lookup(old_range.start())
        .filter(|entry| entry.range == old_range)
        .ok_or(VmMapError::InvalidRange)?;
    let (without_existing, removed, mut touched_entries) = entries.remove_exact(old_range.start());
    debug_assert!(removed.is_some());
    let (rewritten, touched) =
        without_existing.insert_entry(entry_with_range(&existing, new_range)?);
    touched_entries += touched;

    let stats_delta = stats_delta_between(entries, &rewritten);
    Ok((
        rewritten,
        old_range.page_count() + new_range.page_count(),
        stats_delta,
        touched_entries,
    ))
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
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    // Phase 3 only handles whole-VMA registrations: every byte of
    // `range` must be mapped, and no VMA in `range` may straddle the
    // boundary (start before / end after the requested range). The
    // partial-overlap case maps to the Linux `-EINVAL` the shim
    // returns when `UFFDIO_REGISTER` is given an unaligned range.
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }

    let mut rewritten = entries.clone();
    let mut tagged_pages = 0;
    let mut touched_entries = 0usize;
    for existing in entries.overlapping(range) {
        if existing.range.overlaps(range) {
            if !range.contains_range(existing.range) {
                // Partial-VMA registration is out of scope for phase
                // 3. The shim is expected to pre-align the registered
                // range to VMA boundaries; we surface the misuse here
                // rather than silently re-tagging part of a VMA.
                return Err(VmMapError::MissingMapping);
            }
            tagged_pages += existing.range.page_count();
            let (without_existing, removed, touched) =
                rewritten.remove_exact(existing.range.start());
            debug_assert!(removed.is_some());
            rewritten = without_existing;
            touched_entries += touched;
            let (next, touched) = rewritten.insert_entry(existing.with_ufd_registration(Some(tag)));
            rewritten = next;
            touched_entries += touched;
        }
    }

    Ok((
        rewritten,
        tagged_pages,
        AddressSpaceStatsDelta::default(),
        touched_entries,
    ))
}

fn range_is_fully_mapped(entries: &RecipeTree, range: UserRange) -> bool {
    let mut cursor = range.start().as_usize();
    let end = range.end().as_usize();

    while cursor < end {
        let Some(entry) = entries.lookup(UserVirtAddr(cursor)) else {
            return false;
        };
        cursor = entry.range.end().as_usize().min(end);
    }

    true
}

fn stats_for(entries: &RecipeTree) -> AddressSpaceStats {
    AddressSpaceStats {
        recipe_count: entries.len,
        vm_size: entries.vm_size,
    }
}

fn stats_delta_for_insert(entry: &VmEntry) -> AddressSpaceStatsDelta {
    AddressSpaceStatsDelta {
        recipe_count: 1,
        vm_size: entry.range.len() as isize,
    }
}

fn add_entry_stats(delta: &mut AddressSpaceStatsDelta, entry: &VmEntry) {
    delta.recipe_count += 1;
    delta.vm_size += entry.range.len() as isize;
}

fn subtract_entry_stats(delta: &mut AddressSpaceStatsDelta, entry: &VmEntry) {
    delta.recipe_count -= 1;
    delta.vm_size -= entry.range.len() as isize;
}

fn stats_delta_between(before: &RecipeTree, after: &RecipeTree) -> AddressSpaceStatsDelta {
    let before = stats_for(before);
    let after = stats_for(after);
    AddressSpaceStatsDelta {
        recipe_count: after.recipe_count as isize - before.recipe_count as isize,
        vm_size: after.vm_size as isize - before.vm_size as isize,
    }
}

fn emit_vm_recipe_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
    }
}

fn find_gap_in(entries: &RecipeTree, window: UserRange, page_count: usize) -> Option<UserRange> {
    let len = page_count.checked_mul(USER_PAGE_SIZE)?;
    if len == 0 || len > window.len() {
        return None;
    }

    let mut cursor = window.start().as_usize();
    let window_end = window.end().as_usize();

    if let Some(entry) = entries.predecessor_entry(window.start()) {
        if entry.range.overlaps(window) {
            cursor = cursor.max(entry.range.end().as_usize());
        }
    }

    for entry in entries.overlapping(window) {
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
