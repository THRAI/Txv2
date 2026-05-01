use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};
use tx_hal::{
    BootInfoIf, MemoryRegionKind, PlatformConfig, PlatformInfoIf, PmapIf, PmapPermissions,
};

use crate::Platform;

const TEST_PT_NODE_PHYS: usize = 0x8800_0000;
const EXPECTED_DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;
const EXPECTED_KERNEL_VIRT_BASE: usize = 0xffff_ffff_8020_0000;
const EXPECTED_USER_TOP: usize = 0x0000_0040_0000_0000;
const EXPECTED_USER_RESERVED_TOP_SIZE: usize = 4 * 1024 * 1024;

static RELEASED_PT_NODE: AtomicUsize = AtomicUsize::new(0);
static RELEASED_PT_NODE_COUNT: AtomicUsize = AtomicUsize::new(0);
static TEST_PT_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static TEST_PMAP_NODE: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
static TEST_PROCESS_ROOT_NODE: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
static TEST_PROCESS_L1_NODE: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
static TEST_PROCESS_L0_NODE: PageTableCell = PageTableCell(UnsafeCell::new(PageTable::zero()));
static TEST_ALLOCATOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn boot_info_publishes_qemu_ram_gap_and_kernel_image() {
    let _guard = test_state_guard();
    let info = Platform::boot_info();

    assert_eq!(info.initrd, None);
    assert_eq!(info.cmdline, None);
    assert_eq!(info.kernel_image.start, PhysAddr(QEMU_KERNEL_LOAD_BASE));
    assert!(info.kernel_image.size > 0);

    assert_eq!(info.memory_regions.len(), 2);
    assert_eq!(info.memory_regions[0].base, PhysAddr(QEMU_RAM_BASE));
    assert_eq!(info.memory_regions[0].size, QEMU_RAM_SIZE);
    assert_eq!(info.memory_regions[0].kind, MemoryRegionKind::Usable);
    assert_eq!(info.memory_regions[1].base, PhysAddr(QEMU_RAM_BASE));
    assert_eq!(info.memory_regions[1].size, QEMU_LOADER_RESERVED_SIZE);
    assert_eq!(info.memory_regions[1].kind, MemoryRegionKind::Reserved);
}

#[test]
fn bootstrap_pmap_info_describes_identity_direct_ram() {
    let _guard = test_state_guard();
    let info = Platform::boot_info();
    let pmap = Platform::bootstrap_pmap_info().expect("bootstrap pmap info");

    assert_ne!(pmap.root, PhysAddr(0));
    assert_eq!(pmap.mapped, qemu_ram_phys_range());
    assert_eq!(pmap.direct_map_base, VirtAddr(DIRECT_MAP_BASE));
    assert_eq!(pmap.direct_map, qemu_ram_virt_range());
    assert_eq!(
        pmap.identity,
        Some(VirtRange {
            start: VirtAddr(QEMU_RAM_BASE),
            size: QEMU_RAM_SIZE,
        })
    );
    assert_eq!(pmap.kernel_image.start, VirtAddr(KERNEL_VIRT_BASE));
    assert_eq!(pmap.kernel_image.size, info.kernel_image.size);
    assert_eq!(pmap.pt_node_pool, PhysRange::empty());
    assert_eq!(pmap.reserved_page_tables.len(), 3);
    assert!(pmap
        .reserved_page_tables
        .iter()
        .all(|range| range.size == <Platform as PlatformConfig>::PAGE_SIZE));
}

#[test]
fn substrate_gate_is_enabled_and_uart_mmio_is_published() {
    let _guard = test_state_guard();
    let substrate_ready = core::hint::black_box(<Platform as PlatformConfig>::SUBSTRATE_BOOT_READY);
    assert!(substrate_ready);
    assert_eq!(Platform::platform_info().mmio_regions.len(), 1);
    assert_eq!(Platform::platform_info().mmio_regions[0].name, "uart0");
}

#[test]
fn high_half_platform_config_and_mmio_are_declared() {
    let _guard = test_state_guard();

    assert_eq!(
        <Platform as PlatformConfig>::DIRECT_MAP_BASE,
        VirtAddr(EXPECTED_DIRECT_MAP_BASE)
    );
    assert_eq!(
        <Platform as PlatformConfig>::KERNEL_VIRT_BASE,
        VirtAddr(EXPECTED_KERNEL_VIRT_BASE)
    );
    assert_eq!(
        <Platform as PlatformConfig>::USER_TOP,
        VirtAddr(EXPECTED_USER_TOP)
    );
    assert_eq!(
        <Platform as PlatformConfig>::USER_RESERVED_TOP_SIZE,
        EXPECTED_USER_RESERVED_TOP_SIZE
    );
    assert_eq!(
        <Platform as PlatformConfig>::USER_ALLOC_TOP,
        VirtAddr(EXPECTED_USER_TOP - EXPECTED_USER_RESERVED_TOP_SIZE)
    );
    assert_eq!(
        Platform::platform_info().mmio_regions[0].virt.start,
        VirtAddr(EXPECTED_DIRECT_MAP_BASE + QEMU_UART0_BASE)
    );
}

#[test]
fn identity_uart_mmio_page_is_reported_as_precovered() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();

    assert_eq!(
        Platform::reserve_kernel_mapping(
            VirtAddr(direct_map_virt(QEMU_UART0_BASE)),
            PhysAddr(QEMU_UART0_BASE),
            PmapReserveKind::Page4K,
        ),
        Ok(None)
    );
    assert_eq!(
        Platform::reserve_kernel_mapping(
            VirtAddr(direct_map_virt(QEMU_UART0_BASE + 0x1000)),
            PhysAddr(QEMU_UART0_BASE),
            PmapReserveKind::Page4K,
        ),
        Err(PmapError::Unsupported)
    );
}

#[test]
fn uart_neighbor_page_can_reserve_commit_and_become_precovered() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();

    let phys_value = QEMU_UART0_BASE + <Platform as PlatformConfig>::PAGE_SIZE;
    let virt = VirtAddr(direct_map_virt(phys_value));
    let phys = PhysAddr(phys_value);
    let reservation = Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K)
        .expect("reserve neighbor UART page")
        .expect("neighbor page should not already be mapped");

    assert_eq!(reservation.virt(), virt);
    assert_eq!(reservation.phys(), phys);
    assert_eq!(reservation.kind(), PmapReserveKind::Page4K);

    Platform::commit_kernel_mapping(reservation);
    assert_eq!(
        Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K),
        Ok(None)
    );
}

#[test]
fn pt_node_allocator_handoff_is_one_shot_and_releases_typed_frames() {
    let _guard = test_state_guard();
    reset_pt_node_allocator_for_test();
    RELEASED_PT_NODE.store(0, Ordering::Release);

    assert_eq!(Platform::alloc_pt_node(), Err(AllocError::Exhausted));
    assert_eq!(
        Platform::install_pt_node_allocator(test_pt_allocator),
        Ok(())
    );
    assert_eq!(
        Platform::install_pt_node_allocator(exhausted_pt_allocator),
        Err(PmapError::AlreadyMapped)
    );

    let node = Platform::alloc_pt_node().expect("installed allocator returns node");
    assert_eq!(node.phys, PhysAddr(TEST_PT_NODE_PHYS));

    Platform::free_pt_node(node);
    assert_eq!(RELEASED_PT_NODE.load(Ordering::Acquire), TEST_PT_NODE_PHYS);

    reset_pt_node_allocator_for_test();
}

#[test]
fn second_uart_window_allocates_l0_before_commit() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();
    reset_pt_node_allocator_for_test();
    TEST_PT_ALLOCATIONS.store(0, Ordering::Release);

    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_node_allocator),
        Ok(())
    );

    let phys_value = QEMU_UART0_BASE + SUPERPAGE_2M_SIZE;
    let virt = VirtAddr(direct_map_virt(phys_value));
    let phys = PhysAddr(phys_value);
    let reservation = Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K)
        .expect("reserve second UART window")
        .expect("second UART window should need a new leaf");

    assert_eq!(reservation.virt(), virt);
    assert_eq!(reservation.phys(), phys);
    assert_eq!(reservation.intermediates().l1, None);
    assert_eq!(
        reservation.intermediates().l0,
        Some(PtNode::typed_frame(
            PhysAddr(TEST_PMAP_NODE.0.get() as usize),
            release_test_pt_node,
        ))
    );
    assert_eq!(TEST_PT_ALLOCATIONS.load(Ordering::Acquire), 1);

    Platform::commit_kernel_mapping(reservation);
    assert_eq!(
        Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K),
        Ok(None)
    );

    reset_pt_node_allocator_for_test();
}

#[test]
fn unmap_prunes_committed_l0_and_releases_pt_node() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();
    reset_pt_node_allocator_for_test();
    RELEASED_PT_NODE.store(0, Ordering::Release);

    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_node_allocator),
        Ok(())
    );

    let phys_value = QEMU_UART0_BASE + SUPERPAGE_2M_SIZE;
    let virt = VirtAddr(direct_map_virt(phys_value));
    let phys = PhysAddr(phys_value);
    let reservation = Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K)
        .expect("reserve second UART window")
        .expect("second UART window should allocate L0");
    Platform::commit_kernel_mapping(reservation);

    let unmapped = Platform::unmap_kernel_mapping(virt, PmapReserveKind::Page4K)
        .expect("unmap second UART window")
        .expect("mapping should exist");

    assert_eq!(unmapped.virt(), virt);
    assert_eq!(unmapped.phys(), phys);
    assert_eq!(unmapped.kind(), PmapReserveKind::Page4K);
    assert_eq!(unmapped.invalidation().virt(), virt);
    assert_eq!(
        unmapped.invalidation().size(),
        <Platform as PlatformConfig>::PAGE_SIZE
    );
    assert_eq!(
        RELEASED_PT_NODE.load(Ordering::Acquire),
        TEST_PMAP_NODE.0.get() as usize
    );
    assert_eq!(
        Platform::unmap_kernel_mapping(virt, PmapReserveKind::Page4K),
        Ok(None)
    );

    reset_pt_node_allocator_for_test();
}

#[test]
fn protect_kernel_mapping_updates_leaf_and_reports_invalidation() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();

    let phys_value = QEMU_UART0_BASE + <Platform as PlatformConfig>::PAGE_SIZE;
    let virt = VirtAddr(direct_map_virt(phys_value));
    let phys = PhysAddr(phys_value);
    let reservation = Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K)
        .expect("reserve neighbor UART page")
        .expect("neighbor page should need a new leaf");
    Platform::commit_kernel_mapping(reservation);

    let invalidation =
        Platform::protect_kernel_mapping(virt, PmapReserveKind::Page4K, PmapPermissions::KERNEL_RO)
            .expect("protect neighbor UART page")
            .expect("permissions should change");

    assert_eq!(invalidation.virt(), virt);
    assert_eq!(invalidation.size(), <Platform as PlatformConfig>::PAGE_SIZE);
    assert_eq!(
        Platform::protect_kernel_mapping(virt, PmapReserveKind::Page4K, PmapPermissions::KERNEL_RO,),
        Ok(None)
    );
    Platform::shootdown_kernel_mapping(invalidation);
}

#[test]
fn process_root_copies_kernel_half_and_reuses_asid_after_destroy() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();
    reset_pt_node_allocator_for_test();
    reset_asids_for_test();
    TEST_PT_ALLOCATIONS.store(0, Ordering::Release);

    assert_eq!(
        Platform::install_pt_node_allocator(test_process_node_allocator),
        Ok(())
    );

    let root = Platform::create_pmap_root().expect("process root");
    assert_eq!(root.asid().0, 1);

    let root_table = unsafe { page_table_mut_from_phys(root.phys()) };
    let boot_root = unsafe { boot_root_mut() };
    assert_eq!(
        root_table.0[rv64_1g_leaf_index(direct_map_virt(QEMU_UART0_BASE))],
        boot_root.0[rv64_1g_leaf_index(direct_map_virt(QEMU_UART0_BASE))]
    );
    assert_eq!(
        root_table.0[rv64_1g_leaf_index(direct_map_virt(QEMU_RAM_BASE))],
        boot_root.0[rv64_1g_leaf_index(direct_map_virt(QEMU_RAM_BASE))]
    );
    assert_eq!(
        root_table.0[rv64_1g_leaf_index(KERNEL_VIRT_BASE)],
        boot_root.0[rv64_1g_leaf_index(KERNEL_VIRT_BASE)]
    );
    assert_eq!(root_table.0[0], 0);

    Platform::destroy_pmap_root(root);

    TEST_PT_ALLOCATIONS.store(0, Ordering::Release);
    let reused = Platform::create_pmap_root().expect("reused ASID root");
    assert_eq!(reused.asid().0, 1);
    Platform::destroy_pmap_root(reused);

    reset_pt_node_allocator_for_test();
}

#[test]
fn process_root_maps_protects_unmaps_and_prunes_user_tables() {
    let _guard = test_state_guard();
    reset_boot_pmap_for_test();
    reset_pt_node_allocator_for_test();
    reset_asids_for_test();
    reset_committed_pt_nodes_for_test();
    TEST_PT_ALLOCATIONS.store(0, Ordering::Release);
    RELEASED_PT_NODE_COUNT.store(0, Ordering::Release);

    assert_eq!(
        Platform::install_pt_node_allocator(test_process_node_allocator),
        Ok(())
    );

    let root = Platform::create_pmap_root().expect("process root");
    let virt = VirtAddr(0x4000);
    let phys = PhysAddr(QEMU_RAM_BASE + 16 * 1024 * 1024);
    let reservation = Platform::reserve_mapping(&root, virt, phys, PmapReserveKind::Page4K)
        .expect("reserve user page")
        .expect("empty user leaf");
    Platform::commit_mapping(
        &root,
        reservation,
        PmapPermissions::READ
            .union(PmapPermissions::WRITE)
            .union(PmapPermissions::USER),
    );

    let root_table = unsafe { page_table_mut_from_phys(root.phys()) };
    let l1_pte = root_table.0[rv64_1g_leaf_index(virt.0)];
    assert!(pte_is_branch(l1_pte));
    let l1 = unsafe { page_table_mut_from_phys(pte_phys(l1_pte)) };
    let l0_pte = l1.0[rv64_2m_leaf_index(virt.0)];
    assert!(pte_is_branch(l0_pte));
    let l0 = unsafe { page_table_mut_from_phys(pte_phys(l0_pte)) };
    let pte = l0.0[rv64_4k_leaf_index(virt.0)];
    assert_eq!(pte_phys(pte), phys);
    assert_eq!(pte & PTE_U, PTE_U);
    assert_eq!(pte & PTE_G, 0);

    let invalidation = Platform::protect_mapping(
        &root,
        virt,
        PmapReserveKind::Page4K,
        PmapPermissions::READ.union(PmapPermissions::USER),
    )
    .expect("protect user page")
    .expect("permission downgrade");
    assert_eq!(invalidation.virt(), virt);
    Platform::shootdown_mapping(root.asid(), invalidation);

    let result = Platform::unmap_mapping(&root, virt, PmapReserveKind::Page4K)
        .expect("unmap user page")
        .expect("mapped user page");
    assert_eq!(result.phys(), phys);
    assert_eq!(result.invalidation().virt(), virt);

    let root_table = unsafe { page_table_mut_from_phys(root.phys()) };
    assert_eq!(root_table.0[rv64_1g_leaf_index(virt.0)], 0);
    assert_eq!(
        RELEASED_PT_NODE_COUNT.load(Ordering::Acquire),
        2,
        "unmap should release committed L0 and L1 nodes"
    );

    Platform::destroy_pmap_root(root);
    assert_eq!(
        RELEASED_PT_NODE_COUNT.load(Ordering::Acquire),
        3,
        "destroy should release the process root node"
    );

    reset_pt_node_allocator_for_test();
}

fn test_pt_allocator() -> Result<PtNode, AllocError> {
    Ok(PtNode::typed_frame(
        PhysAddr(TEST_PT_NODE_PHYS),
        release_test_pt_node,
    ))
}

fn exhausted_pt_allocator() -> Result<PtNode, AllocError> {
    Err(AllocError::Exhausted)
}

fn test_pmap_node_allocator() -> Result<PtNode, AllocError> {
    TEST_PT_ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
    Ok(PtNode::typed_frame(
        PhysAddr(TEST_PMAP_NODE.0.get() as usize),
        release_test_pt_node,
    ))
}

fn test_process_node_allocator() -> Result<PtNode, AllocError> {
    let allocation = TEST_PT_ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
    let phys = match allocation {
        0 => TEST_PROCESS_ROOT_NODE.0.get() as usize,
        1 => TEST_PROCESS_L1_NODE.0.get() as usize,
        2 => TEST_PROCESS_L0_NODE.0.get() as usize,
        _ => return Err(AllocError::Exhausted),
    };
    Ok(PtNode::typed_frame(PhysAddr(phys), release_test_pt_node))
}

unsafe fn release_test_pt_node(phys: PhysAddr) {
    RELEASED_PT_NODE.store(phys.0, Ordering::Release);
    RELEASED_PT_NODE_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn reset_pt_node_allocator_for_test() {
    INSTALLED_PT_NODE_ALLOCATOR.store(0, Ordering::Release);
}

fn test_state_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_ALLOCATOR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reset_boot_pmap_for_test() {
    BOOT_PMAP_STATE.store(0, Ordering::Release);
    ensure_boot_pmap_tables();
    reset_committed_pt_nodes_for_test();
}

fn reset_asids_for_test() {
    ALLOCATED_ASIDS.store(1, Ordering::Release);
}
