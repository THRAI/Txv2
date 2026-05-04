//! VM mutating and step-like execution entrypoints.
//!
//! These methods consume or create structure-side evidence, reserve declared
//! ranges, commit recipe changes, and publish pmap materializations. The
//! broader syscall scripts still live outside the VM subsystem.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::execution::{Guard, StepOutcome};
use crate::page_backed::{step_fsync, PageContainerKind};
use crate::vm::checks::{
    require_disjoint_remap, require_fault_publication, require_fault_recipe, require_map_admission,
};
use crate::vm::{
    AcquirePairResult, AcquireResult, AddressSpace, LockMode, MapPlacement, PmapPublishOutcome,
    Prot, RangeGuard, UserRange, VmBacking, VmEntry, VmFault, VmFaultError, VmFaultMaterialization,
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

        let _entry = require_fault_publication(self, &outcome, &materialization)?;

        self.pmap
            .publish_page_with_replacement(
                outcome.page_range.start().containing_page(),
                materialization.page.ppn,
                materialization.publish_prot,
                materialization.page.map_pin,
                materialization.replace_existing,
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

    /// Async wrapper around `map_script` that yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel. Honors VM_v1_2 §3.6 cross-async-wait discipline by dropping
    /// every reservation and observation before each `.await`.
    pub async fn map_script_async(
        &self,
        request: VmMapRequest,
    ) -> Result<VmMapOutcome, VmMapError> {
        loop {
            let (range, placement) = match request.target {
                VmMapTarget::Anywhere { window, page_count } => {
                    let range = self
                        .find_free_range(window, page_count)
                        .ok_or(VmMapError::NoFreeRange)?;
                    (range, MapPlacement::RequireFree)
                }
                VmMapTarget::Fixed { range, placement } => (range, placement),
            };
            let entry = VmEntry::new(range, request.prot, request.flags, request.backing.clone());

            match self.reserve_map(entry, placement) {
                MapReserveResult::Reserved(reservation) => {
                    let commit = reservation.commit()?;
                    return Ok(VmMapOutcome { range, commit });
                }
                MapReserveResult::WouldBlock(blocked) => {
                    let token = blocked.wait_token();
                    drop(blocked);
                    if let Some(future) = crate::wait_carrier::wait_on_token(token) {
                        let _ = future.await;
                    }
                    // continue loop to retry
                }
                MapReserveResult::Err(error) => return Err(error),
            }
        }
    }

    /// Async wrapper around `unmap` that yields on `RangeLock` `WouldBlock`
    /// and retries after a release wakes the lock's wait channel. Honors
    /// VM_v1_2 §3.6 by dropping the blocked guard before each `.await`.
    pub async fn unmap_async(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        loop {
            let _guard = match self.range_lock.acquire(range, LockMode::ExclusiveWriter) {
                AcquireResult::Acquired(guard) => guard,
                AcquireResult::WouldBlock(blocked) => {
                    let token = blocked.wait_token();
                    drop(blocked);
                    if let Some(future) = crate::wait_carrier::wait_on_token(token) {
                        let _ = future.await;
                    }
                    continue;
                }
            };
            let commit = self.recipes.unmap(range)?;
            self.pmap.teardown_range(range)?;
            let guard = tx_substrate::epoch::guard();
            self.stats.store(self.recipes.stats(&guard));
            return Ok(commit);
        }
    }

    /// Async wrapper around `protect` that yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel.
    pub async fn protect_async(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        loop {
            let _guard = match self.range_lock.acquire(range, LockMode::ExclusiveWriter) {
                AcquireResult::Acquired(guard) => guard,
                AcquireResult::WouldBlock(blocked) => {
                    let token = blocked.wait_token();
                    drop(blocked);
                    if let Some(future) = crate::wait_carrier::wait_on_token(token) {
                        let _ = future.await;
                    }
                    continue;
                }
            };
            let commit = self.recipes.protect(range, prot)?;
            self.pmap.teardown_range(range)?;
            let guard = tx_substrate::epoch::guard();
            self.stats.store(self.recipes.stats(&guard));
            return Ok(commit);
        }
    }

    /// Async wrapper around `remap_script` that yields on `RangeLock`
    /// pair `WouldBlock` and retries after either covered range's release
    /// wakes the lock's wait channel.
    pub async fn remap_async(&self, request: VmRemapRequest) -> Result<VmRemapOutcome, VmMapError> {
        require_disjoint_remap(request.old_range, request.new_range)?;
        loop {
            let _guard_pair = match self.range_lock.acquire_pair(
                (request.old_range, LockMode::ExclusiveWriter),
                (request.new_range, LockMode::ExclusiveWriter),
            ) {
                AcquirePairResult::Acquired(pair) => pair,
                AcquirePairResult::WouldBlock(blocked) => {
                    let token = blocked.wait_token();
                    drop(blocked);
                    if let Some(future) = crate::wait_carrier::wait_on_token(token) {
                        let _ = future.await;
                    }
                    continue;
                }
            };
            let commit = self
                .recipes
                .remap_disjoint(request.old_range, request.new_range)?;
            self.pmap.teardown_range(request.old_range)?;
            let guard = tx_substrate::epoch::guard();
            self.stats.store(self.recipes.stats(&guard));
            return Ok(VmRemapOutcome {
                old_range: request.old_range,
                new_range: request.new_range,
                commit,
            });
        }
    }

    /// Async brk script: grow or shrink the program break of an Anon
    /// mapping anchored at `brk_base`.
    ///
    /// VM_v1_2 §5.8. The current implementation models brk as a single
    /// `VmBacking::PrivateAnon` mapping whose extent is
    /// `[brk_base, current_brk)`. The script:
    ///
    /// - returns `Ok(current_brk)` when `requested_brk == current_brk`.
    /// - rejects with `InvalidRange` when `requested_brk < brk_base`.
    /// - on grow, calls `map_script_async` to map the page-aligned range
    ///   `[current_brk, requested_brk)`.
    /// - on shrink, calls `unmap_async` on `[requested_brk, current_brk)`.
    ///
    /// All three of `brk_base`, `current_brk`, and `requested_brk` must be
    /// page-aligned. Process-level tracking of the brk value (which hart
    /// holds it, exec-time base, fork inheritance) lives in the Process
    /// subsystem and is out of scope for VM.
    pub async fn brk_script_async(
        &self,
        brk_base: crate::vm::UserVirtAddr,
        current_brk: crate::vm::UserVirtAddr,
        requested_brk: crate::vm::UserVirtAddr,
    ) -> Result<crate::vm::UserVirtAddr, VmMapError> {
        if requested_brk.0 < brk_base.0 {
            return Err(VmMapError::InvalidRange);
        }
        if requested_brk.0 == current_brk.0 {
            return Ok(current_brk);
        }
        if requested_brk.0 > current_brk.0 {
            let len = requested_brk.0 - current_brk.0;
            let range =
                UserRange::new_aligned(current_brk, len).map_err(|_| VmMapError::InvalidRange)?;
            let request = VmMapRequest::fixed(
                range,
                MapPlacement::RequireFree,
                Prot::READ_WRITE,
                crate::vm::VmEntryFlags::PRIVATE,
                VmBacking::PrivateAnon,
            );
            self.map_script_async(request).await?;
        } else {
            let len = current_brk.0 - requested_brk.0;
            let range =
                UserRange::new_aligned(requested_brk, len).map_err(|_| VmMapError::InvalidRange)?;
            self.unmap_async(range).await?;
        }
        Ok(requested_brk)
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
        let guard = tx_substrate::epoch::guard();
        self.stats.store(self.recipes.stats(&guard));
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
        let guard = tx_substrate::epoch::guard();
        self.stats.store(self.recipes.stats(&guard));
        Ok(commit)
    }

    pub fn protect(&self, range: UserRange, prot: Prot) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.protect(range, prot)?;
        self.pmap.teardown_range(range)?;
        let guard = tx_substrate::epoch::guard();
        self.stats.store(self.recipes.stats(&guard));
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
        let guard = tx_substrate::epoch::guard();
        self.stats.store(self.recipes.stats(&guard));
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

/// Hint values passed to `AddressSpace::madvise`.
///
/// V1 keeps madvise observation-only. `WillNeed` is a no-op per VM_v1_2 §9.7;
/// `DontNeed` is a no-op while reclaim is deferred (PAGE_BACKED_v1 §8); the
/// remaining advice values are accepted without behavior changes. The
/// surface exists so callers and future syscall wrappers can compile against
/// the documented spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MadviseAdvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
}

impl AddressSpace {
    /// Returns one boolean per page in `range`: `true` if the page currently
    /// has a published pmap entry, `false` otherwise. The boolean at index
    /// `i` corresponds to `range`'s page at offset `i`.
    ///
    /// V1 implementation reads the snapshot returned by `VmPmap::walk_range`;
    /// concurrent publishes after the call returns are not reflected.
    pub fn mincore(&self, range: UserRange) -> Vec<bool> {
        let walked = self.pmap().walk_range(range);
        let mut walked_iter = walked.into_iter().peekable();
        range
            .iter_pages()
            .map(|page| match walked_iter.peek() {
                Some((p, _)) if *p == page => {
                    walked_iter.next();
                    true
                }
                _ => false,
            })
            .collect()
    }

    /// Records madvise advice for `range`. V1 is observation-only per VM_v1_2
    /// §9.7; the advice does not yet drive readahead, eviction, or layout
    /// changes. Returns `Ok(())` for any well-formed `range`.
    pub fn madvise(&self, range: UserRange, advice: MadviseAdvice) -> Result<(), VmMapError> {
        let _ = (range, advice);
        Ok(())
    }

    /// Synchronously flushes dirty pages of any File-backed `PageContainer`
    /// intersecting `range` by calling `page_backed::step_fsync` per unique
    /// PC. Anon, PrivateAnon, Device, and `None` backings are no-op. Returns
    /// the first non-`Done` outcome from any underlying fsync; `Done(())` if
    /// every visited PC flushed cleanly.
    pub fn msync(&self, range: UserRange, guard: &Guard<'_>) -> StepOutcome<()> {
        let entries = self.recipes.snapshot(guard);
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        for entry in entries {
            if !entry.range.overlaps(range) {
                continue;
            }
            let VmBacking::Page { pc, .. } = &entry.backing else {
                continue;
            };
            if !matches!(pc.kind(), PageContainerKind::File { .. }) {
                continue;
            }
            if !visited.insert(pc.raw()) {
                continue;
            }
            match step_fsync(pc, guard) {
                StepOutcome::Done(()) => continue,
                other => return other,
            }
        }
        StepOutcome::Done(())
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
