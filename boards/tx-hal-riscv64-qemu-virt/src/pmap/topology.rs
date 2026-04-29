//! Sv39/QEMU virtual-address topology.
//!
//! This module is the board configuration namespace for pmap constants and
//! page-size policy.
//!
//! Core data structures/state described here:
//! - there is no mutable state; the module defines the Sv39/QEMU address-space
//!   contract as constants.
//! - address ranges include the direct map, high kernel alias, user top/helper
//!   band, QEMU RAM base, and the fixed boot PT-node pool geometry.
//! - page-size constants define the 4 KiB / 2 MiB / 1 GiB choices used by pmap
//!   mutation modules.
//!
//! Main data-flow functions:
//! - `validate_aligned_mapping()`, `validate_aligned_virt()`, and
//!   `validate_user_mapping_virt()` reject requests outside the Sv39/QEMU
//!   contract before mutation.
//! - `bootstrap_satp_value()` constructs the H1 SATP value.
//! - `direct_map_virt()` forms the board direct-map alias for a physical
//!   address.
//! - `rv64_1g_leaf_index()`, `rv64_2m_leaf_index()`, and
//!   `rv64_4k_leaf_index()` centralize Sv39 index extraction.
//!
//! Helper functions are intentionally arithmetic-only. Non-pmap board code
//! imports from here when it needs address facts; pmap operation modules import
//! from here when choosing mapping granularity. See
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`.

use tx_hal::{PhysAddr, PmapError, PmapReserveKind};

// Architectural and board address constants. The upper-half layout reserves a
// direct map, a high kernel alias, and a small user-top helper band while the
// bootstrap path temporarily keeps one low identity leaf.
pub(crate) const SV39_MODE: usize = 8;

pub(crate) const SV39_USER_TOP: usize = 0x0000_0040_0000_0000;
pub(crate) const USER_RESERVED_TOP_SIZE: usize = 4 * 1024 * 1024;
pub(crate) const SV39_USER_ALLOC_TOP: usize = SV39_USER_TOP - USER_RESERVED_TOP_SIZE;
pub(crate) const DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;
pub(crate) const DIRECT_MAP_SIZE: usize = 128 * 1024 * 1024 * 1024;
pub(crate) const KERNEL_VIRT_BASE: usize = 0xffff_ffff_8020_0000;

pub(crate) const QEMU_RAM_BASE: usize = 0x8000_0000;
pub(crate) const QEMU_KERNEL_PHYS_BASE: usize = 0x8020_0000;
pub(crate) const QEMU_BOOTSTRAP_MAP_SIZE: usize = 1024 * 1024 * 1024;
pub(crate) const KERNEL_BOOTSTRAP_ALIAS_SIZE: usize = 16 * 1024 * 1024;
pub(crate) const SUPERPAGE_1G_SIZE: usize = 1024 * 1024 * 1024;
pub(crate) const SUPERPAGE_2M_SIZE: usize = 2 * 1024 * 1024;
pub(crate) const KERNEL_ALIAS_L0_TABLES: usize = KERNEL_BOOTSTRAP_ALIAS_SIZE / SUPERPAGE_2M_SIZE;
pub(crate) const PT_NODE_POOL_ENTRIES: usize = 8;
pub(crate) const PAGE_SIZE: usize = 4096;

// Validation helpers enforce alignment and user-top policy before page-table
// mutation. The caller still owns semantic authority; these checks only reject
// impossible or out-of-contract Sv39 requests.
pub(crate) fn validate_aligned_mapping(
    virt: tx_hal::VirtAddr,
    phys: PhysAddr,
    size: usize,
) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) || !phys.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn validate_aligned_virt(virt: tx_hal::VirtAddr, size: usize) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn validate_user_mapping_virt(
    virt: tx_hal::VirtAddr,
    kind: PmapReserveKind,
) -> Result<(), PmapError> {
    let end = virt
        .0
        .checked_add(kind.size())
        .ok_or(PmapError::InvalidRequest)?;
    if end > SV39_USER_ALLOC_TOP {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

// Indexing and address-construction helpers centralize the Sv39 bit slicing so
// mapping code reads in terms of page-table levels rather than shifts.
pub(crate) fn bootstrap_satp_value(root: PhysAddr) -> usize {
    (SV39_MODE << 60) | (root.0 >> 12)
}

pub(crate) fn align_up(value: usize, align: usize) -> Option<usize> {
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

pub(crate) fn rv64_1g_leaf_index(virt: usize) -> usize {
    (virt >> 30) & 0x1ff
}

pub(crate) fn rv64_2m_leaf_index(virt: usize) -> usize {
    (virt >> 21) & 0x1ff
}

pub(crate) fn rv64_4k_leaf_index(virt: usize) -> usize {
    (virt >> 12) & 0x1ff
}

pub(crate) const fn direct_map_virt(phys: usize) -> usize {
    DIRECT_MAP_BASE + phys
}
