//! Stable resident binding ownership while the page index remains lock-backed.
//!
//! P2 deliberately leaves lookup and mutation behind `PageContainerState`.
//! The cell is nevertheless the only owner of a resident frame's binding
//! evidence, so later immutable-root publication does not have to migrate pin
//! ownership a second time.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use tx_hal::Ppn;
use tx_substrate::epoch::Guard;

use super::{
    BitmapPageAllocator, CachePin, CachedFrame, DeviceFrame, MaterializedPagePin,
    MaterializedPageSnapshotPin, PageCacheError, PageCachePin, PageSlot, PageSlotState,
};
use crate::sync::SpinMutex;

#[derive(Debug)]
pub(crate) enum ResidentBindingPin {
    Allocated(CachePin<'static, BitmapPageAllocator<'static>>),
    Device(DeviceFrame),
}

/// One stable resident binding. The binding pin is independent of transient
/// MapPin, PageLease, writeback, reflink/gift, and DMA evidence.
#[derive(Debug)]
pub(crate) struct ResidentCell {
    ppn: Ppn,
    slot: Arc<PageSlot>,
    /// Whether this cell is reachable from the immutable RCU root.  The
    /// lock-backed page table remains authoritative when publication is
    /// temporarily backpressured, so mutation paths must distinguish the two
    /// representations before deciding whether an old root must be retired.
    published: AtomicBool,
    /// The existing read/withdrawal gate also owns the optional resident
    /// binding. Keeping both in one cell avoids adding a second lock to every
    /// RCU read hit while still allowing withdrawal to release the frame
    /// independently of an old immutable root's lifetime.
    materialization: SpinMutex<Option<ResidentBindingPin>>,
}

impl ResidentCell {
    pub(crate) fn from_cached_frame(frame: CachedFrame, slot: Arc<PageSlot>) -> Self {
        Self {
            ppn: frame.ppn,
            slot,
            published: AtomicBool::new(false),
            materialization: SpinMutex::new(Some(frame.pin.into())),
        }
    }

    pub(crate) const fn ppn(&self) -> Ppn {
        self.ppn
    }

    pub(crate) fn slot(&self) -> &Arc<PageSlot> {
        &self.slot
    }

    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
    }

    pub(crate) fn is_published(&self) -> bool {
        self.published.load(Ordering::Acquire)
    }

    pub(crate) fn mark_withdrawn(&self) {
        // An old RCU root can retain this cell past the publication grace
        // period, but it must not retain the physical frame.  A reader that
        // raced across this gate already owns a MapPin; all later readers see
        // the empty binding and cannot begin using the frame.
        drop(self.materialization.lock().take());
    }

    fn slot_is_current(&self) -> bool {
        matches!(
            self.slot.snapshot().state,
            PageSlotState::Resident { ppn }
                | PageSlotState::Dirty { ppn }
                | PageSlotState::Writeback { ppn, .. }
                if ppn == self.ppn
        )
    }

    fn try_materialize(&self) -> Result<Option<(MaterializedPagePin, bool)>, PageCacheError> {
        // Withdrawal takes the same cell-local gate before making its root
        // change visible. A reader that crosses this point has already taken
        // its independent MapPin; a later reader cannot begin one.
        let binding = self.materialization.lock();
        let Some(binding) = binding.as_ref() else {
            return Ok(None);
        };
        if !self.slot_is_current() {
            return Ok(None);
        }
        let map_pin = match binding {
            ResidentBindingPin::Allocated(_) => MaterializedPagePin::Allocated(
                super::acquire_map_pin_for_materialization(self.ppn)?,
            ),
            ResidentBindingPin::Device(device) => MaterializedPagePin::Device(*device),
        };
        let dirty = matches!(
            self.slot.snapshot().state,
            PageSlotState::Dirty { .. } | PageSlotState::Writeback { .. }
        );
        Ok(Some((map_pin, dirty)))
    }

    /// Acquire snapshot evidence while holding the same gate as withdrawal.
    /// The returned CachePin/DeviceFrame remains valid after the gate is
    /// released and lets the caller convert the snapshot into a MapPin.
    pub(crate) fn try_snapshot_pin(
        &self,
    ) -> Result<Option<MaterializedPageSnapshotPin>, PageCacheError> {
        let binding = self.materialization.lock();
        let Some(binding) = binding.as_ref() else {
            return Ok(None);
        };
        if !self.slot_is_current() {
            return Ok(None);
        }
        let pin = match binding {
            ResidentBindingPin::Allocated(cache_pin) => {
                debug_assert_eq!(cache_pin.ppn(), self.ppn);
                MaterializedPageSnapshotPin::Allocated(
                    super::page_allocator::acquire_cache_pin(self.ppn)
                        .map_err(PageCacheError::Alloc)?,
                )
            }
            ResidentBindingPin::Device(device) => MaterializedPageSnapshotPin::Device(*device),
        };
        Ok(Some(pin))
    }

    #[cfg(test)]
    pub(crate) fn binding_evidence_count(&self) -> usize {
        usize::from(self.materialization.lock().is_some())
    }

    #[cfg(test)]
    pub(crate) fn binding_is_device(&self) -> bool {
        matches!(
            self.materialization.lock().as_ref(),
            Some(ResidentBindingPin::Device(_))
        )
    }
}

impl From<PageCachePin> for ResidentBindingPin {
    fn from(pin: PageCachePin) -> Self {
        match pin {
            PageCachePin::Allocated(pin) => Self::Allocated(pin),
            PageCachePin::Device(frame) => Self::Device(frame),
        }
    }
}

const RESIDENT_RADIX_BITS: usize = 4;
const RESIDENT_RADIX_FANOUT: usize = 1 << RESIDENT_RADIX_BITS;
const RESIDENT_RADIX_MAX_LEVELS: usize = u64::BITS as usize / RESIDENT_RADIX_BITS;

/// One structurally shared node in the immutable resident-page radix tree.
///
/// A BTreeMap snapshot made every publication proportional to the number of
/// pages already resident. Sequentially faulting an N-page executable then
/// cloned 1 + 2 + ... + N entries. This adaptive-depth tree copies only the
/// significant nibbles on the modified key's path; all untouched subtrees
/// remain shared with the previous epoch-published root. In particular, the
/// ordinary low file offsets used by exec and Cargo no longer allocate and
/// clone seventeen radix nodes for every installed 4 KiB page.
#[derive(Clone, Debug)]
struct ResidentRadixNode {
    children: [Option<Arc<ResidentRadixNode>>; RESIDENT_RADIX_FANOUT],
    cell: Option<Arc<ResidentCell>>,
}

impl ResidentRadixNode {
    fn empty() -> Self {
        Self {
            children: core::array::from_fn(|_| None),
            cell: None,
        }
    }

    fn child_index(key: u64, level: usize, levels: usize) -> usize {
        debug_assert!(level < levels);
        let shift = (levels - level - 1) * RESIDENT_RADIX_BITS;
        ((key >> shift) & (RESIDENT_RADIX_FANOUT as u64 - 1)) as usize
    }

    fn lookup(&self, key: u64, level: usize, levels: usize) -> Option<&ResidentCell> {
        if level == levels {
            return self.cell.as_deref();
        }
        let index = Self::child_index(key, level, levels);
        self.children[index]
            .as_deref()?
            .lookup(key, level + 1, levels)
    }

    fn insert(
        current: Option<&Arc<Self>>,
        key: u64,
        level: usize,
        levels: usize,
        cell: Arc<ResidentCell>,
    ) -> (Arc<Self>, bool) {
        let mut next = current
            .map(|node| node.as_ref().clone())
            .unwrap_or_else(Self::empty);
        if level == levels {
            let replaced = next.cell.replace(cell).is_some();
            return (Arc::new(next), replaced);
        }
        let index = Self::child_index(key, level, levels);
        let (child, replaced) =
            Self::insert(next.children[index].as_ref(), key, level + 1, levels, cell);
        next.children[index] = Some(child);
        (Arc::new(next), replaced)
    }

    fn remove(
        current: Option<&Arc<Self>>,
        key: u64,
        level: usize,
        levels: usize,
    ) -> (Option<Arc<Self>>, bool) {
        let Some(current) = current else {
            return (None, false);
        };
        let mut next = current.as_ref().clone();
        let removed = if level == levels {
            next.cell.take().is_some()
        } else {
            let index = Self::child_index(key, level, levels);
            let (child, removed) =
                Self::remove(next.children[index].as_ref(), key, level + 1, levels);
            if !removed {
                return (Some(Arc::clone(current)), false);
            }
            next.children[index] = child;
            removed
        };
        if next.cell.is_none() && next.children.iter().all(Option::is_none) {
            (None, removed)
        } else {
            (Some(Arc::new(next)), removed)
        }
    }
}

/// Immutable, epoch-published projection of the current resident bindings.
/// Mutable fetch, writeback, range, and slot state remains outside this root.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResidentRoot {
    root: Option<Arc<ResidentRadixNode>>,
    levels: usize,
    len: usize,
}

impl ResidentRoot {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn lookup(&self, page: super::PageIndex) -> Option<&ResidentCell> {
        let key = page.as_u64();
        // A root can intentionally lag the lock-backed authority when EBR
        // publication is backpressured.  Do not truncate a key that needs
        // more nibbles than this published root represents: page 0x10 would
        // otherwise alias page 0x00 in a one-level root, page 0x100 would
        // alias page 0x000 in a two-level root, and so on.
        if resident_radix_levels(key) > self.levels {
            return None;
        }
        self.root.as_deref()?.lookup(key, 0, self.levels)
    }

    pub(crate) fn with_cell(mut self, page: super::PageIndex, cell: Arc<ResidentCell>) -> Self {
        let required_levels = resident_radix_levels(page.as_u64());
        while self.levels < required_levels {
            let mut expanded = ResidentRadixNode::empty();
            expanded.children[0] = self.root.take();
            self.root = Some(Arc::new(expanded));
            self.levels += 1;
        }
        let (root, replaced) =
            ResidentRadixNode::insert(self.root.as_ref(), page.as_u64(), 0, self.levels, cell);
        self.root = Some(root);
        if !replaced {
            self.len = self.len.saturating_add(1);
        }
        self
    }

    pub(crate) fn without_cell(mut self, page: super::PageIndex) -> Self {
        if resident_radix_levels(page.as_u64()) > self.levels {
            return self;
        }
        let (root, removed) =
            ResidentRadixNode::remove(self.root.as_ref(), page.as_u64(), 0, self.levels);
        self.root = root;
        if removed {
            self.len = self.len.saturating_sub(1);
        }
        while self.levels > 1 {
            let Some(root) = self.root.as_ref() else {
                break;
            };
            if root.cell.is_some()
                || root.children[1..].iter().any(Option::is_some)
                || root.children[0].is_none()
            {
                break;
            }
            self.root = root.children[0].clone();
            self.levels -= 1;
        }
        if self.root.is_none() {
            self.levels = 0;
        }
        self
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(crate) fn levels(&self) -> usize {
        self.levels
    }
}

fn resident_radix_levels(key: u64) -> usize {
    let significant_bits = u64::BITS as usize - key.leading_zeros() as usize;
    significant_bits
        .saturating_add(RESIDENT_RADIX_BITS - 1)
        .checked_div(RESIDENT_RADIX_BITS)
        .unwrap_or(0)
        .clamp(1, RESIDENT_RADIX_MAX_LEVELS)
}

// A root shares exactly the pin evidence already carried by PageContainer;
// publication exposes no mutable access to that evidence.
unsafe impl Send for ResidentRoot {}

/// A resident binding observed from one immutable root under an epoch guard.
pub(crate) struct ResidentHit<'g> {
    cell: &'g ResidentCell,
}

impl<'g> ResidentHit<'g> {
    pub(crate) fn from_root(
        root: &'g ResidentRoot,
        _guard: &'g Guard<'_>,
        page: super::PageIndex,
    ) -> Option<Self> {
        root.lookup(page).map(|cell| Self { cell })
    }

    pub(crate) const fn ppn(&self) -> Ppn {
        self.cell.ppn()
    }

    pub(crate) fn cell(&self) -> &'g ResidentCell {
        self.cell
    }

    pub(crate) fn try_materialize(
        &self,
    ) -> Result<Option<(MaterializedPagePin, bool)>, PageCacheError> {
        self.cell.try_materialize()
    }
}
