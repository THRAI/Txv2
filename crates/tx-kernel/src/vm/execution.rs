//! VM mutating and step-like execution entrypoints.
//!
//! These methods consume or create structure-side evidence, reserve declared
//! ranges, commit recipe changes, and publish pmap materializations. The
//! broader syscall scripts still live outside the VM subsystem.

use crate::vm::checks::{
    require_disjoint_remap, require_fault_publication, require_fault_recipe, require_map_admission,
};
use crate::vm::{
    AcquirePairResult, AcquireResult, AddressSpace, LockMode, MapPlacement, PmapPublishOutcome,
    Prot, RangeGuard, UserRange, VmEntry, VmFault, VmFaultError, VmFaultMaterialization,
    VmFaultOutcome, VmMapCommit, VmMapError, VmMapOutcome, VmMapRequest, VmMapTarget,
    VmRemapOutcome, VmRemapRequest, WouldBlock,
};

impl AddressSpace {
    pub fn resolve_fault(&self, fault: VmFault) -> Result<VmFaultOutcome, VmFaultError> {
        let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
        let _guard = match self.range_lock.acquire(page_range, LockMode::Materializer) {
            AcquireResult::Acquired(guard) => guard,
            AcquireResult::WouldBlock(_) => return Err(VmFaultError::WouldBlock),
        };

        require_fault_recipe(self, fault)
    }

    pub fn publish_fault_materialization(
        &self,
        outcome: VmFaultOutcome,
        materialization: VmFaultMaterialization,
    ) -> Result<PmapPublishOutcome, VmFaultError> {
        let _guard = match self
            .range_lock
            .acquire(outcome.page_range, LockMode::Materializer)
        {
            AcquireResult::Acquired(guard) => guard,
            AcquireResult::WouldBlock(_) => return Err(VmFaultError::WouldBlock),
        };

        let entry = require_fault_publication(self, &outcome, materialization.page_index)?;

        self.pmap
            .publish_page(
                outcome.page_range.start().containing_page(),
                materialization.page.ppn,
                entry.prot,
                materialization.page.map_pin,
            )
            .map_err(VmFaultError::Pmap)
    }

    pub fn map_script(&self, request: VmMapRequest) -> Result<VmMapOutcome, VmMapError> {
        let (range, placement) = match request.target {
            VmMapTarget::Anywhere { window, page_count } => {
                let range = self
                    .find_free_range(window, page_count)
                    .ok_or(VmMapError::NoFreeRange)?;
                (range, MapPlacement::RequireFree)
            }
            VmMapTarget::Fixed { range, placement } => (range, placement),
        };
        let entry = VmEntry::new(range, request.prot, request.flags, request.backing);

        match self.reserve_map(entry, placement) {
            MapReserveResult::Reserved(reservation) => {
                let commit = reservation.commit()?;
                Ok(VmMapOutcome { range, commit })
            }
            MapReserveResult::WouldBlock(_) => Err(VmMapError::WouldBlock),
            MapReserveResult::Err(error) => Err(error),
        }
    }

    pub fn remap_script(&self, request: VmRemapRequest) -> Result<VmRemapOutcome, VmMapError> {
        require_disjoint_remap(request.old_range, request.new_range)?;

        let _guard_pair = match self.range_lock.acquire_pair(
            (request.old_range, LockMode::ExclusiveWriter),
            (request.new_range, LockMode::ExclusiveWriter),
        ) {
            AcquirePairResult::Acquired(pair) => pair,
            AcquirePairResult::WouldBlock(_) => return Err(VmMapError::WouldBlock),
        };
        let commit = self
            .recipes
            .remap_disjoint(request.old_range, request.new_range)?;
        self.pmap.teardown_range(request.old_range)?;
        self.stats.store(self.recipes.stats());
        Ok(VmRemapOutcome {
            old_range: request.old_range,
            new_range: request.new_range,
            commit,
        })
    }

    pub fn reserve_map(&self, entry: VmEntry, placement: MapPlacement) -> MapReserveResult<'_> {
        let guard = match self
            .range_lock
            .acquire(entry.range, LockMode::ExclusiveWriter)
        {
            AcquireResult::Acquired(guard) => guard,
            AcquireResult::WouldBlock(blocked) => return MapReserveResult::WouldBlock(blocked),
        };

        if let Err(error) = require_map_admission(self, &entry, placement) {
            return MapReserveResult::Err(error);
        }

        MapReserveResult::Reserved(MapReservation {
            aspace: self,
            entry,
            placement,
            _guard: guard,
        })
    }

    pub fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.unmap(range)?;
        self.pmap.teardown_range(range)?;
        self.stats.store(self.recipes.stats());
        Ok(commit)
    }

    pub fn protect(&self, range: UserRange, prot: Prot) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.protect(range, prot)?;
        self.pmap.teardown_range(range)?;
        self.stats.store(self.recipes.stats());
        Ok(commit)
    }

    fn acquire_writer(&self, range: UserRange) -> Result<RangeGuard<'_>, VmMapError> {
        match self.range_lock.acquire(range, LockMode::ExclusiveWriter) {
            AcquireResult::Acquired(guard) => Ok(guard),
            AcquireResult::WouldBlock(_) => Err(VmMapError::WouldBlock),
        }
    }

    fn commit_reserved_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let range = entry.range;
        let commit = self.recipes.commit_map(entry, placement)?;
        if placement == MapPlacement::FixedReplace {
            self.pmap.teardown_range(range)?;
        }
        self.stats.store(self.recipes.stats());
        Ok(commit)
    }
}

pub enum MapReserveResult<'a> {
    Reserved(MapReservation<'a>),
    WouldBlock(WouldBlock<'a>),
    Err(VmMapError),
}

impl core::fmt::Debug for MapReserveResult<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reserved(_) => f.write_str("Reserved(..)"),
            Self::WouldBlock(_) => f.write_str("WouldBlock(..)"),
            Self::Err(error) => f.debug_tuple("Err").field(error).finish(),
        }
    }
}

pub struct MapReservation<'a> {
    aspace: &'a AddressSpace,
    entry: VmEntry,
    placement: MapPlacement,
    _guard: RangeGuard<'a>,
}

impl MapReservation<'_> {
    pub fn entry(&self) -> VmEntry {
        self.entry.clone()
    }

    pub fn placement(&self) -> MapPlacement {
        self.placement
    }

    pub fn commit(self) -> Result<VmMapCommit, VmMapError> {
        self.aspace.commit_reserved_map(self.entry, self.placement)
    }
}
