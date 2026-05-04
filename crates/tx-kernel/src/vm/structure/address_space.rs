//! AddressSpace identity and read accessors.

use alloc::vec::Vec;
use tx_hal::PmapIf;
use tx_substrate::epoch;
use tx_substrate::zone::{self, Cap, Zone, ZoneAllocated};

#[cfg(test)]
use crate::vm::pmap::TestPmap;
use crate::vm::pmap::VmPmap;
use crate::vm::VmPmapError;

use super::{
    AddressSpaceStats, AddressSpaceStatsCell, RangeLock, RecipeIndex, UserRange, UserVirtAddr,
    VmEntry,
};

static ADDRESS_SPACE_ZONE: Zone<AddressSpace> = Zone::const_new();

unsafe impl ZoneAllocated for AddressSpace {
    fn zone() -> &'static Zone<Self> {
        &ADDRESS_SPACE_ZONE
    }
}

pub struct AddressSpace {
    pub(in crate::vm) recipes: RecipeIndex,
    pub(in crate::vm) pmap: VmPmap,
    pub(in crate::vm) range_lock: RangeLock,
    pub(in crate::vm) stats: AddressSpaceStatsCell,
}

impl AddressSpace {
    pub fn new_for_platform<P: PmapIf>() -> Result<Self, VmPmapError> {
        Ok(Self {
            recipes: RecipeIndex::new(),
            pmap: VmPmap::new_for_platform::<P>()?,
            range_lock: RangeLock::new(),
            stats: AddressSpaceStatsCell::new(),
        })
    }

    pub fn new_cap_for_platform<P: PmapIf>() -> Result<Cap<AddressSpace>, VmPmapError> {
        let reservation = zone::reserve_for::<AddressSpace>()?;
        Ok(zone::sign_for(reservation, Self::new_for_platform::<P>()?))
    }

    #[cfg(test)]
    pub fn new() -> Self {
        Self::new_for_platform::<TestPmap>().expect("test pmap creates root")
    }

    #[cfg(test)]
    pub fn new_cap() -> Result<Cap<AddressSpace>, VmPmapError> {
        Self::new_cap_for_platform::<TestPmap>()
    }

    pub const fn pmap(&self) -> &VmPmap {
        &self.pmap
    }

    pub const fn range_lock(&self) -> &RangeLock {
        &self.range_lock
    }

    pub fn stats(&self) -> AddressSpaceStats {
        self.stats.load()
    }

    pub fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        let guard = epoch::guard();
        self.recipes.lookup(addr, &guard)
    }

    pub fn find_free_range(&self, window: UserRange, page_count: usize) -> Option<UserRange> {
        let guard = epoch::guard();
        self.recipes.find_free_range(window, page_count, &guard)
    }

    pub fn recipes_overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        let guard = epoch::guard();
        self.recipes.overlapping(range, &guard)
    }

    pub fn recipes_snapshot(&self) -> Vec<VmEntry> {
        let guard = epoch::guard();
        self.recipes.snapshot(&guard)
    }
}

#[cfg(test)]
impl Default for AddressSpace {
    fn default() -> Self {
        Self::new()
    }
}
