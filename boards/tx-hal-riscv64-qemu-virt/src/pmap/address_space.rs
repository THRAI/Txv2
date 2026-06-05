//! Process-root pmap orchestration.
//!
//! This module owns the board-specific `PmapRoot` lifecycle used by VM
//! materialization.
//!
//! Core data structures/state maintained here:
//! - `PmapRoot`: a typed root PT-node plus its ASID.
//! - `ALLOCATED_ASIDS`: the fixed v1 ASID bitmap, with ASID 0 reserved.
//! - `RootEnsuredTable`: root-relative speculative table creation state.
//! - the committed PT-node registry in `pt_node`: used during recursive
//!   teardown to recover ownership from branch PTE physical addresses.
//!
//! Main state modification functions:
//! - `create_pmap_root()` / `destroy_pmap_root()` allocate and release process
//!   roots, copy the shared kernel half, and recursively clear user tables.
//! - `reserve_mapping()`, `rollback_mapping()`, and `commit_mapping()` implement
//!   the reservation-to-publication transaction for user mappings.
//! - `unmap_mapping()` and `protect_mapping()` mutate existing user leaves and
//!   return invalidation/unmap evidence for later shootdown/accounting.
//! - `shootdown_mapping()` is the local v1 ASID-shaped invalidation hook.
//!
//! Helper groups:
//! - ASID helpers allocate/free the fixed bitmap;
//! - root table helpers allocate, roll back, find, and prune L1/L0 tables;
//! - teardown helpers walk committed branch subtrees and return PT-node
//!   ownership through pmap-only release.
//!
//! See `docs/progress/decisions/2026-04-29-process-root-asid-shootdown-anchors.md`
//! and `docs/progress/decisions/2026-04-29-rv64-pmap-module-extraction.md`.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReservationIntermediates, PmapReserveKind, PmapRoot, PmapUnmapResult, VirtAddr,
};

use crate::boot_static::{BootStaticBag, IdentityDropped, PageTable};

use super::kernel_space::{page_table_is_empty, protect_leaf_slot, unmap_leaf_slot};
use super::pt_node::{
    alloc_pt_node_from_bag, free_pt_node_from_bag, register_committed_intermediates,
    release_committed_pt_node_from_bag,
};
use super::{
    encode_branch_pte, encode_leaf_pte_with_permissions, ensure_l0_table_for_reservation,
    l0_table_mut, page_table_mut_from_phys, pte_is_branch, pte_phys, rv64_1g_leaf_index,
    rv64_2m_leaf_index, rv64_4k_leaf_index, sfence_vma_all, sfence_vma_range_asid,
    validate_aligned_mapping, validate_aligned_virt, validate_rv64_leaf_permissions,
    validate_user_mapping_virt,
};

const ASID_BITMAP_WORDS: usize = 16;
pub(crate) const ASID_CAPACITY: usize = ASID_BITMAP_WORDS * u64::BITS as usize;

static ALLOCATED_ASIDS: [AtomicU64; ASID_BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; ASID_BITMAP_WORDS];

/// Root-relative ensured table plus the fresh PT-node that backs it.
///
/// `node == None` means the table already existed. `Some(node)` means the
/// caller must either commit the reservation or roll the node back on failure.
struct RootEnsuredTable {
    table: &'static mut PageTable,
    node: Option<tx_hal::PtNode>,
}

struct RootInvalidated;

// Root lifecycle: new process roots get a fresh PT-node, a small ASID, and a
// copy of the upper-half kernel template. Destruction walks only the user half,
// releases committed intermediates through the PT-node registry, and then
// returns the root node.
pub(crate) fn create_pmap_root() -> Result<PmapRoot, PmapError> {
    create_pmap_root_from_bag(BootStaticBag::<IdentityDropped>::global_ref())
}

pub(super) fn create_pmap_root_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<PmapRoot, PmapError> {
    let asid = alloc_asid()?;
    let node = match alloc_pt_node_from_bag(bag) {
        Ok(node) => node,
        Err(_) => {
            free_asid(asid);
            return Err(PmapError::Exhausted);
        }
    };

    let root = unsafe { page_table_mut_from_phys(node.phys) };
    root.0.fill(0);
    let kernel_root = unsafe { bag.bootstrap_root_mut() };
    root.0[256..].copy_from_slice(&kernel_root.0[256..]);

    Ok(PmapRoot::new(node, asid))
}

pub(crate) fn destroy_pmap_root(root: PmapRoot) {
    destroy_pmap_root_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), root);
}

pub(super) fn destroy_pmap_root_from_bag<State>(bag: &BootStaticBag<State>, root: PmapRoot) {
    let table = unsafe { page_table_mut_from_phys(root.phys()) };
    for slot in &mut table.0[..256] {
        if pte_is_branch(*slot) {
            release_page_table_tree_from_bag(bag, pte_phys(*slot));
        }
        *slot = 0;
    }
    let invalidated = invalidate_destroyed_root();
    free_asid_after_invalidation(root.asid(), invalidated);
    free_pt_node_from_bag(bag, root.into_node());
}

// Mapping lifecycle for user roots. Reservation may allocate intermediate
// tables and carries those nodes in the reservation token; commit publishes the
// final leaf and records committed intermediates; rollback releases any
// uncommitted tables. Protect only updates same-granularity present leaves,
// leaving unsafe rematerialization cases to VM/fault handling.
pub(crate) fn reserve_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    reserve_mapping_from_root(
        BootStaticBag::<IdentityDropped>::global_ref(),
        root.phys(),
        virt,
        phys,
        kind,
    )
}

pub(super) fn reserve_mapping_from_root<State>(
    bag: &BootStaticBag<State>,
    root: PhysAddr,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_mapping(virt, phys, kind.size())?;

    let root = unsafe { page_table_mut_from_phys(root) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &root.0[rv64_1g_leaf_index(virt.0)];
            if *slot != 0 {
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new(virt, phys, kind)))
        }
        PmapReserveKind::Superpage2M => {
            let ensured_l1 = ensure_l1_table_for_root(bag, root, virt)?;
            let current = ensured_l1.table.0[rv64_2m_leaf_index(virt.0)];
            if current != 0 {
                if let Some(node) = ensured_l1.node {
                    rollback_intermediates_in_root(
                        bag,
                        root,
                        virt,
                        PmapReservationIntermediates {
                            l2: None,
                            l1: Some(node),
                            l0: None,
                        },
                    );
                }
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new_with_intermediates(
                virt,
                phys,
                kind,
                PmapReservationIntermediates {
                    l2: None,
                    l1: ensured_l1.node,
                    l0: None,
                },
            )))
        }
        PmapReserveKind::Page4K => {
            let ensured_l1 = ensure_l1_table_for_root(bag, root, virt)?;
            let l1_node = ensured_l1.node;
            let ensured_l0 = match ensure_l0_table_for_reservation(bag, ensured_l1.table, virt) {
                Ok(table) => table,
                Err(err) => {
                    if let Some(node) = l1_node {
                        rollback_intermediates_in_root(
                            bag,
                            root,
                            virt,
                            PmapReservationIntermediates {
                                l2: None,
                                l1: Some(node),
                                l0: None,
                            },
                        );
                    }
                    return Err(err);
                }
            };
            let current = ensured_l0.table.0[rv64_4k_leaf_index(virt.0)];
            if current != 0 {
                rollback_intermediates_in_root(
                    bag,
                    root,
                    virt,
                    PmapReservationIntermediates {
                        l2: None,
                        l1: l1_node,
                        l0: ensured_l0.node,
                    },
                );
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new_with_intermediates(
                virt,
                phys,
                kind,
                PmapReservationIntermediates {
                    l2: None,
                    l1: l1_node,
                    l0: ensured_l0.node,
                },
            )))
        }
    }
}

pub(crate) fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
    rollback_mapping_from_root(
        BootStaticBag::<IdentityDropped>::global_ref(),
        root.phys(),
        reservation,
    );
}

fn rollback_mapping_from_root<State>(
    bag: &BootStaticBag<State>,
    root: PhysAddr,
    reservation: PmapReservation,
) {
    let root = unsafe { page_table_mut_from_phys(root) };
    rollback_intermediates_in_root(bag, root, reservation.virt(), reservation.intermediates());
    sfence_vma_all();
}

pub(crate) fn commit_mapping(
    root: &PmapRoot,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    commit_mapping_from_root(root.phys(), reservation, permissions);
}

pub(super) fn commit_mapping_from_root(
    root: PhysAddr,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    validate_rv64_leaf_permissions(permissions, true).expect("invalid process pmap permissions");
    register_committed_intermediates(reservation.intermediates());
    let pte = encode_leaf_pte_with_permissions(reservation.phys(), permissions);
    let root = unsafe { page_table_mut_from_phys(root) };
    match reservation.kind() {
        PmapReserveKind::Superpage1G => {
            root.0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Superpage2M => {
            let l1 = l1_table_mut_from_root(root, reservation.virt()).expect("reserved L1 table");
            l1.0[rv64_2m_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Page4K => {
            let l1 = l1_table_mut_from_root(root, reservation.virt()).expect("reserved L1 table");
            let l0 = l0_table_mut(l1, reservation.virt()).expect("reserved L0 table");
            l0.0[rv64_4k_leaf_index(reservation.virt().0)] = pte;
        }
    }
    // User pmap commits are consumed at the next userspace entry, where
    // `activate_user_pmap` writes `satp` and issues `sfence.vma`. Avoid a
    // second per-PTE fence here; unmap/protect still fence at invalidation.
}

pub(crate) fn unmap_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    unmap_mapping_from_root(
        BootStaticBag::<IdentityDropped>::global_ref(),
        root.phys(),
        virt,
        kind,
    )
}

pub(super) fn unmap_mapping_from_root<State>(
    bag: &BootStaticBag<State>,
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    let root = unsafe { page_table_mut_from_phys(root) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
            unmap_leaf_slot(slot, virt, kind)
        }
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
                return Ok(None);
            };
            let result = {
                let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)?
            };
            if result.is_some() {
                prune_empty_l1_table_in_root(bag, root, virt);
            }
            Ok(result)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
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
                prune_empty_l0_table_in_root(bag, root, virt);
                prune_empty_l1_table_in_root(bag, root, virt);
            }
            Ok(result)
        }
    }
}

pub(crate) fn protect_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    protect_mapping_from_root(root.phys(), virt, kind, permissions)
}

pub(super) fn protect_mapping_from_root(
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    validate_rv64_leaf_permissions(permissions, true)?;
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    let root = unsafe { page_table_mut_from_phys(root) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
                return Ok(None);
            };
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
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

// ASIDs are intentionally tiny and local in this v1 implementation. The API is
// ASID-shaped so later remote shootdown and reuse discipline can grow behind
// the same HAL surface.
pub(crate) fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {
    sfence_vma_all();
}

pub(crate) fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
    if invalidations.is_empty() {
        return;
    }

    let coalesced = coalesce_invalidation_ranges(invalidations);
    for invalidation in &coalesced {
        sfence_vma_range_asid(invalidation.virt(), invalidation.size(), asid);
    }
    crate::remote_sfence_vma_asid_batch(asid, &coalesced);
}

fn alloc_asid() -> Result<Asid, PmapError> {
    for word_index in 0..ASID_BITMAP_WORDS {
        loop {
            let allocated = ALLOCATED_ASIDS[word_index].load(Ordering::Acquire);
            let reserved = if word_index == 0 { 1 } else { 0 };
            if allocated | reserved == u64::MAX {
                break;
            }
            for bit_index in 0..u64::BITS as usize {
                let asid = word_index * u64::BITS as usize + bit_index;
                if asid == 0 || asid >= ASID_CAPACITY {
                    continue;
                }
                let bit = 1u64 << bit_index;
                if allocated & bit != 0 {
                    continue;
                }
                if ALLOCATED_ASIDS[word_index]
                    .compare_exchange(
                        allocated,
                        allocated | bit,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(Asid(asid as u16));
                }
                break;
            }
        }
    }
    Err(PmapError::Exhausted)
}

fn free_asid(asid: Asid) {
    let asid = asid.0 as usize;
    if asid == 0 || asid >= ASID_CAPACITY {
        return;
    }
    let word_index = asid / u64::BITS as usize;
    let bit_index = asid % u64::BITS as usize;
    ALLOCATED_ASIDS[word_index].fetch_and(!(1u64 << bit_index), Ordering::AcqRel);
}

fn invalidate_destroyed_root() -> RootInvalidated {
    sfence_vma_all();
    RootInvalidated
}

fn free_asid_after_invalidation(asid: Asid, _invalidated: RootInvalidated) {
    crate::clear_asid_residency(asid);
    free_asid(asid);
}

pub(crate) fn coalesce_invalidation_ranges(
    invalidations: &[PmapInvalidation],
) -> Vec<PmapInvalidation> {
    let mut coalesced: Vec<PmapInvalidation> = Vec::with_capacity(invalidations.len());
    for invalidation in invalidations {
        if let Some(last) = coalesced.last_mut() {
            let last_end = last.virt().0 + last.size();
            if last_end == invalidation.virt().0 {
                *last = PmapInvalidation::new(last.virt(), last.size() + invalidation.size());
                continue;
            }
        }
        coalesced.push(*invalidation);
    }
    coalesced
}

// Root-relative intermediate table management mirrors the kernel-bootstrap
// helpers but works from an arbitrary process root. Empty L0/L1 tables are
// pruned after unmap so committed PT-node ownership returns through the pmap
// path rather than being lost in raw branch PTEs.
fn ensure_l1_table_for_root<State>(
    bag: &BootStaticBag<State>,
    root: &mut PageTable,
    virt: VirtAddr,
) -> Result<RootEnsuredTable, PmapError> {
    if let Some(table) = l1_table_mut_from_root(root, virt) {
        return Ok(RootEnsuredTable { table, node: None });
    }

    let index = rv64_1g_leaf_index(virt.0);
    if root.0[index] != 0 {
        return Err(PmapError::AlreadyMapped);
    }

    let node = alloc_pt_node_from_bag(bag).map_err(|_| PmapError::Exhausted)?;
    root.0[index] = encode_branch_pte(node.phys);
    let Some(table) = l1_table_mut_from_root(root, virt) else {
        root.0[index] = 0;
        free_pt_node_from_bag(bag, node);
        return Err(PmapError::InvalidRequest);
    };
    Ok(RootEnsuredTable {
        table,
        node: Some(node),
    })
}

fn rollback_intermediates_in_root<State>(
    bag: &BootStaticBag<State>,
    root: &mut PageTable,
    virt: VirtAddr,
    intermediates: PmapReservationIntermediates,
) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = l1_table_mut_from_root(root, virt) {
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            if pte_is_branch(*slot) && pte_phys(*slot) == l0.phys {
                *slot = 0;
            }
        }
        free_pt_node_from_bag(bag, l0);
    }

    if let Some(l1) = intermediates.l1 {
        let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
        if pte_is_branch(*slot) && pte_phys(*slot) == l1.phys {
            *slot = 0;
        }
        free_pt_node_from_bag(bag, l1);
    }
}

pub(super) fn l1_table_mut_from_root(
    root: &mut PageTable,
    virt: VirtAddr,
) -> Option<&'static mut PageTable> {
    let pte = root.0[rv64_1g_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }
    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

fn prune_empty_l0_table_in_root<State>(
    bag: &BootStaticBag<State>,
    root: &mut PageTable,
    virt: VirtAddr,
) {
    let Some(l1) = l1_table_mut_from_root(root, virt) else {
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

fn prune_empty_l1_table_in_root<State>(
    bag: &BootStaticBag<State>,
    root: &mut PageTable,
    virt: VirtAddr,
) {
    let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
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

fn release_page_table_tree_from_bag<State>(bag: &BootStaticBag<State>, phys: PhysAddr) {
    let table = unsafe { page_table_mut_from_phys(phys) };
    for slot in table.0.iter_mut() {
        if pte_is_branch(*slot) {
            release_page_table_tree_from_bag(bag, pte_phys(*slot));
        }
        *slot = 0;
    }
    release_committed_pt_node_from_bag(bag, phys);
}

#[cfg(test)]
pub(super) fn reset_asids_for_test() {
    for word in &ALLOCATED_ASIDS {
        word.store(0, Ordering::Release);
    }
}
