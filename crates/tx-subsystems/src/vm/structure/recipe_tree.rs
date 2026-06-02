//! Backend-neutral recipe range tree implementations.
//!
//! `RecipeTree` is the production cfg-selected alias used by `RecipeIndex`.
//! `RecipeTreeWith<TreapRecipeIndex>` and `RecipeTreeWith<BPlusRecipeIndex>`
//! are intentionally available to host tests and custom microbenchmarks so the
//! range-index backends can be compared before running full kernel observe
//! captures.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{
    Prot, UfdRegistration, UserRange, UserVirtAddr, VmBacking, VmEntry, VmEntryFlags, VmMapError,
};
use crate::vm::adapter::step_engine::Cap;

#[cfg(not(tx_vm_recipe_bplus))]
type DefaultRecipeIndex = TreapRecipeIndex;

#[cfg(tx_vm_recipe_bplus)]
type DefaultRecipeIndex = BPlusRecipeIndex;

pub(in crate::vm) type RecipeTree = RecipeTreeWith<DefaultRecipeIndex>;

#[derive(Clone, Default)]
pub(in crate::vm) struct RecipeTreeWith<B: RecipeBackend> {
    inner: B,
    len: usize,
    vm_size: usize,
}

impl<B: RecipeBackend> RecipeTreeWith<B> {
    pub(in crate::vm) fn new() -> Self {
        Self {
            inner: B::new(),
            len: 0,
            vm_size: 0,
        }
    }

    #[allow(dead_code)]
    pub(in crate::vm) fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    pub(in crate::vm) fn len(&self) -> usize {
        self.len
    }

    pub(in crate::vm) fn vm_size(&self) -> usize {
        self.vm_size
    }

    pub(in crate::vm) fn values_vec(&self) -> Vec<VmEntry> {
        self.inner.values_vec()
    }

    pub(in crate::vm) fn reclaim_stats(&self) -> RecipeReclaimStats {
        self.inner.reclaim_stats()
    }

    pub(in crate::vm) fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        self.inner.lookup(addr)
    }

    #[allow(dead_code)]
    pub(in crate::vm) fn lookup_view(&self, addr: UserVirtAddr) -> Option<VmEntryView<'_>> {
        self.inner.lookup_view(addr)
    }

    pub(in crate::vm) fn predecessor_entry(&self, key: UserVirtAddr) -> Option<VmEntry> {
        self.inner.predecessor_entry(key)
    }

    pub(in crate::vm) fn successor_entry(&self, key: UserVirtAddr) -> Option<VmEntry> {
        self.inner.successor_entry(key)
    }

    pub(in crate::vm) fn overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        self.inner.overlapping(range)
    }

    pub(in crate::vm) fn insert_entry(&self, entry: VmEntry) -> (Self, usize) {
        let (inner, touched) = self.inner.insert_entry(entry.clone());
        (
            Self {
                inner,
                len: self.len + 1,
                vm_size: self.vm_size + entry.range.len(),
            },
            touched,
        )
    }

    pub(in crate::vm) fn remove_exact(&self, key: UserVirtAddr) -> (Self, Option<VmEntry>, usize) {
        let (inner, removed, touched) = self.inner.remove_exact(key);
        let (len, vm_size) = match &removed {
            Some(entry) => (self.len - 1, self.vm_size - entry.range.len()),
            None => (self.len, self.vm_size),
        };
        (
            Self {
                inner,
                len,
                vm_size,
            },
            removed,
            touched,
        )
    }

    pub(in crate::vm) fn replace_exact(&self, entry: VmEntry) -> Result<(Self, usize), VmMapError> {
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

    pub(in crate::vm) fn replace_range(
        &self,
        range: UserRange,
        replacements: Vec<VmEntry>,
    ) -> (Self, Vec<VmEntry>, usize) {
        let (inner, removed, touched) = self.inner.replace_range(range, replacements.clone());
        let removed_vm_size = removed.iter().map(|entry| entry.range.len()).sum::<usize>();
        let replacement_vm_size = replacements
            .iter()
            .map(|entry| entry.range.len())
            .sum::<usize>();
        (
            Self {
                inner,
                len: self.len - removed.len() + replacements.len(),
                vm_size: self.vm_size - removed_vm_size + replacement_vm_size,
            },
            removed,
            touched,
        )
    }
}

#[allow(dead_code)]
impl RecipeTreeWith<TreapRecipeIndex> {
    pub(in crate::vm) fn replace_entry_with_entries(
        &self,
        existing: &VmEntry,
        replacements: Vec<VmEntry>,
    ) -> (Self, Option<VmEntry>, usize) {
        let mut replacement_tree = TreapRecipeIndex { root: None };
        let mut touched = 0usize;
        let replacement_count = replacements.len();
        let replacement_vm_size: usize = replacements.iter().map(|entry| entry.range.len()).sum();
        for replacement in replacements {
            let (next, replacement_touched) =
                RecipeBackend::insert_entry(&replacement_tree, replacement);
            replacement_tree = next;
            touched += replacement_touched;
        }

        let mut removed = None;
        let root = replace_node_with_subtree(
            self.inner.root.clone(),
            existing.range.start(),
            replacement_tree.root,
            &mut removed,
            &mut touched,
        );
        let len = if removed.is_some() {
            self.len - 1 + replacement_count
        } else {
            self.len
        };
        let vm_size = match removed.as_ref() {
            Some(removed) => self.vm_size - removed.range.len() + replacement_vm_size,
            None => self.vm_size,
        };
        (
            Self {
                inner: TreapRecipeIndex { root },
                len,
                vm_size,
            },
            removed,
            touched,
        )
    }
}

#[allow(dead_code)]
impl RecipeTreeWith<BPlusRecipeIndex> {
    pub(in crate::vm) fn replace_entry_with_entries(
        &self,
        existing: &VmEntry,
        replacements: Vec<VmEntry>,
    ) -> (Self, Option<VmEntry>, usize) {
        let (rewritten, mut removed, touched) = self.replace_range(existing.range, replacements);
        (rewritten, removed.pop(), touched)
    }
}

pub(in crate::vm) trait RecipeBackend: Clone + Default {
    fn new() -> Self;
    #[allow(dead_code)]
    fn backend_name(&self) -> &'static str;
    fn values_vec(&self) -> Vec<VmEntry>;
    fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry>;
    #[allow(dead_code)]
    fn lookup_view(&self, addr: UserVirtAddr) -> Option<VmEntryView<'_>>;
    fn predecessor_entry(&self, key: UserVirtAddr) -> Option<VmEntry>;
    fn successor_entry(&self, key: UserVirtAddr) -> Option<VmEntry>;
    fn overlapping(&self, range: UserRange) -> Vec<VmEntry>;
    fn insert_entry(&self, entry: VmEntry) -> (Self, usize);
    fn remove_exact(&self, key: UserVirtAddr) -> (Self, Option<VmEntry>, usize);
    fn replace_range(
        &self,
        range: UserRange,
        replacements: Vec<VmEntry>,
    ) -> (Self, Vec<VmEntry>, usize);
    fn reclaim_stats(&self) -> RecipeReclaimStats;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::vm) struct RecipeReclaimStats {
    pub entries: usize,
    pub node_chunks: usize,
    pub leaf_count: usize,
    pub copied_child_refs: usize,
    pub tree_depth: usize,
    pub internal_count: usize,
    pub internal_fanout_min: usize,
    pub internal_fanout_max: usize,
    pub internal_fanout_total: usize,
    pub nonroot_internal_count: usize,
    pub nonroot_internal_fanout_min: usize,
    pub nonroot_internal_fanout_max: usize,
    pub nonroot_internal_fanout_total: usize,
    pub leaf_fill_min: usize,
    pub leaf_fill_max: usize,
    pub leaf_fill_total: usize,
}

impl RecipeReclaimStats {
    fn observe_leaf(&mut self, fill: usize) {
        self.leaf_count += 1;
        self.node_chunks += 1;
        self.entries += fill;
        self.leaf_fill_total += fill;
        if self.leaf_fill_min == 0 || fill < self.leaf_fill_min {
            self.leaf_fill_min = fill;
        }
        self.leaf_fill_max = self.leaf_fill_max.max(fill);
    }

    fn observe_internal(&mut self, fanout: usize, is_root: bool) {
        self.internal_count += 1;
        self.node_chunks += 1;
        self.copied_child_refs += fanout;
        self.internal_fanout_total += fanout;
        if self.internal_fanout_min == 0 || fanout < self.internal_fanout_min {
            self.internal_fanout_min = fanout;
        }
        self.internal_fanout_max = self.internal_fanout_max.max(fanout);

        if !is_root {
            self.nonroot_internal_count += 1;
            self.nonroot_internal_fanout_total += fanout;
            if self.nonroot_internal_fanout_min == 0 || fanout < self.nonroot_internal_fanout_min {
                self.nonroot_internal_fanout_min = fanout;
            }
            self.nonroot_internal_fanout_max = self.nonroot_internal_fanout_max.max(fanout);
        }
    }
}

const BPLUS_LEAF_MIN_FILL: usize = BPLUS_LEAF_CAP / 2;
const BPLUS_INTERNAL_MIN_FANOUT: usize = BPLUS_INTERNAL_FANOUT / 2;

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(in crate::vm) struct VmEntryView<'a> {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: &'a VmBacking,
    pub ufd_registration: Option<UfdRegistration>,
    pub private: Option<&'a Cap<super::PrivatePageSet>>,
}

#[allow(dead_code)]
impl VmEntryView<'_> {
    pub(in crate::vm) fn permits_fault(self, access: crate::vm::AccessMode) -> bool {
        self.prot.permits(access)
    }
}

#[derive(Clone, Default)]
pub(in crate::vm) struct TreapRecipeIndex {
    root: Option<Arc<RecipeNode>>,
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

impl RecipeBackend for TreapRecipeIndex {
    fn new() -> Self {
        Self { root: None }
    }

    fn backend_name(&self) -> &'static str {
        "treap"
    }

    fn values_vec(&self) -> Vec<VmEntry> {
        let mut out = Vec::new();
        collect_values(&self.root, &mut out);
        out
    }

    fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        self.lookup_ref(addr).cloned()
    }

    fn lookup_view(&self, addr: UserVirtAddr) -> Option<VmEntryView<'_>> {
        self.lookup_ref(addr).map(view_for_entry)
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
        (Self { root: Some(root) }, touched)
    }

    fn remove_exact(&self, key: UserVirtAddr) -> (Self, Option<VmEntry>, usize) {
        let mut touched = 0usize;
        let mut removed = None;
        let root = remove_node(self.root.clone(), key, &mut removed, &mut touched);
        (Self { root }, removed, touched)
    }

    fn replace_range(
        &self,
        range: UserRange,
        replacements: Vec<VmEntry>,
    ) -> (Self, Vec<VmEntry>, usize) {
        let mut touched_total = 0usize;
        let (before_start, from_start) =
            split_root_before(self.root.clone(), range.start(), &mut touched_total);
        let (middle, after_end) = split_root_before(from_start, range.end(), &mut touched_total);

        let mut removed = Vec::new();
        let mut left = TreapRecipeIndex { root: before_start };
        if let Some(boundary) = left.predecessor_entry(range.start()) {
            if boundary.range.overlaps(range) {
                let (next_left, removed_boundary, touched) =
                    RecipeBackend::remove_exact(&left, boundary.range.start());
                left = next_left;
                touched_total += touched;
                if let Some(entry) = removed_boundary {
                    removed.push(entry);
                }
            }
        }

        collect_values(&middle, &mut removed);
        let mut replacement_root = TreapRecipeIndex { root: None };
        for entry in replacements {
            let (next, touched) = RecipeBackend::insert_entry(&replacement_root, entry);
            replacement_root = next;
            touched_total += touched;
        }

        let joined = merge_nodes(left.root, replacement_root.root, &mut touched_total);
        let root = merge_nodes(joined, after_end, &mut touched_total);
        (Self { root }, removed, touched_total)
    }

    fn reclaim_stats(&self) -> RecipeReclaimStats {
        RecipeReclaimStats {
            entries: node_len(&self.root),
            node_chunks: node_len(&self.root),
            tree_depth: treap_depth(&self.root),
            ..RecipeReclaimStats::default()
        }
    }
}

impl TreapRecipeIndex {
    fn lookup_ref(&self, addr: UserVirtAddr) -> Option<&VmEntry> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            if node.key.as_usize() <= addr.as_usize() {
                candidate = Some(&node.entry);
                cursor = node.right.as_deref();
            } else {
                cursor = node.left.as_deref();
            }
        }
        candidate.filter(|entry| entry.range.contains_addr(addr))
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

fn treap_depth(root: &Option<Arc<RecipeNode>>) -> usize {
    root.as_ref().map_or(0, |node| {
        1 + treap_depth(&node.left).max(treap_depth(&node.right))
    })
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
    if tx_observe::current().is_none() {
        return Arc::new(RecipeNode {
            key,
            priority,
            entry,
            left,
            right,
            subtree_len,
            subtree_vm_size,
        });
    }

    super::recipe::record_recipe_node_alloc_for_tree();
    let alloc_start_ns = tx_observe::clock_now_ns();
    let node = Arc::new(RecipeNode {
        key,
        priority,
        entry,
        left,
        right,
        subtree_len,
        subtree_vm_size,
    });
    super::recipe::emit_vm_recipe_allocation_for_tree(b"debug.alloc.vm.recipe_node", 1);
    super::recipe::emit_vm_recipe_allocation_for_tree(
        b"debug.alloc.vm.recipe_node.duration_ns",
        tx_observe::clock_now_ns().saturating_sub(alloc_start_ns),
    );
    node
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

#[allow(dead_code)]
fn replace_node_with_subtree(
    root: Option<Arc<RecipeNode>>,
    key: UserVirtAddr,
    replacement: Option<Arc<RecipeNode>>,
    replaced: &mut Option<VmEntry>,
    touched: &mut usize,
) -> Option<Arc<RecipeNode>> {
    let node = root?;
    *touched += 1;
    if key.as_usize() < node.key.as_usize() {
        return Some(build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            replace_node_with_subtree(node.left.clone(), key, replacement, replaced, touched),
            node.right.clone(),
        ));
    }
    if key.as_usize() > node.key.as_usize() {
        return Some(build_node(
            node.key,
            node.priority,
            node.entry.clone(),
            node.left.clone(),
            replace_node_with_subtree(node.right.clone(), key, replacement, replaced, touched),
        ));
    }

    *replaced = Some(node.entry.clone());
    let root = merge_nodes(node.left.clone(), replacement, touched);
    merge_nodes(root, node.right.clone(), touched)
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

const BPLUS_LEAF_CAP: usize = 16;
const BPLUS_INTERNAL_FANOUT: usize = 16;
const BPLUS_INTERNAL_SEPARATOR_CAP: usize = BPLUS_INTERNAL_FANOUT - 1;

// Persistent chunked B+ recipe-index status.
//
// The first block tracks the scoped v1 backend needed for cfg-selected kernel
// A/B testing. The second block tracks the remaining distance to a production
// persistent B+ tree. Keep both blocks honest: v1 can be functionally staged
// while still failing the promotion and full-tree boxes below.
//
// [x] Backend-neutral facade keeps treap and B+ behind the same `RecipeBackend`
//     interface for host A/B and cfg-selected kernel integration.
// [x] Clone-free `lookup_view` read path exists for fault-side callers that can
//     hold an epoch guard.
// [x] B+ roots remain immutable and are published/retired through the existing
//     RecipeIndex EBR path; fork snapshots still clone the root Arc graph.
// [x] Hot map/protect/unmap rewrites use `replace_range` instead of public
//     split/concat primitives.
// [x] Leaf chunks are sorted and capacity-bounded. Delete/trim paths now repair
//     underfull copied chunks by coalescing and repartitioning siblings.
// [x] Host tests cover leaf split lookup, exact-boundary unmap,
//     whole-leaf-covering unmap, spanning unmap, cross-leaf coalescing,
//     single-leaf replacement, gap insertion, and empty-gap no-op replacement.
// [x] Previous full-tree flatten/rebuild paths were removed from B+ insert,
//     remove, predecessor/successor, overlap, and replace_range.
// [x] Post-cleanup pthread observe completed losslessly rather than timing out.
// [x] Replace internal-node range rewrites that scan every sibling with a
//     separator-guided child slice edit, with local leaf-sibling repair instead
//     of flattening the whole parent leaf group after a copied child underflows.
// [x] Avoid rebuilding internal parents when a child edit returns an identical
//     Arc set, especially empty-gap no-op and missing-key remove.
// [x] Emit B+-specific chunk-copy/build counters so the remaining
//     `publish.duration_ns` cost is attributable without another broad probe.
// [ ] Beat the treap promotion gate on pthread SMP4
//     (`debug.lock.vm.recipe_index.mutation` avg <= 516us, normalized total
//     down at least 20%) before making B+ the default.
//
// Distance to a full persistent B+ tree:
//
// [x] Immutable root snapshots are preserved: readers can keep old roots and
//     writers publish a new root graph through the existing EBR path.
// [x] Separator-guided internal descent avoids whole-tree rebuilds for common
//     lookup, overlap, insert, remove, and replace operations.
// [x] Range replacement is the backend primitive, so map/protect/unmap do not
//     expose split/concat as the public mutation shape.
// [x] Delete rebalancing is implemented as coalesce-on-copy. Copied leaf
//     siblings are merged/repartitioned when a leaf goes under half full, and
//     copied internal siblings are similarly regrouped when a non-root internal
//     node goes below half fanout.
// [x] Non-root occupancy repair is covered for copied paths. Root fanout remains
//     root-exempt, while leaf fill and non-root internal fanout tests enforce
//     the lower-fill floor after representative delete/trim operations.
// [x] Persistent node storage no longer depends on heap-backed `Vec` fields.
//     Leaves, separators, and children use fixed-capacity inline chunks; leaf
//     entries are `Arc<VmEntry>` so rebuilding a leaf pointer-shares unchanged
//     entries instead of deep-cloning every occupied slot.
// [x] Multi-leaf range replacement is localized and performs coalesce-on-copy
//     repair for copied leaves and internal child groups after trims/removes.
//     Leaf repair now rebalances only the underfull leaf plus adjacent sibling
//     window, leaving unrelated siblings shared by Arc.
// [x] Reclaim attribution has backend-shape counters. Reclaim now reports
//     entries, node/chunk count, child refs, depth, leaf count, and leaf fill so
//     old-root cleanup can be compared with publish/build costs. It still does
//     not break down destructor time inside individual chunk drops.
// [ ] Leaf-capacity and fanout are not tuned. Current constants are
//     `BPLUS_LEAF_CAP=16` and `BPLUS_INTERNAL_FANOUT=16`; after leaf entries
//     became pointer-shared, service_ns evidence should decide whether a sweep
//     still matters.
// [ ] The backend is not default. Treap remains the baseline until B+ passes
//     functional tests, pthread SMP4 performance gates, and VM malloc
//     no-regression checks.
//
// Latest evidence, 2026-06-02:
// `recipe-bplus-arc-pthread-lock-20260602-124405` proved the Arc-entry leaf
// fix removed the deep-clone regression (`bplus.copied_entries avg=0.997`), but
// full pthread service was still `avg=1.102ms` and publish was `avg=982us`.
// `recipe-bplus-fanout16-pthread-lock-20260602-132000` narrows internal fanout
// to 16, builds internal and leaf chunks directly from occupied slices, and
// collapses single-child roots after delete rebuilds. That improved full
// pthread service to `avg=720.8us` and publish to `avg=604.5us`
// (`bplus.copied_child_refs avg=5.22`, `tree_depth avg=1.63`). It still loses
// to the treap reference (`avg=645.5us`) and misses the promotion target
// (`<=516us`), so B+ remains cfg-only.
#[derive(Clone, Default)]
pub(in crate::vm) struct BPlusRecipeIndex {
    root: Option<Arc<BPlusNode>>,
}

enum BPlusNode {
    Leaf(BPlusLeaf),
    Internal(BPlusInternal),
}

struct BPlusLeaf {
    entries: InlineVec<Arc<VmEntry>, BPLUS_LEAF_CAP>,
    len: usize,
    vm_size: usize,
}

struct BPlusInternal {
    separators: InlineVec<UserVirtAddr, BPLUS_INTERNAL_SEPARATOR_CAP>,
    children: InlineVec<Arc<BPlusNode>, BPLUS_INTERNAL_FANOUT>,
    len: usize,
    vm_size: usize,
    height: usize,
}

struct InlineVec<T, const N: usize> {
    entries: [Option<T>; N],
    len: usize,
}

impl<T, const N: usize> Default for InlineVec<T, N> {
    fn default() -> Self {
        Self {
            entries: core::array::from_fn(|_| None),
            len: 0,
        }
    }
}

impl<T: Clone, const N: usize> Clone for InlineVec<T, N> {
    fn clone(&self) -> Self {
        let mut out = Self::default();
        for value in self.iter() {
            out.push(value.clone());
        }
        out
    }
}

impl<T, const N: usize> InlineVec<T, N> {
    fn push(&mut self, value: T) {
        assert!(self.len < N, "inline B+ node capacity exceeded: cap={N}");
        self.entries[self.len] = Some(value);
        self.len += 1;
    }

    fn len(&self) -> usize {
        self.len
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries[..self.len]
            .iter()
            .map(|entry| entry.as_ref().expect("initialized inline B+ slot"))
    }

    fn first(&self) -> Option<&T> {
        self.get(0)
    }

    fn last(&self) -> Option<&T> {
        self.len
            .checked_sub(1)
            .and_then(|idx| self.entries[idx].as_ref())
    }

    fn get(&self, idx: usize) -> Option<&T> {
        if idx >= self.len {
            return None;
        }
        self.entries[idx].as_ref()
    }
}

impl<T: Clone, const N: usize> InlineVec<T, N> {
    fn to_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }
}

impl<T, const N: usize> core::ops::Index<usize> for InlineVec<T, N> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("inline B+ index in bounds")
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RewriteMetrics {
    changed_pages: usize,
    touched_entries: usize,
    allocated_nodes_or_chunks: usize,
    copied_entries: usize,
    copied_child_refs: usize,
    tree_depth: usize,
    leaf_splits: usize,
    leaf_fill_min: usize,
    leaf_fill_max: usize,
    leaf_fill_total: usize,
    leaf_count: usize,
}

impl RewriteMetrics {
    fn observe_leaf_fill(&mut self, fill: usize) {
        if self.leaf_count == 0 || fill < self.leaf_fill_min {
            self.leaf_fill_min = fill;
        }
        self.leaf_fill_max = self.leaf_fill_max.max(fill);
        self.leaf_fill_total += fill;
        self.leaf_count += 1;
    }
}

impl RecipeBackend for BPlusRecipeIndex {
    fn new() -> Self {
        Self { root: None }
    }

    fn backend_name(&self) -> &'static str {
        "bplus"
    }

    fn values_vec(&self) -> Vec<VmEntry> {
        let mut out = Vec::new();
        collect_bplus_values(&self.root, &mut out);
        out
    }

    fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        self.lookup_ref(addr).cloned()
    }

    fn lookup_view(&self, addr: UserVirtAddr) -> Option<VmEntryView<'_>> {
        self.lookup_ref(addr).map(view_for_entry)
    }

    fn predecessor_entry(&self, key: UserVirtAddr) -> Option<VmEntry> {
        self.root
            .as_ref()
            .and_then(|root| bplus_predecessor(root, key))
            .cloned()
    }

    fn successor_entry(&self, key: UserVirtAddr) -> Option<VmEntry> {
        self.root
            .as_ref()
            .and_then(|root| bplus_successor(root, key))
            .cloned()
    }

    fn overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        let mut out = Vec::new();
        collect_bplus_overlapping(&self.root, range, &mut out);
        out
    }

    fn insert_entry(&self, entry: VmEntry) -> (Self, usize) {
        let mut metrics = RewriteMetrics::default();
        metrics.copied_entries += 1;
        let entry = Arc::new(entry);
        let root = match &self.root {
            Some(root) => {
                let height = bplus_height(root);
                let nodes = insert_bplus_node(root, entry, &mut metrics);
                metrics.leaf_splits = nodes.len().saturating_sub(1);
                super::recipe::record_last_publish_leaf_splits_for_tree(metrics.leaf_splits);
                build_bplus_root_from_level(nodes, height, &mut metrics)
            }
            None => {
                let mut leaves = Vec::new();
                append_leaf_chunks(&mut leaves, alloc::vec![entry], &mut metrics);
                super::recipe::record_last_publish_leaf_splits_for_tree(0);
                leaves.pop()
            }
        };
        (Self { root }, metrics.touched_entries)
    }

    fn remove_exact(&self, key: UserVirtAddr) -> (Self, Option<VmEntry>, usize) {
        let Some(root) = &self.root else {
            return (Self { root: None }, None, 0);
        };
        if bplus_predecessor(root, key).is_none_or(|entry| entry.range.start() != key) {
            return (self.clone(), None, 0);
        }
        let mut metrics = RewriteMetrics::default();
        let mut removed = None;
        let nodes = remove_bplus_node(root, key, &mut removed, &mut metrics);
        let root = build_bplus_root_from_level(nodes, bplus_height(root), &mut metrics);
        (Self { root }, removed, metrics.touched_entries)
    }

    fn replace_range(
        &self,
        range: UserRange,
        replacements: Vec<VmEntry>,
    ) -> (Self, Vec<VmEntry>, usize) {
        let mut replacements = replacements;
        replacements.sort_by_key(|entry| entry.range.start().as_usize());
        let mut removed = Vec::new();
        let mut metrics = RewriteMetrics::default();
        let root = match &self.root {
            Some(root) => {
                let height = bplus_height(root);
                metrics.copied_entries += replacements.len();
                let mut cursor = ReplacementCursor::new(replacements);
                let nodes =
                    replace_range_bplus_node(root, range, &mut cursor, &mut removed, &mut metrics);
                let mut root = build_bplus_root_from_level(nodes, height, &mut metrics);
                if !cursor.inserted {
                    for replacement in cursor.remaining() {
                        let (next, touched) =
                            BPlusRecipeIndex { root }.insert_entry(replacement.as_ref().clone());
                        root = next.root;
                        metrics.touched_entries += touched;
                    }
                }
                root
            }
            None => {
                let (next, next_metrics) = build_bplus_from_entries(replacements, 0);
                metrics.touched_entries += next_metrics.touched_entries;
                metrics.copied_entries += next_metrics.copied_entries;
                metrics.allocated_nodes_or_chunks += next_metrics.allocated_nodes_or_chunks;
                next.root
            }
        };

        if self.root.is_none() && root.is_none() {
            return (Self { root: None }, removed, 0);
        } else {
            super::recipe::record_last_publish_leaf_splits_for_tree(metrics.leaf_splits);
            emit_bplus_rewrite_metrics(&metrics);
        }
        metrics.changed_pages = removed
            .iter()
            .map(|entry| entry.range.page_count())
            .sum::<usize>();
        (Self { root }, removed, metrics.touched_entries)
    }

    fn reclaim_stats(&self) -> RecipeReclaimStats {
        let mut stats = RecipeReclaimStats::default();
        if let Some(root) = &self.root {
            collect_bplus_reclaim_stats(root, &mut stats, true);
            stats.tree_depth = bplus_height(root);
        }
        stats
    }
}

fn append_leaf_chunks(
    leaves: &mut Vec<Arc<BPlusNode>>,
    entries: Vec<Arc<VmEntry>>,
    metrics: &mut RewriteMetrics,
) {
    for (start, end) in
        bplus_balanced_chunk_ranges(entries.len(), BPLUS_LEAF_CAP, BPLUS_LEAF_MIN_FILL)
    {
        let chunk = &entries[start..end];
        if chunk.is_empty() {
            continue;
        }
        metrics.observe_leaf_fill(chunk.len());
        metrics.touched_entries += chunk.len();
        metrics.allocated_nodes_or_chunks += 1;
        super::recipe::record_recipe_chunk_alloc_for_tree();
        leaves.push(Arc::new(BPlusNode::Leaf(BPlusLeaf::from_entry_slice(
            chunk,
        ))));
    }
}

fn emit_bplus_rewrite_metrics(metrics: &RewriteMetrics) {
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.allocated_chunks",
        metrics.allocated_nodes_or_chunks as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.copied_entries",
        metrics.copied_entries as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.copied_child_refs",
        metrics.copied_child_refs as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.leaf_splits",
        metrics.leaf_splits as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.tree_depth",
        metrics.tree_depth as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.leaf_count",
        metrics.leaf_count as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.leaf_fill_min",
        metrics.leaf_fill_min as i64,
    );
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.leaf_fill_max",
        metrics.leaf_fill_max as i64,
    );
    let avg_fill = if metrics.leaf_count == 0 {
        0
    } else {
        metrics.leaf_fill_total / metrics.leaf_count
    };
    super::recipe::emit_vm_recipe_trace_for_tree(
        b"debug.vm.recipe.bplus.leaf_fill_avg",
        avg_fill as i64,
    );
}

impl BPlusRecipeIndex {
    fn lookup_ref(&self, addr: UserVirtAddr) -> Option<&VmEntry> {
        let leaf = self.find_leaf(addr)?;
        let mut candidate = None;
        for entry in leaf.entries.iter() {
            if entry.range.start().as_usize() <= addr.as_usize() {
                candidate = Some(entry.as_ref());
            } else {
                break;
            }
        }
        candidate.filter(|entry| entry.range.contains_addr(addr))
    }

    fn find_leaf(&self, addr: UserVirtAddr) -> Option<&BPlusLeaf> {
        let mut node = self.root.as_deref()?;
        loop {
            match node {
                BPlusNode::Leaf(leaf) => return Some(leaf),
                BPlusNode::Internal(internal) => {
                    debug_assert!(internal.height > 1);
                    let idx = child_index_for(internal.separators.iter(), addr);
                    node = internal.children.get(idx).map(Arc::as_ref)?;
                }
            }
        }
    }
}

impl BPlusLeaf {
    fn from_entry_slice(entries: &[Arc<VmEntry>]) -> Self {
        let len = entries.len();
        let vm_size = entries.iter().map(|entry| entry.range.len()).sum();
        let mut inline_entries = InlineVec::default();
        for entry in entries {
            inline_entries.push(entry.clone());
        }
        Self {
            entries: inline_entries,
            len,
            vm_size,
        }
    }
}

struct ReplacementCursor {
    entries: Vec<Arc<VmEntry>>,
    inserted: bool,
}

impl ReplacementCursor {
    fn new(entries: Vec<VmEntry>) -> Self {
        Self {
            entries: entries.into_iter().map(Arc::new).collect(),
            inserted: false,
        }
    }

    fn take_all(&mut self) -> Vec<Arc<VmEntry>> {
        self.inserted = true;
        core::mem::take(&mut self.entries)
    }

    fn mark_inserted(&mut self) {
        self.inserted = true;
    }

    fn remaining(&self) -> &[Arc<VmEntry>] {
        &self.entries
    }
}

fn bplus_predecessor(node: &Arc<BPlusNode>, key: UserVirtAddr) -> Option<&VmEntry> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf
            .entries
            .iter()
            .take_while(|entry| entry.range.start().as_usize() <= key.as_usize())
            .last()
            .map(Arc::as_ref),
        BPlusNode::Internal(internal) => {
            let idx = child_index_for(internal.separators.iter(), key);
            let mut candidate = internal
                .children
                .get(idx)
                .and_then(|child| bplus_predecessor(child, key));
            let mut cursor = idx;
            while candidate.is_none() && cursor > 0 {
                cursor -= 1;
                candidate = bplus_last_entry(&internal.children[cursor]);
            }
            candidate
        }
    }
}

fn bplus_successor(node: &Arc<BPlusNode>, key: UserVirtAddr) -> Option<&VmEntry> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf
            .entries
            .iter()
            .find(|entry| entry.range.start().as_usize() >= key.as_usize())
            .map(Arc::as_ref),
        BPlusNode::Internal(internal) => {
            let idx = child_index_for(internal.separators.iter(), key);
            let mut candidate = internal
                .children
                .get(idx)
                .and_then(|child| bplus_successor(child, key));
            let mut cursor = idx + 1;
            while candidate.is_none() && cursor < internal.children.len() {
                candidate = bplus_first_entry(&internal.children[cursor]);
                cursor += 1;
            }
            candidate
        }
    }
}

fn collect_bplus_overlapping(
    root: &Option<Arc<BPlusNode>>,
    range: UserRange,
    out: &mut Vec<VmEntry>,
) {
    let Some(root) = root else {
        return;
    };
    collect_bplus_overlapping_node(root, range, out);
}

fn collect_bplus_overlapping_node(node: &Arc<BPlusNode>, range: UserRange, out: &mut Vec<VmEntry>) {
    if !bplus_node_may_overlap(node, range) {
        return;
    }
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => {
            for entry in leaf.entries.iter() {
                if entry.range.end().as_usize() <= range.start().as_usize() {
                    continue;
                }
                if entry.range.start().as_usize() >= range.end().as_usize() {
                    break;
                }
                if entry.range.overlaps(range) {
                    out.push(entry.as_ref().clone());
                }
            }
        }
        BPlusNode::Internal(internal) => {
            for child in internal.children.iter() {
                collect_bplus_overlapping_node(child, range, out);
            }
        }
    }
}

fn insert_bplus_node(
    node: &Arc<BPlusNode>,
    entry: Arc<VmEntry>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => {
            let mut entries = leaf.entries.to_vec();
            let pos = entries
                .binary_search_by_key(&entry.range.start().as_usize(), |candidate| {
                    candidate.range.start().as_usize()
                })
                .unwrap_or_else(|pos| pos);
            entries.insert(pos, entry);
            bplus_leaf_nodes_from_entries(entries, metrics)
        }
        BPlusNode::Internal(internal) => {
            let idx = child_index_for(internal.separators.iter(), entry.range.start());
            let child_nodes = insert_bplus_node(&internal.children[idx], entry, metrics);
            bplus_internal_nodes_replacing_child(internal, idx, child_nodes, metrics)
        }
    }
}

fn remove_bplus_node(
    node: &Arc<BPlusNode>,
    key: UserVirtAddr,
    removed: &mut Option<VmEntry>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => {
            let mut entries = leaf.entries.to_vec();
            if let Ok(pos) = entries.binary_search_by_key(&key.as_usize(), |candidate| {
                candidate.range.start().as_usize()
            }) {
                *removed = Some(entries.remove(pos).as_ref().clone());
            }
            bplus_leaf_nodes_from_entries(entries, metrics)
        }
        BPlusNode::Internal(internal) => {
            let idx = child_index_for(internal.separators.iter(), key);
            let child_nodes = remove_bplus_node(&internal.children[idx], key, removed, metrics);
            bplus_internal_nodes_replacing_child(internal, idx, child_nodes, metrics)
        }
    }
}

fn replace_range_bplus_node(
    node: &Arc<BPlusNode>,
    range: UserRange,
    replacements: &mut ReplacementCursor,
    removed: &mut Vec<VmEntry>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    if bplus_node_before_range(node, range) {
        return alloc::vec![node.clone()];
    }
    if bplus_node_after_range(node, range) {
        if !replacements.inserted {
            if replacements.entries.is_empty() {
                replacements.mark_inserted();
                return alloc::vec![node.clone()];
            }
            let mut nodes = bplus_leaf_nodes_from_entries(replacements.take_all(), metrics);
            nodes.push(node.clone());
            return nodes;
        }
        return alloc::vec![node.clone()];
    }

    match node.as_ref() {
        BPlusNode::Leaf(leaf) => {
            let mut entries = Vec::with_capacity(leaf.entries.len() + replacements.entries.len());
            let mut changed = false;
            for entry in leaf.entries.iter() {
                if entry.range.overlaps(range) {
                    changed = true;
                    removed.push(entry.as_ref().clone());
                } else {
                    entries.push(entry.clone());
                }
            }
            if !replacements.inserted {
                if replacements.entries.is_empty() && !changed {
                    replacements.mark_inserted();
                    return alloc::vec![node.clone()];
                }
                entries.extend(replacements.take_all());
            }
            entries.sort_by_key(|entry| entry.range.start().as_usize());
            bplus_leaf_nodes_from_entries(entries, metrics)
        }
        BPlusNode::Internal(internal) => {
            let Some((first, last)) = bplus_child_range_for_replace(internal, range) else {
                return alloc::vec![node.clone()];
            };
            let mut child_nodes = Vec::with_capacity(last - first + 3);
            for idx in first..=last {
                let child = &internal.children[idx];
                child_nodes.extend(replace_range_bplus_node(
                    child,
                    range,
                    replacements,
                    removed,
                    metrics,
                ));
            }
            if bplus_replaced_child_range_unchanged(internal, first, last, &child_nodes) {
                return alloc::vec![node.clone()];
            }
            bplus_internal_nodes_replacing_child_range(internal, first, last, child_nodes, metrics)
        }
    }
}

fn bplus_replaced_child_range_unchanged(
    internal: &BPlusInternal,
    first: usize,
    last: usize,
    replacements: &[Arc<BPlusNode>],
) -> bool {
    replacements.len() == last - first + 1
        && replacements
            .iter()
            .enumerate()
            .all(|(offset, replacement)| {
                Arc::ptr_eq(replacement, &internal.children[first + offset])
            })
}

fn bplus_child_range_for_replace(
    internal: &BPlusInternal,
    range: UserRange,
) -> Option<(usize, usize)> {
    let child_count = internal.children.len();
    if child_count == 0 {
        return None;
    }

    let mut first = child_index_for(internal.separators.iter(), range.start());
    first = first.saturating_sub(1);
    while first < child_count && bplus_node_before_range(&internal.children[first], range) {
        first += 1;
    }
    if first == child_count || bplus_node_after_range(&internal.children[first], range) {
        return Some((first.min(child_count - 1), first.min(child_count - 1)));
    }

    let mut last = child_index_for(internal.separators.iter(), range.end());
    last = last.min(child_count - 1);
    while last + 1 < child_count && !bplus_node_after_range(&internal.children[last + 1], range) {
        last += 1;
    }
    while last > first && bplus_node_after_range(&internal.children[last], range) {
        last -= 1;
    }
    Some((first, last))
}

fn bplus_leaf_nodes_from_entries(
    entries: Vec<Arc<VmEntry>>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    let mut leaves = Vec::new();
    append_leaf_chunks(&mut leaves, entries, metrics);
    leaves
}

fn bplus_internal_nodes_from_children(
    mut children: Vec<Arc<BPlusNode>>,
    height: usize,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    if height == 2 {
        children = bplus_rebalance_leaf_children(children, metrics);
    } else {
        children = bplus_rebalance_internal_children(children, height, metrics);
    }

    let mut nodes = Vec::new();
    for (start, end) in bplus_balanced_chunk_ranges(
        children.len(),
        BPLUS_INTERNAL_FANOUT,
        BPLUS_INTERNAL_MIN_FANOUT,
    ) {
        let chunk = &children[start..end];
        if chunk.is_empty() {
            continue;
        }
        metrics.allocated_nodes_or_chunks += 1;
        metrics.copied_child_refs += chunk.len();
        super::recipe::record_recipe_chunk_alloc_for_tree();
        nodes.push(Arc::new(BPlusNode::Internal(
            BPlusInternal::from_child_slice(chunk, height),
        )));
    }
    nodes
}

fn bplus_internal_nodes_replacing_child(
    internal: &BPlusInternal,
    idx: usize,
    child_nodes: Vec<Arc<BPlusNode>>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    bplus_internal_nodes_replacing_child_range(internal, idx, idx, child_nodes, metrics)
}

fn bplus_internal_nodes_replacing_child_range(
    internal: &BPlusInternal,
    first: usize,
    last: usize,
    child_nodes: Vec<Arc<BPlusNode>>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    debug_assert!(first <= last);
    debug_assert!(last < internal.children.len());
    let removed_count = last - first + 1;
    let child_count = internal.children.len() - removed_count + child_nodes.len();
    if child_count > 0
        && child_count <= BPLUS_INTERNAL_FANOUT
        && !bplus_replacement_children_need_repair(&child_nodes, internal.height)
    {
        metrics.allocated_nodes_or_chunks += 1;
        metrics.copied_child_refs += child_count;
        super::recipe::record_recipe_chunk_alloc_for_tree();
        return alloc::vec![Arc::new(BPlusNode::Internal(
            BPlusInternal::from_replaced_child_range(internal, first, last, &child_nodes),
        ))];
    }

    let mut children = Vec::with_capacity(child_count);
    children.extend((0..first).map(|child_idx| internal.children[child_idx].clone()));
    children.extend(child_nodes);
    children.extend(
        (last + 1..internal.children.len()).map(|child_idx| internal.children[child_idx].clone()),
    );
    bplus_internal_nodes_from_children(children, internal.height, metrics)
}

fn bplus_replacement_children_need_repair(
    child_nodes: &[Arc<BPlusNode>],
    parent_height: usize,
) -> bool {
    if child_nodes.is_empty() {
        return true;
    }
    child_nodes.iter().any(|child| match child.as_ref() {
        BPlusNode::Leaf(leaf) => parent_height == 2 && leaf.entries.len() < BPLUS_LEAF_MIN_FILL,
        BPlusNode::Internal(internal) => {
            parent_height > 2 && internal.children.len() < BPLUS_INTERNAL_MIN_FANOUT
        }
    })
}

fn bplus_rebalance_leaf_children(
    children: Vec<Arc<BPlusNode>>,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    if children.len() <= 1 {
        return children;
    }
    let has_underfull_leaf = children.iter().any(|child| match child.as_ref() {
        BPlusNode::Leaf(leaf) => leaf.entries.len() < BPLUS_LEAF_MIN_FILL,
        BPlusNode::Internal(_) => false,
    });
    if !has_underfull_leaf {
        return children;
    }

    let mut rebalanced = Vec::with_capacity(children.len());
    let mut idx = 0usize;
    while idx < children.len() {
        let BPlusNode::Leaf(leaf) = children[idx].as_ref() else {
            debug_assert!(false, "height-2 B+ rebalance received an internal child");
            return alloc::vec![children[idx].clone()];
        };

        if leaf.entries.len() >= BPLUS_LEAF_MIN_FILL {
            rebalanced.push(children[idx].clone());
            idx += 1;
            continue;
        }

        let mut group = Vec::new();
        if let Some(previous) = rebalanced.pop() {
            group.push(previous);
        }
        group.push(children[idx].clone());
        idx += 1;

        if bplus_group_entry_count(&group) < BPLUS_LEAF_MIN_FILL && idx < children.len() {
            group.push(children[idx].clone());
            idx += 1;
        }

        let mut entries = Vec::with_capacity(bplus_group_entry_count(&group));
        for child in group {
            match child.as_ref() {
                BPlusNode::Leaf(leaf) => entries.extend(leaf.entries.iter().cloned()),
                BPlusNode::Internal(_) => {
                    debug_assert!(false, "height-2 B+ rebalance received an internal child");
                    return alloc::vec![child];
                }
            }
        }
        rebalanced.extend(bplus_leaf_nodes_from_entries(entries, metrics));
    }
    rebalanced
}

fn bplus_group_entry_count(children: &[Arc<BPlusNode>]) -> usize {
    children.iter().map(|child| bplus_len(child)).sum()
}

fn bplus_rebalance_internal_children(
    children: Vec<Arc<BPlusNode>>,
    height: usize,
    metrics: &mut RewriteMetrics,
) -> Vec<Arc<BPlusNode>> {
    if children.len() <= 1 {
        return children;
    }
    let has_underfull_internal = children.iter().any(|child| match child.as_ref() {
        BPlusNode::Internal(internal) => internal.children.len() < BPLUS_INTERNAL_MIN_FANOUT,
        BPlusNode::Leaf(_) => false,
    });
    if !has_underfull_internal {
        return children;
    }

    let mut rebalanced = Vec::with_capacity(children.len());
    let mut idx = 0usize;
    while idx < children.len() {
        let BPlusNode::Internal(internal) = children[idx].as_ref() else {
            debug_assert!(false, "internal B+ rebalance received a leaf child");
            return alloc::vec![children[idx].clone()];
        };

        if internal.children.len() >= BPLUS_INTERNAL_MIN_FANOUT {
            rebalanced.push(children[idx].clone());
            idx += 1;
            continue;
        }

        let mut group = Vec::new();
        if let Some(previous) = rebalanced.pop() {
            group.push(previous);
        }
        group.push(children[idx].clone());
        idx += 1;

        if bplus_group_child_count(&group) < BPLUS_INTERNAL_MIN_FANOUT && idx < children.len() {
            group.push(children[idx].clone());
            idx += 1;
        }

        let mut grandchildren = Vec::with_capacity(bplus_group_child_count(&group));
        for child in group {
            match child.as_ref() {
                BPlusNode::Internal(internal) => {
                    grandchildren.extend(internal.children.iter().cloned())
                }
                BPlusNode::Leaf(_) => {
                    debug_assert!(false, "internal B+ rebalance received a leaf child");
                    return alloc::vec![child];
                }
            }
        }
        rebalanced.extend(bplus_internal_nodes_from_children(
            grandchildren,
            height - 1,
            metrics,
        ));
    }
    rebalanced
}

fn bplus_group_child_count(children: &[Arc<BPlusNode>]) -> usize {
    children.iter().map(|child| bplus_child_count(child)).sum()
}

fn build_bplus_root_from_level(
    mut level: Vec<Arc<BPlusNode>>,
    mut height: usize,
    metrics: &mut RewriteMetrics,
) -> Option<Arc<BPlusNode>> {
    if level.is_empty() {
        metrics.tree_depth = 0;
        return None;
    }
    while level.len() > 1 {
        height += 1;
        level = bplus_internal_nodes_from_children(level, height, metrics);
    }
    let root = collapse_bplus_root(level.pop().expect("non-empty B+ root level"));
    metrics.tree_depth = bplus_height(&root);
    Some(root)
}

fn bplus_node_before_range(node: &Arc<BPlusNode>, range: UserRange) -> bool {
    bplus_last_entry(node)
        .is_some_and(|entry| entry.range.end().as_usize() <= range.start().as_usize())
}

fn bplus_node_after_range(node: &Arc<BPlusNode>, range: UserRange) -> bool {
    bplus_first_entry(node)
        .is_some_and(|entry| entry.range.start().as_usize() >= range.end().as_usize())
}

fn bplus_node_may_overlap(node: &Arc<BPlusNode>, range: UserRange) -> bool {
    !bplus_node_before_range(node, range) && !bplus_node_after_range(node, range)
}

fn bplus_first_entry(node: &Arc<BPlusNode>) -> Option<&VmEntry> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf.entries.first().map(Arc::as_ref),
        BPlusNode::Internal(internal) => internal.children.first().and_then(bplus_first_entry),
    }
}

fn bplus_last_entry(node: &Arc<BPlusNode>) -> Option<&VmEntry> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf.entries.last().map(Arc::as_ref),
        BPlusNode::Internal(internal) => internal.children.last().and_then(bplus_last_entry),
    }
}

fn build_bplus_from_entries(
    entries: Vec<VmEntry>,
    previous_leaf_count: usize,
) -> (BPlusRecipeIndex, RewriteMetrics) {
    let copied_entries = entries.len();
    let entries: Vec<Arc<VmEntry>> = entries.into_iter().map(Arc::new).collect();
    let mut leaves = Vec::new();
    let mut metrics = RewriteMetrics {
        tree_depth: if entries.is_empty() { 0 } else { 1 },
        copied_entries,
        ..RewriteMetrics::default()
    };
    append_leaf_chunks(&mut leaves, entries, &mut metrics);
    metrics.leaf_splits = leaves.len().saturating_sub(previous_leaf_count);
    super::recipe::record_last_publish_leaf_splits_for_tree(metrics.leaf_splits);
    let root = build_bplus_root_from_leaves(leaves, &mut metrics);
    (BPlusRecipeIndex { root }, metrics)
}

fn build_bplus_root_from_leaves(
    leaves: Vec<Arc<BPlusNode>>,
    metrics: &mut RewriteMetrics,
) -> Option<Arc<BPlusNode>> {
    let mut level = leaves;
    if level.is_empty() {
        metrics.tree_depth = 0;
        return None;
    }
    let mut depth = 1usize;
    while level.len() > 1 {
        let mut parents = Vec::new();
        for (start, end) in bplus_balanced_chunk_ranges(
            level.len(),
            BPLUS_INTERNAL_FANOUT,
            BPLUS_INTERNAL_MIN_FANOUT,
        ) {
            let chunk = &level[start..end];
            metrics.allocated_nodes_or_chunks += 1;
            metrics.copied_child_refs += chunk.len();
            super::recipe::record_recipe_chunk_alloc_for_tree();
            parents.push(Arc::new(BPlusNode::Internal(
                BPlusInternal::from_child_slice(chunk, depth + 1),
            )));
        }
        level = parents;
        depth += 1;
    }
    let root = collapse_bplus_root(level.pop().expect("non-empty B+ root level"));
    metrics.tree_depth = bplus_height(&root);
    Some(root)
}

fn collapse_bplus_root(mut root: Arc<BPlusNode>) -> Arc<BPlusNode> {
    while let BPlusNode::Internal(internal) = root.as_ref() {
        if internal.children.len() != 1 {
            break;
        }
        root = internal.children[0].clone();
    }
    root
}

fn bplus_balanced_chunk_ranges(
    len: usize,
    capacity: usize,
    min_fill: usize,
) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }

    let mut chunk_count = len.div_ceil(capacity);
    while chunk_count > 1 && len / chunk_count < min_fill {
        chunk_count -= 1;
    }

    let mut ranges = Vec::with_capacity(chunk_count);
    let mut start = 0usize;
    for chunk_index in 0..chunk_count {
        let remaining = len - start;
        let remaining_chunks = chunk_count - chunk_index;
        let chunk_len = remaining.div_ceil(remaining_chunks);
        debug_assert!(chunk_len <= capacity);
        debug_assert!(chunk_count == 1 || chunk_len >= min_fill);
        let end = start + chunk_len;
        ranges.push((start, end));
        start = end;
    }
    ranges
}

impl BPlusInternal {
    fn from_child_slice(children: &[Arc<BPlusNode>], height: usize) -> Self {
        assert!(
            children.len() <= BPLUS_INTERNAL_FANOUT,
            "inline B+ internal fanout exceeded: len={} cap={BPLUS_INTERNAL_FANOUT}",
            children.len()
        );
        let mut separators = InlineVec::default();
        let mut inline_children = InlineVec::default();
        for child in children.iter() {
            inline_children.push(child.clone());
        }
        for child in children.iter().skip(1) {
            if let Some(start) = bplus_first_key(child) {
                separators.push(start);
            }
        }
        let len = children.iter().map(|child| bplus_len(child)).sum();
        let vm_size = children.iter().map(|child| bplus_vm_size(child)).sum();
        Self {
            separators,
            children: inline_children,
            len,
            vm_size,
            height,
        }
    }

    fn from_replaced_child_range(
        existing: &BPlusInternal,
        first: usize,
        last: usize,
        replacements: &[Arc<BPlusNode>],
    ) -> Self {
        debug_assert!(first <= last);
        debug_assert!(last < existing.children.len());
        let removed_count = last - first + 1;
        let child_count = existing.children.len() - removed_count + replacements.len();
        assert!(
            child_count <= BPLUS_INTERNAL_FANOUT,
            "inline B+ internal fanout exceeded: len={} cap={BPLUS_INTERNAL_FANOUT}",
            child_count
        );
        let mut separators = InlineVec::default();
        let mut inline_children = InlineVec::default();
        let mut len = 0usize;
        let mut vm_size = 0usize;
        let mut child_index = 0usize;

        for child in existing.children.iter().take(first) {
            push_bplus_internal_child(
                child,
                &mut child_index,
                &mut separators,
                &mut inline_children,
                &mut len,
                &mut vm_size,
            );
        }
        for child in replacements {
            push_bplus_internal_child(
                child,
                &mut child_index,
                &mut separators,
                &mut inline_children,
                &mut len,
                &mut vm_size,
            );
        }
        for child in existing.children.iter().skip(last + 1) {
            push_bplus_internal_child(
                child,
                &mut child_index,
                &mut separators,
                &mut inline_children,
                &mut len,
                &mut vm_size,
            );
        }

        Self {
            separators,
            children: inline_children,
            len,
            vm_size,
            height: existing.height,
        }
    }
}

fn push_bplus_internal_child(
    child: &Arc<BPlusNode>,
    child_index: &mut usize,
    separators: &mut InlineVec<UserVirtAddr, BPLUS_INTERNAL_SEPARATOR_CAP>,
    children: &mut InlineVec<Arc<BPlusNode>, BPLUS_INTERNAL_FANOUT>,
    len: &mut usize,
    vm_size: &mut usize,
) {
    if *child_index > 0 {
        if let Some(start) = bplus_first_key(child) {
            separators.push(start);
        }
    }
    children.push(child.clone());
    *len += bplus_len(child);
    *vm_size += bplus_vm_size(child);
    *child_index += 1;
}

fn collect_bplus_values(root: &Option<Arc<BPlusNode>>, out: &mut Vec<VmEntry>) {
    let Some(root) = root else {
        return;
    };
    collect_bplus_values_node(root, out);
}

fn collect_bplus_values_node(node: &Arc<BPlusNode>, out: &mut Vec<VmEntry>) {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => {
            out.extend(leaf.entries.iter().map(|entry| entry.as_ref().clone()))
        }
        BPlusNode::Internal(internal) => {
            for child in internal.children.iter() {
                collect_bplus_values_node(child, out);
            }
        }
    }
}

fn child_index_for<'a>(
    separators: impl IntoIterator<Item = &'a UserVirtAddr>,
    addr: UserVirtAddr,
) -> usize {
    separators
        .into_iter()
        .take_while(|separator| separator.as_usize() <= addr.as_usize())
        .count()
}

fn bplus_first_key(node: &Arc<BPlusNode>) -> Option<UserVirtAddr> {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf.entries.first().map(|entry| entry.range.start()),
        BPlusNode::Internal(internal) => internal.children.first().and_then(bplus_first_key),
    }
}

fn bplus_len(node: &Arc<BPlusNode>) -> usize {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf.len,
        BPlusNode::Internal(internal) => internal.len,
    }
}

fn bplus_vm_size(node: &Arc<BPlusNode>) -> usize {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => leaf.vm_size,
        BPlusNode::Internal(internal) => internal.vm_size,
    }
}

fn bplus_height(node: &Arc<BPlusNode>) -> usize {
    match node.as_ref() {
        BPlusNode::Leaf(_) => 1,
        BPlusNode::Internal(internal) => internal.height,
    }
}

fn bplus_child_count(node: &Arc<BPlusNode>) -> usize {
    match node.as_ref() {
        BPlusNode::Leaf(_) => 0,
        BPlusNode::Internal(internal) => internal.children.len(),
    }
}

fn collect_bplus_reclaim_stats(
    node: &Arc<BPlusNode>,
    stats: &mut RecipeReclaimStats,
    is_root: bool,
) {
    match node.as_ref() {
        BPlusNode::Leaf(leaf) => stats.observe_leaf(leaf.entries.len()),
        BPlusNode::Internal(internal) => {
            stats.observe_internal(internal.children.len(), is_root);
            for child in internal.children.iter() {
                collect_bplus_reclaim_stats(child, stats, false);
            }
        }
    }
}

#[allow(dead_code)]
fn view_for_entry(entry: &VmEntry) -> VmEntryView<'_> {
    VmEntryView {
        range: entry.range,
        prot: entry.prot,
        flags: entry.flags,
        backing: &entry.backing,
        ufd_registration: entry.ufd_registration,
        private: entry.private.as_ref(),
    }
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
pub mod bench {
    use super::*;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct RecipeTreePerf {
        pub touched_total: usize,
        pub final_len: usize,
        pub vm_size: usize,
    }

    pub(in crate::vm) trait RecipeTreePerfBackend: RecipeBackend {}

    impl RecipeTreePerfBackend for TreapRecipeIndex {}
    impl RecipeTreePerfBackend for BPlusRecipeIndex {}

    pub(in crate::vm) fn run_sparse_map_unmap<B: RecipeTreePerfBackend>(
        entries: &[VmEntry],
        unmap: UserRange,
    ) -> RecipeTreePerf {
        let mut tree = RecipeTreeWith::<B>::new();
        let mut touched_total = 0usize;
        for entry in entries {
            let (next, touched) = tree.insert_entry(entry.clone());
            tree = next;
            touched_total += touched;
        }
        let (next, _, touched) = tree.replace_range(unmap, Vec::new());
        tree = next;
        touched_total += touched;
        RecipeTreePerf {
            touched_total,
            final_len: tree.len(),
            vm_size: tree.vm_size(),
        }
    }

    pub fn run_sparse_map_unmap_treap(entries: &[VmEntry], unmap: UserRange) -> RecipeTreePerf {
        run_sparse_map_unmap::<TreapRecipeIndex>(entries, unmap)
    }

    pub fn run_sparse_map_unmap_bplus(entries: &[VmEntry], unmap: UserRange) -> RecipeTreePerf {
        run_sparse_map_unmap::<BPlusRecipeIndex>(entries, unmap)
    }
}

#[cfg(test)]
mod tests {
    use super::bench::{run_sparse_map_unmap_bplus, run_sparse_map_unmap_treap};
    use super::*;

    fn range(start: usize, pages: usize) -> UserRange {
        UserRange::new_aligned(UserVirtAddr(start), pages * super::super::USER_PAGE_SIZE)
            .expect("valid range")
    }

    #[test]
    fn bplus_node_storage_is_inline_not_vec() {
        let leaf_entries_ty =
            core::any::type_name_of_val(&BPlusLeaf::from_entry_slice(&[]).entries);
        let internal = BPlusInternal::from_child_slice(&[], 2);
        let internal_separators_ty = core::any::type_name_of_val(&internal.separators);
        let internal_children_ty = core::any::type_name_of_val(&internal.children);

        assert!(
            !leaf_entries_ty.contains("alloc::vec::Vec"),
            "B+ leaf entries should be inline fixed-capacity storage, got {leaf_entries_ty}"
        );
        assert!(
            !internal_separators_ty.contains("alloc::vec::Vec"),
            "B+ internal separators should be inline fixed-capacity storage, got {internal_separators_ty}"
        );
        assert!(
            !internal_children_ty.contains("alloc::vec::Vec"),
            "B+ internal children should be inline fixed-capacity storage, got {internal_children_ty}"
        );
    }

    #[test]
    fn bplus_leaf_entries_are_pointer_shared() {
        let entry = Arc::new(VmEntry::new(
            range(0x88_0000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ));
        let leaf = BPlusLeaf::from_entry_slice(&[entry]);
        let leaf_entries_ty = core::any::type_name_of_val(&leaf.entries);

        assert!(
            leaf_entries_ty.contains("alloc::sync::Arc"),
            "B+ leaf entries should carry Arc<VmEntry> so unchanged leaf members are pointer-shared, got {leaf_entries_ty}"
        );
    }

    #[test]
    fn bplus_internal_fanout_is_narrow_for_inline_build_cost() {
        assert_eq!(
            BPLUS_INTERNAL_FANOUT, 16,
            "B+ internal fanout should stay narrow while internal nodes use fixed inline storage"
        );
    }

    #[test]
    fn bplus_single_child_replacement_rebuilds_parent_directly() {
        let leaves = (0..3)
            .map(|idx| {
                let entries = (0..BPLUS_LEAF_MIN_FILL)
                    .map(|entry_idx| {
                        Arc::new(VmEntry::new(
                            range(
                                0x89_0000 + (idx * BPLUS_LEAF_MIN_FILL + entry_idx) * 0x4000,
                                1,
                            ),
                            Prot::READ,
                            VmEntryFlags::PRIVATE,
                            VmBacking::PrivateAnon,
                        ))
                    })
                    .collect::<Vec<_>>();
                Arc::new(BPlusNode::Leaf(BPlusLeaf::from_entry_slice(&entries)))
            })
            .collect::<Vec<_>>();
        let internal = BPlusInternal::from_child_slice(&leaves, 2);
        let replacement_entries = (0..BPLUS_LEAF_MIN_FILL)
            .map(|entry_idx| {
                Arc::new(VmEntry::new(
                    range(0x89_0000 + (BPLUS_LEAF_MIN_FILL + entry_idx) * 0x4000, 1),
                    Prot::READ_WRITE,
                    VmEntryFlags::PRIVATE,
                    VmBacking::PrivateAnon,
                ))
            })
            .collect::<Vec<_>>();
        let replacement = Arc::new(BPlusNode::Leaf(BPlusLeaf::from_entry_slice(
            &replacement_entries,
        )));
        let mut metrics = RewriteMetrics::default();

        let nodes = bplus_internal_nodes_replacing_child(
            &internal,
            1,
            alloc::vec![replacement],
            &mut metrics,
        );

        assert_eq!(nodes.len(), 1);
        assert_eq!(metrics.copied_child_refs, 3);
        let rewritten = BPlusRecipeIndex {
            root: nodes.first().cloned(),
        };
        assert_eq!(
            rewritten
                .values_vec()
                .into_iter()
                .map(|entry| entry.range)
                .collect::<Vec<_>>(),
            (0..3 * BPLUS_LEAF_MIN_FILL)
                .map(|idx| range(0x89_0000 + idx * 0x4000, 1))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn bplus_child_range_replacement_rebuilds_parent_directly() {
        let leaves = (0..4)
            .map(|idx| {
                let entries = (0..BPLUS_LEAF_MIN_FILL)
                    .map(|entry_idx| {
                        Arc::new(VmEntry::new(
                            range(
                                0x8a_0000 + (idx * BPLUS_LEAF_MIN_FILL + entry_idx) * 0x4000,
                                1,
                            ),
                            Prot::READ,
                            VmEntryFlags::PRIVATE,
                            VmBacking::PrivateAnon,
                        ))
                    })
                    .collect::<Vec<_>>();
                Arc::new(BPlusNode::Leaf(BPlusLeaf::from_entry_slice(&entries)))
            })
            .collect::<Vec<_>>();
        let internal = BPlusInternal::from_child_slice(&leaves, 2);
        let replacements = (1..=2)
            .map(|idx| {
                let entries = (0..BPLUS_LEAF_MIN_FILL)
                    .map(|entry_idx| {
                        Arc::new(VmEntry::new(
                            range(
                                0x8a_0000 + (idx * BPLUS_LEAF_MIN_FILL + entry_idx) * 0x4000,
                                1,
                            ),
                            Prot::READ_WRITE,
                            VmEntryFlags::PRIVATE,
                            VmBacking::PrivateAnon,
                        ))
                    })
                    .collect::<Vec<_>>();
                Arc::new(BPlusNode::Leaf(BPlusLeaf::from_entry_slice(&entries)))
            })
            .collect::<Vec<_>>();
        let mut metrics = RewriteMetrics::default();

        let nodes =
            bplus_internal_nodes_replacing_child_range(&internal, 1, 2, replacements, &mut metrics);

        assert_eq!(nodes.len(), 1);
        assert_eq!(metrics.copied_child_refs, 4);
        let rewritten = BPlusRecipeIndex {
            root: nodes.first().cloned(),
        };
        assert_eq!(
            rewritten
                .values_vec()
                .into_iter()
                .map(|entry| entry.range)
                .collect::<Vec<_>>(),
            (0..4 * BPLUS_LEAF_MIN_FILL)
                .map(|idx| range(0x8a_0000 + idx * 0x4000, 1))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn recipe_tree_backends_share_perf_interface() {
        let entries = (0..96)
            .map(|i| {
                VmEntry::new(
                    range(0x90_0000 + i * 2 * super::super::USER_PAGE_SIZE, 1),
                    Prot::READ,
                    VmEntryFlags::PRIVATE,
                    VmBacking::PrivateAnon,
                )
            })
            .collect::<Vec<_>>();
        let unmap = range(
            0x90_0000 + 24 * 2 * super::super::USER_PAGE_SIZE,
            40 * 2 - 1,
        );

        let treap = run_sparse_map_unmap_treap(&entries, unmap);
        let bplus = run_sparse_map_unmap_bplus(&entries, unmap);

        assert_eq!(treap.final_len, bplus.final_len);
        assert_eq!(treap.vm_size, bplus.vm_size);
        assert!(treap.touched_total > 0);
        assert!(bplus.touched_total > 0);
    }

    #[test]
    fn bplus_sparse_insert_and_queries_stay_local() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..4096 {
            let (next, touched) = tree.insert_entry(VmEntry::new(
                range(0x1000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
            assert!(
                touched <= BPLUS_LEAF_CAP + 1,
                "insert should edit one leaf, not rebuild the tree; i={i} touched={touched}"
            );
        }

        assert_eq!(
            tree.predecessor_entry(UserVirtAddr(0x1000 + 1234 * 0x4000 + 1))
                .expect("predecessor")
                .range,
            range(0x1000 + 1234 * 0x4000, 1)
        );
        assert_eq!(
            tree.successor_entry(UserVirtAddr(0x1000 + 1234 * 0x4000 + 1))
                .expect("successor")
                .range,
            range(0x1000 + 1235 * 0x4000, 1)
        );
        let overlaps = tree.overlapping(range(0x1000 + 2048 * 0x4000, 9));
        assert_eq!(overlaps.len(), 3);
        assert_eq!(overlaps[0].range, range(0x1000 + 2048 * 0x4000, 1));
    }

    #[test]
    fn bplus_single_leaf_replace_range_is_bounded() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..4096 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0x20_0000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        let existing = tree
            .lookup(UserVirtAddr(0x20_0000 + 3000 * 0x4000))
            .expect("existing entry");
        let replacement = VmEntry::new(
            existing.range,
            Prot::READ_WRITE,
            existing.flags,
            existing.backing.clone(),
        )
        .with_ufd_registration(existing.ufd_registration)
        .with_private(existing.private.clone());
        let (rewritten, removed, touched) =
            tree.replace_range(existing.range, alloc::vec![replacement.clone()]);

        assert_eq!(removed, alloc::vec![existing]);
        assert_eq!(
            rewritten.lookup(replacement.range.start()),
            Some(replacement)
        );
        assert!(
            touched <= BPLUS_LEAF_CAP + 1,
            "single-leaf replacement should stay local; touched={touched}"
        );
    }

    #[test]
    fn bplus_spanning_replace_preserves_unaffected_subtrees() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..4096 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0x40_0000 + i * 0x2000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        let unmap = range(0x40_0000 + 1024 * 0x2000, 64 * 2 - 1);
        let (rewritten, removed, touched) = tree.replace_range(unmap, Vec::new());

        assert_eq!(removed.len(), 64);
        assert_eq!(rewritten.len(), 4096 - 64);
        assert_eq!(
            rewritten
                .successor_entry(UserVirtAddr(0x40_0000 + 1024 * 0x2000))
                .expect("right survivor")
                .range,
            range(0x40_0000 + 1088 * 0x2000, 1)
        );
        assert!(
            touched <= 96,
            "spanning range should copy boundary leaves, not rebuild all entries; touched={touched}"
        );
    }

    #[test]
    fn bplus_gap_replace_inserts_at_first_after_subtree() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..64 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0x80_0000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        let replacement = VmEntry::new(
            range(0x80_0000 + 7 * 0x4000 + 0x2000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        );
        let replace_window = range(0x80_0000 + 7 * 0x4000 + 0x2000, 1);
        let (rewritten, removed, touched) =
            tree.replace_range(replace_window, alloc::vec![replacement.clone()]);

        assert!(removed.is_empty());
        assert_eq!(rewritten.len(), 65);
        assert_eq!(
            rewritten.lookup(replacement.range.start()),
            Some(replacement)
        );
        assert!(
            touched <= BPLUS_LEAF_CAP + 1,
            "gap insertion should splice at the destination leaf; touched={touched}"
        );
        assert!(rewritten
            .values_vec()
            .windows(2)
            .all(|pair| pair[0].range.start().as_usize() < pair[1].range.start().as_usize()));
    }

    #[test]
    fn bplus_empty_gap_replace_is_noop() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..64 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0xa0_0000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        let before = tree.values_vec();
        let (rewritten, removed, touched) =
            tree.replace_range(range(0xa0_0000 + 13 * 0x4000 + 0x2000, 1), Vec::new());

        assert!(removed.is_empty());
        assert_eq!(rewritten.values_vec(), before);
        assert!(
            touched <= BPLUS_LEAF_CAP + 1,
            "empty gap replacement should only copy the destination leaf; touched={touched}"
        );
    }

    #[test]
    fn bplus_reclaim_shape_counts_chunks_separately_from_entries() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..40 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0xb0_0000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        let shape = tree.reclaim_stats();

        assert_eq!(shape.entries, 40);
        assert!(shape.node_chunks < shape.entries);
        assert!(shape.leaf_count >= 3);
        assert!(shape.tree_depth >= 2);
    }

    #[test]
    fn bplus_delete_merges_underfull_leaves() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..48 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0xc0_0000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        for i in 0..12 {
            let (next, removed, _) = tree.remove_exact(UserVirtAddr(0xc0_0000 + i * 0x4000));
            assert!(removed.is_some());
            tree = next;
        }

        let shape = tree.reclaim_stats();

        assert_eq!(shape.entries, 36);
        assert!(
            shape.leaf_fill_min >= BPLUS_LEAF_MIN_FILL,
            "leaf merge should repair underfull leaves; shape={shape:?}"
        );
        assert_eq!(tree.values_vec().len(), 36);
        assert_eq!(
            tree.lookup(UserVirtAddr(0xc0_0000 + 12 * 0x4000))
                .expect("first survivor")
                .range,
            range(0xc0_0000 + 12 * 0x4000, 1)
        );
    }

    #[test]
    fn bplus_delete_rebalances_nonroot_internal_nodes() {
        let mut tree = RecipeTreeWith::<BPlusRecipeIndex>::new();
        for i in 0..1536 {
            let (next, _) = tree.insert_entry(VmEntry::new(
                range(0xd0_0000 + i * 0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            ));
            tree = next;
        }

        let unmap = range(0xd0_0000, 768 * 4);
        let (tree, removed, _) = tree.replace_range(unmap, Vec::new());
        assert_eq!(removed.len(), 768);

        let shape = tree.reclaim_stats();

        assert_eq!(shape.entries, 768);
        if shape.nonroot_internal_count > 0 {
            assert!(
                shape.nonroot_internal_fanout_min >= BPLUS_INTERNAL_MIN_FANOUT,
                "non-root internal merge should repair underfull nodes; shape={shape:?}"
            );
        }
        assert_eq!(tree.values_vec().len(), 768);
        assert_eq!(
            tree.lookup(UserVirtAddr(0xd0_0000 + 768 * 0x4000))
                .expect("first survivor")
                .range,
            range(0xd0_0000 + 768 * 0x4000, 1)
        );
    }
}
