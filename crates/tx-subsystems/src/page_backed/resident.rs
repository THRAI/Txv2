//! Stable resident binding ownership while the page index remains lock-backed.
//!
//! P2 deliberately leaves lookup and mutation behind `PageContainerState`.
//! The cell is nevertheless the only owner of a resident frame's binding
//! evidence, so later immutable-root publication does not have to migrate pin
//! ownership a second time.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use tx_hal::Ppn;
use tx_substrate::epoch::Guard;

use super::{
    BitmapPageAllocator, CachePin, CachedFrame, DeviceFrame, MaterializedPagePin, PageCacheError,
    PageCachePin, PageSlot, PageSlotState,
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
    resident_binding: ResidentBindingPin,
    slot: Arc<PageSlot>,
    withdrawn: AtomicBool,
    materialization: SpinMutex<()>,
}

impl ResidentCell {
    pub(crate) fn from_cached_frame(frame: CachedFrame, slot: Arc<PageSlot>) -> Self {
        Self {
            ppn: frame.ppn,
            resident_binding: frame.pin.into(),
            slot,
            withdrawn: AtomicBool::new(false),
            materialization: SpinMutex::new(()),
        }
    }

    pub(crate) const fn ppn(&self) -> Ppn {
        self.ppn
    }

    pub(crate) fn slot(&self) -> &Arc<PageSlot> {
        &self.slot
    }

    pub(crate) fn binding(&self) -> &ResidentBindingPin {
        &self.resident_binding
    }

    pub(crate) fn mark_withdrawn(&self) {
        let _materialization = self.materialization.lock();
        self.withdrawn.store(true, Ordering::Release);
    }

    fn is_current(&self) -> bool {
        if self.withdrawn.load(Ordering::Acquire) {
            return false;
        }
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
        let _materialization = self.materialization.lock();
        if !self.is_current() {
            return Ok(None);
        }
        let map_pin = match &self.resident_binding {
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

    #[cfg(test)]
    pub(crate) fn binding_evidence_count(&self) -> usize {
        1
    }

    #[cfg(test)]
    pub(crate) fn binding_is_device(&self) -> bool {
        matches!(self.resident_binding, ResidentBindingPin::Device(_))
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

/// Immutable, epoch-published projection of the current resident bindings.
/// Mutable fetch, writeback, range, and slot state remains outside this root.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResidentRoot {
    pages: BTreeMap<super::PageIndex, Arc<ResidentCell>>,
}

impl ResidentRoot {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn lookup(&self, page: super::PageIndex) -> Option<&ResidentCell> {
        self.pages.get(&page).map(Arc::as_ref)
    }

    pub(crate) fn with_cell(mut self, page: super::PageIndex, cell: Arc<ResidentCell>) -> Self {
        self.pages.insert(page, cell);
        self
    }

    pub(crate) fn without_cell(mut self, page: super::PageIndex) -> Self {
        self.pages.remove(&page);
        self
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len()
    }
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
