//! Sv39 PTE encoding and decoding helpers.
//!
//! Core data structures/state interpreted here:
//! - PTE flag constants `PTE_*`: the Sv39 valid, permission, global, accessed,
//!   and dirty bits used by this backend.
//! - `PmapPermissions`: the HAL permission vocabulary translated into Sv39
//!   leaves.
//! - `PageTable`: decoded from physical addresses through the direct map on
//!   target and through host pointers in tests.
//!
//! Main data-flow functions:
//! - `validate_rv64_leaf_permissions()` rejects permission combinations this
//!   backend cannot encode safely.
//! - `kernel_alias_permissions_for_phys()` derives final kernel text/rodata/data
//!   permissions from linker ranges captured in `BootStaticBag`.
//! - `encode_leaf_pte_with_permissions()`, `encode_leaf_pte()`, and
//!   `encode_branch_pte()` turn physical addresses into PTE words.
//! - `pte_is_branch()`, `pte_is_leaf()`, and `pte_phys()` recover page-table
//!   shape from raw entries.
//! - `page_table_mut_from_phys()` is the only PTE helper that forms a mutable
//!   `PageTable` reference.
//!
//! Helper functions here do not own policy: higher modules decide whether a
//! mapping belongs to the kernel bootstrap, direct map, MMIO path, or process
//! root. See
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`.

use tx_hal::{PhysAddr, PhysRange, PmapError, PmapPermissions};

use crate::boot_static::{BootStaticBag, PageTable};

#[cfg(target_arch = "riscv64")]
use super::topology::direct_map_virt;

pub(crate) const PTE_V: u64 = 1 << 0;
pub(crate) const PTE_R: u64 = 1 << 1;
pub(crate) const PTE_W: u64 = 1 << 2;
pub(crate) const PTE_X: u64 = 1 << 3;
pub(crate) const PTE_U: u64 = 1 << 4;
pub(crate) const PTE_G: u64 = 1 << 5;
pub(crate) const PTE_A: u64 = 1 << 6;
pub(crate) const PTE_D: u64 = 1 << 7;

// Permission validation and kernel-alias classification live together because
// both define which HAL permission vocabulary is legal for this Sv39 backend.
// Kernel aliases split text/rodata/data using boot-static linker facts, while
// process mappings may additionally set the user bit.
pub(crate) fn validate_rv64_leaf_permissions(
    permissions: PmapPermissions,
    allow_user: bool,
) -> Result<(), PmapError> {
    let readable = permissions.contains(PmapPermissions::READ);
    let writable = permissions.contains(PmapPermissions::WRITE);
    let executable = permissions.contains(PmapPermissions::EXECUTE);
    let user = permissions.contains(PmapPermissions::USER);
    if (user && !allow_user) || (!readable && !executable) || (writable && !readable) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn encode_kernel_mapping_leaf(phys: PhysAddr) -> u64 {
    encode_leaf_pte_with_permissions(phys, PmapPermissions::KERNEL_RW)
}

pub(crate) fn kernel_alias_permissions_for_phys<State>(
    bag: &BootStaticBag<State>,
    phys: usize,
) -> PmapPermissions {
    let phys = PhysAddr(phys);
    if range_contains(bag.kernel_text_phys(), phys) {
        PmapPermissions::KERNEL_RX
    } else if range_contains(bag.kernel_rodata_phys(), phys) {
        PmapPermissions::KERNEL_RO
    } else {
        debug_assert!(
            range_contains(bag.kernel_data_phys(), phys)
                || range_contains(bag.kernel_bss_phys(), phys)
                || range_contains(bag.kernel_stack_phys(), phys)
                || range_contains(bag.kernel_image_phys(), phys)
        );
        PmapPermissions::KERNEL_RW
    }
}

fn range_contains(range: PhysRange, phys: PhysAddr) -> bool {
    phys.0 >= range.start.0 && phys.0 < range.end().0
}

// Encoding helpers produce Sv39 leaf and branch PTEs from typed HAL address and
// permission values. The caller chooses the mapping role; these helpers only set
// V/A/D and R/W/X/U/G according to the requested leaf shape.
pub(crate) fn encode_leaf_pte_with_permissions(
    phys: PhysAddr,
    permissions: PmapPermissions,
) -> u64 {
    let mut flags = 0;
    if permissions.contains(PmapPermissions::READ) {
        flags |= PTE_R;
    }
    if permissions.contains(PmapPermissions::WRITE) {
        flags |= PTE_W;
    }
    if permissions.contains(PmapPermissions::EXECUTE) {
        flags |= PTE_X;
    }
    if permissions.contains(PmapPermissions::USER) {
        flags |= PTE_U;
    }
    if permissions.contains(PmapPermissions::GLOBAL) {
        flags |= PTE_G;
    }
    encode_leaf_pte(phys, flags)
}

pub(crate) fn encode_leaf_pte(phys: PhysAddr, flags: u64) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D
}

pub(crate) fn encode_branch_pte(phys: PhysAddr) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | PTE_V
}

// PTE query helpers keep branch/leaf detection and physical-address recovery in
// one place. The rest of pmap code treats non-leaf branch entries as page-table
// ownership edges and leaf entries as mapping state.
pub(crate) fn pte_is_branch(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) == 0
}

pub(crate) fn pte_is_leaf(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) != 0
}

pub(crate) fn pte_phys(pte: u64) -> PhysAddr {
    PhysAddr(((pte >> 10) << 12) as usize)
}

// Page-table pages are addressed through the direct map on real RV64 and by
// host pointers in unit tests. Keeping that conditional here prevents the
// higher-level pmap code from open-coding physical-to-pointer conversion.
pub(crate) unsafe fn page_table_mut_from_phys(phys: PhysAddr) -> &'static mut PageTable {
    #[cfg(target_arch = "riscv64")]
    {
        unsafe { &mut *(direct_map_virt(phys.0) as *mut PageTable) }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        unsafe { &mut *(phys.0 as *mut PageTable) }
    }
}
