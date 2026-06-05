//! RV64 QEMU page-map facade.
//!
//! This module is the board-private pmap entry point exposed through
//! `PmapIf`.
//!
//! Core data structures maintained here:
//! - `BootStaticBag<IdentityLive>` / `BootStaticBag<IdentityDropped>`: the
//!   typestate carrier for the low-to-high boot transition.
//! - `PageTable`: the board page-table page shape shared by all pmap modules.
//! - `EnsuredTable`: a speculative L1/L0 table plus the PT-node authority that
//!   must be committed or rolled back.
//! - `HighSentinel`: the PC/SP/GP proof that the high alias is live before
//!   any future identity teardown.
//!
//! Main data-flow functions:
//! - the assembly trampoline builds the first low-LMA bootstrap tables before
//!   enabling Sv39.
//! - `adopt_high_linked_bootstrap_pmap()` captures those tables as HAL facts
//!   and refines the high kernel alias leaves once Rust is executing high.
//! - `complete_post_entry_pipeline()` validates the high sentinel and drops the
//!   temporary lower identity bridge.
//!
//! Helper groups:
//! - intermediate-table helpers build and roll back speculative branch tables;
//! - table lookup helpers decode branch PTEs into `PageTable` references;
//! - local maintenance helpers cover `sfence.vma`, boot-pool indexing, and page
//!   zeroing for PT nodes.
//!
//! Responsibility-specific modules own process roots, kernel mappings, PT-node
//! ownership, PTE encoding, and topology constants. See
//! `docs/progress/decisions/2026-04-29-rv64-pmap-module-extraction.md` and
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`.

use tx_hal::{
    BootstrapPmapInfo, PhysAddr, PhysRange, PmapError, PmapReservationIntermediates, PtNode,
    VirtAddr, VirtRange,
};

use crate::boot_static::{BootStaticBag, IdentityDropped, IdentityLive, PageTable};

mod address_space;
mod kernel_space;
mod pt_node;
mod pte;

pub(crate) mod topology;

pub(crate) use address_space::{
    commit_mapping, create_pmap_root, destroy_pmap_root, protect_mapping, reserve_mapping,
    rollback_mapping, shootdown_mapping, shootdown_mappings, unmap_mapping, ASID_CAPACITY,
};
pub(crate) use kernel_space::{
    bootstrap_pmap_info, commit_kernel_direct_map_1g, commit_kernel_mapping, extend_direct_map,
    protect_kernel_mapping, reserve_kernel_direct_map_1g, reserve_kernel_mapping,
    rollback_kernel_mapping, shootdown_kernel_mapping, unmap_kernel_mapping,
};
pub(crate) use pt_node::{alloc_pt_node, free_pt_node, install_pt_node_allocator};
use pt_node::{alloc_pt_node_from_bag, free_pt_node_from_bag};
use pte::*;
use topology::*;

// Bootstrap Sv39 address-space shape:
//
//   lower canonical half
//   0x0000_0000_0000_0000
//        | user mappings live here once VM exists
//        |
//        +-- 0x0000_0000_8000_0000  temporary identity bridge for QEMU RAM
//        |                           root slot 2, 1 GiB leaf, boot only
//        |
//        +-- USER_ALLOC_TOP          ordinary user allocation ceiling
//        +-- USER_TOP - 4 MiB        reserved helper-page band
//        |                           future signal/trampoline/VDSO pages
//   0x0000_0040_0000_0000  USER_TOP
//
//   upper canonical half
//   0xffff_ffc0_0000_0000  DIRECT_MAP_BASE
//        +-- +0x8000_0000           QEMU RAM direct-map alias
//        |                           root slot 258, 1 GiB leaf
//        |
//        +-- ...                     future RAM/MMIO direct-map extension
//        |
//        +-- 0xffff_ffff_8020_0000   high kernel alias
//                                    root slot 510 -> bootstrap L1,
//                                    2 MiB leaves for this first slice
//
// Process roots will leave the lower half private to the AddressSpace and
// share/copy the upper-half kernel entries. The BSP enters Rust through the
// high kernel alias. The low identity leaf intentionally stays live for this
// low-linked Rust image because compiler-generated absolute tables can still
// target low text addresses; explicit teardown is deferred to the high-linker
// slice.

#[cfg(any(test, target_arch = "riscv64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HighSentinelError {
    ProgramCounter,
    StackPointer,
    GlobalPointer,
}

/// L1/L0 table returned by the reservation path, plus the fresh PT-node
/// authority that must be rolled back unless the reservation commits.
struct EnsuredTable {
    table: &'static mut PageTable,
    node: Option<PtNode>,
}

/// Snapshot used by the identity-teardown sentinel.
///
/// On real RV64 this captures the current PC, SP, and GP registers; in tests it
/// is constructed from supplied values so the same validation path is covered.
#[cfg(any(test, target_arch = "riscv64"))]
#[derive(Clone, Copy)]
struct HighSentinel {
    pc: usize,
    sp: usize,
    gp: usize,
}

// The identity-live bag models the H1/H2 transition as a sequence of
// borrow-returning steps. In the high-VMA/low-LMA path, the pre-entry half is
// pure assembly and Rust starts only after the high alias is active. Rust then
// adopts/refines the assembly-built tables, publishes pmap facts, and finally
// removes the temporary identity bridge after BootInfo has consumed firmware
// pointers.
pub(crate) fn adopt_high_linked_bootstrap_pmap(bag: &mut BootStaticBag<IdentityLive>) {
    bag.adopt_high_linked_bootstrap_pmap();
}

pub(crate) fn install_secondary_identity_bridge() {
    let bag = BootStaticBag::<IdentityDropped>::global_ref();
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] =
            encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_X);
    }
    sfence_vma_all();
}

pub(crate) fn remove_secondary_identity_bridge() {
    let bag = BootStaticBag::<IdentityDropped>::global_ref();
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] = 0;
        if let Some(info) = bag.bootstrap_pmap_info_mut().as_mut() {
            info.identity = None;
        }
    }
    sfence_vma_all();
}

impl BootStaticBag<IdentityLive> {
    pub(crate) fn adopt_high_linked_bootstrap_pmap(&mut self) -> &mut Self {
        self.refine_kernel_high_alias()
            .publish_bootstrap_pmap_info()
    }

    fn refine_kernel_high_alias(&mut self) -> &mut Self {
        unsafe {
            let image = self.kernel_image_phys();
            let Some(image_end) = align_up(image.end().0, PAGE_SIZE) else {
                return self;
            };
            let alias_end = QEMU_KERNEL_PHYS_BASE + KERNEL_BOOTSTRAP_ALIAS_SIZE;
            let mut phys = image.start.0;
            while phys < image_end.min(alias_end) {
                if phys >= QEMU_KERNEL_PHYS_BASE {
                    let offset = phys - QEMU_KERNEL_PHYS_BASE;
                    let table_index = offset / SUPERPAGE_2M_SIZE;
                    if table_index < KERNEL_ALIAS_L0_TABLES {
                        let virt = KERNEL_VIRT_BASE + offset;
                        let permissions = kernel_alias_permissions_for_phys(self, phys);
                        let table = self.kernel_alias_l0_mut(table_index);
                        table.0[rv64_4k_leaf_index(virt)] =
                            encode_leaf_pte_with_permissions(PhysAddr(phys), permissions);
                    }
                }
                phys += PAGE_SIZE;
            }
        }
        self
    }

    #[cfg(test)]
    fn begin_bootstrap_pmap(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0.fill(0);
            self.kernel_alias_l1_mut().0.fill(0);
        }
        self
    }

    #[cfg(test)]
    fn map_identity_bridge(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] =
                encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_X);
        }
        self
    }

    #[cfg(test)]
    fn map_direct_map_window(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(direct_map_virt(QEMU_RAM_BASE))] =
                encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_G);
        }
        self
    }

    #[cfg(test)]
    fn map_kernel_high_alias(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(KERNEL_VIRT_BASE)] =
                encode_branch_pte(self.kernel_alias_l1_phys());

            self.kernel_alias_l1_mut().0.fill(0);
            for table_index in 0..KERNEL_ALIAS_L0_TABLES {
                let table = self.kernel_alias_l0_mut(table_index);
                table.0.fill(0);

                let virt = KERNEL_VIRT_BASE + table_index * SUPERPAGE_2M_SIZE;
                self.kernel_alias_l1_mut().0[rv64_2m_leaf_index(virt)] =
                    encode_branch_pte(self.kernel_alias_l0_phys(table_index));
            }

            let image = self.kernel_image_phys();
            let Some(image_end) = align_up(image.end().0, PAGE_SIZE) else {
                return self;
            };
            let alias_end = QEMU_KERNEL_PHYS_BASE + KERNEL_BOOTSTRAP_ALIAS_SIZE;
            let mut phys = image.start.0;
            while phys < image_end.min(alias_end) {
                if phys >= QEMU_KERNEL_PHYS_BASE {
                    let offset = phys - QEMU_KERNEL_PHYS_BASE;
                    let virt = KERNEL_VIRT_BASE + offset;
                    let table_index = offset / SUPERPAGE_2M_SIZE;
                    let table = self.kernel_alias_l0_mut(table_index);
                    table.0[rv64_4k_leaf_index(virt)] = encode_leaf_pte_with_permissions(
                        PhysAddr(phys),
                        kernel_alias_permissions_for_phys(self, phys),
                    );
                }
                phys += PAGE_SIZE;
            }
        }
        self
    }

    fn publish_bootstrap_pmap_info(&mut self) -> &mut Self {
        let root = self.bootstrap_root_phys();
        let kernel_alias_l1 = self.kernel_alias_l1_phys();
        let kernel_alias_l0 = self.kernel_alias_l0_phys_range();
        let pt_node_pool = self.pt_node_pool_phys_range();
        unsafe {
            let reserved_page_tables = self.reserved_page_tables_mut();
            *reserved_page_tables = [
                PhysRange {
                    start: root,
                    size: PAGE_SIZE,
                },
                PhysRange {
                    start: kernel_alias_l1,
                    size: PAGE_SIZE,
                },
                kernel_alias_l0,
                pt_node_pool,
            ];

            *self.bootstrap_pmap_info_mut() = Some(BootstrapPmapInfo {
                root,
                mapped: PhysRange {
                    start: PhysAddr(QEMU_RAM_BASE),
                    size: QEMU_BOOTSTRAP_MAP_SIZE,
                },
                direct_map_base: VirtAddr(DIRECT_MAP_BASE),
                direct_map: VirtRange {
                    start: VirtAddr(direct_map_virt(QEMU_RAM_BASE)),
                    size: QEMU_BOOTSTRAP_MAP_SIZE,
                },
                kernel_image: VirtRange {
                    start: VirtAddr(KERNEL_VIRT_BASE),
                    size: KERNEL_BOOTSTRAP_ALIAS_SIZE,
                },
                identity: Some(VirtRange {
                    start: VirtAddr(QEMU_RAM_BASE),
                    size: QEMU_BOOTSTRAP_MAP_SIZE,
                }),
                pt_node_pool,
                reserved_page_tables: &reserved_page_tables[..],
            });
        }
        self
    }

    #[cfg(target_arch = "riscv64")]
    pub(crate) fn require_current_high_sentinel_or_spin(self) -> Self {
        // Last guard before the boot path relies on the high alias: if the high
        // jump or sp/gp rewrite regresses, spin while low memory is still
        // mapped.
        match self.require_high_sentinel(HighSentinel::current()) {
            Ok(bag) => bag,
            Err(_) => loop {
                core::hint::spin_loop();
            },
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    pub(crate) fn finish_host_boot_without_mmu(self) -> BootStaticBag<IdentityDropped> {
        self.into_dropped()
    }

    pub(crate) fn complete_post_entry_pipeline(self) -> BootStaticBag<IdentityDropped> {
        #[cfg(target_arch = "riscv64")]
        {
            self.require_current_high_sentinel_or_spin().drop_lower()
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            self.finish_host_boot_without_mmu()
        }
    }

    #[cfg(test)]
    pub(crate) fn validate_high_values(
        &self,
        pc: usize,
        sp: usize,
        gp: usize,
    ) -> Result<&Self, HighSentinelError> {
        validate_high_sentinel(HighSentinel { pc, sp, gp })?;
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn drop_lower_after_high_values(
        self,
        pc: usize,
        sp: usize,
        gp: usize,
    ) -> Result<BootStaticBag<IdentityDropped>, HighSentinelError> {
        Ok(self
            .require_high_sentinel(HighSentinel { pc, sp, gp })?
            .drop_lower())
    }

    #[cfg(any(test, target_arch = "riscv64"))]
    fn require_high_sentinel(self, sentinel: HighSentinel) -> Result<Self, HighSentinelError> {
        validate_high_sentinel(sentinel)?;
        Ok(self)
    }

    #[cfg(any(test, target_arch = "riscv64"))]
    pub(crate) fn drop_lower(mut self) -> BootStaticBag<IdentityDropped> {
        self.drop_identity_bridge();
        self.into_dropped()
    }

    #[cfg(any(test, target_arch = "riscv64"))]
    fn drop_identity_bridge(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] = 0;

            if let Some(info) = self.bootstrap_pmap_info_mut().as_mut() {
                info.identity = None;
            }
        }
        sfence_vma_all();
        self
    }
}

// The sentinel is deliberately narrow: it only proves that control flow and the
// two static-ish registers that could still reference low addresses have crossed
// into the kernel alias range.
#[cfg(target_arch = "riscv64")]
impl HighSentinel {
    fn current() -> Self {
        let pc: usize;
        let sp: usize;
        let gp: usize;

        unsafe {
            core::arch::asm!("auipc {pc}, 0", pc = out(reg) pc, options(nomem, nostack));
            core::arch::asm!("mv {sp}, sp", sp = out(reg) sp, options(nomem, nostack));
            core::arch::asm!("mv {gp}, gp", gp = out(reg) gp, options(nomem, nostack));
        }

        Self { pc, sp, gp }
    }
}

#[cfg(any(test, target_arch = "riscv64"))]
fn validate_high_sentinel(sentinel: HighSentinel) -> Result<(), HighSentinelError> {
    if !is_high_kernel_alias(sentinel.pc) {
        return Err(HighSentinelError::ProgramCounter);
    }
    if !is_high_kernel_alias(sentinel.sp) {
        return Err(HighSentinelError::StackPointer);
    }
    if !is_high_kernel_alias(sentinel.gp) {
        return Err(HighSentinelError::GlobalPointer);
    }
    Ok(())
}

#[cfg(any(test, target_arch = "riscv64"))]
fn is_high_kernel_alias(value: usize) -> bool {
    (KERNEL_VIRT_BASE..KERNEL_VIRT_BASE + KERNEL_BOOTSTRAP_ALIAS_SIZE).contains(&value)
}

// Shared intermediate-table construction for kernel mappings. These helpers
// install branch PTEs speculatively and carry the new PT-node in the
// reservation token, so abandoned reservations can unwind without leaking a
// page-table frame.
fn ensure_l1_table_for_reservation<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Result<EnsuredTable, PmapError> {
    if let Some(table) = l1_table_mut(bag, virt) {
        return Ok(EnsuredTable { table, node: None });
    }

    let node = alloc_pt_node_from_bag(bag).map_err(|_| PmapError::Exhausted)?;
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] = encode_branch_pte(node.phys);
    }
    let Some(table) = l1_table_mut(bag, virt) else {
        unsafe {
            bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] = 0;
        }
        free_pt_node_from_bag(bag, node);
        return Err(PmapError::InvalidRequest);
    };
    Ok(EnsuredTable {
        table,
        node: Some(node),
    })
}

fn ensure_l0_table_for_reservation<State>(
    bag: &BootStaticBag<State>,
    l1: &mut PageTable,
    virt: VirtAddr,
) -> Result<EnsuredTable, PmapError> {
    if let Some(table) = l0_table_mut(l1, virt) {
        return Ok(EnsuredTable { table, node: None });
    }

    let index = rv64_2m_leaf_index(virt.0);
    if l1.0[index] != 0 {
        return Err(PmapError::AlreadyMapped);
    }

    let node = alloc_pt_node_from_bag(bag).map_err(|_| PmapError::Exhausted)?;
    l1.0[index] = encode_branch_pte(node.phys);
    let Some(table) = l0_table_mut(l1, virt) else {
        l1.0[index] = 0;
        free_pt_node_from_bag(bag, node);
        return Err(PmapError::InvalidRequest);
    };
    Ok(EnsuredTable {
        table,
        node: Some(node),
    })
}

fn rollback_intermediates_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    intermediates: PmapReservationIntermediates,
) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = l1_table_mut(bag, virt) {
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            if pte_is_branch(*slot) && pte_phys(*slot) == l0.phys {
                *slot = 0;
            }
        }
        free_pt_node_from_bag(bag, l0);
    }

    if let Some(l1) = intermediates.l1 {
        let slot = unsafe { &mut bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
        if pte_is_branch(*slot) && pte_phys(*slot) == l1.phys {
            *slot = 0;
        }
        free_pt_node_from_bag(bag, l1);
    }
}

// Table lookup helpers decode branch PTEs into direct-map pointers. They are
// intentionally small and shared by kernel-space operations and tests; process
// roots use their own root-relative variants in `address_space`.
fn l1_table_mut<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Option<&'static mut PageTable> {
    let pte = unsafe { bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
    if !pte_is_branch(pte) {
        return None;
    }
    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

fn l0_table_mut(l1: &mut PageTable, virt: VirtAddr) -> Option<&'static mut PageTable> {
    let pte = l1.0[rv64_2m_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }
    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

#[cfg(test)]
fn l1_table_for_test<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Option<&'static PageTable> {
    l1_table_mut(bag, virt).map(|table| &*table)
}

#[cfg(test)]
fn l0_table_for_test<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Option<&'static PageTable> {
    let l1 = l1_table_mut(bag, virt)?;
    l0_table_mut(l1, virt).map(|table| &*table)
}

// Low-level table maintenance and boot-pool pointer helpers. `sfence.vma` is a
// local invalidation for this v1 path; remote shootdown is still tracked as a
// later substrate blocker in progress memory.
pub(crate) fn sfence_vma_all() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("sfence.vma", options(nostack));
    }
}

pub(crate) fn sfence_vma_range_asid(virt: VirtAddr, size: usize, asid: tx_hal::Asid) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        let mut addr = virt.0;
        let end = virt.0.saturating_add(size);
        while addr < end {
            core::arch::asm!(
                "sfence.vma {addr}, {asid}",
                addr = in(reg) addr,
                asid = in(reg) asid.0 as usize,
                options(nostack)
            );
            addr = addr.saturating_add(PAGE_SIZE);
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = (virt, size, asid);
}

fn pool_index<State>(bag: &BootStaticBag<State>, phys: PhysAddr) -> Option<usize> {
    let range = bag.pt_node_pool_phys_range();
    let base = range.start.0;
    let end = range.end().0;
    if phys.0 < base || phys.0 >= end || !(phys.0 - base).is_multiple_of(PAGE_SIZE) {
        return None;
    }
    Some((phys.0 - base) / PAGE_SIZE)
}

fn pt_node_zero_ptr<State>(bag: &BootStaticBag<State>, index: usize) -> *mut u8 {
    #[cfg(target_arch = "riscv64")]
    {
        bag.pt_node_direct_va(index).0 as *mut u8
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        bag.pt_node_phys(index).0 as *mut u8
    }
}

unsafe fn zero_page(page: *mut u8) {
    core::ptr::write_bytes(page, 0, PAGE_SIZE);
}

#[cfg(test)]
pub(crate) fn reset_pt_node_pool_for_test() {
    pt_node::reset_pt_node_pool_allocations_for_test();
    address_space::reset_asids_for_test();
}

#[cfg(test)]
mod tests;
