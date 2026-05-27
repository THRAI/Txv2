//! M1 Dock mock bootstrap pmap and HAL pmap hooks.
//!
//! This module owns the QEMU/OpenSBI mock's Sv39 bootstrap facts: the high
//! direct-map topology, board-owned boot page tables, the bootstrap
//! `BootInfo`/`BootstrapPmapInfo` cells, and the small committed PT-node
//! registry used by kernel and process-root teardown.
//!
//! Main data-flow functions:
//! - `ensure_static_boot_facts()` publishes the immutable boot facts consumed
//!   by substrate.
//! - `reserve_kernel_mapping()` / `commit_kernel_mapping()` / rollback mutate
//!   high direct-map low-MMIO leaves through reservation tokens.
//! - `unmap_kernel_mapping()` / `protect_kernel_mapping()` / shootdown cover
//!   the current kernel mapping lifecycle.
//! - `create_pmap_root()` / `destroy_pmap_root()` allocate process roots, copy
//!   the kernel half, assign ASIDs, and release committed user intermediates.
//! - `reserve_mapping()` / `commit_mapping()` / rollback plus protect/unmap
//!   provide the user-root HAL surface needed before VM materialization.
//!
//! The module intentionally remains board-private: it follows the shared
//! `PmapIf` shape without lifting M1's static boot tables into a shared bag
//! abstraction.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use tx_hal::{
    AllocError, Asid, BootInfo, BootstrapPmapInfo, MemoryRegion, MemoryRegionKind, PhysAddr,
    PhysRange, PmapError, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReservationIntermediates, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode,
    PtNodeAllocator, VirtAddr, VirtRange,
};

pub(crate) const QEMU_RAM_BASE: usize = 0x8000_0000;
pub(crate) const QEMU_RAM_SIZE: usize = 256 * 1024 * 1024;
pub(crate) const QEMU_KERNEL_LOAD_BASE: usize = 0x8020_0000;
pub(crate) const QEMU_LOADER_RESERVED_SIZE: usize = QEMU_KERNEL_LOAD_BASE - QEMU_RAM_BASE;
pub(crate) const DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;
pub(crate) const DIRECT_MAP_SIZE: usize = 128 * 1024 * 1024 * 1024;
pub(crate) const KERNEL_VIRT_BASE: usize = 0xffff_ffff_8020_0000;
#[cfg(target_arch = "riscv64")]
const KERNEL_VIRT_OFFSET: usize = KERNEL_VIRT_BASE - QEMU_KERNEL_LOAD_BASE;
pub(crate) const SV39_USER_TOP: usize = 0x0000_0040_0000_0000;
pub(crate) const USER_RESERVED_TOP_SIZE: usize = 4 * 1024 * 1024;
pub(crate) const SV39_USER_ALLOC_TOP: usize = SV39_USER_TOP - USER_RESERVED_TOP_SIZE;
pub(crate) const BOOT_STACK_SIZE: usize = 64 * 1024;
pub(crate) const CACHE_LINE_SIZE: usize = 64;
pub(crate) const SV39_VIRT_ADDR_BITS: u8 = 39;
pub(crate) const RV64_PHYS_ADDR_BITS: u8 = 56;
pub(crate) const RV64_ASID_BITS: u8 = 16;
pub(crate) const QEMU_UART0_BASE: usize = 0x1000_0000;
pub(crate) const QEMU_UART0_SIZE: usize = 0x100;
const QEMU_LOW_MMIO_IDENTITY_END: usize = 0x4000_0000;
#[cfg(test)]
const SUPERPAGE_2M_SIZE: usize = 2 * 1024 * 1024;
const PAGE_SIZE: usize = 4096;
const PTE_V: usize = 1 << 0;
const PTE_R: usize = 1 << 1;
const PTE_W: usize = 1 << 2;
const PTE_X: usize = 1 << 3;
const PTE_U: usize = 1 << 4;
const PTE_G: usize = 1 << 5;
const PTE_A: usize = 1 << 6;
const PTE_D: usize = 1 << 7;
const COMMITTED_PT_NODE_REGISTRY_ENTRIES: usize = 32;

pub(crate) const fn direct_map_virt(phys: usize) -> usize {
    DIRECT_MAP_BASE + phys
}

static MEMORY_REGIONS: [MemoryRegion; 2] = [
    MemoryRegion {
        base: PhysAddr(QEMU_RAM_BASE),
        size: QEMU_RAM_SIZE,
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(QEMU_RAM_BASE),
        size: QEMU_LOADER_RESERVED_SIZE,
        kind: MemoryRegionKind::Reserved,
    },
];

struct BootInfoCell(UnsafeCell<BootInfo>);

unsafe impl Sync for BootInfoCell {}

static BOOT_INFO: BootInfoCell = BootInfoCell(UnsafeCell::new(BootInfo::empty()));

struct BootstrapPmapInfoCell(UnsafeCell<Option<BootstrapPmapInfo>>);

unsafe impl Sync for BootstrapPmapInfoCell {}

#[repr(C, align(4096))]
struct PageTable([usize; 512]);

impl PageTable {
    const fn zero() -> Self {
        Self([0; 512])
    }
}

struct PageTableCell(UnsafeCell<PageTable>);

unsafe impl Sync for PageTableCell {}

struct ReservedPageTablesCell(UnsafeCell<[PhysRange; 3]>);

unsafe impl Sync for ReservedPageTablesCell {}

struct EnsuredTable {
    table: &'static mut PageTable,
    node: Option<PtNode>,
}

struct CommittedPtNodeRegistry(UnsafeCell<[Option<PtNode>; COMMITTED_PT_NODE_REGISTRY_ENTRIES]>);

unsafe impl Sync for CommittedPtNodeRegistry {}

struct RegistryGuard;

static BOOT_FACTS_STATE: AtomicU8 = AtomicU8::new(0);
#[cfg(test)]
static BOOT_PMAP_STATE: AtomicU8 = AtomicU8::new(0);
#[cfg(not(test))]
static BOOT_PMAP_STATE: AtomicU8 = AtomicU8::new(2);
static BOOTSTRAP_PMAP_INFO: BootstrapPmapInfoCell = BootstrapPmapInfoCell(UnsafeCell::new(None));
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.m1dock_mock_root"
)]
static BOOT_ROOT: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.m1dock_mock_low_l1"
)]
static BOOT_LOW_L1: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
#[cfg_attr(
    target_arch = "riscv64",
    link_section = ".bss.boot.pagetable.m1dock_mock_uart_l0"
)]
static BOOT_UART_L0: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
static BOOT_RESERVED_PAGE_TABLES: ReservedPageTablesCell =
    ReservedPageTablesCell(UnsafeCell::new([PhysRange::empty(); 3]));
static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_ASIDS: AtomicU64 = AtomicU64::new(1);
static COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
static COMMITTED_PT_NODES: CommittedPtNodeRegistry =
    CommittedPtNodeRegistry(UnsafeCell::new([None; COMMITTED_PT_NODE_REGISTRY_ENTRIES]));

pub(crate) fn ensure_static_boot_facts() {
    loop {
        match BOOT_FACTS_STATE.load(Ordering::Acquire) {
            2 => return,
            0 => {
                if BOOT_FACTS_STATE
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    publish_static_boot_facts();
                    BOOT_FACTS_STATE.store(2, Ordering::Release);
                    return;
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

pub(crate) fn boot_info() -> &'static BootInfo {
    ensure_static_boot_facts();

    unsafe { &*BOOT_INFO.0.get() }
}

pub(crate) fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
    ensure_static_boot_facts();

    unsafe { (&*BOOTSTRAP_PMAP_INFO.0.get()).as_ref() }
}

pub(crate) fn alloc_pt_node() -> Result<PtNode, AllocError> {
    let Some(allocator) = installed_pt_node_allocator() else {
        return Err(AllocError::Exhausted);
    };

    allocator()
}

pub(crate) fn free_pt_node(node: PtNode) {
    unsafe {
        let _ = node.release_typed_frame();
    }
}

pub(crate) fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
    let value = allocator as usize;
    INSTALLED_PT_NODE_ALLOCATOR
        .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| PmapError::AlreadyMapped)
}

pub(crate) fn reserve_kernel_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    ensure_boot_pmap_tables();

    if kind != PmapReserveKind::Page4K || !identity_low_mmio_mapping(virt, phys) {
        return Err(PmapError::Unsupported);
    }
    if !virt.0.is_multiple_of(PAGE_SIZE) || !phys.0.is_multiple_of(PAGE_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    let ensured_l1 = ensure_l1_table_for_reservation(virt)?;
    let l1_node = ensured_l1.node;
    let ensured_l0 = match ensure_l0_table_for_reservation(ensured_l1.table, virt) {
        Ok(table) => table,
        Err(error) => {
            rollback_intermediates(
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
        l0: ensured_l0.node,
    };
    let expected = leaf_pte(phys, PTE_R | PTE_W | PTE_G | PTE_A | PTE_D);
    let current = ensured_l0.table.0[rv64_4k_leaf_index(virt.0)];
    if current == expected {
        rollback_intermediates(virt, intermediates);
        return Ok(None);
    }
    if current != 0 {
        rollback_intermediates(virt, intermediates);
        return Err(PmapError::AlreadyMapped);
    }

    Ok(Some(PmapReservation::new_with_intermediates(
        virt,
        phys,
        kind,
        intermediates,
    )))
}

pub(crate) fn rollback_kernel_mapping(reservation: PmapReservation) {
    rollback_intermediates(reservation.virt(), reservation.intermediates());
}

pub(crate) fn commit_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
    assert_eq!(reservation.kind(), PmapReserveKind::Page4K);
    assert!(identity_low_mmio_mapping(
        reservation.virt(),
        reservation.phys()
    ));
    validate_kernel_leaf_permissions(permissions).expect("valid kernel leaf permissions");
    let pte = leaf_pte_with_permissions(reservation.phys(), permissions);
    let l0 = l0_table_for_virt(reservation.virt()).expect("reserved L0 table must exist");
    register_committed_intermediates(reservation.intermediates());
    l0.0[rv64_4k_leaf_index(reservation.virt().0)] = pte;
}

pub(crate) fn unmap_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    ensure_boot_pmap_tables();

    let Some(expected_phys) = high_mmio_phys_for_virt(virt) else {
        return Err(PmapError::Unsupported);
    };
    if kind != PmapReserveKind::Page4K {
        return Err(PmapError::Unsupported);
    }
    if !virt.0.is_multiple_of(PAGE_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    let Some(l0) = l0_table_for_virt(virt) else {
        return Ok(None);
    };
    let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = pte_phys(current);
    if phys != expected_phys || !phys.0.is_multiple_of(PAGE_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    *slot = 0;
    prune_empty_l0_table(virt);
    Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
}

pub(crate) fn protect_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    ensure_boot_pmap_tables();
    validate_kernel_leaf_permissions(permissions)?;

    let Some(expected_phys) = high_mmio_phys_for_virt(virt) else {
        return Err(PmapError::Unsupported);
    };
    if kind != PmapReserveKind::Page4K {
        return Err(PmapError::Unsupported);
    }
    if !virt.0.is_multiple_of(PAGE_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    let Some(l0) = l0_table_for_virt(virt) else {
        return Ok(None);
    };
    let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = pte_phys(current);
    if phys != expected_phys || !phys.0.is_multiple_of(PAGE_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    let updated = leaf_pte_with_permissions(phys, permissions);
    if current == updated {
        return Ok(None);
    }
    *slot = updated;
    Ok(Some(PmapInvalidation::new(virt, PAGE_SIZE)))
}

pub(crate) fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
    let _ = invalidation;
    sfence_vma_all();
}

pub(crate) fn create_pmap_root() -> Result<PmapRoot, PmapError> {
    let asid = alloc_asid()?;
    let node = match alloc_pt_node() {
        Ok(node) => node,
        Err(_) => {
            free_asid(asid);
            return Err(PmapError::Exhausted);
        }
    };

    let root = unsafe { page_table_mut_from_phys(node.phys) };
    root.0.fill(0);
    let boot_root = unsafe { boot_root_mut() };
    root.0[256..].copy_from_slice(&boot_root.0[256..]);

    Ok(PmapRoot::new(node, asid))
}

pub(crate) fn destroy_pmap_root(root: PmapRoot) {
    let table = unsafe { page_table_mut_from_phys(root.phys()) };
    for slot in &mut table.0[..256] {
        if pte_is_branch(*slot) {
            release_page_table_tree(pte_phys(*slot));
        }
        *slot = 0;
    }
    sfence_vma_all();
    free_asid(root.asid());
    free_pt_node_to_source(root.into_node());
}

pub(crate) fn reserve_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_mapping(virt, phys, kind.size())?;

    let root = unsafe { page_table_mut_from_phys(root.phys()) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &root.0[rv64_1g_leaf_index(virt.0)];
            if *slot != 0 {
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new(virt, phys, kind)))
        }
        PmapReserveKind::Superpage2M => {
            let ensured_l1 = ensure_l1_table_for_root(root, virt)?;
            let current = ensured_l1.table.0[rv64_2m_leaf_index(virt.0)];
            if current != 0 {
                rollback_intermediates_in_root(
                    root,
                    virt,
                    PmapReservationIntermediates {
                        l2: None,
                        l1: ensured_l1.node,
                        l0: None,
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
                    l1: ensured_l1.node,
                    l0: None,
                },
            )))
        }
        PmapReserveKind::Page4K => {
            let ensured_l1 = ensure_l1_table_for_root(root, virt)?;
            let l1_node = ensured_l1.node;
            let ensured_l0 = match ensure_l0_table_for_reservation(ensured_l1.table, virt) {
                Ok(table) => table,
                Err(error) => {
                    rollback_intermediates_in_root(
                        root,
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
                l0: ensured_l0.node,
            };
            let current = ensured_l0.table.0[rv64_4k_leaf_index(virt.0)];
            if current != 0 {
                rollback_intermediates_in_root(root, virt, intermediates);
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new_with_intermediates(
                virt,
                phys,
                kind,
                intermediates,
            )))
        }
    }
}

pub(crate) fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
    let root = unsafe { page_table_mut_from_phys(root.phys()) };
    rollback_intermediates_in_root(root, reservation.virt(), reservation.intermediates());
    sfence_vma_all();
}

pub(crate) fn commit_mapping(
    root: &PmapRoot,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    validate_user_leaf_permissions(permissions).expect("invalid process pmap permissions");
    register_committed_intermediates(reservation.intermediates());

    let pte = leaf_pte_with_permissions(reservation.phys(), permissions);
    let root = unsafe { page_table_mut_from_phys(root.phys()) };
    match reservation.kind() {
        PmapReserveKind::Superpage1G => {
            root.0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Superpage2M => {
            let l1 = l1_table_for_root(root, reservation.virt()).expect("reserved L1 table");
            l1.0[rv64_2m_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Page4K => {
            let l1 = l1_table_for_root(root, reservation.virt()).expect("reserved L1 table");
            let l0 = l0_table_for_root_l1(l1, reservation.virt()).expect("reserved L0 table");
            l0.0[rv64_4k_leaf_index(reservation.virt().0)] = pte;
        }
    }
    sfence_vma_all();
}

pub(crate) fn unmap_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;

    let root = unsafe { page_table_mut_from_phys(root.phys()) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
            unmap_leaf_slot(slot, virt, kind)
        }
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_for_root(root, virt) else {
                return Ok(None);
            };
            let result = {
                let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)?
            };
            if result.is_some() {
                prune_empty_l1_table_in_root(root, virt);
            }
            Ok(result)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_for_root(root, virt) else {
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
                prune_empty_l0_table_in_root(root, virt);
                prune_empty_l1_table_in_root(root, virt);
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
    validate_user_leaf_permissions(permissions)?;
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;

    let root = unsafe { page_table_mut_from_phys(root.phys()) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_for_root(root, virt) else {
                return Ok(None);
            };
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_for_root(root, virt) else {
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

pub(crate) fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
    let _ = (asid, invalidation);
    sfence_vma_all();
}

fn sfence_vma_all() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("sfence.vma", options(nostack));
    }
}

fn publish_static_boot_facts() {
    ensure_boot_pmap_tables();
    let kernel_image = kernel_image_phys_range();
    let reserved_page_tables = reserved_page_tables();

    unsafe {
        *BOOT_INFO.0.get() = BootInfo {
            memory_regions: &MEMORY_REGIONS,
            kernel_image,
            initrd: None,
            cmdline: None,
        };

        *BOOTSTRAP_PMAP_INFO.0.get() = Some(BootstrapPmapInfo {
            root: boot_root_phys(),
            mapped: qemu_ram_phys_range(),
            direct_map_base: VirtAddr(DIRECT_MAP_BASE),
            direct_map: qemu_ram_virt_range(),
            kernel_image: VirtRange {
                start: VirtAddr(KERNEL_VIRT_BASE),
                size: kernel_image.size,
            },
            identity: Some(VirtRange {
                start: VirtAddr(QEMU_RAM_BASE),
                size: QEMU_RAM_SIZE,
            }),
            pt_node_pool: PhysRange::empty(),
            reserved_page_tables,
        });
    }
}

fn ensure_boot_pmap_tables() {
    loop {
        match BOOT_PMAP_STATE.load(Ordering::Acquire) {
            2 => return,
            0 => {
                if BOOT_PMAP_STATE
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    initialize_boot_pmap_tables();
                    BOOT_PMAP_STATE.store(2, Ordering::Release);
                    return;
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

fn initialize_boot_pmap_tables() {
    unsafe {
        let root = boot_root_mut();
        let low_l1 = boot_low_l1_mut();
        let uart_l0 = boot_uart_l0_mut();

        root.0.fill(0);
        low_l1.0.fill(0);
        uart_l0.0.fill(0);

        root.0[rv64_1g_leaf_index(QEMU_RAM_BASE)] = leaf_pte(
            PhysAddr(QEMU_RAM_BASE),
            PTE_R | PTE_W | PTE_X | PTE_A | PTE_D,
        );
        root.0[rv64_1g_leaf_index(direct_map_virt(QEMU_RAM_BASE))] = leaf_pte(
            PhysAddr(QEMU_RAM_BASE),
            PTE_R | PTE_W | PTE_G | PTE_A | PTE_D,
        );
        root.0[rv64_1g_leaf_index(KERNEL_VIRT_BASE)] = leaf_pte(
            PhysAddr(QEMU_RAM_BASE),
            PTE_R | PTE_W | PTE_X | PTE_G | PTE_A | PTE_D,
        );
        root.0[rv64_1g_leaf_index(direct_map_virt(QEMU_UART0_BASE))] =
            branch_pte(boot_low_l1_phys());
        low_l1.0[rv64_2m_leaf_index(direct_map_virt(QEMU_UART0_BASE))] =
            branch_pte(boot_uart_l0_phys());
        uart_l0.0[rv64_4k_leaf_index(direct_map_virt(QEMU_UART0_BASE))] = leaf_pte(
            PhysAddr(QEMU_UART0_BASE),
            PTE_R | PTE_W | PTE_G | PTE_A | PTE_D,
        );
    }
}

const fn qemu_ram_phys_range() -> PhysRange {
    PhysRange {
        start: PhysAddr(QEMU_RAM_BASE),
        size: QEMU_RAM_SIZE,
    }
}

const fn qemu_ram_virt_range() -> VirtRange {
    VirtRange {
        start: VirtAddr(direct_map_virt(QEMU_RAM_BASE)),
        size: QEMU_RAM_SIZE,
    }
}

fn kernel_image_phys_range() -> PhysRange {
    #[cfg(target_arch = "riscv64")]
    {
        let start = core::ptr::addr_of!(__kernel_start) as usize;
        let end = core::ptr::addr_of!(__kernel_end) as usize;
        let start = start.saturating_sub(KERNEL_VIRT_OFFSET);
        let end = end.saturating_sub(KERNEL_VIRT_OFFSET);
        PhysRange {
            start: PhysAddr(start),
            size: end.saturating_sub(start),
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        PhysRange {
            start: PhysAddr(QEMU_KERNEL_LOAD_BASE),
            size: 128 * 1024,
        }
    }
}

fn installed_pt_node_allocator() -> Option<PtNodeAllocator> {
    let value = INSTALLED_PT_NODE_ALLOCATOR.load(Ordering::Acquire);
    if value == 0 {
        return None;
    }

    Some(unsafe { core::mem::transmute::<usize, PtNodeAllocator>(value) })
}

fn identity_low_mmio_mapping(virt: VirtAddr, phys: PhysAddr) -> bool {
    high_mmio_mapping(virt, phys)
}

fn high_mmio_mapping(virt: VirtAddr, phys: PhysAddr) -> bool {
    phys.0 < QEMU_LOW_MMIO_IDENTITY_END && virt.0 == direct_map_virt(phys.0)
}

fn high_mmio_phys_for_virt(virt: VirtAddr) -> Option<PhysAddr> {
    if virt.0 < DIRECT_MAP_BASE {
        return None;
    }

    let phys = virt.0 - DIRECT_MAP_BASE;
    if phys < QEMU_LOW_MMIO_IDENTITY_END {
        Some(PhysAddr(phys))
    } else {
        None
    }
}

fn ensure_l1_table_for_reservation(virt: VirtAddr) -> Result<EnsuredTable, PmapError> {
    let index = rv64_1g_leaf_index(virt.0);
    let current = unsafe { boot_root_mut().0[index] };
    if current == 0 {
        let (node, table) = alloc_pt_node_table()?;
        unsafe {
            boot_root_mut().0[index] = branch_pte(node.phys);
        }
        return Ok(EnsuredTable {
            table,
            node: Some(node),
        });
    }
    if pte_is_branch(current) {
        return Ok(EnsuredTable {
            table: unsafe { page_table_mut_from_phys(pte_phys(current)) },
            node: None,
        });
    }

    Err(PmapError::AlreadyMapped)
}

fn ensure_l0_table_for_reservation(
    l1: &'static mut PageTable,
    virt: VirtAddr,
) -> Result<EnsuredTable, PmapError> {
    let index = rv64_2m_leaf_index(virt.0);
    let current = l1.0[index];
    if current == 0 {
        let (node, table) = alloc_pt_node_table()?;
        l1.0[index] = branch_pte(node.phys);
        return Ok(EnsuredTable {
            table,
            node: Some(node),
        });
    }
    if pte_is_branch(current) {
        return Ok(EnsuredTable {
            table: unsafe { page_table_mut_from_phys(pte_phys(current)) },
            node: None,
        });
    }

    Err(PmapError::AlreadyMapped)
}

fn alloc_pt_node_table() -> Result<(PtNode, &'static mut PageTable), PmapError> {
    let node = alloc_pt_node().map_err(|_| PmapError::Exhausted)?;
    let table = unsafe { page_table_mut_from_phys(node.phys) };
    table.0.fill(0);
    Ok((node, table))
}

fn rollback_intermediates(virt: VirtAddr, intermediates: PmapReservationIntermediates) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = l1_table_for_virt(virt) {
            let index = rv64_2m_leaf_index(virt.0);
            if l1.0[index] == branch_pte(l0.phys) {
                l1.0[index] = 0;
            }
        }
        free_pt_node_to_source(l0);
    }

    if let Some(l1) = intermediates.l1 {
        let root_index = rv64_1g_leaf_index(virt.0);
        unsafe {
            let root = boot_root_mut();
            if root.0[root_index] == branch_pte(l1.phys) {
                root.0[root_index] = 0;
            }
        }
        free_pt_node_to_source(l1);
    }
}

fn l1_table_for_virt(virt: VirtAddr) -> Option<&'static mut PageTable> {
    let pte = unsafe { boot_root_mut().0[rv64_1g_leaf_index(virt.0)] };
    if !pte_is_branch(pte) {
        return None;
    }

    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

fn l0_table_for_virt(virt: VirtAddr) -> Option<&'static mut PageTable> {
    let l1 = l1_table_for_virt(virt)?;
    let pte = l1.0[rv64_2m_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }

    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

fn alloc_asid() -> Result<Asid, PmapError> {
    loop {
        let allocated = ALLOCATED_ASIDS.load(Ordering::Acquire);
        for asid in 1..u64::BITS {
            let bit = 1u64 << asid;
            if allocated & bit != 0 {
                continue;
            }
            if ALLOCATED_ASIDS
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
        if allocated == u64::MAX {
            return Err(PmapError::Exhausted);
        }
    }
}

fn free_asid(asid: Asid) {
    if asid.0 == 0 || asid.0 as u32 >= u64::BITS {
        return;
    }
    ALLOCATED_ASIDS.fetch_and(!(1u64 << asid.0), Ordering::AcqRel);
}

fn validate_aligned_mapping(virt: VirtAddr, phys: PhysAddr, size: usize) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) || !phys.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

fn validate_aligned_virt(virt: VirtAddr, size: usize) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

fn validate_user_mapping_virt(virt: VirtAddr, kind: PmapReserveKind) -> Result<(), PmapError> {
    let end = virt
        .0
        .checked_add(kind.size())
        .ok_or(PmapError::InvalidRequest)?;
    if end > SV39_USER_ALLOC_TOP {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

fn ensure_l1_table_for_root(
    root: &mut PageTable,
    virt: VirtAddr,
) -> Result<EnsuredTable, PmapError> {
    if let Some(table) = l1_table_for_root(root, virt) {
        return Ok(EnsuredTable { table, node: None });
    }

    let index = rv64_1g_leaf_index(virt.0);
    if root.0[index] != 0 {
        return Err(PmapError::AlreadyMapped);
    }

    let (node, table) = alloc_pt_node_table()?;
    root.0[index] = branch_pte(node.phys);
    Ok(EnsuredTable {
        table,
        node: Some(node),
    })
}

fn rollback_intermediates_in_root(
    root: &mut PageTable,
    virt: VirtAddr,
    intermediates: PmapReservationIntermediates,
) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = l1_table_for_root(root, virt) {
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            if pte_is_branch(*slot) && pte_phys(*slot) == l0.phys {
                *slot = 0;
            }
        }
        free_pt_node_to_source(l0);
    }

    if let Some(l1) = intermediates.l1 {
        let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
        if pte_is_branch(*slot) && pte_phys(*slot) == l1.phys {
            *slot = 0;
        }
        free_pt_node_to_source(l1);
    }
}

fn l1_table_for_root(root: &mut PageTable, virt: VirtAddr) -> Option<&'static mut PageTable> {
    let pte = root.0[rv64_1g_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }

    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

fn l0_table_for_root_l1(l1: &mut PageTable, virt: VirtAddr) -> Option<&'static mut PageTable> {
    let pte = l1.0[rv64_2m_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }

    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

fn unmap_leaf_slot(
    slot: &mut usize,
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

fn protect_leaf_slot(
    slot: &mut usize,
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

    let updated = leaf_pte_with_permissions(phys, permissions);
    if current == updated {
        return Ok(None);
    }
    *slot = updated;
    Ok(Some(PmapInvalidation::new(virt, kind.size())))
}

fn prune_empty_l0_table_in_root(root: &mut PageTable, virt: VirtAddr) {
    let Some(l1) = l1_table_for_root(root, virt) else {
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
    release_committed_pt_node(phys);
}

fn prune_empty_l1_table_in_root(root: &mut PageTable, virt: VirtAddr) {
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
    release_committed_pt_node(phys);
}

fn release_page_table_tree(phys: PhysAddr) {
    let table = unsafe { page_table_mut_from_phys(phys) };
    for slot in table.0.iter_mut() {
        if pte_is_branch(*slot) {
            release_page_table_tree(pte_phys(*slot));
        }
        *slot = 0;
    }
    release_committed_pt_node(phys);
}

fn free_pt_node_to_source(node: PtNode) {
    unsafe {
        let _ = node.release_typed_frame();
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        COMMITTED_PT_NODE_REGISTRY_LOCK.store(false, Ordering::Release);
    }
}

fn lock_committed_pt_node_registry() -> RegistryGuard {
    while COMMITTED_PT_NODE_REGISTRY_LOCK
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        core::hint::spin_loop();
    }

    RegistryGuard
}

fn register_committed_intermediates(intermediates: PmapReservationIntermediates) {
    if let Some(l1) = intermediates.l1 {
        register_committed_pt_node(l1);
    }
    if let Some(l0) = intermediates.l0 {
        register_committed_pt_node(l0);
    }
}

fn register_committed_pt_node(node: PtNode) {
    let _guard = lock_committed_pt_node_registry();
    let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };
    for slot in nodes.iter() {
        if slot.is_some_and(|committed| committed.phys == node.phys) {
            return;
        }
    }
    for slot in nodes.iter_mut() {
        if slot.is_none() {
            *slot = Some(node);
            return;
        }
    }

    panic!("M1 mock committed PT-node registry exhausted");
}

fn release_committed_pt_node(phys: PhysAddr) {
    let Some(node) = take_committed_pt_node(phys) else {
        return;
    };

    free_pt_node_to_source(node);
}

fn take_committed_pt_node(phys: PhysAddr) -> Option<PtNode> {
    let _guard = lock_committed_pt_node_registry();
    let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };
    for slot in nodes.iter_mut() {
        if slot.is_some_and(|committed| committed.phys == phys) {
            return slot.take();
        }
    }

    None
}

#[cfg(test)]
fn reset_committed_pt_nodes_for_test() {
    let mut released = [None; COMMITTED_PT_NODE_REGISTRY_ENTRIES];
    {
        let _guard = lock_committed_pt_node_registry();
        let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };
        for (released_slot, node_slot) in released.iter_mut().zip(nodes.iter_mut()) {
            *released_slot = node_slot.take();
        }
    }

    for node in released.into_iter().flatten() {
        free_pt_node_to_source(node);
    }
}

fn reserved_page_tables() -> &'static [PhysRange; 3] {
    unsafe {
        let tables = &mut *BOOT_RESERVED_PAGE_TABLES.0.get();
        *tables = [
            PhysRange {
                start: boot_root_phys(),
                size: PAGE_SIZE,
            },
            PhysRange {
                start: boot_low_l1_phys(),
                size: PAGE_SIZE,
            },
            PhysRange {
                start: boot_uart_l0_phys(),
                size: PAGE_SIZE,
            },
        ];
        tables
    }
}

fn leaf_pte(phys: PhysAddr, flags: usize) -> usize {
    (phys.0 >> 2) | flags | PTE_V
}

fn branch_pte(phys: PhysAddr) -> usize {
    (phys.0 >> 2) | PTE_V
}

fn pte_is_branch(pte: usize) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) == 0
}

fn pte_is_leaf(pte: usize) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) != 0
}

fn pte_phys(pte: usize) -> PhysAddr {
    PhysAddr((pte >> 10) << 12)
}

fn validate_kernel_leaf_permissions(permissions: PmapPermissions) -> Result<(), PmapError> {
    validate_leaf_permissions(permissions, false)
}

fn validate_user_leaf_permissions(permissions: PmapPermissions) -> Result<(), PmapError> {
    validate_leaf_permissions(permissions, true)
}

fn validate_leaf_permissions(
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

fn leaf_pte_with_permissions(phys: PhysAddr, permissions: PmapPermissions) -> usize {
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

    leaf_pte(phys, flags | PTE_A | PTE_D)
}

fn prune_empty_l0_table(virt: VirtAddr) {
    let Some(l1) = l1_table_for_virt(virt) else {
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
    release_committed_pt_node(phys);
    prune_empty_l1_table(virt);
}

fn prune_empty_l1_table(virt: VirtAddr) {
    let root_index = rv64_1g_leaf_index(virt.0);
    let slot = unsafe { &mut boot_root_mut().0[root_index] };
    if !pte_is_branch(*slot) {
        return;
    }

    let phys = pte_phys(*slot);
    let l1 = unsafe { page_table_mut_from_phys(phys) };
    if !page_table_is_empty(l1) {
        return;
    }

    *slot = 0;
    release_committed_pt_node(phys);
}

fn page_table_is_empty(table: &PageTable) -> bool {
    table.0.iter().all(|entry| *entry == 0)
}

fn rv64_1g_leaf_index(virt: usize) -> usize {
    (virt >> 30) & 0x1ff
}

fn rv64_2m_leaf_index(virt: usize) -> usize {
    (virt >> 21) & 0x1ff
}

fn rv64_4k_leaf_index(virt: usize) -> usize {
    (virt >> 12) & 0x1ff
}

#[cfg(target_arch = "riscv64")]
fn linked_to_phys(addr: usize) -> usize {
    addr - KERNEL_VIRT_OFFSET
}

#[cfg(target_arch = "riscv64")]
fn boot_root_phys() -> PhysAddr {
    PhysAddr(linked_to_phys(BOOT_ROOT.0.get() as usize))
}

#[cfg(not(target_arch = "riscv64"))]
fn boot_root_phys() -> PhysAddr {
    PhysAddr(BOOT_ROOT.0.get() as usize)
}

#[cfg(target_arch = "riscv64")]
fn boot_low_l1_phys() -> PhysAddr {
    PhysAddr(linked_to_phys(BOOT_LOW_L1.0.get() as usize))
}

#[cfg(not(target_arch = "riscv64"))]
fn boot_low_l1_phys() -> PhysAddr {
    PhysAddr(BOOT_LOW_L1.0.get() as usize)
}

#[cfg(target_arch = "riscv64")]
fn boot_uart_l0_phys() -> PhysAddr {
    PhysAddr(linked_to_phys(BOOT_UART_L0.0.get() as usize))
}

#[cfg(not(target_arch = "riscv64"))]
fn boot_uart_l0_phys() -> PhysAddr {
    PhysAddr(BOOT_UART_L0.0.get() as usize)
}

unsafe fn boot_root_mut() -> &'static mut PageTable {
    unsafe { &mut *BOOT_ROOT.0.get() }
}

unsafe fn boot_low_l1_mut() -> &'static mut PageTable {
    unsafe { &mut *BOOT_LOW_L1.0.get() }
}

unsafe fn boot_uart_l0_mut() -> &'static mut PageTable {
    unsafe { &mut *BOOT_UART_L0.0.get() }
}

#[cfg(target_arch = "riscv64")]
unsafe fn page_table_mut_from_phys(phys: PhysAddr) -> &'static mut PageTable {
    unsafe { &mut *(direct_map_virt(phys.0) as *mut PageTable) }
}

#[cfg(not(target_arch = "riscv64"))]
unsafe fn page_table_mut_from_phys(phys: PhysAddr) -> &'static mut PageTable {
    unsafe { &mut *(phys.0 as *mut PageTable) }
}

#[cfg(target_arch = "riscv64")]
extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[cfg(test)]
#[path = "pmap_tests.rs"]
mod tests;
