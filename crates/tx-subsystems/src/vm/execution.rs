//! VM mutating and step-like execution entrypoints.
//!
//! These methods consume or create structure-side evidence, reserve declared
//! ranges, commit recipe changes, and publish pmap materializations. The
//! broader syscall scripts still live outside the VM subsystem.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use tx_hal::PmapIf;

use crate::execution::Guard;
use crate::execution::WaitToken;
use tx_substrate::step_v3::{StepOutcome as V3StepOutcome, YieldShape};
use crate::page_backed::{step_fsync, PageContainerKind};
use crate::vm::checks::{
    require_disjoint_remap, require_fault_publication, require_fault_recipe, require_map_admission,
};
use crate::vm::{
    AddressSpace, LockMode, MapPlacement, PmapPublishOutcome, Prot, RangeGuard, UserRange,
    VmBacking, VmEntry, VmFault, VmFaultError, VmFaultMaterialization, VmFaultOutcome, VmMapCommit,
    VmMapError, VmMapOutcome, VmMapRequest, VmMapTarget, VmRemapOutcome, VmRemapRequest,
};

impl AddressSpace {
    pub fn resolve_fault(&self, fault: VmFault) -> Result<VmFaultOutcome, VmFaultError> {
        let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
        let V3StepOutcome::Done(_guard) = self
            .range_lock
            .acquire_step(page_range, LockMode::Materializer)
        else {
            return Err(VmFaultError::WouldBlock);
        };

        require_fault_recipe(self, fault)
    }

    pub fn publish_fault_materialization(
        &self,
        outcome: VmFaultOutcome,
        materialization: VmFaultMaterialization,
    ) -> Result<PmapPublishOutcome, VmFaultError> {
        let V3StepOutcome::Done(_guard) = self
            .range_lock
            .acquire_step(outcome.page_range, LockMode::Materializer)
        else {
            return Err(VmFaultError::WouldBlock);
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

    /// Duplicate `parent`'s recipes into a fresh child `AddressSpace` and
    /// demote `parent`'s MAP_PRIVATE PTEs so subsequent writes refault and
    /// CoW.
    ///
    /// VM_v1_2 §5.6. The child is constructed via the same platform pmap
    /// type as `parent`. Each parent recipe is cloned (a `Cap<PageContainer>`
    /// clone is a refcount bump) and committed into the child's recipe
    /// index via `RecipeIndex::commit_map(_, RequireFree)`. For every
    /// MAP_PRIVATE entry, the parent's pmap range is torn down so the
    /// next access on either side refaults and the existing
    /// `materialize_page_recipe` CoW path produces a private frame for the
    /// writer. MAP_SHARED entries leave the parent's PTEs intact; the
    /// child's pmap starts empty and rebuilds via refault.
    ///
    /// Per VM_v1_2 §9.5, fork acquires an `ExclusiveWriter` reservation on
    /// the full user range (`UserRange::full_user_v1()`) so no concurrent
    /// VM operation runs against `parent` for the duration of the call.
    /// V1 fork is intentionally serialized end to end with respect to
    /// every parent VM operation; range-by-range pmap walks are a
    /// potential v2 optimization.
    ///
    /// Returns `VmMapError::WouldBlock` if a concurrent operation already
    /// holds an overlapping reservation. Callers may retry; no async
    /// retry is built in at the VM layer because the Process subsystem
    /// orchestrates fork above this function.
    pub fn fork_aspace<P: PmapIf>(parent: &AddressSpace) -> Result<AddressSpace, VmMapError> {
        let V3StepOutcome::Done(_full_guard) = parent
            .range_lock
            .acquire_step(UserRange::full_user_v1(), LockMode::ExclusiveWriter)
        else {
            return Err(VmMapError::WouldBlock);
        };

        let parent_recipes = parent.recipes_snapshot();
        let child = AddressSpace::new_for_platform::<P>()?;

        for entry in parent_recipes {
            let private = !entry.flags.shared;
            let range = entry.range;
            child.recipes.commit_map(entry, MapPlacement::RequireFree)?;
            if private {
                let _ = parent.pmap.teardown_range(range);
            }
        }

        let guard = tx_substrate::epoch::guard();
        child.stats.store(child.recipes.stats(&guard));
        Ok(child)
    }

    /// Reset `old_aspace` for exec by tearing down every materialized PTE
    /// across all current recipes.
    ///
    /// VM_v1_2 §5.7. Caller is responsible for replacing the recipes (and
    /// for choosing whether to drop and recreate the `AddressSpace` or
    /// reuse the same instance with new content). This function only
    /// performs the pmap teardown step; recipe management belongs to the
    /// caller because the new image's recipe shape is determined by the
    /// exec image loader, which is Process-side and not yet implemented.
    /// V1 callers typically follow with `aspace.recipes` modifications
    /// that withdraw old entries and install the new image's mappings, or
    /// drop `old_aspace` entirely and use a fresh AddressSpace.
    ///
    /// Caller invariant (per VM §5.7 prologue): exec has already reduced
    /// the thread group to one and no concurrent VM operations exist on
    /// `old_aspace`.
    ///
    /// Returns the count of pages torn down for observability.
    pub fn exec_aspace(old_aspace: &AddressSpace) -> usize {
        old_aspace.teardown_all_pmap()
    }

    /// Async wrapper around `resolve_fault` + `materialize_pagebacked` +
    /// `publish_fault_materialization` that yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel.
    ///
    /// VM_v1_2 §5.1 + §3.6 cross-async-wait discipline. Each iteration:
    ///
    /// 1. Acquires a Materializer reservation, observes the recipe, and
    ///    drops the reservation. On WouldBlock the future awaits on the
    ///    `RangeLock`'s wait channel and retries.
    /// 2. Materializes the page through `materialize_pagebacked`. PC-side
    ///    Blocked outcomes (File-variant `step_fsync` / FsPageBacking)
    ///    are not yet exposed through this script — they remain a
    ///    follow-up that requires per-`PageContainer` wait channels.
    /// 3. Re-acquires the Materializer reservation and publishes the
    ///    materialization through the pmap. WouldBlock here drops the
    ///    materialization (releasing the MapPin) and retries from
    ///    step 1, re-observing the recipe afresh.
    pub async fn fault_script(&self, fault: VmFault) -> Result<PmapPublishOutcome, VmFaultError> {
        let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
        loop {
            let outcome = {
                let _guard = match self
                    .range_lock
                    .acquire_step(page_range, LockMode::Materializer)
                {
                    V3StepOutcome::Done(guard) => guard,
                    V3StepOutcome::Yield {
                        shape: YieldShape::OnCarrier { carrier, interests },
                        ..
                    } => {
                        await_range_lock(WaitToken::new(carrier.raw(), interests.raw())).await;
                        continue;
                    }
                    _ => unreachable_acquire_step(),
                };
                require_fault_recipe(self, fault)?
            };

            let materialization = outcome.materialize_pagebacked()?;

            let _guard = match self
                .range_lock
                .acquire_step(outcome.page_range, LockMode::Materializer)
            {
                V3StepOutcome::Done(guard) => guard,
                V3StepOutcome::Yield {
                    shape: YieldShape::OnCarrier { carrier, interests },
                    ..
                } => {
                    drop(materialization);
                    await_range_lock(WaitToken::new(carrier.raw(), interests.raw())).await;
                    continue;
                }
                _ => unreachable_acquire_step(),
            };
            let _entry = require_fault_publication(self, &outcome, &materialization)?;
            return self
                .pmap
                .publish_page_with_replacement(
                    outcome.page_range.start().containing_page(),
                    materialization.page.ppn,
                    materialization.publish_prot,
                    materialization.page.map_pin,
                    materialization.replace_existing,
                )
                .map_err(VmFaultError::Pmap);
        }
    }

    pub fn try_mmap(&self, request: VmMapRequest) -> Result<VmMapOutcome, VmMapError> {
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
            MapReserveResult::Blocked(_) => Err(VmMapError::WouldBlock),
            MapReserveResult::Err(error) => Err(error),
        }
    }

    /// Canonical async mmap script per VM_v1_2 §5.2. Yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel. Honors VM_v1_2 §3.6 cross-async-wait discipline by dropping
    /// every reservation and observation before each `.await`.
    pub async fn mmap_script(&self, request: VmMapRequest) -> Result<VmMapOutcome, VmMapError> {
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
                MapReserveResult::Blocked(token) => {
                    await_range_lock(token).await;
                    // continue loop to retry
                }
                MapReserveResult::Err(error) => return Err(error),
            }
        }
    }

    /// Canonical async munmap script per VM_v1_2 §5.3. Yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel. Honors VM_v1_2 §3.6 by dropping the blocked guard before
    /// each `.await`.
    pub async fn munmap_script(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        loop {
            let _guard = match self
                .range_lock
                .acquire_step(range, LockMode::ExclusiveWriter)
            {
                V3StepOutcome::Done(guard) => guard,
                V3StepOutcome::Yield {
                    shape: YieldShape::OnCarrier { carrier, interests },
                    ..
                } => {
                    await_range_lock(WaitToken::new(carrier.raw(), interests.raw())).await;
                    continue;
                }
                _ => unreachable_acquire_step(),
            };
            let commit = self.recipes.unmap(range)?;
            self.pmap.teardown_range(range)?;
            let guard = tx_substrate::epoch::guard();
            self.stats.store(self.recipes.stats(&guard));
            return Ok(commit);
        }
    }

    /// Canonical async mprotect script per VM_v1_2 §5.4. Yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait channel.
    pub async fn mprotect_script(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        loop {
            let _guard = match self
                .range_lock
                .acquire_step(range, LockMode::ExclusiveWriter)
            {
                V3StepOutcome::Done(guard) => guard,
                V3StepOutcome::Yield {
                    shape: YieldShape::OnCarrier { carrier, interests },
                    ..
                } => {
                    await_range_lock(WaitToken::new(carrier.raw(), interests.raw())).await;
                    continue;
                }
                _ => unreachable_acquire_step(),
            };
            let commit = self.recipes.protect(range, prot)?;
            self.pmap.teardown_range(range)?;
            let guard = tx_substrate::epoch::guard();
            self.stats.store(self.recipes.stats(&guard));
            return Ok(commit);
        }
    }

    /// Canonical async mremap script per VM_v1_2 §5.5. Yields on `RangeLock`
    /// pair `WouldBlock` and retries after either covered range's release
    /// wakes the lock's wait channel.
    pub async fn mremap_script(
        &self,
        request: VmRemapRequest,
    ) -> Result<VmRemapOutcome, VmMapError> {
        require_disjoint_remap(request.old_range, request.new_range)?;
        loop {
            let _guard_pair = match self.range_lock.acquire_pair_step(
                (request.old_range, LockMode::ExclusiveWriter),
                (request.new_range, LockMode::ExclusiveWriter),
            ) {
                V3StepOutcome::Done(pair) => pair,
                V3StepOutcome::Yield {
                    shape: YieldShape::OnCarrier { carrier, interests },
                    ..
                } => {
                    let token = WaitToken::new(carrier.raw(), interests.raw());
                    await_range_lock(token).await;
                    continue;
                }
                _ => unreachable_acquire_step(),
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
    /// - on grow, calls `mmap_script` to map the page-aligned range
    ///   `[current_brk, requested_brk)`.
    /// - on shrink, calls `munmap_script` on `[requested_brk, current_brk)`.
    ///
    /// All three of `brk_base`, `current_brk`, and `requested_brk` must be
    /// page-aligned. Process-level tracking of the brk value (which hart
    /// holds it, exec-time base, fork inheritance) lives in the Process
    /// subsystem and is out of scope for VM.
    pub async fn brk_script(
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
            self.mmap_script(request).await?;
        } else {
            let len = current_brk.0 - requested_brk.0;
            let range =
                UserRange::new_aligned(requested_brk, len).map_err(|_| VmMapError::InvalidRange)?;
            self.munmap_script(range).await?;
        }
        Ok(requested_brk)
    }

    pub fn try_mremap(&self, request: VmRemapRequest) -> Result<VmRemapOutcome, VmMapError> {
        require_disjoint_remap(request.old_range, request.new_range)?;

        let _guard_pair = match self.range_lock.acquire_pair_step(
            (request.old_range, LockMode::ExclusiveWriter),
            (request.new_range, LockMode::ExclusiveWriter),
        ) {
            V3StepOutcome::Done(pair) => pair,
            V3StepOutcome::Yield { .. } => return Err(VmMapError::WouldBlock),
            _ => unreachable_acquire_step(),
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
            .acquire_step(entry.range, LockMode::ExclusiveWriter)
        {
            V3StepOutcome::Done(guard) => guard,
            V3StepOutcome::Yield {
                shape: YieldShape::OnCarrier { carrier, interests },
                ..
            } => {
                return MapReserveResult::Blocked(WaitToken::new(carrier.raw(), interests.raw()));
            }
            _ => unreachable_acquire_step(),
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

    pub fn try_munmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.unmap(range)?;
        self.pmap.teardown_range(range)?;
        let guard = tx_substrate::epoch::guard();
        self.stats.store(self.recipes.stats(&guard));
        Ok(commit)
    }

    pub fn try_mprotect(&self, range: UserRange, prot: Prot) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.protect(range, prot)?;
        self.pmap.teardown_range(range)?;
        let guard = tx_substrate::epoch::guard();
        self.stats.store(self.recipes.stats(&guard));
        Ok(commit)
    }

    fn acquire_writer(&self, range: UserRange) -> Result<RangeGuard<'_>, VmMapError> {
        match self
            .range_lock
            .acquire_step(range, LockMode::ExclusiveWriter)
        {
            V3StepOutcome::Done(guard) => Ok(guard),
            V3StepOutcome::Yield { .. } => Err(VmMapError::WouldBlock),
            _ => unreachable_acquire_step(),
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
    Blocked(WaitToken),
    Err(VmMapError),
}

impl core::fmt::Debug for MapReserveResult<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reserved(_) => f.write_str("Reserved(..)"),
            Self::Blocked(token) => f.debug_tuple("Blocked").field(token).finish(),
            Self::Err(error) => f.debug_tuple("Err").field(error).finish(),
        }
    }
}

/// Hint values passed to `AddressSpace::madvise`.
///
/// Per VM_v1_2 §5.9: `WillNeed`, `Normal`, `Random`, `Sequential` are
/// observation-only no-ops (§9.7). `DontNeed` and `Free` perform a
/// range-scoped pmap teardown (mini-munmap that preserves recipes) so the
/// next access refaults clean.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MadviseAdvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
    Free,
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

    /// Apply `advice` to `range`. V1 implements:
    ///
    /// - `DontNeed` / `Free` — range-scoped pmap teardown plus shootdown,
    ///   preserving recipes (next access refaults clean). Acquires an
    ///   `ExclusiveWriter` reservation per VM_v1_2 §5.9. Returns
    ///   `VmMapError::WouldBlock` on contention; callers should retry on
    ///   the wait channel.
    /// - `WillNeed`, `Normal`, `Random`, `Sequential` — observation-only
    ///   no-ops per §9.7; readahead and layout hints are deferred.
    pub fn madvise(&self, range: UserRange, advice: MadviseAdvice) -> Result<(), VmMapError> {
        match advice {
            MadviseAdvice::DontNeed | MadviseAdvice::Free => {
                let _guard = self.acquire_writer(range)?;
                self.pmap.teardown_range(range)?;
                let guard = tx_substrate::epoch::guard();
                self.stats.store(self.recipes.stats(&guard));
                Ok(())
            }
            MadviseAdvice::Normal
            | MadviseAdvice::Random
            | MadviseAdvice::Sequential
            | MadviseAdvice::WillNeed => Ok(()),
        }
    }

    /// Synchronously flushes dirty pages of any File-backed `PageContainer`
    /// intersecting `range` by calling `page_backed::step_fsync` per unique
    /// PC. Anon, PrivateAnon, Device, and `None` backings are no-op. Returns
    /// the first non-`Done` outcome from any underlying fsync; `Done(())` if
    /// every visited PC flushed cleanly.
    pub fn msync(
        &self,
        range: UserRange,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::PageProgress> {
        use tx_substrate::step_v3::StepOutcome as V3;
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
                V3::Done(()) => continue,
                other => return other,
            }
        }
        V3::done(())
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

/// Await a `RangeLock` release after the canonical `acquire_step` returned
/// `StepOutcome::Blocked(token)`. Resolved through the global wait-carrier
/// registry; if the token's channel has been retired the await is a no-op
/// and the caller's retry loop runs immediately.
async fn await_range_lock(token: WaitToken) {
    if let Some(future) = crate::wait_carrier::wait_on_token(token) {
        let _ = future.await;
    }
}

/// `RangeLock::acquire_step` only ever produces `V3StepOutcome::Done`
/// or `V3StepOutcome::Yield { OnCarrier }`; `acquire_pair_step` only
/// ever produces `StepOutcome::Done` or `StepOutcome::Blocked`. Other
/// variants are unreachable by construction; this helper centralises
/// the panic message so the asserts stay terse at the call sites.
#[inline(always)]
fn unreachable_acquire_step() -> ! {
    unreachable!(
        "RangeLock::acquire_step / acquire_pair_step never produce \
         Continue / Yield-OnAgent / Err / Advanced / AdvancedThenBlocked"
    );
}
