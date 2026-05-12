//! AddressSpace identity and read accessors.

use alloc::vec::Vec;
use tx_hal::PmapIf;
use tx_substrate::epoch;
use tx_substrate::zone::{Cap, Zone, ZoneAllocated};

#[cfg(test)]
use crate::vm::pmap::TestPmap;
use crate::vm::pmap::VmPmap;
use crate::vm::VmPmapError;

use super::{
    AddressSpaceStats, AddressSpaceStatsCell, RangeLock, RecipeIndex, UfdRegistration, UserRange,
    UserVirtAddr, VmEntry, VmMapCommit, VmMapError,
};
use crate::vm::adapter::step_engine::{self as step_engine};

static ADDRESS_SPACE_ZONE: Zone<AddressSpace> = Zone::const_new();

unsafe impl ZoneAllocated for AddressSpace {
    fn zone() -> &'static Zone<Self> {
        &ADDRESS_SPACE_ZONE
    }
}

// AddressSpace contains substrate `MapPin` tokens (via `VmPmap` →
// `PmapMapping` → `MaterializedPagePin`) that are deliberately !Send
// to enforce per-CPU pinning at the page-allocator level. The
// AddressSpace as a whole is still safe to share across CPUs under
// the kernel's epoch + pmap discipline: external access goes through
// the zone slot via `Cap<AddressSpace>` and is guarded by
// `tx_substrate::epoch::guard`. The Send/Sync impls here lift the
// stricter token-level !Send into a kernel-level shared-by-discipline
// shape so `Cap<AddressSpace>` can flow through `ProcessPayload`,
// which is itself shared by `Cap<ProcessIdentity>` references.
unsafe impl Send for AddressSpace {}
unsafe impl Sync for AddressSpace {}

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
        let reservation = step_engine::reserve_for::<AddressSpace>()?;
        Ok(step_engine::sign_for(reservation, Self::new_for_platform::<P>()?))
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

    /// Activate the platform pmap root owned by this address space.
    ///
    /// This is the VM-facing boundary used by the future ThreadRuntime
    /// userspace-entry path before it calls the HAL's return-to-user primitive.
    pub fn activate_pmap(&self) -> Result<(), VmPmapError> {
        self.pmap.activate()
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

    /// Stamp a [`UfdRegistration`] tag on every VMA whose range is
    /// fully contained in `range` (PR-10 phase 3).
    ///
    /// The Linux `UFFDIO_REGISTER` semantic this method backs:
    /// - `range` must be page-aligned and non-empty (the caller — the
    ///   shim's `step_uffdio_register` — pre-validates this against
    ///   the user-VA-range invariant).
    /// - Every byte of `range` must be mapped. Holes fail with
    ///   `Err(VmMapError::MissingMapping)` which the shim maps to
    ///   `-EINVAL`.
    /// - Every overlapping VMA must be fully contained in `range`.
    ///   Phase 3 does **not** split VMAs to register a sub-range; the
    ///   shim is expected to register whole VMAs only.
    ///
    /// On success returns a [`VmMapCommit`] whose `changed_pages` is
    /// the page count tagged. The recipe tree's atomic publish ensures
    /// readers observe either the pre-tag or post-tag state.
    pub fn tag_ufd_registration(
        &self,
        range: UserRange,
        tag: UfdRegistration,
    ) -> Result<VmMapCommit, VmMapError> {
        self.recipes.tag_ufd_registration(range, tag)
    }

    /// Drop every materialized PTE in the AddressSpace by tearing down each
    /// recipe range through `VmPmap::teardown_range`. Used by exec to
    /// reset the AS before the new image's mappings install. Returns the
    /// total number of pages torn down across all entries.
    ///
    /// Recipe entries are not removed; the caller is expected to discard
    /// the AddressSpace (Drop frees the recipe tree) or follow up with
    /// recipe withdrawals. Self is borrowed `&` because the pmap mutation
    /// takes its own internal lock.
    pub fn teardown_all_pmap(&self) -> usize {
        let entries = self.recipes_snapshot();
        let mut torn = 0usize;
        for entry in entries {
            if let Ok(count) = self.pmap.teardown_range(entry.range) {
                torn += count;
            }
        }
        torn
    }
}

#[cfg(test)]
impl Default for AddressSpace {
    fn default() -> Self {
        Self::new()
    }
}
