use crate::vm::adapter::step_engine::ZoneError;
use crate::vm::lock_metrics::{vm_spin_mutex, VmSpinMutex};
#[cfg(test)]
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{LazyLock, Mutex};
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, Ppn, VirtAddr,
};

use crate::page_backed::MaterializedPagePin;

use super::{Prot, UserPage, UserRange, USER_PAGE_SIZE};

type ReserveMappingFn = fn(
    &PmapRoot,
    VirtAddr,
    PhysAddr,
    PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError>;
type UnmapMappingFn =
    fn(&PmapRoot, VirtAddr, PmapReserveKind) -> Result<Option<tx_hal::PmapUnmapResult>, PmapError>;
type ProtectMappingFn = fn(
    &PmapRoot,
    VirtAddr,
    PmapReserveKind,
    PmapPermissions,
) -> Result<Option<tx_hal::PmapInvalidation>, PmapError>;

static PMAP_DROP_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static PMAP_BATCH_INSERT_COUNT: AtomicU64 = AtomicU64::new(0);
static PMAP_BATCH_INSERT_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static PMAP_BATCH_INSERT_MAX_NS: AtomicU64 = AtomicU64::new(0);
static PMAP_TEARDOWN_REMOVE_COUNT: AtomicU64 = AtomicU64::new(0);
static PMAP_TEARDOWN_REMOVE_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static PMAP_TEARDOWN_REMOVE_MAX_NS: AtomicU64 = AtomicU64::new(0);
static PMAP_TEARDOWN_REMOVE_SHIFTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static PMAP_TEARDOWN_REMOVE_SHIFTED_MAX: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::vm) struct PmapDebugTotals {
    pub batch_insert_count: u64,
    pub batch_insert_total_ns: u64,
    pub batch_insert_max_ns: u64,
    pub teardown_remove_count: u64,
    pub teardown_remove_total_ns: u64,
    pub teardown_remove_max_ns: u64,
    pub teardown_remove_shifted_total: u64,
    pub teardown_remove_shifted_max: u64,
}

#[derive(Clone, Copy)]
struct VmPmapOps {
    destroy_root: fn(PmapRoot),
    activate_root: fn(&PmapRoot) -> Result<(), PmapError>,
    reserve_mapping: ReserveMappingFn,
    commit_mapping: fn(&PmapRoot, PmapReservation, PmapPermissions),
    unmap_mapping: UnmapMappingFn,
    protect_mapping: ProtectMappingFn,
    shootdown_mappings: fn(Asid, &[tx_hal::PmapInvalidation]),
}

impl VmPmapOps {
    fn for_platform<P: PmapIf>() -> Self {
        Self {
            destroy_root: P::destroy_pmap_root,
            activate_root: P::activate_pmap,
            reserve_mapping: P::reserve_mapping,
            commit_mapping: P::commit_mapping,
            unmap_mapping: P::unmap_mapping,
            protect_mapping: P::protect_mapping,
            shootdown_mappings: P::shootdown_mappings,
        }
    }
}

#[derive(Debug)]
pub struct PmapMapping {
    pub ppn: Ppn,
    pub prot: Prot,
    pin: MaterializedPagePin,
}

impl PmapMapping {
    fn new(ppn: Ppn, prot: Prot, pin: MaterializedPagePin) -> Self {
        Self { ppn, prot, pin }
    }

    fn snapshot(&self) -> PmapMappingSnapshot {
        PmapMappingSnapshot {
            ppn: self.ppn,
            prot: self.prot,
        }
    }

    fn into_pin(self) -> MaterializedPagePin {
        self.pin
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PmapMappingSnapshot {
    pub ppn: Ppn,
    pub prot: Prot,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PmapStats {
    pub mapped_pages: usize,
    pub reservations: usize,
    pub commits: usize,
    pub rollbacks: usize,
    pub shootdowns: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmPmapError {
    Pmap(PmapError),
    Zone(ZoneError),
    MissingReservation,
    AlreadyMappedDrift,
    MappingMismatch,
}

impl From<PmapError> for VmPmapError {
    fn from(value: PmapError) -> Self {
        Self::Pmap(value)
    }
}

impl From<ZoneError> for VmPmapError {
    fn from(value: ZoneError) -> Self {
        Self::Zone(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PmapPublishOutcome {
    pub page: UserPage,
    pub replaced: bool,
}

pub(in crate::vm) struct PmapBatchPage {
    pub page: UserPage,
    pub ppn: Ppn,
    pub prot: Prot,
    pub map_pin: MaterializedPagePin,
}

pub struct VmPmap {
    root: Option<PmapRoot>,
    ops: VmPmapOps,
    state: VmSpinMutex<VmPmapState>,
}

impl VmPmap {
    pub fn new_for_platform<P: PmapIf>() -> Result<Self, VmPmapError> {
        let root = P::create_pmap_root()?;
        Ok(Self {
            root: Some(root),
            ops: VmPmapOps::for_platform::<P>(),
            state: vm_spin_mutex(VmPmapState::new(), b"debug.lock.vm.pmap.state"),
        })
    }

    pub const fn materialization_deferred(&self) -> bool {
        false
    }

    /// Borrow the platform's `PmapRoot` handle. Used by the thread
    /// runtime to call `PmapIf::activate_user_pmap(root)` immediately
    /// before `enter_userspace_with_context` so the MMU consults this
    /// process's per-aspace pmap on user-mode fetches/loads.
    pub fn root_handle(&self) -> &PmapRoot {
        self.root()
    }

    pub fn lookup(&self, page: UserPage) -> Option<PmapMappingSnapshot> {
        self.state
            .lock()
            .mappings
            .get(&page)
            .map(PmapMapping::snapshot)
    }

    /// Returns mapped `(page, snapshot)` tuples for every page in `range`
    /// that currently has a published pmap entry, in ascending page order.
    ///
    /// This is the read-only walk surface used by `mincore`-style enumeration
    /// and by future fork CoW demotion to discover which pages need
    /// teardown. The lock is held only for the duration of the walk; concurrent
    /// publishes after the call returns are the caller's concern.
    pub fn walk_range(&self, range: UserRange) -> Vec<(UserPage, PmapMappingSnapshot)> {
        let state = self.state.lock();
        let (start, end) = page_bounds_for_range(range);
        state.mappings.snapshots_in_range(start, end)
    }

    pub fn stats(&self) -> PmapStats {
        let state = self.state.lock();
        PmapStats {
            mapped_pages: state.mappings.len(),
            reservations: state.reservations,
            commits: state.commits,
            rollbacks: state.rollbacks,
            shootdowns: state.shootdowns,
        }
    }

    /// Install this VM pmap root on the current CPU.
    ///
    /// ThreadRuntime should call this immediately before returning to a user
    /// context owned by the surrounding AddressSpace. The HAL owns the actual
    /// ASID/root CSR writes; this layer only supplies the root captured when the
    /// AddressSpace was created for that platform.
    pub fn activate(&self) -> Result<(), VmPmapError> {
        (self.ops.activate_root)(self.root()).map_err(VmPmapError::Pmap)
    }

    pub fn publish_page(
        &self,
        page: UserPage,
        ppn: Ppn,
        prot: Prot,
        map_pin: MaterializedPagePin,
    ) -> Result<PmapPublishOutcome, VmPmapError> {
        self.publish_page_with_replacement(page, ppn, prot, map_pin, false)
    }

    pub fn publish_page_with_replacement(
        &self,
        page: UserPage,
        ppn: Ppn,
        prot: Prot,
        map_pin: MaterializedPagePin,
        replace_existing: bool,
    ) -> Result<PmapPublishOutcome, VmPmapError> {
        let virt = virt_for_page(page)?;
        let phys = phys_for_ppn(ppn)?;
        let permissions = permissions_for_prot(prot);
        let root = self.root();
        let mut state = self.state.lock();

        let mut replaced = false;
        if let Some(existing) = state.mappings.get(&page) {
            if existing.ppn == ppn && existing.prot == prot {
                return Ok(PmapPublishOutcome {
                    page,
                    replaced: false,
                });
            }
            if !replace_existing {
                return Err(VmPmapError::MappingMismatch);
            }
            let existing = state.mappings.remove(&page).expect("existing mapping");
            let result = match self.unmap_tracked_page(page, existing.ppn) {
                Ok(result) => result,
                Err(error) => {
                    state.mappings.insert(page, existing);
                    return Err(error);
                }
            };
            match existing.into_pin() {
                MaterializedPagePin::Allocated(map_pin) => {
                    self.issue_single_unmap_result(result, MaterializedPagePin::Allocated(map_pin));
                }
                MaterializedPagePin::Device(_) => {
                    (self.ops.shootdown_mappings)(self.asid(), &[result.invalidation()]);
                }
            }
            state.shootdowns += 1;
            replaced = true;
        }

        state.reservations += 1;
        let reservation =
            match (self.ops.reserve_mapping)(root, virt, phys, PmapReserveKind::Page4K) {
                Ok(Some(reservation)) => reservation,
                Ok(None) => return Err(VmPmapError::MissingReservation),
                Err(PmapError::AlreadyMapped) => return Err(VmPmapError::AlreadyMappedDrift),
                Err(error) => return Err(VmPmapError::Pmap(error)),
            };

        (self.ops.commit_mapping)(root, reservation, permissions);
        if !replaced {
            state.mappings.reserve_additional(1);
        }
        state
            .mappings
            .insert(page, PmapMapping::new(ppn, prot, map_pin));
        state.commits += 1;
        Ok(PmapPublishOutcome { page, replaced })
    }

    /// Best-effort publication for speculative contiguous prefault pages.
    ///
    /// The leading fault has already completed through the canonical
    /// single-page path. Tail prefaults may stop early on allocation,
    /// shadow-state drift, or a racing publish; dropping the remaining pins is
    /// correct because these pages are only an optimization.
    pub(in crate::vm) fn publish_new_pages_best_effort(&self, pages: Vec<PmapBatchPage>) -> usize {
        emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.pages", pages.len() as i64);
        emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 0);
        let root = self.root();
        let mut state = self.state.lock();
        emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 1);
        state.mappings.reserve_additional(pages.len());
        let mut published = 0usize;

        for page in pages {
            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 2);
            let virt = match virt_for_page(page.page) {
                Ok(virt) => virt,
                Err(_) => break,
            };
            let phys = match phys_for_ppn(page.ppn) {
                Ok(phys) => phys,
                Err(_) => break,
            };
            if let Some(existing) = state.mappings.get(&page.page) {
                if existing.ppn == page.ppn && existing.prot == page.prot {
                    continue;
                }
                break;
            }

            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 3);
            state.reservations += 1;
            let reservation =
                match (self.ops.reserve_mapping)(root, virt, phys, PmapReserveKind::Page4K) {
                    Ok(Some(reservation)) => reservation,
                    _ => break,
                };

            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 4);
            (self.ops.commit_mapping)(root, reservation, permissions_for_prot(page.prot));
            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 5);
            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.insert.phase", 0);
            let mapping = PmapMapping::new(page.ppn, page.prot, page.map_pin);
            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.insert.phase", 1);
            let insert_start_ns = if cfg!(tx_vm_pmap_metrics) && tx_observe::current().is_some() {
                Some(tx_observe::clock_now_ns())
            } else {
                None
            };
            state.mappings.insert(page.page, mapping);
            if let Some(insert_start_ns) = insert_start_ns {
                record_pmap_batch_insert_debug(
                    tx_observe::clock_now_ns().saturating_sub(insert_start_ns),
                );
            }
            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.insert.phase", 2);
            emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 6);
            state.commits += 1;
            published += 1;
        }

        emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.published", published as i64);
        emit_pmap_teardown_trace(b"debug.vm.pmap.publish_batch.phase", 7);
        published
    }

    /// Tears down every published pmap entry in `range`, releasing the
    /// associated `MapPin` and issuing the ASID-scoped shootdown. The pmap is
    /// indexed by resident pages, so sparse VMAs enumerate resident mappings
    /// in the range rather than scanning every virtual page.
    ///
    /// Used by `unmap` to remove mappings entirely, and by `mprotect` /
    /// fork CoW demotion to demote permissions through tear-down + refault
    /// (per VM_v1_2 §9.8: in-place PTE permission patching is deferred). The
    /// next access to the affected pages refaults, observes the new recipe
    /// protection, and republishes with the demoted permissions.
    pub fn teardown_range(&self, range: UserRange) -> Result<usize, VmPmapError> {
        emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 0);
        let mut removed = 0;
        let (start, end) = page_bounds_for_range(range);
        let remove_start_ns = if cfg!(tx_vm_pmap_metrics) && tx_observe::current().is_some() {
            Some(tx_observe::clock_now_ns())
        } else {
            None
        };
        let (mappings, shifted) = {
            let mut state = self.state.lock();
            state.mappings.drain_range(start, end)
        };
        if let Some(remove_start_ns) = remove_start_ns {
            record_pmap_teardown_remove_debug(
                tx_observe::clock_now_ns().saturating_sub(remove_start_ns),
                shifted,
                mappings.len(),
            );
        }
        emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 1);
        emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.pages", mappings.len() as i64);

        let mut invalidations = Vec::new();
        let mut pins = Vec::new();
        let mut mappings = mappings.into_iter();
        while let Some((page, mapping)) = mappings.next() {
            emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 2);
            emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 3);

            let result = match self.unmap_tracked_page(page, mapping.ppn) {
                Ok(result) => result,
                Err(error) => {
                    let mut state = self.state.lock();
                    state.mappings.insert(page, mapping);
                    for (remaining_page, remaining_mapping) in mappings {
                        state.mappings.insert(remaining_page, remaining_mapping);
                    }
                    drop(state);
                    self.issue_unmap_batch(&mut invalidations, &mut pins);
                    return Err(error);
                }
            };
            emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 4);
            invalidations.push(result.invalidation());
            pins.push(mapping.into_pin());
            emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 5);
            removed += 1;
        }
        self.issue_unmap_batch(&mut invalidations, &mut pins);
        emit_pmap_teardown_trace(b"debug.vm.pmap.teardown.phase", 6);
        Ok(removed)
    }

    /// Demote existing tracked mappings in `range` to `prot`.
    ///
    /// Used by fork CoW: parent private mappings must stop being writable,
    /// but keeping them mapped read-only preserves the parent's hot code,
    /// stack, and data bytes. The next write faults through the recipe and
    /// materializes an exclusive private frame.
    pub fn protect_range(&self, range: UserRange, prot: Prot) -> Result<usize, VmPmapError> {
        let permissions = permissions_for_prot(prot);
        let mut protected = 0;
        let pages = {
            let state = self.state.lock();
            mapped_pages_in_range(&state, range)
        };

        for page in pages {
            let Some(current) = self
                .state
                .lock()
                .mappings
                .get(&page)
                .map(PmapMapping::snapshot)
            else {
                continue;
            };
            if current.prot == prot {
                continue;
            }

            let virt = virt_for_page(page)?;
            let invalidation = match (self.ops.protect_mapping)(
                self.root(),
                virt,
                PmapReserveKind::Page4K,
                permissions,
            ) {
                Ok(Some(invalidation)) => invalidation,
                Ok(None) => return Err(VmPmapError::MappingMismatch),
                Err(error) => return Err(VmPmapError::Pmap(error)),
            };

            if let Some(mapping) = self.state.lock().mappings.get_mut(&page) {
                mapping.prot = prot;
            }
            (self.ops.shootdown_mappings)(self.asid(), &[invalidation]);
            self.state.lock().shootdowns += 1;
            protected += 1;
        }

        Ok(protected)
    }

    fn root(&self) -> &PmapRoot {
        self.root
            .as_ref()
            .expect("VM pmap root is unavailable during active operation")
    }

    fn asid(&self) -> Asid {
        self.root().asid()
    }

    fn unmap_tracked_page(
        &self,
        page: UserPage,
        expected_ppn: Ppn,
    ) -> Result<tx_hal::PmapUnmapResult, VmPmapError> {
        let virt = virt_for_page(page)?;
        let result = (self.ops.unmap_mapping)(self.root(), virt, PmapReserveKind::Page4K)?;
        let Some(result) = result else {
            return Err(VmPmapError::MappingMismatch);
        };
        if result.base_ppn() != expected_ppn || result.page_count() != 1 {
            return Err(VmPmapError::MappingMismatch);
        }
        Ok(result)
    }

    fn issue_single_unmap_result(&self, result: tx_hal::PmapUnmapResult, pin: MaterializedPagePin) {
        (self.ops.shootdown_mappings)(self.asid(), &[result.invalidation()]);
        if let MaterializedPagePin::Allocated(map_pin) = pin {
            drop(map_pin);
        }
    }

    fn issue_unmap_batch(
        &self,
        invalidations: &mut Vec<PmapInvalidation>,
        pins: &mut Vec<MaterializedPagePin>,
    ) {
        if invalidations.is_empty() {
            return;
        }
        emit_pmap_teardown_trace(
            b"debug.vm.pmap.teardown.invalidations",
            invalidations.len() as i64,
        );
        (self.ops.shootdown_mappings)(self.asid(), invalidations);
        self.state.lock().shootdowns += 1;
        invalidations.clear();
        pins.clear();
    }
}

impl Drop for VmPmap {
    fn drop(&mut self) {
        let mapped_pages = self.state.lock().mappings.len();
        let trace_seq = pmap_drop_trace_sample();
        if let Some(seq) = trace_seq {
            emit_pmap_teardown_trace(b"debug.vm.pmap.drop.begin", seq);
            emit_pmap_teardown_trace(b"debug.vm.pmap.drop.mapped_pages", mapped_pages as i64);
        }

        if let Some(root) = self.root.take() {
            if let Some(seq) = trace_seq {
                emit_pmap_teardown_trace(b"debug.vm.pmap.drop.destroy_root", seq);
            }
            (self.ops.destroy_root)(root);
        }

        let released = {
            let mut state = self.state.lock();
            core::mem::take(&mut state.mappings)
        };
        if let Some(_seq) = trace_seq {
            emit_pmap_teardown_trace(b"debug.vm.pmap.drop.release_pins", released.len() as i64);
        }
        drop(released);

        if let Some(seq) = trace_seq {
            emit_pmap_teardown_trace(b"debug.vm.pmap.drop.end", seq);
        }
    }
}

#[derive(Debug)]
struct VmPmapState {
    mappings: PmapResidentStore,
    reservations: usize,
    commits: usize,
    rollbacks: usize,
    shootdowns: usize,
}

impl VmPmapState {
    fn new() -> Self {
        Self {
            mappings: PmapResidentStore::new(),
            reservations: 0,
            commits: 0,
            rollbacks: 0,
            shootdowns: 0,
        }
    }
}

#[derive(Debug, Default)]
struct PmapResidentStore {
    entries: Vec<(UserPage, PmapMapping)>,
}

impl PmapResidentStore {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn reserve_additional(&mut self, additional: usize) {
        self.entries.reserve(additional);
    }

    fn get(&self, page: &UserPage) -> Option<&PmapMapping> {
        self.search(*page).ok().map(|index| &self.entries[index].1)
    }

    fn get_mut(&mut self, page: &UserPage) -> Option<&mut PmapMapping> {
        self.search(*page)
            .ok()
            .map(|index| &mut self.entries[index].1)
    }

    fn insert(&mut self, page: UserPage, mapping: PmapMapping) -> Option<PmapMapping> {
        match self.search(page) {
            Ok(index) => Some(core::mem::replace(&mut self.entries[index].1, mapping)),
            Err(index) if index == self.entries.len() => {
                self.entries.push((page, mapping));
                None
            }
            Err(index) => {
                self.entries.insert(index, (page, mapping));
                None
            }
        }
    }

    fn remove(&mut self, page: &UserPage) -> Option<PmapMapping> {
        self.remove_with_shift(page).map(|(mapping, _)| mapping)
    }

    fn remove_with_shift(&mut self, page: &UserPage) -> Option<(PmapMapping, usize)> {
        self.search(*page).ok().map(|index| {
            let shifted = self.entries.len().saturating_sub(index + 1);
            (self.entries.remove(index).1, shifted)
        })
    }

    fn drain_range(
        &mut self,
        start: UserPage,
        end: UserPage,
    ) -> (Vec<(UserPage, PmapMapping)>, usize) {
        let start_index = self.search(start).unwrap_or_else(|index| index);
        let end_index = self.search(end).unwrap_or_else(|index| index);
        if start_index >= end_index {
            return (Vec::new(), 0);
        }

        let shifted = self.entries.len().saturating_sub(end_index);
        let removed = self.entries.drain(start_index..end_index).collect();
        (removed, shifted)
    }

    fn snapshots_in_range(
        &self,
        start: UserPage,
        end: UserPage,
    ) -> Vec<(UserPage, PmapMappingSnapshot)> {
        let mut snapshots = Vec::new();
        let start_index = self.search(start).unwrap_or_else(|index| index);
        for (page, mapping) in self.entries[start_index..].iter() {
            if *page >= end {
                break;
            }
            snapshots.push((*page, mapping.snapshot()));
        }
        snapshots
    }

    fn pages_in_range(&self, start: UserPage, end: UserPage) -> Vec<UserPage> {
        let mut pages = Vec::new();
        let start_index = self.search(start).unwrap_or_else(|index| index);
        for (page, _) in self.entries[start_index..].iter() {
            if *page >= end {
                break;
            }
            pages.push(*page);
        }
        pages
    }

    fn search(&self, page: UserPage) -> Result<usize, usize> {
        self.entries
            .binary_search_by_key(&page, |(entry_page, _)| *entry_page)
    }
}

fn page_bounds_for_range(range: UserRange) -> (UserPage, UserPage) {
    (
        range.start().containing_page(),
        range.end().containing_page(),
    )
}

fn emit_pmap_teardown_trace(name: &[u8], value: i64) {
    if !cfg!(tx_vm_pmap_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
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

pub(in crate::vm) fn reset_pmap_debug_totals() {
    PMAP_BATCH_INSERT_COUNT.store(0, Ordering::Relaxed);
    PMAP_BATCH_INSERT_TOTAL_NS.store(0, Ordering::Relaxed);
    PMAP_BATCH_INSERT_MAX_NS.store(0, Ordering::Relaxed);
    PMAP_TEARDOWN_REMOVE_COUNT.store(0, Ordering::Relaxed);
    PMAP_TEARDOWN_REMOVE_TOTAL_NS.store(0, Ordering::Relaxed);
    PMAP_TEARDOWN_REMOVE_MAX_NS.store(0, Ordering::Relaxed);
    PMAP_TEARDOWN_REMOVE_SHIFTED_TOTAL.store(0, Ordering::Relaxed);
    PMAP_TEARDOWN_REMOVE_SHIFTED_MAX.store(0, Ordering::Relaxed);
}

pub(in crate::vm) fn pmap_debug_totals() -> PmapDebugTotals {
    PmapDebugTotals {
        batch_insert_count: PMAP_BATCH_INSERT_COUNT.load(Ordering::Relaxed),
        batch_insert_total_ns: PMAP_BATCH_INSERT_TOTAL_NS.load(Ordering::Relaxed),
        batch_insert_max_ns: PMAP_BATCH_INSERT_MAX_NS.load(Ordering::Relaxed),
        teardown_remove_count: PMAP_TEARDOWN_REMOVE_COUNT.load(Ordering::Relaxed),
        teardown_remove_total_ns: PMAP_TEARDOWN_REMOVE_TOTAL_NS.load(Ordering::Relaxed),
        teardown_remove_max_ns: PMAP_TEARDOWN_REMOVE_MAX_NS.load(Ordering::Relaxed),
        teardown_remove_shifted_total: PMAP_TEARDOWN_REMOVE_SHIFTED_TOTAL.load(Ordering::Relaxed),
        teardown_remove_shifted_max: PMAP_TEARDOWN_REMOVE_SHIFTED_MAX.load(Ordering::Relaxed),
    }
}

fn record_pmap_batch_insert_debug(duration_ns: u64) {
    PMAP_BATCH_INSERT_COUNT.fetch_add(1, Ordering::Relaxed);
    PMAP_BATCH_INSERT_TOTAL_NS.fetch_add(duration_ns, Ordering::Relaxed);
    atomic_max(&PMAP_BATCH_INSERT_MAX_NS, duration_ns);
}

fn record_pmap_teardown_remove_debug(duration_ns: u64, shifted: usize, removed: usize) {
    if removed == 0 {
        return;
    }
    PMAP_TEARDOWN_REMOVE_COUNT.fetch_add(removed as u64, Ordering::Relaxed);
    PMAP_TEARDOWN_REMOVE_TOTAL_NS.fetch_add(duration_ns, Ordering::Relaxed);
    atomic_max(&PMAP_TEARDOWN_REMOVE_MAX_NS, duration_ns);
    PMAP_TEARDOWN_REMOVE_SHIFTED_TOTAL.fetch_add(shifted as u64, Ordering::Relaxed);
    atomic_max(&PMAP_TEARDOWN_REMOVE_SHIFTED_MAX, shifted as u64);
}

fn pmap_drop_trace_sample() -> Option<i64> {
    let seq = PMAP_DROP_TRACE_SAMPLE.fetch_add(1, Ordering::Relaxed);
    (seq < 64 || seq.is_power_of_two()).then_some(seq as i64)
}

fn mapped_pages_in_range(state: &VmPmapState, range: UserRange) -> Vec<UserPage> {
    let (start, end) = page_bounds_for_range(range);
    state.mappings.pages_in_range(start, end)
}

fn virt_for_page(page: UserPage) -> Result<VirtAddr, VmPmapError> {
    page.checked_start_addr()
        .map(|addr| VirtAddr(addr.as_usize()))
        .map_err(|_| VmPmapError::Pmap(PmapError::InvalidRequest))
}

fn phys_for_ppn(ppn: Ppn) -> Result<PhysAddr, VmPmapError> {
    ppn.0
        .checked_mul(USER_PAGE_SIZE)
        .map(PhysAddr)
        .ok_or(VmPmapError::Pmap(PmapError::InvalidRequest))
}

fn permissions_for_prot(prot: Prot) -> PmapPermissions {
    let mut permissions = PmapPermissions::USER;
    if prot.read {
        permissions = permissions.union(PmapPermissions::READ);
    }
    if prot.write {
        permissions = permissions.union(PmapPermissions::WRITE);
    }
    if prot.execute {
        permissions = permissions.union(PmapPermissions::EXECUTE);
    }
    permissions
}

#[cfg(test)]
pub struct TestPmap;

#[cfg(test)]
struct TestPmapState {
    next_root: usize,
    mappings: BTreeMap<(usize, usize), (PhysAddr, PmapPermissions)>,
}

#[cfg(test)]
impl TestPmapState {
    fn new() -> Self {
        Self {
            next_root: 1,
            mappings: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
static TEST_PMAP_STATE: LazyLock<Mutex<TestPmapState>> =
    LazyLock::new(|| Mutex::new(TestPmapState::new()));

#[cfg(test)]
fn root_key(root: &PmapRoot) -> usize {
    root.phys().0
}

#[cfg(test)]
impl PmapIf for TestPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = TEST_PMAP_STATE.lock().expect("test pmap lock");
        let root_id = state.next_root;
        state.next_root += 1;
        Ok(PmapRoot::new(
            tx_hal::PtNode::boot_pool(PhysAddr(root_id * USER_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(root: PmapRoot) {
        let mut state = TEST_PMAP_STATE.lock().expect("test pmap lock");
        let root_key = root.phys().0;
        state
            .mappings
            .retain(|(mapped_root, _), _| *mapped_root != root_key);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        let state = TEST_PMAP_STATE.lock().expect("test pmap lock");
        if state.mappings.contains_key(&(root_key(root), virt.0)) {
            return Err(PmapError::AlreadyMapped);
        }
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

    fn commit_mapping(root: &PmapRoot, reservation: PmapReservation, permissions: PmapPermissions) {
        let mut state = TEST_PMAP_STATE.lock().expect("test pmap lock");
        state.mappings.insert(
            (root_key(root), reservation.virt().0),
            (reservation.phys(), permissions),
        );
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<tx_hal::PmapUnmapResult>, PmapError> {
        let mut state = TEST_PMAP_STATE.lock().expect("test pmap lock");
        let Some((phys, _)) = state.mappings.remove(&(root_key(root), virt.0)) else {
            return Ok(None);
        };
        Ok(Some(tx_hal::PmapUnmapResult::new(virt, phys, kind)))
    }

    fn protect_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<tx_hal::PmapInvalidation>, PmapError> {
        let mut state = TEST_PMAP_STATE.lock().expect("test pmap lock");
        let Some((_, entry_permissions)) = state.mappings.get_mut(&(root_key(root), virt.0)) else {
            return Ok(None);
        };
        *entry_permissions = permissions;
        Ok(Some(tx_hal::PmapInvalidation::new(virt, kind.size())))
    }

    fn shootdown_mapping(asid: Asid, _invalidation: tx_hal::PmapInvalidation) {
        let _ = asid;
    }
}
