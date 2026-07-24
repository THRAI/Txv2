//! Authoritative recipe range index, published lock-free under EBR.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use tx_substrate::Published;

use crate::vm::adapter::step_engine::{borrow_current_guard, guard as epoch_guard};
use crate::vm::lock_metrics::{vm_spin_mutex, VmSpinMutex};

use crate::execution::Guard;

use super::recipe_tree::{RecipeTree, VmEntryView};
use super::{
    AddressSpaceStats, AddressSpaceStatsDelta, MapPlacement, Prot, UfdRegistration, UserRange,
    UserVirtAddr, VmBacking, VmEntry, VmEntryError, VmEntryFlags, VmEntryProtectRewrite,
    VmMapCommit, VmMapError, VmRemapPlacement, USER_PAGE_SIZE,
};

static RECIPE_OP_COUNT: AtomicUsize = AtomicUsize::new(0);
static RECIPE_OP_TOTAL_NS: AtomicUsize = AtomicUsize::new(0);
static RECIPE_OP_MAX_NS: AtomicUsize = AtomicUsize::new(0);
static RECIPE_OP_TOUCHED_TOTAL: AtomicUsize = AtomicUsize::new(0);
static RECIPE_OP_NODE_ALLOC_TOTAL: AtomicUsize = AtomicUsize::new(0);
static RECIPE_OP_NODE_ALLOC_MAX: AtomicUsize = AtomicUsize::new(0);
static RECIPE_NODE_ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static RECIPE_CHUNK_ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
const VM_RECIPE_PUBLISH_SHAPE_METRICS: bool = cfg!(tx_vm_recipe_publish_shape_metrics);
const VM_RECIPE_NODE_ALLOC_METRICS: bool = cfg!(tx_vm_recipe_node_alloc_metrics);
const VM_RECIPE_PHASE_METRICS: bool = cfg!(tx_vm_recipe_phase_metrics);

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::vm) struct RecipeDebugTotals {
    pub op_count: u64,
    pub op_total_ns: u64,
    pub op_max_ns: u64,
    pub op_touched_total: u64,
    pub op_node_alloc_total: u64,
    pub op_node_alloc_max: u64,
    pub node_alloc_count: u64,
    pub chunk_alloc_count: u64,
}

static LAST_PUBLISH_LEAF_SPLITS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug)]
enum RecipePublishOp {
    MapRequireFree = 0,
    MapFixedReplace = 1,
    Unmap = 2,
    Protect = 3,
    Locked = 4,
    RemapMove = 5,
    RemapInPlace = 6,
    ReplaceEntry = 7,
    UfdTag = 8,
}

#[derive(Clone, Copy, Debug)]
struct RecipePublishDebug {
    op: RecipePublishOp,
    before_len: usize,
    after_len: usize,
    changed_pages: usize,
    touched_entries: usize,
    node_allocs: usize,
    duration_ns: u64,
}

struct RecipeRewriteResult {
    commit: VmMapCommit,
    touched_entries: usize,
    publish_debug: Option<RecipePublishDebug>,
    rewrite_ns: Option<u64>,
    publish_ns: Option<u64>,
}

/// Authoritative recipe range index.
///
/// `Published<RecipeTree>` owns the immutable root and defers old-root
/// destruction through substrate RCU. Readers observe one acquire-published
/// root under a `Guard<'_>` without taking the semantic mutation lock. Writers
/// retain that lock across derive, prepare, and commit so no stale derived root
/// can replace a newer one.
pub(in crate::vm) struct RecipeIndex {
    current: Published<RecipeTree>,
    mutation: VmSpinMutex<()>,
    #[cfg(test)]
    last_publish_touched_entries: AtomicUsize,
    #[cfg(test)]
    last_publish_leaf_splits: AtomicUsize,
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
        Self {
            current: Published::try_new(RecipeTree::new())
                .expect("initial RecipeIndex publication allocation"),
            mutation: vm_spin_mutex((), b"debug.lock.vm.recipe_index.mutation"),
            #[cfg(test)]
            last_publish_touched_entries: AtomicUsize::new(0),
            #[cfg(test)]
            last_publish_leaf_splits: AtomicUsize::new(0),
        }
    }

    fn writer_guard() -> Guard<'static> {
        borrow_current_guard().unwrap_or_else(epoch_guard)
    }

    fn rewrite_with_debug<F>(
        &self,
        op: RecipePublishOp,
        current: &RecipeTree,
        rewrite: F,
    ) -> Result<RecipeRewriteResult, VmMapError>
    where
        F: FnOnce(
            &RecipeTree,
        )
            -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError>,
    {
        if tx_observe::current().is_none() {
            let rewrite_start = recipe_phase_clock_now();
            let (rewritten, changed_pages, stats_delta, touched_entries) = rewrite(current)?;
            let rewrite_ns = recipe_phase_elapsed(rewrite_start);
            let leaf_splits = LAST_PUBLISH_LEAF_SPLITS.load(Ordering::Acquire);
            let publish_start = recipe_phase_clock_now();
            self.publish(rewritten, touched_entries, leaf_splits)?;
            let publish_ns = recipe_phase_elapsed(publish_start);
            return Ok(RecipeRewriteResult {
                commit: VmMapCommit {
                    changed_pages,
                    stats_delta,
                },
                touched_entries,
                publish_debug: None,
                rewrite_ns,
                publish_ns,
            });
        }

        let before_len = current.len();
        let before_allocs = RECIPE_NODE_ALLOC_COUNT.load(Ordering::Relaxed);
        let before_chunks = RECIPE_CHUNK_ALLOC_COUNT.load(Ordering::Relaxed);
        let start_ns = tx_observe::clock_now_ns();
        let (rewritten, changed_pages, stats_delta, touched_entries) = rewrite(current)?;
        let duration_ns = tx_observe::clock_now_ns().saturating_sub(start_ns);
        let node_allocs = RECIPE_NODE_ALLOC_COUNT
            .load(Ordering::Relaxed)
            .saturating_sub(before_allocs)
            + RECIPE_CHUNK_ALLOC_COUNT
                .load(Ordering::Relaxed)
                .saturating_sub(before_chunks);
        let after_len = rewritten.len();
        let publish_start = recipe_phase_clock_now();
        self.publish(rewritten, touched_entries, node_allocs)?;
        let publish_ns = recipe_phase_elapsed(publish_start);
        Ok(RecipeRewriteResult {
            commit: VmMapCommit {
                changed_pages,
                stats_delta,
            },
            touched_entries,
            publish_debug: Some(RecipePublishDebug {
                op,
                before_len,
                after_len,
                changed_pages,
                touched_entries,
                node_allocs,
                duration_ns,
            }),
            rewrite_ns: Some(duration_ns),
            publish_ns,
        })
    }

    pub(in crate::vm) fn clone_shared(&self, guard: &Guard<'_>) -> Self {
        Self {
            current: Published::try_new(self.pinned(guard).clone())
                .expect("cloned RecipeIndex publication allocation"),
            mutation: vm_spin_mutex((), b"debug.lock.vm.recipe_index.mutation"),
            #[cfg(test)]
            last_publish_touched_entries: AtomicUsize::new(0),
            #[cfg(test)]
            last_publish_leaf_splits: AtomicUsize::new(0),
        }
    }

    /// Borrow the published tree under the caller-supplied epoch guard. The
    /// returned reference is valid for the guard's lifetime.
    fn pinned<'g>(&'g self, guard: &'g Guard<'_>) -> &'g RecipeTree {
        self.current.read(guard)
    }

    #[cfg(test)]
    pub(in crate::vm) fn snapshot_reader(&self, guard: &Guard<'_>) -> RecipeSnapshot {
        RecipeSnapshot {
            entries: self.pinned(guard).clone(),
        }
    }

    #[cfg(test)]
    pub(in crate::vm) fn lookup_ref_for_test<'g>(
        &'g self,
        addr: UserVirtAddr,
        guard: &'g Guard<'_>,
    ) -> Option<&'g VmEntry> {
        self.pinned(guard).lookup_ref(addr)
    }

    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr, guard: &Guard<'_>) -> Option<VmEntry> {
        self.pinned(guard).lookup(addr)
    }

    #[allow(dead_code)]
    pub(in crate::vm) fn lookup_view<'g>(
        &'g self,
        addr: UserVirtAddr,
        guard: &'g Guard<'_>,
    ) -> Option<VmEntryView<'g>> {
        self.pinned(guard).lookup_view(addr)
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

    /// Prepare and commit one replacement while the semantic writer lock is
    /// held. Allocation failure leaves the current root authoritative.
    fn publish(
        &self,
        next: RecipeTree,
        touched_entries: usize,
        node_allocs: usize,
    ) -> Result<(), VmMapError> {
        let replacement = self.current.prepare_replace(next).map_err(|_| {
            emit_recipe_phase_count(b"debug.vm.recipe.phase.publish_alloc_error", 1);
            VmMapError::NoFreeRange
        })?;
        #[cfg(test)]
        self.last_publish_touched_entries
            .store(touched_entries, Ordering::Release);
        #[cfg(test)]
        self.last_publish_leaf_splits
            .fetch_max(node_allocs, Ordering::AcqRel);
        #[cfg(not(test))]
        let _ = touched_entries;
        #[cfg(not(test))]
        let _ = node_allocs;
        replacement.commit();
        Ok(())
    }

    fn finish_rewrite(result: RecipeRewriteResult) -> VmMapCommit {
        emit_recipe_phase_duration(b"debug.vm.recipe.phase.rewrite_ns", result.rewrite_ns);
        emit_recipe_phase_duration(b"debug.vm.recipe.phase.publish_swap_ns", result.publish_ns);
        let debug_start = recipe_phase_clock_now();
        if let Some(debug) = result.publish_debug {
            emit_recipe_publish_debug(&debug, true);
        }
        emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.debug_emit_ns", debug_start);
        result.commit
    }

    #[cfg(test)]
    pub(in crate::vm) fn debug_last_publish_touched_entries(&self) -> usize {
        self.last_publish_touched_entries.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(in crate::vm) fn debug_backend_name(&self) -> &'static str {
        let guard = crate::vm::adapter::step_engine::guard();
        self.pinned(&guard).backend_name()
    }

    #[cfg(test)]
    #[cfg_attr(not(tx_vm_recipe_bplus), allow(dead_code))]
    pub(in crate::vm) fn debug_last_publish_leaf_splits(&self) -> usize {
        self.last_publish_leaf_splits.load(Ordering::Acquire)
    }

    pub(in crate::vm) fn commit_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            match placement {
                MapPlacement::RequireFree => {
                    validate_insert_free(current, &entry)?;
                    self.rewrite_with_debug(RecipePublishOp::MapRequireFree, current, |current| {
                        let changed_pages = entry.range.page_count();
                        let (rewritten, stats_delta, touched_entries) =
                            insert_coalescing_adjacent(current, entry)?;
                        Ok((rewritten, changed_pages, stats_delta, touched_entries))
                    })
                }
                MapPlacement::FixedReplace => {
                    self.rewrite_with_debug(RecipePublishOp::MapFixedReplace, current, |current| {
                        rewrite_fixed(current, &entry)
                    })
                }
            }
        };
        Ok(Self::finish_rewrite(result?))
    }

    pub(in crate::vm) fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            self.rewrite_with_debug(RecipePublishOp::Unmap, current, |current| {
                rewrite_unmap(current, range)
            })
        };
        Ok(Self::finish_rewrite(result?))
    }

    pub(in crate::vm) fn protect(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 0);
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 1);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            self.rewrite_with_debug(RecipePublishOp::Protect, current, |current| {
                rewrite_protect(current, range, prot)
            })
        };
        let result = result?;
        let touched_entries = result.touched_entries;
        let commit = Self::finish_rewrite(result);
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 2);
        emit_vm_recipe_trace(
            b"debug.vm.recipe.protect.changed_pages",
            commit.changed_pages as i64,
        );
        emit_vm_recipe_trace(
            b"debug.vm.recipe.protect.touched_entries",
            touched_entries as i64,
        );
        emit_vm_recipe_trace(b"debug.vm.recipe.protect.phase", 3);
        Ok(commit)
    }

    pub(in crate::vm) fn set_locked(
        &self,
        range: UserRange,
        locked: bool,
    ) -> Result<VmMapCommit, VmMapError> {
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            self.rewrite_with_debug(RecipePublishOp::Locked, current, |current| {
                rewrite_locked(current, range, locked)
            })
        };
        Ok(Self::finish_rewrite(result?))
    }

    pub(in crate::vm) fn remap(
        &self,
        old_range: UserRange,
        new_range: UserRange,
        placement: VmRemapPlacement,
        destination: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            let op = match placement {
                VmRemapPlacement::Move => RecipePublishOp::RemapMove,
                VmRemapPlacement::InPlace => RecipePublishOp::RemapInPlace,
            };
            self.rewrite_with_debug(op, current, |current| match placement {
                VmRemapPlacement::Move => {
                    rewrite_remap_disjoint(current, old_range, new_range, destination)
                }
                VmRemapPlacement::InPlace => rewrite_remap_in_place(current, old_range, new_range),
            })
        };
        Ok(Self::finish_rewrite(result?))
    }

    pub(in crate::vm) fn replace_entry(&self, entry: VmEntry) -> Result<VmMapCommit, VmMapError> {
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            self.rewrite_with_debug(RecipePublishOp::ReplaceEntry, current, |current| {
                let (rewritten, touched_entries) = current.replace_exact(entry)?;
                let stats_delta = stats_delta_between(current, &rewritten);
                Ok((rewritten, 0, stats_delta, touched_entries))
            })
        };
        Ok(Self::finish_rewrite(result?))
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
        let result: Result<RecipeRewriteResult, VmMapError> = {
            let lock_start = recipe_phase_clock_now();
            let _writer = self.mutation.lock();
            emit_recipe_phase_duration_start(b"debug.vm.recipe.phase.lock_wait_ns", lock_start);
            let guard = Self::writer_guard();
            let current = self.pinned(&guard);
            self.rewrite_with_debug(RecipePublishOp::UfdTag, current, |current| {
                rewrite_tag_ufd_registration(current, range, tag)
            })
        };
        Ok(Self::finish_rewrite(result?))
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

pub(in crate::vm) fn reset_recipe_debug_totals() {
    for cell in [
        &RECIPE_OP_COUNT,
        &RECIPE_OP_TOTAL_NS,
        &RECIPE_OP_MAX_NS,
        &RECIPE_OP_TOUCHED_TOTAL,
        &RECIPE_OP_NODE_ALLOC_TOTAL,
        &RECIPE_OP_NODE_ALLOC_MAX,
        &RECIPE_NODE_ALLOC_COUNT,
        &RECIPE_CHUNK_ALLOC_COUNT,
    ] {
        cell.store(0, Ordering::Relaxed);
    }
}

pub(in crate::vm) fn recipe_debug_totals() -> RecipeDebugTotals {
    RecipeDebugTotals {
        op_count: RECIPE_OP_COUNT.load(Ordering::Relaxed) as u64,
        op_total_ns: RECIPE_OP_TOTAL_NS.load(Ordering::Relaxed) as u64,
        op_max_ns: RECIPE_OP_MAX_NS.load(Ordering::Relaxed) as u64,
        op_touched_total: RECIPE_OP_TOUCHED_TOTAL.load(Ordering::Relaxed) as u64,
        op_node_alloc_total: RECIPE_OP_NODE_ALLOC_TOTAL.load(Ordering::Relaxed) as u64,
        op_node_alloc_max: RECIPE_OP_NODE_ALLOC_MAX.load(Ordering::Relaxed) as u64,
        node_alloc_count: RECIPE_NODE_ALLOC_COUNT.load(Ordering::Relaxed) as u64,
        chunk_alloc_count: RECIPE_CHUNK_ALLOC_COUNT.load(Ordering::Relaxed) as u64,
    }
}

fn emit_recipe_publish_debug(debug: &RecipePublishDebug, retired_old: bool) {
    record_recipe_publish_debug(debug);
    emit_vm_recipe_trace(
        b"debug.vm.recipe.publish.touched_entries",
        debug.touched_entries as i64,
    );
    emit_vm_recipe_trace(b"debug.vm.recipe.publish.op", debug.op as i64);
    emit_vm_recipe_trace(
        b"debug.vm.recipe.publish.node_allocs",
        debug.node_allocs as i64,
    );
    emit_vm_recipe_trace(
        b"debug.vm.recipe.publish.duration_ns",
        debug.duration_ns as i64,
    );
    if VM_RECIPE_PUBLISH_SHAPE_METRICS {
        emit_recipe_publish_shape_debug(debug, retired_old);
    }
}

fn record_recipe_publish_debug(debug: &RecipePublishDebug) {
    RECIPE_OP_COUNT.fetch_add(1, Ordering::Relaxed);
    saturating_fetch_add_usize(&RECIPE_OP_TOTAL_NS, debug.duration_ns as usize);
    saturating_fetch_add_usize(&RECIPE_OP_TOUCHED_TOTAL, debug.touched_entries);
    saturating_fetch_add_usize(&RECIPE_OP_NODE_ALLOC_TOTAL, debug.node_allocs);
    atomic_max_usize(&RECIPE_OP_MAX_NS, debug.duration_ns as usize);
    atomic_max_usize(&RECIPE_OP_NODE_ALLOC_MAX, debug.node_allocs);

    if VM_RECIPE_PUBLISH_SHAPE_METRICS {
        emit_vm_recipe_trace(b"debug.vm.recipe.publish.len_delta", {
            let delta = debug.after_len as isize - debug.before_len as isize;
            delta as i64
        });
        emit_vm_recipe_allocation(
            b"debug.alloc.vm.recipe_node.per_publish",
            debug.node_allocs as u64,
        );
        emit_vm_recipe_allocation(
            b"debug.alloc.vm.recipe_node.publish_duration_ns",
            debug.duration_ns,
        );
    }
}

fn emit_recipe_publish_shape_debug(debug: &RecipePublishDebug, retired_old: bool) {
    emit_vm_recipe_trace(
        b"debug.vm.recipe.publish.before_len",
        debug.before_len as i64,
    );
    emit_vm_recipe_trace(b"debug.vm.recipe.publish.after_len", debug.after_len as i64);
    emit_vm_recipe_trace(
        b"debug.vm.recipe.publish.changed_pages",
        debug.changed_pages as i64,
    );
    if retired_old {
        emit_vm_recipe_trace(b"debug.vm.recipe.publish.retire_old", 1);
    }
}

fn saturating_fetch_add_usize(cell: &AtomicUsize, delta: usize) {
    let mut current = cell.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(delta);
        match cell.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn atomic_max_usize(cell: &AtomicUsize, value: usize) {
    let mut current = cell.load(Ordering::Relaxed);
    while value > current {
        match cell.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn validate_insert_free(entries: &RecipeTree, entry: &VmEntry) -> Result<(), VmMapError> {
    if entries
        .predecessor_ref(entry.range.start())
        .is_some_and(|existing| existing.range.overlaps(entry.range))
        || entries
            .successor_ref(entry.range.start())
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

    if let Some(left) = entries.predecessor_ref(merged.range.start()) {
        if let Some(joined) = try_merge_adjacent_entries(left, &merged)? {
            let (next, removed, touched) = rewritten.remove_exact(left.range.start());
            debug_assert!(removed.is_some());
            rewritten = next;
            touched_entries += touched;
            removed_entries += 1;
            removed_vm_size += left.range.len() as isize;
            merged = joined;
        }
    }

    if let Some(right) = entries.successor_ref(merged.range.end()) {
        if let Some(joined) = try_merge_adjacent_entries(&merged, right)? {
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
        || !left.same_backing(right)
        || left.ufd_registration != right.ufd_registration
    {
        return Ok(None);
    }

    let private = match (left.private_handle(), right.private_handle()) {
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
        VmEntry::new(range, left.prot, left.flags, left.backing())
            .with_ufd_registration(left.ufd_registration)
            .with_private(private),
    ))
}

fn rewrite_fixed(
    entries: &RecipeTree,
    replacement: &VmEntry,
) -> Result<(RecipeTree, usize, AddressSpaceStatsDelta, usize), VmMapError> {
    let mut replaced_pages = 0;
    let mut stats_delta = stats_delta_for_insert(replacement);
    let mut replacements = Vec::new();

    let mut error = None;
    entries.for_each_overlapping(replacement.range, &mut |existing| {
        if error.is_some() {
            return;
        }
        let Some(overlap) = range_intersection(existing.range, replacement.range) else {
            return;
        };

        replaced_pages += overlap.page_count();
        subtract_entry_stats(&mut stats_delta, existing);
        let rewrite = match existing.split_for_unmap(overlap).map_err(vm_entry_error) {
            Ok(rewrite) => rewrite,
            Err(err) => {
                error = Some(err);
                return;
            }
        };
        if let Some(before) = rewrite.before {
            add_entry_stats(&mut stats_delta, &before);
            replacements.push(before);
        }
        if let Some(after) = rewrite.after {
            add_entry_stats(&mut stats_delta, &after);
            replacements.push(after);
        }
    });
    if let Some(error) = error {
        return Err(error);
    }

    replacements.push(replacement.clone());
    replacements.sort_by_key(|entry| entry.range.start().as_usize());
    let (rewritten, _, touched_entries) =
        entries.replace_range_summary(replacement.range, replacements);
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
    let mut error = None;
    entries.for_each_overlapping(range, &mut |existing| {
        if error.is_some() {
            return;
        }
        removed_entries += 1;
        removed_vm_size += existing.range.len();
        if let Err(err) = push_unmap_survivors(&mut survivors, existing, range) {
            error = Some(err);
        }
    });
    if let Some(error) = error {
        return Err(error);
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

    let (rewritten, _, touched_entries) = entries.replace_range_summary(range, survivors);

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

    let mut stats_delta = AddressSpaceStatsDelta::default();
    let mut overlapping_count = 0usize;
    entries.for_each_overlapping(range, &mut |_| {
        overlapping_count += 1;
    });
    emit_vm_recipe_trace(
        b"debug.vm.recipe.rewrite_protect.overlap_count",
        overlapping_count as i64,
    );
    emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 2);

    if overlapping_count == 1 {
        let mut existing_start = None;
        let mut changed_pages = 0usize;
        let mut replacement_count = 0usize;
        let mut error = None;
        entries.for_each_overlapping(range, &mut |existing| {
            if error.is_some() {
                return;
            }
            existing_start = Some(existing.range.start());
            let Some(overlap) = range_intersection(existing.range, range) else {
                error = Some(VmMapError::MissingMapping);
                return;
            };
            changed_pages = overlap.page_count();
            let descriptor = VmEntryProtectRewrite::new(overlap, prot);
            replacement_count = match descriptor
                .replacement_count_for(existing)
                .map_err(vm_entry_error)
            {
                Ok(count) => count,
                Err(err) => {
                    error = Some(err);
                    return;
                }
            };
        });
        if let Some(error) = error {
            return Err(error);
        }
        let existing_start = existing_start.ok_or(VmMapError::MissingMapping)?;
        if replacement_count == 0 {
            return Err(VmMapError::MissingMapping);
        }
        stats_delta.recipe_count = replacement_count as isize - 1;
        stats_delta.vm_size = 0;
        let replacement = VmEntryProtectRewrite::new(range, prot);
        emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 3);

        let (rewritten, removed, touched_entries) =
            entries.replace_entry_at_with_protect_summary(existing_start, replacement)?;
        debug_assert_eq!(removed.count, 1);
        return Ok((rewritten, changed_pages, stats_delta, touched_entries));
    }

    let mut changed_pages = 0;
    let mut replacements = Vec::new();
    let mut error = None;

    entries.for_each_overlapping(range, &mut |existing| {
        if error.is_some() {
            return;
        }
        let overlap =
            match range_intersection(existing.range, range).ok_or(VmMapError::MissingMapping) {
                Ok(overlap) => overlap,
                Err(err) => {
                    error = Some(err);
                    return;
                }
            };

        changed_pages += overlap.page_count();
        subtract_entry_stats(&mut stats_delta, existing);
        let rewrite = match existing
            .split_for_protect(overlap, prot)
            .map_err(vm_entry_error)
        {
            Ok(rewrite) => rewrite,
            Err(err) => {
                error = Some(err);
                return;
            }
        };
        if let Some(before) = rewrite.before {
            add_entry_stats(&mut stats_delta, &before);
            replacements.push(before);
        }
        if let Some(target) = rewrite.target {
            add_entry_stats(&mut stats_delta, &target);
            replacements.push(target);
        }
        if let Some(after) = rewrite.after {
            add_entry_stats(&mut stats_delta, &after);
            replacements.push(after);
        }
    });
    if let Some(error) = error {
        return Err(error);
    }
    emit_vm_recipe_trace(b"debug.vm.recipe.rewrite_protect.phase", 3);

    let (rewritten, _, touched_entries) = entries.replace_range_summary(range, replacements);

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

    let mut changed_pages = 0;
    let mut replacements = Vec::new();

    entries.for_each_overlapping(range, &mut |existing| {
        changed_pages += existing.range.page_count();
        replacements.push(existing.clone().with_locked(locked));
    });
    let (rewritten, _, touched_entries) = entries.replace_range_summary(range, replacements);
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

    let mut error = None;
    entries.for_each_overlapping(move_source, &mut |existing| {
        if error.is_some() {
            return;
        }
        let Some(moving_overlap) = range_intersection(existing.range, move_source) else {
            return;
        };
        match rebased_entry_for_move(existing, moving_overlap, old_range, new_range) {
            Ok(entry) => moved.push(entry),
            Err(err) => error = Some(err),
        }
    });
    if let Some(error) = error {
        return Err(error);
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
    existing: &VmEntry,
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
    moving
        .with_range_preserving_owners(target_range)
        .map_err(vm_entry_error)
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
        .lookup_ref(old_range.start())
        .filter(|entry| entry.range == old_range)
        .ok_or(VmMapError::InvalidRange)?;
    let replacement = entry_with_range(existing, new_range)?;
    let (without_existing, removed, mut touched_entries) = entries.remove_exact(old_range.start());
    debug_assert!(removed.is_some());
    let (rewritten, touched) = without_existing.insert_entry(replacement);
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
    entry
        .with_range_preserving_owners(range)
        .map_err(vm_entry_error)
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

    let mut tagged_pages = 0;
    let mut replacements = Vec::new();
    let mut error = None;
    entries.for_each_overlapping(range, &mut |existing| {
        if error.is_some() {
            return;
        }
        if !range.contains_range(existing.range) {
            // Partial-VMA registration is out of scope for phase
            // 3. The shim is expected to pre-align the registered
            // range to VMA boundaries; we surface the misuse here
            // rather than silently re-tagging part of a VMA.
            error = Some(VmMapError::MissingMapping);
            return;
        }
        tagged_pages += existing.range.page_count();
        replacements.push(existing.clone().with_ufd_registration(Some(tag)));
    });
    if let Some(error) = error {
        return Err(error);
    }
    let (rewritten, _, touched_entries) = entries.replace_range_summary(range, replacements);

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
    let mut contiguous = true;

    entries.for_each_overlapping(range, &mut |entry| {
        if !contiguous || cursor >= end {
            return;
        }
        if entry.range.start().as_usize() > cursor {
            contiguous = false;
            return;
        }
        cursor = cursor.max(entry.range.end().as_usize().min(end));
    });

    contiguous && cursor >= end
}

fn stats_for(entries: &RecipeTree) -> AddressSpaceStats {
    AddressSpaceStats {
        recipe_count: entries.len(),
        vm_size: entries.vm_size(),
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
        observer.debug_counter(name, value);
    }
}

#[inline(always)]
fn recipe_phase_metrics_enabled() -> bool {
    VM_RECIPE_PHASE_METRICS && tx_observe::current().is_some()
}

#[inline(always)]
fn recipe_phase_clock_now() -> Option<u64> {
    if recipe_phase_metrics_enabled() {
        Some(tx_observe::clock_now_ns())
    } else {
        None
    }
}

fn recipe_phase_elapsed(start: Option<u64>) -> Option<u64> {
    start.map(|start| tx_observe::clock_now_ns().saturating_sub(start))
}

fn emit_recipe_phase_duration(name: &[u8], duration: Option<u64>) {
    let Some(duration) = duration else {
        return;
    };
    emit_recipe_phase_count(name, duration);
}

fn emit_recipe_phase_duration_start(name: &[u8], start: Option<u64>) {
    emit_recipe_phase_duration(name, recipe_phase_elapsed(start));
}

fn emit_recipe_phase_count(name: &[u8], value: u64) {
    if !recipe_phase_metrics_enabled() {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value as i64);
    }
}

fn emit_vm_recipe_allocation(name: &[u8], value: u64) {
    if let Some(observer) = tx_observe::current() {
        observer.allocation(
            tx_observe::AllocationTrack::VmRecipeNode,
            tx_observe::EventNameId::from_name(name),
            value,
        );
    }
}

pub(super) fn record_recipe_node_alloc_for_tree() {
    RECIPE_NODE_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn record_recipe_chunk_alloc_for_tree() {
    RECIPE_CHUNK_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn record_last_publish_leaf_splits_for_tree(leaf_splits: usize) {
    LAST_PUBLISH_LEAF_SPLITS.fetch_max(leaf_splits, Ordering::AcqRel);
}

#[cfg(tx_vm_recipe_bplus_shape_metrics)]
pub(super) fn emit_vm_recipe_trace_for_tree(name: &[u8], value: i64) {
    emit_vm_recipe_trace(name, value);
}

pub(super) fn emit_vm_recipe_allocation_for_tree(name: &[u8], value: u64) {
    if VM_RECIPE_NODE_ALLOC_METRICS {
        emit_vm_recipe_allocation(name, value);
    }
}

pub(super) fn recipe_node_alloc_metrics_enabled_for_tree() -> bool {
    VM_RECIPE_NODE_ALLOC_METRICS && tx_observe::current().is_some()
}

fn find_gap_in(entries: &RecipeTree, window: UserRange, page_count: usize) -> Option<UserRange> {
    let len = page_count.checked_mul(USER_PAGE_SIZE)?;
    if len == 0 || len > window.len() {
        return None;
    }

    let mut cursor = window.start().as_usize();
    let window_end = window.end().as_usize();

    if let Some(entry) = entries.predecessor_ref(window.start()) {
        if entry.range.overlaps(window) {
            cursor = cursor.max(entry.range.end().as_usize());
        }
    }

    let mut found = None;
    entries.for_each_overlapping(window, &mut |entry| {
        if found.is_some() {
            return;
        }
        if entry.range.end().as_usize() <= cursor {
            return;
        }
        if entry.range.start().as_usize() >= window_end {
            cursor = window_end;
            return;
        }
        if !entry.range.overlaps(window) {
            return;
        }

        let entry_start = entry
            .range
            .start()
            .as_usize()
            .max(window.start().as_usize());
        if cursor
            .checked_add(len)
            .is_some_and(|end| end <= entry_start)
        {
            found = UserRange::new_aligned(UserVirtAddr(cursor), len).ok();
            return;
        }
        cursor = cursor.max(entry.range.end().as_usize());
        if cursor >= window_end {
            cursor = window_end;
        }
    });
    if found.is_some() {
        return found;
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
