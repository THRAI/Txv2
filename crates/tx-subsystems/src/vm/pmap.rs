use crate::vm::adapter::step_engine::{SpinMutex, ZoneError};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
#[cfg(test)]
use std::sync::{LazyLock, Mutex};
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot,
    Ppn, VirtAddr,
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

pub struct VmPmap {
    root: Option<PmapRoot>,
    ops: VmPmapOps,
    state: SpinMutex<VmPmapState>,
}

impl VmPmap {
    pub fn new_for_platform<P: PmapIf>() -> Result<Self, VmPmapError> {
        let root = P::create_pmap_root()?;
        Ok(Self {
            root: Some(root),
            ops: VmPmapOps::for_platform::<P>(),
            state: SpinMutex::new(VmPmapState::new()),
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
        range
            .iter_pages()
            .filter_map(|page| state.mappings.get(&page).map(|m| (page, m.snapshot())))
            .collect()
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
        state
            .mappings
            .insert(page, PmapMapping::new(ppn, prot, map_pin));
        state.commits += 1;
        Ok(PmapPublishOutcome { page, replaced })
    }

    /// Tears down every published pmap entry in `range`, releasing the
    /// associated `MapPin` and issuing the ASID-scoped shootdown.
    ///
    /// Used by `unmap` to remove mappings entirely, and by `mprotect` /
    /// fork CoW demotion to demote permissions through tear-down + refault
    /// (per VM_v1_2 §9.8: in-place PTE permission patching is deferred). The
    /// next access to the affected pages refaults, observes the new recipe
    /// protection, and republishes with the demoted permissions.
    pub fn teardown_range(&self, range: UserRange) -> Result<usize, VmPmapError> {
        let mut removed = 0;

        for page in range.iter_pages() {
            let Some(mapping) = self.state.lock().mappings.remove(&page) else {
                continue;
            };

            let result = match self.unmap_tracked_page(page, mapping.ppn) {
                Ok(result) => result,
                Err(error) => {
                    self.state.lock().mappings.insert(page, mapping);
                    return Err(error);
                }
            };
            self.issue_single_unmap_result(result, mapping.into_pin());
            self.state.lock().shootdowns += 1;
            removed += 1;
        }
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

        for page in range.iter_pages() {
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
}

impl Drop for VmPmap {
    fn drop(&mut self) {
        let pages: alloc::vec::Vec<UserPage> = self.state.lock().mappings.keys().copied().collect();
        for page in pages {
            let Ok(range) = UserRange::containing_page(page.start_addr()) else {
                continue;
            };
            if self.teardown_range(range).is_err() {
                let leaked = core::mem::take(&mut self.state.lock().mappings);
                core::mem::forget(leaked);
                break;
            }
        }

        if let Some(root) = self.root.take() {
            (self.ops.destroy_root)(root);
        }
    }
}

#[derive(Debug)]
struct VmPmapState {
    mappings: BTreeMap<UserPage, PmapMapping>,
    reservations: usize,
    commits: usize,
    rollbacks: usize,
    shootdowns: usize,
}

impl VmPmapState {
    fn new() -> Self {
        Self {
            mappings: BTreeMap::new(),
            reservations: 0,
            commits: 0,
            rollbacks: 0,
            shootdowns: 0,
        }
    }
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
