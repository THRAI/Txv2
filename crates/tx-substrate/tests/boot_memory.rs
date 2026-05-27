use tx_hal::{
    BootInfo, BootstrapPmapInfo, MemoryRegion, MemoryRegionKind, PhysAddr, PhysRange,
    PmapReserveKind, Ppn, VirtAddr, VirtRange,
};
use tx_substrate::boot_memory::{
    build_boot_memory_plan, choose_mmio_mapping, required_direct_map_end, BootMemoryError,
    BootPageSpan, PAGE_SIZE_4K,
};

static MEMORY_REGIONS: [MemoryRegion; 3] = [
    MemoryRegion {
        base: PhysAddr(0x9000),
        size: 0x7000,
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(0x1000),
        size: 0x5000,
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(0xa000),
        size: 0x1000,
        kind: MemoryRegionKind::Reserved,
    },
];

static RESERVED_PAGE_TABLES: [PhysRange; 1] = [PhysRange {
    start: PhysAddr(0x3000),
    size: 0x1000,
}];

fn boot_info() -> BootInfo {
    BootInfo {
        memory_regions: &MEMORY_REGIONS,
        kernel_image: PhysRange {
            start: PhysAddr(0x2000),
            size: 0x1000,
        },
        initrd: Some(PhysRange {
            start: PhysAddr(0x5000),
            size: 0x1000,
        }),
        cmdline: None,
    }
}

fn pmap_info(direct_map_size: usize) -> BootstrapPmapInfo {
    BootstrapPmapInfo {
        root: PhysAddr(0x3000),
        mapped: PhysRange {
            start: PhysAddr(0x1000),
            size: 0xf000,
        },
        direct_map_base: VirtAddr(0xffff_ffc0_0000_0000),
        direct_map: VirtRange {
            start: VirtAddr(0xffff_ffc0_0000_1000),
            size: direct_map_size,
        },
        kernel_image: VirtRange::empty(),
        identity: None,
        pt_node_pool: PhysRange::empty(),
        reserved_page_tables: &RESERVED_PAGE_TABLES,
    }
}

#[test]
fn boot_plan_subtracts_reserved_ranges_and_metadata_carve() {
    let boot = boot_info();
    let pmap = pmap_info(0xf000);

    let plan = build_boot_memory_plan(&boot, &pmap, PAGE_SIZE_4K).expect("boot memory plan");

    assert_eq!(plan.base_ppn, Ppn(1));
    assert_eq!(plan.frame_count, 15);
    assert_eq!(
        plan.frame_meta,
        PhysRange {
            start: PhysAddr(0x1000),
            size: 15 * core::mem::size_of::<tx_substrate::page_allocator::FrameMeta>(),
        }
    );
    assert_eq!(
        plan.metadata_storage,
        PhysRange {
            start: PhysAddr(0x1000),
            size: 0x1000,
        }
    );
    assert_eq!(
        plan.free_spans(),
        &[
            BootPageSpan {
                base: Ppn(4),
                count: 1,
            },
            BootPageSpan {
                base: Ppn(9),
                count: 1,
            },
            BootPageSpan {
                base: Ppn(11),
                count: 5,
            },
        ]
    );
}

#[test]
fn boot_plan_rejects_ram_outside_direct_map() {
    let boot = boot_info();
    let pmap = pmap_info(0x1000);

    let err = build_boot_memory_plan(&boot, &pmap, PAGE_SIZE_4K)
        .expect_err("all RAM must be direct-mapped for v1 init");

    assert_eq!(err, BootMemoryError::DirectMapTooSmall);
}

#[test]
fn required_direct_map_end_uses_page_covered_ram_end() {
    static REGIONS: [MemoryRegion; 2] = [
        MemoryRegion {
            base: PhysAddr(0x8000_1000),
            size: 0x1000,
            kind: MemoryRegionKind::Usable,
        },
        MemoryRegion {
            base: PhysAddr(0x9000_0123),
            size: 1,
            kind: MemoryRegionKind::Reserved,
        },
    ];
    let boot = BootInfo {
        memory_regions: &REGIONS,
        kernel_image: PhysRange::empty(),
        initrd: None,
        cmdline: None,
    };

    assert_eq!(
        required_direct_map_end(&boot, PAGE_SIZE_4K).expect("direct map end"),
        PhysAddr(0x9000_1000)
    );
}

#[test]
fn mmio_mapping_choice_prefers_2m_when_aligned() {
    assert_eq!(
        choose_mmio_mapping(
            VirtAddr(0xffff_ffc0_0c00_0000),
            PhysAddr(0x0c00_0000),
            0x400000
        )
        .expect("2M mapping"),
        (PmapReserveKind::Superpage2M, 0x20_0000)
    );
}

#[test]
fn mmio_mapping_choice_uses_4k_for_unaligned_tail() {
    assert_eq!(
        choose_mmio_mapping(
            VirtAddr(0xffff_ffc0_1000_1000),
            PhysAddr(0x1000_1000),
            0x3000
        )
        .expect("4K mapping"),
        (PmapReserveKind::Page4K, 0x1000)
    );
}
