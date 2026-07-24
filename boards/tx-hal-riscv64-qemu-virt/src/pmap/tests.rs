//! Unit tests for the RV64 QEMU pmap internals.
//!
//! Core test data structures/state:
//! - `PMAP_TEST_LOCK`: serializes tests that mutate global PT-node/ASID state.
//! - `TEST_TYPED_PT_TABLES`: host memory used as fake typed page-table frames.
//! - `TEST_TYPED_ALLOC_NEXT` / `TEST_TYPED_RELEASED`: counters for allocator
//!   handoff and teardown assertions.
//!
//! Main test data flow:
//! - `test_bag()` creates a host `BootStaticBag<IdentityLive>`.
//! - `map_bootstrap_pmap()` and `publish_bootstrap_bag()` drive the same boot
//!   pmap pipeline methods used by the RV64 assembly path.
//! - individual tests then exercise bootstrap aliasing, identity teardown,
//!   direct-map/MMIO mapping, typed PT-node fallback, root lifecycle, and
//!   protect/unmap retention.
//!
//! These stay as a private unit-test module instead of crate-level integration
//! tests because they intentionally inspect board-private PTEs, PT-node pool
//! state, and staged `BootStaticBag` transitions. See
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;

use tx_hal::{AllocError, PhysAddr, PmapError, PmapPermissions, PmapReserveKind, PtNode, VirtAddr};

use super::address_space::{
    coalesce_invalidation_ranges, commit_mapping_from_root, create_pmap_root_from_bag,
    destroy_pmap_root_from_bag, l1_table_mut_from_root, protect_mapping_from_root,
    reserve_mapping_from_root, unmap_mapping_from_root,
};
use super::kernel_space::{
    commit_direct_map_1g_from_bag, commit_kernel_mapping_from_bag,
    cover_boot_firmware_dtb_from_bag, extend_direct_map_from_bag, protect_kernel_mapping_from_bag,
    reserve_direct_map_1g_from_bag, reserve_kernel_mapping_from_bag,
    rollback_kernel_mapping_from_bag, unmap_kernel_mapping_from_bag,
};
use super::pt_node::{
    alloc_pt_node_from_bag, free_pt_node_from_bag, install_pt_node_allocator_for_test,
    pt_node_allocated_for_test, register_committed_pt_node, reset_committed_pt_nodes_for_test,
};
use super::pte::{
    encode_leaf_pte, page_table_mut_from_phys, pte_phys, PTE_A, PTE_D, PTE_G, PTE_R, PTE_U, PTE_V,
    PTE_W, PTE_X,
};
use super::topology::{
    bootstrap_satp_value, rv64_1g_leaf_index, rv64_2m_leaf_index, rv64_4k_leaf_index,
    DIRECT_MAP_BASE, PAGE_SIZE, QEMU_BOOTSTRAP_MAP_SIZE, QEMU_RAM_BASE, SV39_MODE,
    SV39_USER_ALLOC_TOP,
};
use super::{
    l0_table_for_test, l0_table_mut, l1_table_for_test, reset_pt_node_pool_for_test,
    shootdown_kernel_mapping, HighSentinelError, ASID_CAPACITY,
};
use crate::boot_static::{BootLinkedAddr, BootStaticBag, IdentityLive};

const EXPECTED_DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;
const EXPECTED_KERNEL_VIRT_BASE: usize = 0xffff_ffff_8020_0000;
const PTE_G_TEST: u64 = 1 << 5;

static PMAP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TEST_TYPED_ALLOC_NEXT: AtomicUsize = AtomicUsize::new(0);
static TEST_TYPED_RELEASED: AtomicUsize = AtomicUsize::new(0);
static TEST_ROOT_ALLOC_NEXT: AtomicUsize = AtomicUsize::new(0);
static TEST_ROOT_RELEASED: AtomicUsize = AtomicUsize::new(0);

/// Host-side table storage used by the fake typed PT-node allocator.
///
/// Tests hand out raw physical addresses that are actually host pointers, which
/// matches the non-RV64 `page_table_mut_from_phys` test path.
struct TestTypedPtTables(UnsafeCell<[crate::boot_static::PageTable; 2]>);

unsafe impl Sync for TestTypedPtTables {}

static TEST_TYPED_PT_TABLES: TestTypedPtTables = TestTypedPtTables(UnsafeCell::new(
    [crate::boot_static::PageTable([0; 512]); 2],
));

struct TestRootPtTables(UnsafeCell<[crate::boot_static::PageTable; ASID_CAPACITY]>);

unsafe impl Sync for TestRootPtTables {}

static TEST_ROOT_PT_TABLES: TestRootPtTables = TestRootPtTables(UnsafeCell::new(
    [crate::boot_static::PageTable([0; 512]); ASID_CAPACITY],
));

// Test fixtures construct a host `BootStaticBag`, then drive the same pmap
// pipeline methods the assembly boot path uses on RV64.
fn pmap_test_guard() -> std::sync::MutexGuard<'static, ()> {
    PMAP_TEST_LOCK.lock().expect("pmap test lock poisoned")
}

fn reset_typed_pt_allocator_test_state() {
    TEST_TYPED_ALLOC_NEXT.store(0, Ordering::Release);
    TEST_TYPED_RELEASED.store(0, Ordering::Release);
    TEST_ROOT_ALLOC_NEXT.store(0, Ordering::Release);
    TEST_ROOT_RELEASED.store(0, Ordering::Release);
    install_pt_node_allocator_for_test(None);
}

fn test_typed_pt_allocator() -> Result<PtNode, AllocError> {
    let index = TEST_TYPED_ALLOC_NEXT.fetch_add(1, Ordering::AcqRel);
    if index >= 2 {
        return Err(AllocError::Exhausted);
    }

    unsafe {
        let tables = &mut *TEST_TYPED_PT_TABLES.0.get();
        tables[index].0.fill(0);
        Ok(PtNode::typed_frame(
            PhysAddr((&mut tables[index]) as *mut _ as usize),
            test_typed_pt_release,
        ))
    }
}

unsafe fn test_typed_pt_release(_phys: PhysAddr) {
    TEST_TYPED_RELEASED.fetch_add(1, Ordering::AcqRel);
}

fn test_root_pt_allocator() -> Result<PtNode, AllocError> {
    let index = TEST_ROOT_ALLOC_NEXT.fetch_add(1, Ordering::AcqRel);
    if index >= ASID_CAPACITY {
        return Err(AllocError::Exhausted);
    }

    unsafe {
        let tables = &mut *TEST_ROOT_PT_TABLES.0.get();
        tables[index].0.fill(0);
        Ok(PtNode::typed_frame(
            PhysAddr((&mut tables[index]) as *mut _ as usize),
            test_root_pt_release,
        ))
    }
}

unsafe fn test_root_pt_release(_phys: PhysAddr) {
    TEST_ROOT_RELEASED.fetch_add(1, Ordering::AcqRel);
}

fn test_bag() -> BootStaticBag<IdentityLive> {
    BootStaticBag::<IdentityLive>::new_for_test(0)
}

fn map_bootstrap_pmap(bag: &mut BootStaticBag<IdentityLive>) -> &mut BootStaticBag<IdentityLive> {
    bag.begin_bootstrap_pmap()
        .map_identity_bridge()
        .map_direct_map_window()
        .map_kernel_high_alias()
}

fn publish_bootstrap_bag(
    bag: &mut BootStaticBag<IdentityLive>,
) -> &mut BootStaticBag<IdentityLive> {
    map_bootstrap_pmap(bag).publish_bootstrap_pmap_info()
}

#[test]
fn boot_linked_addr_explicitly_converts_across_boot_mappings() {
    let linked = BootLinkedAddr::from_linked(0x8020_0000);

    assert_eq!(linked.phys(), PhysAddr(0x8020_0000));
    assert_eq!(linked.identity_va(), VirtAddr(0x8020_0000));
    assert_eq!(
        linked.direct_va(),
        VirtAddr(EXPECTED_DIRECT_MAP_BASE + 0x8020_0000)
    );
    assert_eq!(
        linked.kernel_alias_va(),
        Some(VirtAddr(EXPECTED_KERNEL_VIRT_BASE))
    );
    assert_eq!(
        BootLinkedAddr::from_linked(0x8000_0000).kernel_alias_va(),
        None
    );
}

#[test]
fn boot_linked_addr_canonicalizes_runtime_high_aliases() {
    let linked = BootLinkedAddr::from_runtime_addr(EXPECTED_KERNEL_VIRT_BASE + 0x4000);

    assert_eq!(linked.phys(), PhysAddr(0x8020_4000));
    assert_eq!(
        linked.kernel_alias_va(),
        Some(VirtAddr(EXPECTED_KERNEL_VIRT_BASE + 0x4000))
    );
}

#[test]
fn high_boot_transition_converts_stack_gp_and_entry_to_kernel_aliases() {
    let transition = crate::boot_static::HighBootTransition::from_linked(
        BootLinkedAddr::from_linked(0x8020_1000),
        BootLinkedAddr::from_linked(0x8020_2000),
        BootLinkedAddr::from_linked(0x8020_3000),
    )
    .expect("high boot transition");

    assert_eq!(
        transition.stack_top,
        VirtAddr(EXPECTED_KERNEL_VIRT_BASE + 0x1000)
    );
    assert_eq!(
        transition.global_pointer,
        VirtAddr(EXPECTED_KERNEL_VIRT_BASE + 0x2000)
    );
    assert_eq!(
        transition.rust_entry,
        VirtAddr(EXPECTED_KERNEL_VIRT_BASE + 0x3000)
    );
    assert_eq!(
        crate::boot_static::HighBootTransition::from_linked(
            BootLinkedAddr::from_linked(0x8000_0000),
            BootLinkedAddr::from_linked(0x8020_2000),
            BootLinkedAddr::from_linked(0x8020_3000),
        ),
        None
    );
}

#[cfg(not(target_arch = "riscv64"))]
#[test]
fn captured_host_bag_has_no_high_transition_without_rv64_linker_symbols() {
    assert_eq!(test_bag().high_boot_transition(), None);
}

#[test]
fn identity_teardown_requires_high_pc_sp_and_gp() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let low_pc = QEMU_RAM_BASE + 0x1000;
    let high_sp = EXPECTED_KERNEL_VIRT_BASE + 0x2000;
    let high_gp = EXPECTED_KERNEL_VIRT_BASE + 0x3000;

    assert_eq!(
        bag.validate_high_values(low_pc, high_sp, high_gp)
            .map(|_| ()),
        Err(HighSentinelError::ProgramCounter)
    );
    assert_ne!(
        bag.bootstrap_root_ref().0[rv64_1g_leaf_index(QEMU_RAM_BASE)],
        0
    );
}

#[test]
fn identity_teardown_drops_low_leaf_and_updates_bootstrap_info() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let bag = bag
        .drop_lower_after_high_values(
            EXPECTED_KERNEL_VIRT_BASE + 0x1000,
            EXPECTED_KERNEL_VIRT_BASE + 0x2000,
            EXPECTED_KERNEL_VIRT_BASE + 0x3000,
        )
        .expect("identity teardown");

    assert_eq!(
        bag.bootstrap_root_ref().0[rv64_1g_leaf_index(QEMU_RAM_BASE)],
        0
    );
    assert_eq!(
        bag.bootstrap_pmap_info_ref().expect("pmap info").identity,
        None
    );
}

#[test]
fn encodes_sv39_1g_leaf_pte_for_qemu_ram() {
    let pte = encode_leaf_pte(PhysAddr(0x8000_0000), PTE_R | PTE_W | PTE_X);

    assert_eq!(pte & PTE_V, PTE_V);
    assert_eq!(pte & PTE_R, PTE_R);
    assert_eq!(pte & PTE_W, PTE_W);
    assert_eq!(pte & PTE_X, PTE_X);
    assert_eq!(pte & PTE_A, PTE_A);
    assert_eq!(pte & PTE_D, PTE_D);
    assert_eq!(pte >> 28, 0x2);
}

#[test]
fn sv39_root_slots_for_qemu_ram_and_direct_map_alias() {
    // QEMU RAM identity maps into root slot 2.
    assert_eq!(rv64_1g_leaf_index(0x8000_0000), 2);
    assert_eq!(rv64_1g_leaf_index(0x8020_0000), 2);

    // The high direct-map alias uses a distinct high root slot.
    let direct_map_qemu_ram = EXPECTED_DIRECT_MAP_BASE + QEMU_RAM_BASE;
    assert_eq!(rv64_1g_leaf_index(direct_map_qemu_ram), 258);
    assert_ne!(
        rv64_1g_leaf_index(direct_map_qemu_ram),
        rv64_1g_leaf_index(QEMU_RAM_BASE)
    );
}

#[test]
fn bootstrap_pmap_keeps_identity_and_adds_high_aliases() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    map_bootstrap_pmap(&mut bag);
    let root = bag.bootstrap_root_ref();

    let identity = root.0[rv64_1g_leaf_index(QEMU_RAM_BASE)];
    let direct_map = root.0[rv64_1g_leaf_index(EXPECTED_DIRECT_MAP_BASE + QEMU_RAM_BASE)];
    let kernel_alias = root.0[rv64_1g_leaf_index(EXPECTED_KERNEL_VIRT_BASE)];

    assert_eq!(
        identity,
        encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_X)
    );
    assert_eq!(
        direct_map,
        encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_G_TEST)
    );
    assert_ne!(kernel_alias, 0);
    assert_eq!(kernel_alias & (PTE_R | PTE_W | PTE_X), 0);
}

#[test]
fn kernel_alias_uses_4k_leaves_with_final_permissions() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    map_bootstrap_pmap(&mut bag);

    let l0 = l0_table_for_test(&bag, VirtAddr(EXPECTED_KERNEL_VIRT_BASE)).expect("kernel alias L0");
    let text = l0.0[rv64_4k_leaf_index(EXPECTED_KERNEL_VIRT_BASE)];
    let rodata = l0.0[rv64_4k_leaf_index(EXPECTED_KERNEL_VIRT_BASE + 0x2000)];
    let data = l0.0[rv64_4k_leaf_index(EXPECTED_KERNEL_VIRT_BASE + 0x3000)];
    let bss = l0.0[rv64_4k_leaf_index(EXPECTED_KERNEL_VIRT_BASE + 0x4000)];
    let stack = l0.0[rv64_4k_leaf_index(EXPECTED_KERNEL_VIRT_BASE + 0x5000)];
    let unmapped = l0.0[rv64_4k_leaf_index(EXPECTED_KERNEL_VIRT_BASE + 0x6000)];

    assert_eq!(
        text,
        encode_leaf_pte(PhysAddr(0x8020_0000), PTE_R | PTE_X | PTE_G_TEST)
    );
    assert_eq!(
        rodata,
        encode_leaf_pte(PhysAddr(0x8020_2000), PTE_R | PTE_G_TEST)
    );
    assert_eq!(
        data,
        encode_leaf_pte(PhysAddr(0x8020_3000), PTE_R | PTE_W | PTE_G_TEST)
    );
    assert_eq!(
        bss,
        encode_leaf_pte(PhysAddr(0x8020_4000), PTE_R | PTE_W | PTE_G_TEST)
    );
    assert_eq!(
        stack,
        encode_leaf_pte(PhysAddr(0x8020_5000), PTE_R | PTE_W | PTE_G_TEST)
    );
    assert_eq!(unmapped, 0);
}

#[test]
fn bootstrap_info_publishes_direct_map_base_and_reserved_page_table_ranges() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let info = bag.bootstrap_pmap_info_ref().expect("bootstrap pmap info");

    assert_eq!(info.direct_map_base, VirtAddr(EXPECTED_DIRECT_MAP_BASE));
    assert_eq!(info.reserved_page_tables.len(), 4);
    assert_eq!(
        info.reserved_page_tables[0].start,
        bag.bootstrap_root_phys()
    );
    assert_eq!(info.reserved_page_tables[0].size, PAGE_SIZE);
    assert_eq!(
        info.reserved_page_tables[1].start,
        bag.kernel_alias_l1_phys()
    );
    assert_eq!(info.reserved_page_tables[1].size, PAGE_SIZE);
    assert_eq!(
        info.reserved_page_tables[2],
        bag.kernel_alias_l0_phys_range()
    );
    assert_eq!(info.reserved_page_tables[3], bag.pt_node_pool_phys_range());
}

#[test]
fn bootstrap_satp_uses_sv39_mode_and_root_ppn() {
    let satp = bootstrap_satp_value(PhysAddr(0x8020_0000));

    assert_eq!(satp >> 60, SV39_MODE);
    assert_eq!(satp & ((1usize << 44) - 1), 0x8020_0000 >> 12);
}

#[test]
fn pt_node_pool_allocates_fixed_boot_nodes() {
    let _guard = pmap_test_guard();
    reset_pt_node_pool_for_test();
    let bag = test_bag();

    assert_eq!(bag.pt_node_phys(0), bag.pt_node_pool_phys_range().start);

    let first = alloc_pt_node_from_bag(&bag).expect("first node");
    let second = alloc_pt_node_from_bag(&bag).expect("second node");

    assert_ne!(first.phys, second.phys);
    free_pt_node_from_bag(&bag, first);
    let reused = alloc_pt_node_from_bag(&bag).expect("reused node");
    assert_eq!(reused.phys, first.phys);
}

#[test]
fn direct_map_extension_reserves_and_commits_next_1g_leaf() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let next_phys = PhysAddr(QEMU_RAM_BASE + QEMU_BOOTSTRAP_MAP_SIZE);
    let reservation = reserve_direct_map_1g_from_bag(&bag, next_phys)
        .expect("reserve direct-map leaf")
        .expect("next 1GiB leaf should be empty");

    assert_eq!(reservation.phys(), next_phys);
    assert_eq!(
        reservation.virt(),
        VirtAddr(EXPECTED_DIRECT_MAP_BASE + next_phys.0)
    );

    commit_direct_map_1g_from_bag(&bag, reservation);

    let root = bag.bootstrap_root_ref();
    let slot = rv64_1g_leaf_index(EXPECTED_DIRECT_MAP_BASE + next_phys.0);
    assert_eq!(
        root.0[slot],
        encode_leaf_pte(next_phys, PTE_R | PTE_W | PTE_G)
    );
    assert_eq!(
        bag.bootstrap_pmap_info_ref()
            .expect("pmap info")
            .direct_map
            .size,
        QEMU_BOOTSTRAP_MAP_SIZE * 2
    );
}

#[test]
fn direct_map_extension_is_idempotent_for_existing_leaf() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    assert_eq!(
        reserve_direct_map_1g_from_bag(&bag, PhysAddr(QEMU_RAM_BASE))
            .expect("existing direct map leaf"),
        None
    );
}

#[test]
fn high_firmware_dtb_preseeds_its_direct_map_leaf_without_publishing_a_gap() {
    let _guard = pmap_test_guard();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let official_dtb = PhysAddr(0x2_7fe0_0000);
    cover_boot_firmware_dtb_from_bag(&bag, official_dtb).expect("cover official high DTB leaf");

    let leaf_phys = PhysAddr(0x2_4000_0000);
    let slot = rv64_1g_leaf_index(EXPECTED_DIRECT_MAP_BASE + leaf_phys.0);
    assert_eq!(
        bag.bootstrap_root_ref().0[slot],
        encode_leaf_pte(leaf_phys, PTE_R | PTE_W | PTE_G)
    );
    assert_eq!(
        bag.bootstrap_pmap_info_ref()
            .expect("pmap info")
            .direct_map
            .size,
        QEMU_BOOTSTRAP_MAP_SIZE,
        "a non-contiguous bootstrap leaf must not inflate the published span"
    );

    extend_direct_map_from_bag(&bag, PhysAddr(0x2_8000_0000))
        .expect("extend the contiguous direct map through the preseeded leaf");
    assert_eq!(
        bag.bootstrap_pmap_info_ref()
            .expect("pmap info")
            .direct_map
            .size,
        8 * QEMU_BOOTSTRAP_MAP_SIZE,
        "the existing DTB leaf must join the published contiguous span"
    );
}

#[test]
fn mmio_mapping_commits_2m_leaf_through_l1_table() {
    let _guard = pmap_test_guard();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x0c00_0000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation =
        reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Superpage2M)
            .expect("reserve mmio 2M")
            .expect("2M leaf should be empty");

    assert_eq!(reservation.virt(), virt);
    assert_eq!(reservation.phys(), phys);
    assert_eq!(reservation.kind(), PmapReserveKind::Superpage2M);

    commit_kernel_mapping_from_bag(
        &bag,
        reservation,
        PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
    );

    let l1 = l1_table_for_test(&bag, virt).expect("l1 table");
    assert_eq!(
        l1.0[rv64_2m_leaf_index(virt.0)],
        encode_leaf_pte(phys, PTE_R | PTE_W | PTE_G)
    );
}

#[test]
fn mmio_mapping_commits_4k_leaf_through_l0_table() {
    let _guard = pmap_test_guard();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x1000_1000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation = reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Page4K)
        .expect("reserve mmio 4K")
        .expect("4K leaf should be empty");

    assert_eq!(reservation.kind(), PmapReserveKind::Page4K);

    commit_kernel_mapping_from_bag(
        &bag,
        reservation,
        PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
    );

    let l0 = l0_table_for_test(&bag, virt).expect("l0 table");
    assert_eq!(
        l0.0[rv64_4k_leaf_index(virt.0)],
        encode_leaf_pte(phys, PTE_R | PTE_W | PTE_G)
    );
}

#[test]
fn abandoned_4k_reservation_rolls_back_intermediate_nodes() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x1000_1000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation = reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Page4K)
        .expect("reserve mmio 4K")
        .expect("4K leaf should be empty");

    assert!(l1_table_for_test(&bag, virt).is_some());
    assert!(l0_table_for_test(&bag, virt).is_some());

    rollback_kernel_mapping_from_bag(&bag, reservation);

    assert_eq!(bag.bootstrap_root_ref().0[rv64_1g_leaf_index(virt.0)], 0);
    assert_eq!(pt_node_allocated_for_test(), 0);
}

#[test]
fn post_boot_reservation_uses_typed_pt_allocator_before_boot_pool() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    install_pt_node_allocator_for_test(Some(test_typed_pt_allocator));
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x1000_1000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation = reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Page4K)
        .expect("reserve mmio 4K")
        .expect("4K leaf should be empty");

    assert_eq!(TEST_TYPED_ALLOC_NEXT.load(Ordering::Acquire), 2);
    assert_eq!(pt_node_allocated_for_test(), 0);

    rollback_kernel_mapping_from_bag(&bag, reservation);

    assert_eq!(TEST_TYPED_RELEASED.load(Ordering::Acquire), 2);
    assert_eq!(pt_node_allocated_for_test(), 0);
    reset_typed_pt_allocator_test_state();
}

#[test]
fn typed_pt_allocator_exhaustion_falls_back_to_boot_pool() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    install_pt_node_allocator_for_test(Some(test_typed_pt_allocator));
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let first_phys = PhysAddr(0x1000_1000);
    let first_virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + first_phys.0);
    let first =
        reserve_kernel_mapping_from_bag(&bag, first_virt, first_phys, PmapReserveKind::Page4K)
            .expect("first typed reservation")
            .expect("first 4K leaf should be empty");
    rollback_kernel_mapping_from_bag(&bag, first);

    let fallback_phys = PhysAddr(0x2000_1000);
    let fallback_virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + fallback_phys.0);
    let fallback = reserve_kernel_mapping_from_bag(
        &bag,
        fallback_virt,
        fallback_phys,
        PmapReserveKind::Page4K,
    )
    .expect("fallback reservation should use boot pool")
    .expect("fallback 4K leaf should be empty");

    assert_eq!(TEST_TYPED_ALLOC_NEXT.load(Ordering::Acquire), 4);
    assert_ne!(pt_node_allocated_for_test(), 0);

    rollback_kernel_mapping_from_bag(&bag, fallback);
    reset_typed_pt_allocator_test_state();
}

#[test]
fn unmap_kernel_mapping_clears_leaf_and_reports_invalidation() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    reset_committed_pt_nodes_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x0c00_0000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation =
        reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Superpage2M)
            .expect("reserve mmio 2M")
            .expect("2M leaf should be empty");
    commit_kernel_mapping_from_bag(
        &bag,
        reservation,
        PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
    );

    let result = unmap_kernel_mapping_from_bag(&bag, virt, PmapReserveKind::Superpage2M)
        .expect("unmap mmio 2M")
        .expect("committed mapping should unmap");

    assert_eq!(result.virt(), virt);
    assert_eq!(result.phys(), phys);
    assert_eq!(result.kind(), PmapReserveKind::Superpage2M);
    assert_eq!(result.invalidation().virt(), virt);
    assert_eq!(result.invalidation().size(), 2 * 1024 * 1024);

    assert!(pte_is_branch(
        bag.bootstrap_root_ref().0[rv64_1g_leaf_index(virt.0)]
    ));
    let l1 = l1_table_for_test(&bag, virt).expect("retained kernel l1");
    assert_eq!(l1.0[rv64_2m_leaf_index(virt.0)], 0);

    shootdown_kernel_mapping(result.invalidation());
    reset_committed_pt_nodes_for_test();
}

#[test]
fn protect_kernel_mapping_updates_leaf_in_place_and_reports_invalidation() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x0c00_0000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation =
        reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Superpage2M)
            .expect("reserve mmio 2M")
            .expect("2M leaf should be empty");
    commit_kernel_mapping_from_bag(
        &bag,
        reservation,
        PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
    );

    let invalidation = protect_kernel_mapping_from_bag(
        &bag,
        virt,
        PmapReserveKind::Superpage2M,
        PmapPermissions::KERNEL_RX,
    )
    .expect("protect mapped leaf")
    .expect("changed permissions need invalidation");

    assert_eq!(invalidation.virt(), virt);
    assert_eq!(invalidation.size(), 2 * 1024 * 1024);

    let l1 = l1_table_for_test(&bag, virt).expect("l1 table");
    let pte = l1.0[rv64_2m_leaf_index(virt.0)];
    assert_eq!(pte_phys(pte), phys);
    assert_eq!(pte & PTE_R, PTE_R);
    assert_eq!(pte & PTE_X, PTE_X);
    assert_eq!(pte & PTE_W, 0);

    assert_eq!(
        protect_kernel_mapping_from_bag(
            &bag,
            virt,
            PmapReserveKind::Superpage2M,
            PmapPermissions::KERNEL_RX,
        )
        .expect("idempotent protect"),
        None
    );
}

#[test]
fn protect_kernel_mapping_leaves_absent_slots_to_fault_path() {
    let _guard = pmap_test_guard();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x0c00_0000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);

    assert_eq!(
        protect_kernel_mapping_from_bag(
            &bag,
            virt,
            PmapReserveKind::Superpage2M,
            PmapPermissions::KERNEL_RO,
        )
        .expect("absent mapping is not a pmap mutation"),
        None
    );
}

#[test]
fn shootdown_batch_coalesces_contiguous_invalidations() {
    let batch = coalesce_invalidation_ranges(&[
        tx_hal::PmapInvalidation::new(VirtAddr(0x1000), 0x1000),
        tx_hal::PmapInvalidation::new(VirtAddr(0x2000), 0x1000),
        tx_hal::PmapInvalidation::new(VirtAddr(0x5000), 0x1000),
        tx_hal::PmapInvalidation::new(VirtAddr(0x6000), 0x1000),
    ]);

    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].virt(), VirtAddr(0x1000));
    assert_eq!(batch[0].size(), 0x2000);
    assert_eq!(batch[1].virt(), VirtAddr(0x5000));
    assert_eq!(batch[1].size(), 0x2000);
}

#[test]
fn committed_4k_unmap_releases_empty_intermediate_tables() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    reset_committed_pt_nodes_for_test();
    install_pt_node_allocator_for_test(Some(test_typed_pt_allocator));
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let phys = PhysAddr(0x1000_1000);
    let virt = VirtAddr(EXPECTED_DIRECT_MAP_BASE + phys.0);
    let reservation = reserve_kernel_mapping_from_bag(&bag, virt, phys, PmapReserveKind::Page4K)
        .expect("reserve 4K mapping")
        .expect("4K leaf should be empty");
    commit_kernel_mapping_from_bag(
        &bag,
        reservation,
        PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
    );

    assert!(l1_table_for_test(&bag, virt).is_some());
    assert!(l0_table_for_test(&bag, virt).is_some());

    let result = unmap_kernel_mapping_from_bag(&bag, virt, PmapReserveKind::Page4K)
        .expect("unmap 4K")
        .expect("leaf should unmap");

    assert_eq!(result.phys(), phys);
    assert_eq!(bag.bootstrap_root_ref().0[rv64_1g_leaf_index(virt.0)], 0);
    assert_eq!(
        TEST_TYPED_RELEASED.load(Ordering::Acquire),
        2,
        "empty L0 and L1 tables should release their typed frames"
    );

    reset_typed_pt_allocator_test_state();
    reset_committed_pt_nodes_for_test();
}

#[test]
fn process_root_copies_kernel_half_and_reuses_asid_after_destroy() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let first = create_pmap_root_from_bag(&bag).expect("first root");
    assert_eq!(first.asid().0, 1);

    let first_table = unsafe { page_table_mut_from_phys(first.phys()) };
    assert_eq!(
        first_table.0[rv64_1g_leaf_index(EXPECTED_DIRECT_MAP_BASE + QEMU_RAM_BASE)],
        bag.bootstrap_root_ref().0[rv64_1g_leaf_index(EXPECTED_DIRECT_MAP_BASE + QEMU_RAM_BASE)]
    );
    assert_eq!(
        first_table.0[rv64_1g_leaf_index(EXPECTED_KERNEL_VIRT_BASE)],
        bag.bootstrap_root_ref().0[rv64_1g_leaf_index(EXPECTED_KERNEL_VIRT_BASE)]
    );
    for index in 0..256 {
        assert_eq!(
            first_table.0[index], 0,
            "lower-half slot {index} must be empty"
        );
    }
    for index in 256..512 {
        assert_eq!(
            first_table.0[index],
            bag.bootstrap_root_ref().0[index],
            "kernel-half slot {index} must match bootstrap root"
        );
    }

    destroy_pmap_root_from_bag(&bag, first);
    assert_eq!(pt_node_allocated_for_test(), 0);

    let second = create_pmap_root_from_bag(&bag).expect("second root");
    assert_eq!(second.asid().0, 1);
    destroy_pmap_root_from_bag(&bag, second);
}

#[test]
fn process_root_exhausts_asids_without_allocating_reserved_zero() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    install_pt_node_allocator_for_test(Some(test_root_pt_allocator));
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let mut roots = Vec::new();
    for expected_asid in 1..ASID_CAPACITY {
        let root = create_pmap_root_from_bag(&bag).expect("root should allocate until ASIDs end");
        assert_eq!(root.asid().0, expected_asid as u16);
        roots.push(root);
    }

    assert_eq!(
        create_pmap_root_from_bag(&bag).err(),
        Some(PmapError::Exhausted)
    );

    for root in roots {
        assert_ne!(root.asid().0, 0);
        destroy_pmap_root_from_bag(&bag, root);
    }
    reset_typed_pt_allocator_test_state();
}

#[test]
fn process_root_allocation_failure_rolls_back_asid() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let bag = test_bag();
    let mut held_nodes = Vec::new();
    while let Ok(node) = alloc_pt_node_from_bag(&bag) {
        held_nodes.push(node);
    }

    assert_eq!(
        create_pmap_root_from_bag(&bag).err(),
        Some(PmapError::Exhausted)
    );

    for node in held_nodes {
        free_pt_node_from_bag(&bag, node);
    }

    let root = create_pmap_root_from_bag(&bag).expect("ASID should be reusable after rollback");
    assert_eq!(root.asid().0, 1);
    destroy_pmap_root_from_bag(&bag, root);
}

#[test]
fn destroying_process_root_tears_down_committed_user_tables() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    reset_committed_pt_nodes_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let root = create_pmap_root_from_bag(&bag).expect("process root");
    let virt = VirtAddr(0x4000);
    let phys = PhysAddr(0x8100_0000);
    let reservation =
        reserve_mapping_from_root(&bag, root.phys(), virt, phys, PmapReserveKind::Page4K)
            .expect("reserve user page")
            .expect("empty user leaf");
    commit_mapping_from_root(
        root.phys(),
        reservation,
        PmapPermissions::READ.union(PmapPermissions::USER),
    );

    assert_ne!(pt_node_allocated_for_test(), 0);

    destroy_pmap_root_from_bag(&bag, root);

    assert_eq!(
        pt_node_allocated_for_test(),
        0,
        "destroy should release root plus committed L1/L0 user tables"
    );
    reset_committed_pt_nodes_for_test();
}

#[test]
fn process_root_unmap_retains_user_tables_until_root_destroy() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let root = create_pmap_root_from_bag(&bag).expect("process root");
    let virt = VirtAddr(0x4000);
    let phys = PhysAddr(0x8100_0000);
    let reservation =
        reserve_mapping_from_root(&bag, root.phys(), virt, phys, PmapReserveKind::Page4K)
            .expect("reserve user page")
            .expect("empty user leaf");
    commit_mapping_from_root(
        root.phys(),
        reservation,
        PmapPermissions::READ
            .union(PmapPermissions::WRITE)
            .union(PmapPermissions::USER),
    );

    let root_table = unsafe { page_table_mut_from_phys(root.phys()) };
    let l1 = l1_table_mut_from_root(root_table, virt).expect("user l1");
    let l0 = l0_table_mut(l1, virt).expect("user l0");
    let pte = l0.0[rv64_4k_leaf_index(virt.0)];
    assert_eq!(pte_phys(pte), phys);
    assert_eq!(pte & PTE_U, PTE_U);
    assert_eq!(pte & PTE_G, 0);

    let invalidation = protect_mapping_from_root(
        root.phys(),
        virt,
        PmapReserveKind::Page4K,
        PmapPermissions::READ.union(PmapPermissions::USER),
    )
    .expect("protect user page")
    .expect("permission downgrade");
    assert_eq!(invalidation.virt(), virt);

    let result = unmap_mapping_from_root(&bag, root.phys(), virt, PmapReserveKind::Page4K)
        .expect("unmap user page")
        .expect("mapped user page");
    assert_eq!(result.phys(), phys);

    let root_table = unsafe { page_table_mut_from_phys(root.phys()) };
    assert!(pte_is_branch(root_table.0[rv64_1g_leaf_index(virt.0)]));
    let l1 = l1_table_mut_from_root(root_table, virt).expect("retained user l1");
    let l0 = l0_table_mut(l1, virt).expect("retained user l0");
    assert_eq!(l0.0[rv64_4k_leaf_index(virt.0)], 0);
    assert_eq!(
        pt_node_allocated_for_test(),
        3,
        "process root and its empty L1/L0 must remain until root destroy"
    );

    destroy_pmap_root_from_bag(&bag, root);
    assert_eq!(pt_node_allocated_for_test(), 0);
}

#[test]
fn process_root_maps_existing_user_granularities() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let root = create_pmap_root_from_bag(&bag).expect("process root");
    let cases = [
        (
            VirtAddr(0),
            PhysAddr(0x8000_0000),
            PmapReserveKind::Superpage1G,
        ),
        (
            VirtAddr(0x4000_0000),
            PhysAddr(0xc000_0000),
            PmapReserveKind::Superpage2M,
        ),
        (
            VirtAddr(0x4020_0000),
            PhysAddr(0xc020_0000),
            PmapReserveKind::Page4K,
        ),
    ];

    for (virt, phys, kind) in cases {
        let reservation = reserve_mapping_from_root(&bag, root.phys(), virt, phys, kind)
            .expect("reserve user mapping")
            .expect("empty user leaf");
        commit_mapping_from_root(
            root.phys(),
            reservation,
            PmapPermissions::READ.union(PmapPermissions::USER),
        );

        let result = unmap_mapping_from_root(&bag, root.phys(), virt, kind)
            .expect("unmap user mapping")
            .expect("mapping should exist");
        assert_eq!(result.phys(), phys);
        assert_eq!(result.kind(), kind);
        assert_eq!(result.page_count(), kind.size() / PAGE_SIZE);
    }

    destroy_pmap_root_from_bag(&bag, root);
}

#[test]
fn process_root_rejects_upper_half_and_user_reserved_band() {
    let _guard = pmap_test_guard();
    reset_typed_pt_allocator_test_state();
    reset_pt_node_pool_for_test();
    let mut bag = test_bag();
    publish_bootstrap_bag(&mut bag);

    let root = create_pmap_root_from_bag(&bag).expect("process root");
    let permissions = PmapPermissions::READ.union(PmapPermissions::USER);

    assert_eq!(
        reserve_mapping_from_root(
            &bag,
            root.phys(),
            VirtAddr(DIRECT_MAP_BASE),
            PhysAddr(0x8000_0000),
            PmapReserveKind::Page4K,
        )
        .err(),
        Some(PmapError::InvalidRequest)
    );
    assert_eq!(
        reserve_mapping_from_root(
            &bag,
            root.phys(),
            VirtAddr(SV39_USER_ALLOC_TOP),
            PhysAddr(0x8000_0000),
            PmapReserveKind::Page4K,
        )
        .err(),
        Some(PmapError::InvalidRequest)
    );
    assert_eq!(
        protect_mapping_from_root(
            root.phys(),
            VirtAddr(SV39_USER_ALLOC_TOP),
            PmapReserveKind::Page4K,
            permissions,
        )
        .err(),
        Some(PmapError::InvalidRequest)
    );
    assert_eq!(
        unmap_mapping_from_root(
            &bag,
            root.phys(),
            VirtAddr(SV39_USER_ALLOC_TOP),
            PmapReserveKind::Page4K,
        )
        .err(),
        Some(PmapError::InvalidRequest)
    );

    destroy_pmap_root_from_bag(&bag, root);
}
