//! Per-mapping private-CoW page tracking.
//!
//! See `docs/progress/decisions/2026-05-12-pc-cow-implementation-plan.md`
//! and the design rules in `docs/design/03_memory-vm/VM_v1_2.md`
//! (`txdoc:VM-7-2-WRITE-FAULT-ON-MAP-PRIVATE`,
//! `txdoc:VM-7-3-PRIVATE-FRAMES-HAVE-NO-BACK-INDEX`) and
//! `docs/design/03_memory-vm/PAGE_BACKED_v1.md` (`txdoc:PAGE-BACKED-10-3`).
//!
//! `PrivatePageSet` is the authoritative store for a `VmEntry`'s
//! private (CoW-diverged) pages. It is attached to the mapping identity
//! (the `VmEntry`), keyed by `VmPageOff` (page offset within the entry's
//! range) — not by absolute `UserPage`. Absolute-VA keying is fragile
//! under munmap+mmap-same-VA, MAP_FIXED, mremap, and VmEntry
//! split/merge.
//!
//! A `PrivatePageSet` is zone-allocated so a `Cap<PrivatePageSet>` lives
//! on `VmEntry` cheaply (one atomic refcount bump per `VmEntry::clone`).
//! Recipe tree re-publishes (split/merge during mprotect, swap_commit,
//! etc.) preserve mapping identity by reusing the same Cap.
//!
//! Private contents are **authoritative**, not cache: after a write,
//! the bytes are unrecoverable from any other source. `cache_ref` on
//! the underlying `Frame` keeps each entry alive while the set retains
//! it; pmap `MapPin`s are acquired separately on PTE install.

use crate::vm::adapter::step_engine::{self as step_engine};
use crate::vm::adapter::step_engine::{Cap, SpinMutex, Zone, ZoneAllocated, ZoneError};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use step_engine::page_allocator::{BitmapPageAllocator, CachePin};
use tx_hal::Ppn;

/// Page offset within a `VmEntry`'s range. `VmPageOff(0)` is the first
/// page of the entry; offsets are relative to the entry's `range.start`
/// and stay stable across `mremap`-move.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VmPageOff(pub u64);

/// CoW state of a private frame.
///
/// Transitions:
/// - `Exclusive → SharedCow`: fork (`fork_share`).
/// - `SharedCow → Exclusive`: first writer copies + replaces via
///   `replace_if_match`.
/// - New entries always start `Exclusive` (a write fault that missed
///   the set allocated a private frame and won the publish CAS).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateFrameState {
    /// This mapping owns the frame outright; writable PTE OK.
    Exclusive,
    /// Frame is read-aliased between fork-related mappings (parent and
    /// at least one child share it). First writer on any side must
    /// allocate a new frame and transition that side to `Exclusive`.
    SharedCow,
}

/// One private CoW page held by a `PrivatePageSet`.
///
/// The `CachePin` keeps the frame's `cache_ref >= 1` for the lifetime
/// of the entry. `state` discriminates between exclusive ownership and
/// fork-shared aliasing.
pub struct PrivateFrame {
    pub ppn: Ppn,
    state: AtomicU8,
    pub cache_pin: CachePin<'static, BitmapPageAllocator<'static>>,
}

impl PrivateFrame {
    pub fn new(
        ppn: Ppn,
        state: PrivateFrameState,
        cache_pin: CachePin<'static, BitmapPageAllocator<'static>>,
    ) -> Self {
        Self {
            ppn,
            state: AtomicU8::new(state.into_raw()),
            cache_pin,
        }
    }

    fn state(&self) -> PrivateFrameState {
        PrivateFrameState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: PrivateFrameState) {
        self.state.store(state.into_raw(), Ordering::Release);
    }
}

impl core::fmt::Debug for PrivateFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrivateFrame")
            .field("ppn", &self.ppn)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl PrivateFrameState {
    fn into_raw(self) -> u8 {
        match self {
            Self::Exclusive => 0,
            Self::SharedCow => 1,
        }
    }

    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::SharedCow,
            _ => Self::Exclusive,
        }
    }
}

/// Snapshot of a `PrivateFrame` returned by lookup. Carries `ppn` and
/// `state` for the caller to act on without holding the set's lock,
/// plus the source identity used by `replace_if_match`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrivateFrameSnapshot {
    pub ppn: Ppn,
    pub state: PrivateFrameState,
}

/// Source identity for `replace_if_match` linearization. Two writers
/// racing on the same `(off, src_ppn, SharedCow)` will both attempt
/// the CAS; exactly one wins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrivateFrameIdentity {
    pub ppn: Ppn,
    pub state: PrivateFrameState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivatePageError {
    /// Another writer published a different frame at the same offset
    /// after we copied. Caller should drop its candidate frame and
    /// re-read the set.
    Conflict { current: PrivateFrameSnapshot },
    /// `replace_if_match` saw no entry at `off`.
    Missing,
    /// Zone allocation failed.
    Zone(ZoneError),
}

impl From<ZoneError> for PrivatePageError {
    fn from(error: ZoneError) -> Self {
        Self::Zone(error)
    }
}

static PRIVATE_PAGE_SET_ZONE: Zone<PrivatePageSet> = Zone::const_new();

static PRIVATE_INSTALL_COUNT: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_MAX_NS: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_TOUCHED_TOTAL: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_TOUCHED_MAX: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_NODE_ALLOC_TOTAL: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_NODE_ALLOC_MAX: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_LEN_MAX: AtomicU64 = AtomicU64::new(0);
const PRIVATE_INSTALL_SAMPLE_CAP: usize = 16_384;
static PRIVATE_INSTALL_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);
static PRIVATE_INSTALL_SAMPLES: [AtomicU64; PRIVATE_INSTALL_SAMPLE_CAP] =
    [const { AtomicU64::new(0) }; PRIVATE_INSTALL_SAMPLE_CAP];

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::vm) struct PrivateInstallDebugTotals {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub touched_total: u64,
    pub touched_max: u64,
    pub node_alloc_total: u64,
    pub node_alloc_max: u64,
    pub len_max: u64,
    pub sample_count: u64,
    pub sample_dropped: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::vm) struct PrivateInstallDebugSample {
    pub duration_ns: u64,
    pub len_before: u64,
}

unsafe impl ZoneAllocated for PrivatePageSet {
    fn zone() -> &'static Zone<Self> {
        &PRIVATE_PAGE_SET_ZONE
    }
}

/// Per-mapping private CoW store. Recipe clones share the same Cap; split and
/// fork operations publish new Caps whose internal trees structurally share
/// unchanged resident entries, so VMA surgery does not reacquire every page pin.
pub struct PrivatePageSet {
    pages: SpinMutex<PrivatePageTree>,
    key_base: u64,
}

#[derive(Clone, Default)]
struct PrivatePageTree {
    root: Option<Arc<PrivatePageNode>>,
    len: usize,
}

struct PrivatePageNode {
    key: VmPageOff,
    priority: u64,
    frame: Arc<PrivateFrame>,
    left: Option<Arc<PrivatePageNode>>,
    right: Option<Arc<PrivatePageNode>>,
    subtree_len: usize,
}

impl PrivatePageTree {
    fn lookup(&self, key: VmPageOff) -> Option<Arc<PrivateFrame>> {
        let mut cursor = self.root.as_deref();
        while let Some(node) = cursor {
            if key < node.key {
                cursor = node.left.as_deref();
            } else if key > node.key {
                cursor = node.right.as_deref();
            } else {
                return Some(node.frame.clone());
            }
        }
        None
    }

    fn insert_if_absent(
        &self,
        key: VmPageOff,
        frame: PrivateFrame,
    ) -> Result<(Self, usize, usize), Arc<PrivateFrame>> {
        if let Some(existing) = self.lookup(key) {
            return Err(existing);
        }
        let mut touched = 0usize;
        let mut node_allocs = 0usize;
        let root = Some(insert_private_node_counted(
            self.root.clone(),
            key,
            Arc::new(frame),
            &mut touched,
            &mut node_allocs,
        ));
        Ok((
            Self {
                root,
                len: self.len + 1,
            },
            touched,
            node_allocs,
        ))
    }

    fn replace_exact(
        &self,
        key: VmPageOff,
        expected: PrivateFrameIdentity,
        frame: PrivateFrame,
    ) -> Result<Self, PrivatePageError> {
        let Some(existing) = self.lookup(key) else {
            return Err(PrivatePageError::Missing);
        };
        if existing.ppn != expected.ppn || existing.state() != expected.state {
            return Err(PrivatePageError::Conflict {
                current: snapshot_from_frame(&existing),
            });
        }

        let mut touched = 0usize;
        let mut removed = None;
        let root = remove_private_node(self.root.clone(), key, &mut removed, &mut touched);
        debug_assert!(removed.is_some());
        let root = Some(insert_private_node(
            root,
            key,
            Arc::new(frame),
            &mut touched,
        ));
        Ok(Self {
            root,
            len: self.len,
        })
    }

    fn drain_range(&self, start: VmPageOff, end: VmPageOff) -> Self {
        let mut touched = 0usize;
        let (left, at_or_after_start) =
            split_private_root_before(self.root.clone(), start, &mut touched);
        let (_discard, right) = split_private_root_before(at_or_after_start, end, &mut touched);
        let root = merge_private_nodes(left, right, &mut touched);
        Self {
            len: private_node_len(&root),
            root,
        }
    }

    fn slice(&self, start: VmPageOff, end: VmPageOff) -> Self {
        let mut touched = 0usize;
        let (_left, at_or_after_start) =
            split_private_root_before(self.root.clone(), start, &mut touched);
        let (root, _right) = split_private_root_before(at_or_after_start, end, &mut touched);
        Self {
            len: private_node_len(&root),
            root,
        }
    }
}

fn snapshot_from_frame(frame: &PrivateFrame) -> PrivateFrameSnapshot {
    PrivateFrameSnapshot {
        ppn: frame.ppn,
        state: frame.state(),
    }
}

fn atomic_max(slot: &AtomicU64, value: u64) {
    let mut current = slot.load(Ordering::Relaxed);
    while value > current {
        match slot.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

pub(in crate::vm) fn reset_private_page_debug_totals() {
    PRIVATE_INSTALL_COUNT.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_TOTAL_NS.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_MAX_NS.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_TOUCHED_TOTAL.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_TOUCHED_MAX.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_NODE_ALLOC_TOTAL.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_NODE_ALLOC_MAX.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_LEN_MAX.store(0, Ordering::Relaxed);
    PRIVATE_INSTALL_SAMPLE_COUNT.store(0, Ordering::Relaxed);
}

pub(in crate::vm) fn private_page_debug_totals() -> PrivateInstallDebugTotals {
    let sample_seen = PRIVATE_INSTALL_SAMPLE_COUNT.load(Ordering::Relaxed);
    let sample_count = sample_seen.min(PRIVATE_INSTALL_SAMPLE_CAP as u64);
    PrivateInstallDebugTotals {
        count: PRIVATE_INSTALL_COUNT.load(Ordering::Relaxed),
        total_ns: PRIVATE_INSTALL_TOTAL_NS.load(Ordering::Relaxed),
        max_ns: PRIVATE_INSTALL_MAX_NS.load(Ordering::Relaxed),
        touched_total: PRIVATE_INSTALL_TOUCHED_TOTAL.load(Ordering::Relaxed),
        touched_max: PRIVATE_INSTALL_TOUCHED_MAX.load(Ordering::Relaxed),
        node_alloc_total: PRIVATE_INSTALL_NODE_ALLOC_TOTAL.load(Ordering::Relaxed),
        node_alloc_max: PRIVATE_INSTALL_NODE_ALLOC_MAX.load(Ordering::Relaxed),
        len_max: PRIVATE_INSTALL_LEN_MAX.load(Ordering::Relaxed),
        sample_count,
        sample_dropped: sample_seen.saturating_sub(PRIVATE_INSTALL_SAMPLE_CAP as u64),
    }
}

pub(in crate::vm) fn private_page_debug_samples() -> Vec<PrivateInstallDebugSample> {
    let sample_count = PRIVATE_INSTALL_SAMPLE_COUNT
        .load(Ordering::Relaxed)
        .min(PRIVATE_INSTALL_SAMPLE_CAP as u64) as usize;
    let mut samples = Vec::with_capacity(sample_count);
    for slot in PRIVATE_INSTALL_SAMPLES.iter().take(sample_count) {
        let packed = slot.load(Ordering::Relaxed);
        samples.push(PrivateInstallDebugSample {
            duration_ns: packed & 0xffff_ffff,
            len_before: packed >> 32,
        });
    }
    samples
}

fn record_private_install_debug(
    duration_ns: u64,
    touched: usize,
    node_allocs: usize,
    len_before: usize,
) {
    PRIVATE_INSTALL_COUNT.fetch_add(1, Ordering::Relaxed);
    PRIVATE_INSTALL_TOTAL_NS.fetch_add(duration_ns, Ordering::Relaxed);
    PRIVATE_INSTALL_TOUCHED_TOTAL.fetch_add(touched as u64, Ordering::Relaxed);
    PRIVATE_INSTALL_NODE_ALLOC_TOTAL.fetch_add(node_allocs as u64, Ordering::Relaxed);
    atomic_max(&PRIVATE_INSTALL_MAX_NS, duration_ns);
    atomic_max(&PRIVATE_INSTALL_TOUCHED_MAX, touched as u64);
    atomic_max(&PRIVATE_INSTALL_NODE_ALLOC_MAX, node_allocs as u64);
    atomic_max(&PRIVATE_INSTALL_LEN_MAX, len_before as u64);
    let sample_index = PRIVATE_INSTALL_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index < PRIVATE_INSTALL_SAMPLE_CAP as u64 {
        let clamped_duration = duration_ns.min(u32::MAX as u64);
        let clamped_len = (len_before as u64).min(u32::MAX as u64);
        PRIVATE_INSTALL_SAMPLES[sample_index as usize]
            .store((clamped_len << 32) | clamped_duration, Ordering::Relaxed);
    }
}

fn private_node_len(root: &Option<Arc<PrivatePageNode>>) -> usize {
    root.as_ref().map_or(0, |node| node.subtree_len)
}

fn build_private_node(
    key: VmPageOff,
    priority: u64,
    frame: Arc<PrivateFrame>,
    left: Option<Arc<PrivatePageNode>>,
    right: Option<Arc<PrivatePageNode>>,
) -> Arc<PrivatePageNode> {
    Arc::new(PrivatePageNode {
        key,
        priority,
        frame,
        subtree_len: 1 + private_node_len(&left) + private_node_len(&right),
        left,
        right,
    })
}

fn build_private_node_counted(
    key: VmPageOff,
    priority: u64,
    frame: Arc<PrivateFrame>,
    left: Option<Arc<PrivatePageNode>>,
    right: Option<Arc<PrivatePageNode>>,
    node_allocs: &mut usize,
) -> Arc<PrivatePageNode> {
    *node_allocs += 1;
    build_private_node(key, priority, frame, left, right)
}

fn insert_private_node(
    root: Option<Arc<PrivatePageNode>>,
    key: VmPageOff,
    frame: Arc<PrivateFrame>,
    touched: &mut usize,
) -> Arc<PrivatePageNode> {
    let Some(node) = root else {
        *touched += 1;
        return build_private_node(key, private_page_priority(key), frame, None, None);
    };

    *touched += 1;
    if key < node.key {
        let left = Some(insert_private_node(node.left.clone(), key, frame, touched));
        rotate_private_right_if_needed(build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            left,
            node.right.clone(),
        ))
    } else {
        let right = Some(insert_private_node(node.right.clone(), key, frame, touched));
        rotate_private_left_if_needed(build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            node.left.clone(),
            right,
        ))
    }
}

fn insert_private_node_counted(
    root: Option<Arc<PrivatePageNode>>,
    key: VmPageOff,
    frame: Arc<PrivateFrame>,
    touched: &mut usize,
    node_allocs: &mut usize,
) -> Arc<PrivatePageNode> {
    let Some(node) = root else {
        *touched += 1;
        return build_private_node_counted(
            key,
            private_page_priority(key),
            frame,
            None,
            None,
            node_allocs,
        );
    };

    *touched += 1;
    if key < node.key {
        let left = Some(insert_private_node_counted(
            node.left.clone(),
            key,
            frame,
            touched,
            node_allocs,
        ));
        rotate_private_right_if_needed_counted(
            build_private_node_counted(
                node.key,
                node.priority,
                node.frame.clone(),
                left,
                node.right.clone(),
                node_allocs,
            ),
            node_allocs,
        )
    } else {
        let right = Some(insert_private_node_counted(
            node.right.clone(),
            key,
            frame,
            touched,
            node_allocs,
        ));
        rotate_private_left_if_needed_counted(
            build_private_node_counted(
                node.key,
                node.priority,
                node.frame.clone(),
                node.left.clone(),
                right,
                node_allocs,
            ),
            node_allocs,
        )
    }
}

fn remove_private_node(
    root: Option<Arc<PrivatePageNode>>,
    key: VmPageOff,
    removed: &mut Option<Arc<PrivateFrame>>,
    touched: &mut usize,
) -> Option<Arc<PrivatePageNode>> {
    let node = root?;
    *touched += 1;
    if key < node.key {
        return Some(build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            remove_private_node(node.left.clone(), key, removed, touched),
            node.right.clone(),
        ));
    }
    if key > node.key {
        return Some(build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            node.left.clone(),
            remove_private_node(node.right.clone(), key, removed, touched),
        ));
    }
    *removed = Some(node.frame.clone());
    merge_private_nodes(node.left.clone(), node.right.clone(), touched)
}

fn split_private_root_before(
    root: Option<Arc<PrivatePageNode>>,
    key: VmPageOff,
    touched: &mut usize,
) -> (Option<Arc<PrivatePageNode>>, Option<Arc<PrivatePageNode>>) {
    let Some(node) = root else {
        return (None, None);
    };

    *touched += 1;
    if node.key < key {
        let (right_left, right) = split_private_root_before(node.right.clone(), key, touched);
        let left = build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            node.left.clone(),
            right_left,
        );
        (Some(left), right)
    } else {
        let (left, left_right) = split_private_root_before(node.left.clone(), key, touched);
        let right = build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            left_right,
            node.right.clone(),
        );
        (left, Some(right))
    }
}

fn merge_private_nodes(
    left: Option<Arc<PrivatePageNode>>,
    right: Option<Arc<PrivatePageNode>>,
    touched: &mut usize,
) -> Option<Arc<PrivatePageNode>> {
    match (left, right) {
        (None, None) => None,
        (Some(node), None) | (None, Some(node)) => Some(node),
        (Some(left), Some(right)) if left.priority <= right.priority => {
            *touched += 1;
            Some(build_private_node(
                left.key,
                left.priority,
                left.frame.clone(),
                left.left.clone(),
                merge_private_nodes(left.right.clone(), Some(right), touched),
            ))
        }
        (Some(left), Some(right)) => {
            *touched += 1;
            Some(build_private_node(
                right.key,
                right.priority,
                right.frame.clone(),
                merge_private_nodes(Some(left), right.left.clone(), touched),
                right.right.clone(),
            ))
        }
    }
}

fn rotate_private_right_if_needed(node: Arc<PrivatePageNode>) -> Arc<PrivatePageNode> {
    let Some(left) = &node.left else {
        return node;
    };
    if left.priority > node.priority {
        return node;
    }
    build_private_node(
        left.key,
        left.priority,
        left.frame.clone(),
        left.left.clone(),
        Some(build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            left.right.clone(),
            node.right.clone(),
        )),
    )
}

fn rotate_private_right_if_needed_counted(
    node: Arc<PrivatePageNode>,
    node_allocs: &mut usize,
) -> Arc<PrivatePageNode> {
    let Some(left) = &node.left else {
        return node;
    };
    if left.priority > node.priority {
        return node;
    }
    build_private_node_counted(
        left.key,
        left.priority,
        left.frame.clone(),
        left.left.clone(),
        Some(build_private_node_counted(
            node.key,
            node.priority,
            node.frame.clone(),
            left.right.clone(),
            node.right.clone(),
            node_allocs,
        )),
        node_allocs,
    )
}

fn rotate_private_left_if_needed(node: Arc<PrivatePageNode>) -> Arc<PrivatePageNode> {
    let Some(right) = &node.right else {
        return node;
    };
    if right.priority > node.priority {
        return node;
    }
    build_private_node(
        right.key,
        right.priority,
        right.frame.clone(),
        Some(build_private_node(
            node.key,
            node.priority,
            node.frame.clone(),
            node.left.clone(),
            right.left.clone(),
        )),
        right.right.clone(),
    )
}

fn rotate_private_left_if_needed_counted(
    node: Arc<PrivatePageNode>,
    node_allocs: &mut usize,
) -> Arc<PrivatePageNode> {
    let Some(right) = &node.right else {
        return node;
    };
    if right.priority > node.priority {
        return node;
    }
    build_private_node_counted(
        right.key,
        right.priority,
        right.frame.clone(),
        Some(build_private_node_counted(
            node.key,
            node.priority,
            node.frame.clone(),
            node.left.clone(),
            right.left.clone(),
            node_allocs,
        )),
        right.right.clone(),
        node_allocs,
    )
}

fn private_page_priority(key: VmPageOff) -> u64 {
    let mut x = key.0;
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn set_tree_frames_shared(root: &Option<Arc<PrivatePageNode>>) {
    let Some(node) = root else {
        return;
    };
    set_tree_frames_shared(&node.left);
    node.frame.set_state(PrivateFrameState::SharedCow);
    set_tree_frames_shared(&node.right);
}

// `PrivatePageSet` carries `CachePin`s (via `PrivateFrame`) that are
// `!Send` by virtue of pinning to a specific page-allocator. The lock
// + zone discipline make whole-set access kernel-safe; mirrors the
// `AddressSpace` Send/Sync override.
unsafe impl Send for PrivatePageSet {}
unsafe impl Sync for PrivatePageSet {}

impl PrivatePageSet {
    pub fn new() -> Self {
        Self {
            pages: SpinMutex::new(PrivatePageTree::default()),
            key_base: 0,
        }
    }

    pub fn new_cap() -> Result<Cap<Self>, ZoneError> {
        let reservation = step_engine::reserve_for::<Self>()?;
        Ok(step_engine::sign_for(reservation, Self::new()))
    }

    fn new_cap_with_tree(key_base: u64, tree: PrivatePageTree) -> Result<Cap<Self>, ZoneError> {
        let reservation = step_engine::reserve_for::<Self>()?;
        Ok(step_engine::sign_for(
            reservation,
            Self {
                pages: SpinMutex::new(tree),
                key_base,
            },
        ))
    }

    fn key_for(&self, off: VmPageOff) -> VmPageOff {
        VmPageOff(self.key_base + off.0)
    }

    /// Read-only snapshot at `off`.
    pub fn lookup(&self, off: VmPageOff) -> Option<PrivateFrameSnapshot> {
        let pages = self.pages.lock();
        pages
            .lookup(self.key_for(off))
            .map(|frame| snapshot_from_frame(&frame))
    }

    /// CAS-publish a fresh entry. Used by the write-fault miss path.
    /// Returns the published snapshot on success; `Conflict` if another
    /// writer beat us.
    pub fn install_if_absent(
        &self,
        off: VmPageOff,
        frame: PrivateFrame,
    ) -> Result<PrivateFrameSnapshot, PrivatePageError> {
        emit_private_page_trace(b"debug.vm.private_set.install.phase", 0);
        let mut pages = self.pages.lock();
        emit_private_page_trace(b"debug.vm.private_set.install.phase", 1);
        let snap = PrivateFrameSnapshot {
            ppn: frame.ppn,
            state: frame.state(),
        };
        let key = self.key_for(off);
        emit_private_page_trace(b"debug.vm.private_set.install.len", pages.len as i64);
        emit_private_page_trace(b"debug.vm.private_set.install.key", key.0 as i64);
        emit_private_page_trace(b"debug.vm.private_set.install.phase", 2);
        let len_before = pages.len;
        let install_start_ns = tx_observe::clock_now_ns();
        match pages.insert_if_absent(key, frame) {
            Ok((next, touched, node_allocs)) => {
                let install_duration_ns =
                    tx_observe::clock_now_ns().saturating_sub(install_start_ns);
                record_private_install_debug(install_duration_ns, touched, node_allocs, len_before);
                emit_private_page_trace(b"debug.vm.private_set.install.phase", 3);
                emit_private_page_trace(b"debug.vm.private_set.install.touched", touched as i64);
                emit_private_page_trace(
                    b"debug.vm.private_set.install.node_allocs",
                    node_allocs as i64,
                );
                *pages = next;
                emit_private_page_trace(b"debug.vm.private_set.install.phase", 4);
                Ok(snap)
            }
            Err(existing) => {
                emit_private_page_trace(b"debug.vm.private_set.install.conflict", 1);
                Err(PrivatePageError::Conflict {
                    current: snapshot_from_frame(&existing),
                })
            }
        }
    }

    /// CAS-publish a replacement entry. Used by the `SharedCow →
    /// Exclusive` write-fault transition.
    ///
    /// Linearizes against concurrent writers: succeeds iff the current
    /// entry's `(ppn, state)` matches `expected`. On mismatch returns
    /// `Conflict { current }`; on absence returns `Missing`.
    pub fn replace_if_match(
        &self,
        off: VmPageOff,
        expected: PrivateFrameIdentity,
        new: PrivateFrame,
    ) -> Result<PrivateFrameSnapshot, PrivatePageError> {
        let mut pages = self.pages.lock();
        let snap = PrivateFrameSnapshot {
            ppn: new.ppn,
            state: new.state(),
        };
        let next = pages.replace_exact(self.key_for(off), expected, new)?;
        *pages = next;
        Ok(snap)
    }

    /// Drop entries whose offsets fall in `[range_start_off, range_end_off)`.
    /// Each removed `PrivateFrame` releases its `CachePin` on drop.
    ///
    /// Used by munmap / mremap-shrink / MAP_FIXED-replace.
    pub fn drain_range(&self, start: VmPageOff, end: VmPageOff) {
        let mut pages = self.pages.lock();
        *pages = pages.drain_range(self.key_for(start), self.key_for(end));
    }

    /// Number of resident entries (for tests / observability).
    pub fn len(&self) -> usize {
        self.pages.lock().len
    }

    pub fn is_empty(&self) -> bool {
        self.pages.lock().len == 0
    }

    /// Fork: transition every entry to `SharedCow` in `self` and return
    /// a sibling set sharing the same resident frame tree. The shared
    /// `Arc<PrivateFrame>` keeps the cache pin alive until both sets drop
    /// or replace the entry.
    pub fn fork_share(&self) -> Result<Cap<Self>, PrivatePageError> {
        let parent_pages = self.pages.lock();
        set_tree_frames_shared(&parent_pages.root);
        let child_tree = parent_pages.clone();
        drop(parent_pages);
        Self::new_cap_with_tree(self.key_base, child_tree).map_err(PrivatePageError::Zone)
    }

    /// VmEntry split helper: build a fresh set containing only entries
    /// whose offsets fall in `[start, end)` (relative to the parent
    /// entry), rebased by `-rebase_delta` (so offsets become relative
    /// to the new sub-entry's range start).
    ///
    /// Surviving entries share immutable tree nodes and frame Arcs with
    /// the original set. Caller is responsible for dropping the parts of
    /// the parent set that no longer belong to the surviving sub-entry
    /// (via `drain_range`).
    pub fn split(
        &self,
        start: VmPageOff,
        end: VmPageOff,
        rebase_delta: u64,
    ) -> Result<Cap<Self>, PrivatePageError> {
        let parent_pages = self.pages.lock();
        let sliced = parent_pages.slice(self.key_for(start), self.key_for(end));
        drop(parent_pages);
        Self::new_cap_with_tree(self.key_base + rebase_delta, sliced)
            .map_err(PrivatePageError::Zone)
    }
}

impl Default for PrivatePageSet {
    fn default() -> Self {
        Self::new()
    }
}

fn emit_private_page_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
    }
}

impl core::fmt::Debug for PrivatePageSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrivatePageSet")
            .field("len", &self.len())
            .finish()
    }
}
