//! Kernel-space pmap operations.
//!
//! This module owns direct-map extension, MMIO/kernel mapping reserve/commit,
//! in-place kernel protect, and kernel unmap/prune behavior.
//!
//! Core data structures/state maintained here:
//! - the global bootstrap root stored in `BootStaticBag<IdentityDropped>`;
//! - `BootstrapPmapInfo`, especially the direct-map range substrate consumes;
//! - `PmapReservation` intermediates for not-yet-published kernel mappings;
//! - committed PT-node registry entries for branch tables that may later prune.
//!
//! Main state modification functions:
//! - `reserve_kernel_direct_map_1g()`, `commit_kernel_direct_map_1g()`, and
//!   `extend_direct_map()` grow the permanent direct map in 1 GiB leaves.
//! - `reserve_kernel_mapping()`, `rollback_kernel_mapping()`, and
//!   `commit_kernel_mapping()` publish MMIO/direct-map leaves through the global
//!   root.
//! - `unmap_kernel_mapping()` and `protect_kernel_mapping()` clear or rewrite
//!   existing same-granularity leaves and produce invalidation evidence.
//! - `shootdown_kernel_mapping()` is the local v1 global invalidation hook.
//!
//! Helper groups:
//! - reservation helpers decide whether a slot is already mapped, empty, or
//!   conflicting;
//! - leaf helpers implement safe same-granularity unmap/protect;
//! - prune helpers release empty committed L0/L1 tables through PT-node
//!   ownership.
//!
//! It is still board-specific Sv39 code; the portable range API lives in
//! `tx_hal::pmap` and frame-accounting shootdown lives in substrate. See
//! `docs/progress/decisions/2026-04-28-rv64-direct-map-extension.md`,
//! `docs/progress/decisions/2026-04-28-rv64-mmio-pmap-reserve-commit.md`, and
//! `docs/progress/decisions/2026-04-29-pmap-kernel-protect-in-place.md`.

use tx_hal::{
    BootstrapPmapInfo, PhysAddr, PmapError, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReservationIntermediates, PmapReserveKind, PmapUnmapResult, VirtAddr,
};

use crate::boot_static::{BootStaticBag, IdentityDropped, PageTable};

use super::pt_node::{register_committed_intermediates, release_committed_pt_node_from_bag};
use super::{
    align_up, direct_map_virt, encode_kernel_mapping_leaf, encode_leaf_pte,
    encode_leaf_pte_with_permissions, ensure_l0_table_for_reservation,
    ensure_l1_table_for_reservation, l0_table_mut, l1_table_mut, page_table_mut_from_phys,
    pte_is_branch, pte_is_leaf, pte_phys, rollback_intermediates_from_bag, rv64_1g_leaf_index,
    rv64_2m_leaf_index, rv64_4k_leaf_index, sfence_vma_all, validate_aligned_mapping,
    validate_aligned_virt, validate_rv64_leaf_permissions, PAGE_SIZE, PTE_G, PTE_R, PTE_W,
    QEMU_RAM_BASE, SUPERPAGE_1G_SIZE, SUPERPAGE_2M_SIZE,
};

// Bootstrap pmap facts and direct-map extension are grouped because substrate
// consumes the facts first, then asks the board to extend the direct map before
// placing allocator metadata in RAM that was not covered by the initial 1 GiB
// bootstrap leaf.
pub(crate) fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
    BootStaticBag::<IdentityDropped>::global_ref().bootstrap_pmap_info_ref()
}

pub(crate) fn reserve_kernel_direct_map_1g(
    phys: PhysAddr,
) -> Result<Option<PmapReservation>, PmapError> {
    reserve_direct_map_1g_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), phys)
}

pub(super) fn reserve_direct_map_1g_from_bag<State>(
    bag: &BootStaticBag<State>,
    phys: PhysAddr,
) -> Result<Option<PmapReservation>, PmapError> {
    if phys.0 < QEMU_RAM_BASE || !phys.0.is_multiple_of(SUPERPAGE_1G_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    let virt = direct_map_virt(phys.0);
    let expected = encode_leaf_pte(phys, PTE_R | PTE_W | PTE_G);
    let current = unsafe { bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt)] };
    if current == expected {
        return Ok(None);
    }
    if current != 0 {
        return Err(PmapError::AlreadyMapped);
    }

    Ok(Some(PmapReservation::new(
        VirtAddr(virt),
        phys,
        PmapReserveKind::Superpage1G,
    )))
}

pub(crate) fn commit_kernel_direct_map_1g(reservation: PmapReservation) {
    commit_direct_map_1g_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), reservation);
}

pub(super) fn commit_direct_map_1g_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
) {
    assert_eq!(reservation.kind(), PmapReserveKind::Superpage1G);
    let phys = reservation.phys();
    assert_eq!(reservation.virt(), VirtAddr(direct_map_virt(phys.0)));
    assert!(phys.0.is_multiple_of(SUPERPAGE_1G_SIZE));

    let pte = encode_leaf_pte(phys, PTE_R | PTE_W | PTE_G);
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        extend_bootstrap_direct_map_info(bag, phys.0 + SUPERPAGE_1G_SIZE);
    }
    sfence_vma_all();
}

pub(crate) fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError> {
    extend_direct_map_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), phys_end)
}

pub(super) fn extend_direct_map_from_bag<State>(
    bag: &BootStaticBag<State>,
    phys_end: PhysAddr,
) -> Result<(), PmapError> {
    let current_end = direct_map_phys_end_from_bag(bag)?;
    if phys_end.0 <= current_end {
        return Ok(());
    }

    let mut phys = align_up(current_end, SUPERPAGE_1G_SIZE).ok_or(PmapError::InvalidRequest)?;
    let target = align_up(phys_end.0, SUPERPAGE_1G_SIZE).ok_or(PmapError::InvalidRequest)?;
    while phys < target {
        if let Some(reservation) = reserve_direct_map_1g_from_bag(bag, PhysAddr(phys))? {
            commit_direct_map_1g_from_bag(bag, reservation);
        }
        phys = phys
            .checked_add(SUPERPAGE_1G_SIZE)
            .ok_or(PmapError::InvalidRequest)?;
    }

    Ok(())
}

// Kernel mapping reservation mirrors process-root reservation but targets the
// bootstrap/global root. New intermediates stay attached to the reservation
// until commit, which lets rollback return PT nodes cleanly if a later step
// fails.
pub(crate) fn reserve_kernel_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    reserve_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        virt,
        phys,
        kind,
    )
}

pub(super) fn reserve_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    let expected = encode_kernel_mapping_leaf(phys);
    match kind {
        PmapReserveKind::Superpage1G => {
            validate_aligned_mapping(virt, phys, SUPERPAGE_1G_SIZE)?;
            let current = unsafe { bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
            reservation_for_slot(
                virt,
                phys,
                kind,
                current,
                expected,
                PmapReservationIntermediates::empty(),
            )
        }
        PmapReserveKind::Superpage2M => {
            validate_aligned_mapping(virt, phys, SUPERPAGE_2M_SIZE)?;
            let l1 = ensure_l1_table_for_reservation(bag, virt)?;
            let intermediates = PmapReservationIntermediates {
                l2: None,
                l1: l1.node,
                l0: None,
            };
            let current = l1.table.0[rv64_2m_leaf_index(virt.0)];
            match reservation_for_slot(virt, phys, kind, current, expected, intermediates) {
                Ok(reservation) => Ok(reservation),
                Err(error) => {
                    rollback_intermediates_from_bag(bag, virt, intermediates);
                    Err(error)
                }
            }
        }
        PmapReserveKind::Page4K => {
            validate_aligned_mapping(virt, phys, PAGE_SIZE)?;
            let l1 = ensure_l1_table_for_reservation(bag, virt)?;
            let l1_node = l1.node;
            let l0 = match ensure_l0_table_for_reservation(bag, l1.table, virt) {
                Ok(l0) => l0,
                Err(error) => {
                    rollback_intermediates_from_bag(
                        bag,
                        virt,
                        PmapReservationIntermediates {
                            l2: None,
                            l1: l1_node,
                            l0: None,
                        },
                    );
                    return Err(error);
                }
            };
            let intermediates = PmapReservationIntermediates {
                l2: None,
                l1: l1_node,
                l0: l0.node,
            };
            let current = l0.table.0[rv64_4k_leaf_index(virt.0)];
            match reservation_for_slot(virt, phys, kind, current, expected, intermediates) {
                Ok(reservation) => Ok(reservation),
                Err(error) => {
                    rollback_intermediates_from_bag(bag, virt, intermediates);
                    Err(error)
                }
            }
        }
    }
}

// Commit/rollback publish or abandon kernel mappings after reservation.
// Committed intermediates are registered so later unmap/prune can recover their
// `PtNode` authority from a raw branch PTE.
pub(crate) fn rollback_kernel_mapping(reservation: PmapReservation) {
    rollback_kernel_mapping_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), reservation);
}

pub(super) fn rollback_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
) {
    rollback_intermediates_from_bag(bag, reservation.virt(), reservation.intermediates());
    sfence_vma_all();
}

pub(crate) fn commit_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
    commit_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        reservation,
        permissions,
    );
}

pub(super) fn commit_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    validate_rv64_leaf_permissions(permissions, false).expect("valid kernel leaf permissions");
    register_committed_intermediates(reservation.intermediates());
    let pte = encode_leaf_pte_with_permissions(reservation.phys(), permissions);
    match reservation.kind() {
        PmapReserveKind::Superpage1G => unsafe {
            bag.bootstrap_root_mut().0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        },
        PmapReserveKind::Superpage2M => {
            let l1 = l1_table_mut(bag, reservation.virt()).expect("reserved L1 table must exist");
            l1.0[rv64_2m_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Page4K => {
            let l1 = l1_table_mut(bag, reservation.virt()).expect("reserved L1 table must exist");
            let l0 = l0_table_mut(l1, reservation.virt()).expect("reserved L0 table must exist");
            l0.0[rv64_4k_leaf_index(reservation.virt().0)] = pte;
        }
    }
    sfence_vma_all();
}

// Kernel unmap/protect work on existing same-granularity leaves only. Absent
// slots are no-ops, and unsafe transformations such as splitting a superpage are
// rejected so higher VM policy can rematerialize through faults later.
pub(crate) fn unmap_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    unmap_kernel_mapping_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), virt, kind)
}

pub(super) fn unmap_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    validate_aligned_virt(virt, kind.size())?;
    match kind {
        PmapReserveKind::Superpage1G => Err(PmapError::InvalidRequest),
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let result = {
                let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)?
            };
            if result.is_some() {
                prune_empty_l1_table_from_bag(bag, virt);
            }
            Ok(result)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let l1_slot = l1.0[rv64_2m_leaf_index(virt.0)];
            if l1_slot == 0 {
                return Ok(None);
            }
            if !pte_is_branch(l1_slot) {
                return Err(PmapError::InvalidRequest);
            }
            let l0 = unsafe { page_table_mut_from_phys(pte_phys(l1_slot)) };
            let result = {
                let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)?
            };
            if result.is_some() {
                prune_empty_l0_table_from_bag(bag, virt);
                prune_empty_l1_table_from_bag(bag, virt);
            }
            Ok(result)
        }
    }
}

pub(crate) fn protect_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    protect_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        virt,
        kind,
        permissions,
    )
}

pub(super) fn protect_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    validate_rv64_leaf_permissions(permissions, false)?;
    validate_aligned_virt(virt, kind.size())?;
    match kind {
        PmapReserveKind::Superpage1G => Err(PmapError::InvalidRequest),
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let l1_slot = l1.0[rv64_2m_leaf_index(virt.0)];
            if l1_slot == 0 {
                return Ok(None);
            }
            if !pte_is_branch(l1_slot) {
                return Err(PmapError::InvalidRequest);
            }
            let l0 = unsafe { page_table_mut_from_phys(pte_phys(l1_slot)) };
            let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
    }
}

pub(crate) fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {
    sfence_vma_all();
}

// Direct-map helpers keep the public extension path small: one function
// computes the mapped physical end from published HAL facts, and one updates
// those facts after the root leaf is committed.
pub(super) fn direct_map_phys_end_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<usize, PmapError> {
    let Some(info) = bag.bootstrap_pmap_info_ref() else {
        return Err(PmapError::InvalidRequest);
    };
    let phys_start = info
        .direct_map
        .start
        .0
        .checked_sub(info.direct_map_base.0)
        .ok_or(PmapError::InvalidRequest)?;
    phys_start
        .checked_add(info.direct_map.size)
        .ok_or(PmapError::InvalidRequest)
}

pub(super) fn direct_map_phys_start_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<usize, PmapError> {
    let Some(info) = bag.bootstrap_pmap_info_ref() else {
        return Err(PmapError::InvalidRequest);
    };
    info.direct_map
        .start
        .0
        .checked_sub(info.direct_map_base.0)
        .ok_or(PmapError::InvalidRequest)
}

/// Cover RAM below the bootstrap direct-map start with 1 GiB leaves.
///
/// The boot page tables only map the QEMU-virt gigabyte at
/// 0x8000_0000, but boards like the VisionFive 2 report DDR from
/// 0x4000_0000 in their device tree. Called while boot facts are
/// being published (identity still live, root writable): writes the
/// missing root leaves and lowers the published direct_map/mapped
/// ranges so the substrate coverage check and frame allocator see the
/// real span. No-op on QEMU (regions start at the current base).
pub(crate) fn cover_direct_map_low_from_bag<State>(
    bag: &BootStaticBag<State>,
    lowest_phys: PhysAddr,
) -> Result<(), PmapError> {
    let current_start = direct_map_phys_start_from_bag(bag)?;
    let new_start = lowest_phys.0 - (lowest_phys.0 % SUPERPAGE_1G_SIZE);
    if new_start >= current_start {
        return Ok(());
    }

    let mut phys = new_start;
    while phys < current_start {
        let virt = direct_map_virt(phys);
        let expected = encode_leaf_pte(PhysAddr(phys), PTE_R | PTE_W | PTE_G);
        let root = unsafe { bag.bootstrap_root_mut() };
        let slot = &mut root.0[rv64_1g_leaf_index(virt)];
        if *slot == 0 {
            *slot = expected;
        } else if *slot != expected {
            return Err(PmapError::AlreadyMapped);
        }
        phys = phys
            .checked_add(SUPERPAGE_1G_SIZE)
            .ok_or(PmapError::InvalidRequest)?;
    }

    unsafe {
        lower_bootstrap_direct_map_info(bag, new_start, current_start);
    }
    sfence_vma_all();
    Ok(())
}

unsafe fn lower_bootstrap_direct_map_info<State>(
    bag: &BootStaticBag<State>,
    new_start: usize,
    old_start: usize,
) {
    let grown = old_start - new_start;
    unsafe {
        let Some(info) = bag.bootstrap_pmap_info_mut().as_mut() else {
            return;
        };
        info.direct_map.start = VirtAddr(info.direct_map_base.0 + new_start);
        info.direct_map.size += grown;
        info.mapped.start = PhysAddr(new_start);
        info.mapped.size += grown;
    }
}

unsafe fn extend_bootstrap_direct_map_info<State>(bag: &BootStaticBag<State>, phys_end: usize) {
    unsafe {
        let Some(info) = bag.bootstrap_pmap_info_mut().as_mut() else {
            return;
        };
        let Some(phys_start) = info.direct_map.start.0.checked_sub(info.direct_map_base.0) else {
            return;
        };
        if phys_end <= phys_start {
            return;
        }
        info.direct_map.size = info.direct_map.size.max(phys_end - phys_start);
        info.mapped.size = info.mapped.size.max(phys_end - info.mapped.start.0);
    }
}

// Reservation helpers are deliberately side-effect-light. They classify a
// candidate slot as already satisfied, conflicting, or open for a reservation
// token that still owns any fresh intermediates.
fn reservation_for_slot(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
    current: u64,
    expected: u64,
    intermediates: PmapReservationIntermediates,
) -> Result<Option<PmapReservation>, PmapError> {
    if current == expected {
        return Ok(None);
    }
    if current != 0 {
        return Err(PmapError::AlreadyMapped);
    }
    Ok(Some(PmapReservation::new_with_intermediates(
        virt,
        phys,
        kind,
        intermediates,
    )))
}

// Leaf helpers are shared by kernel and process-root pmap code. They only
// accept present leaves of the exact requested granularity; absent leaves are
// benign no-ops and branch/split cases stay with VM rematerialization policy.
pub(super) fn unmap_leaf_slot(
    slot: &mut u64,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = pte_phys(current);
    if !phys.0.is_multiple_of(kind.size()) {
        return Err(PmapError::InvalidRequest);
    }
    *slot = 0;
    Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
}

pub(super) fn protect_leaf_slot(
    slot: &mut u64,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = pte_phys(current);
    if !phys.0.is_multiple_of(kind.size()) {
        return Err(PmapError::InvalidRequest);
    }

    let updated = encode_leaf_pte_with_permissions(phys, permissions);
    if current == updated {
        return Ok(None);
    }
    *slot = updated;
    Ok(Some(PmapInvalidation::new(virt, kind.size())))
}

// Prune helpers release empty committed branch tables after unmap. The PT-node
// registry is the pmap-only authority that turns a branch PTE physical address
// back into the typed owner that may be freed.
fn prune_empty_l0_table_from_bag<State>(bag: &BootStaticBag<State>, virt: VirtAddr) {
    let Some(l1) = l1_table_mut(bag, virt) else {
        return;
    };
    let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
    if !pte_is_branch(*slot) {
        return;
    }

    let phys = pte_phys(*slot);
    let l0 = unsafe { page_table_mut_from_phys(phys) };
    if !page_table_is_empty(l0) {
        return;
    }

    *slot = 0;
    release_committed_pt_node_from_bag(bag, phys);
}

fn prune_empty_l1_table_from_bag<State>(bag: &BootStaticBag<State>, virt: VirtAddr) {
    let slot = unsafe { &mut bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
    if !pte_is_branch(*slot) {
        return;
    }

    let phys = pte_phys(*slot);
    let l1 = unsafe { page_table_mut_from_phys(phys) };
    if !page_table_is_empty(l1) {
        return;
    }

    *slot = 0;
    release_committed_pt_node_from_bag(bag, phys);
}

pub(super) fn page_table_is_empty(table: &PageTable) -> bool {
    table.0.iter().all(|entry| *entry == 0)
}
