use super::la64_irq_trap::*;
use super::la64_pmap::*;
use super::platform_impls::{la64_copy_from_user_raw, la64_copy_to_user_raw};
use super::*;
use core::sync::atomic::AtomicUsize;
use std::sync::Mutex;
use tx_hal::{
    AllocError, AuxvIf, DmaAddr, DmaDirection, MemoryRegionKind, PmapError, PmapIf, PtNode,
    PtNodeSourceKind,
};

static TEST_PMAP_STATE_LOCK: Mutex<()> = Mutex::new(());
static TEST_PT_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static TEST_PT_RELEASES: AtomicUsize = AtomicUsize::new(0);
static TEST_ROOT_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static TEST_ROOT_RELEASES: AtomicUsize = AtomicUsize::new(0);
static TEST_PMAP_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static TEST_PMAP_RELEASES: AtomicUsize = AtomicUsize::new(0);
static TEST_TIMER_TRAPS: AtomicUsize = AtomicUsize::new(0);
static TEST_SYSCALL_TRAPS: AtomicUsize = AtomicUsize::new(0);
static TEST_IRQ_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
static mut TEST_ROOT_PAGE: [u64; 512] = [0; 512];
static mut TEST_IRQ_TABLE: IrqDispatchTable = IrqDispatchTable::new();
#[derive(Clone, Copy)]
#[repr(align(4096))]
struct TestPmapPage {
    _entries: [u64; 512],
}

static mut TEST_PMAP_PAGES: [TestPmapPage; 8] = [TestPmapPage { _entries: [0; 512] }; 8];

struct RecordingTrapSink;

impl KernelTrapSink<Platform> for RecordingTrapSink {
    fn on_page_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        panic!("unexpected page fault")
    }

    fn on_syscall(_view: TrapFrameMut<'_>) -> TrapAction {
        panic!("unexpected syscall")
    }

    fn on_timer_interrupt(cpu: CpuId) -> TrapAction {
        assert_eq!(cpu, CpuId(0));
        assert!(<Platform as IrqIf>::in_irq_context());
        TEST_TIMER_TRAPS.fetch_add(1, Ordering::AcqRel);
        TrapAction::Resume
    }

    fn on_external_irq(_cpu: CpuId) -> TrapAction {
        panic!("unexpected external irq")
    }

    fn on_ipi(_cpu: CpuId) -> TrapAction {
        panic!("unexpected ipi")
    }

    fn on_illegal_or_sync_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        panic!("unexpected sync fault")
    }
}

struct RecordingSyscallSink;

impl KernelTrapSink<Platform> for RecordingSyscallSink {
    fn on_page_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        panic!("unexpected page fault")
    }

    fn on_syscall(mut view: TrapFrameMut<'_>) -> TrapAction {
        let snapshot = view.view();
        assert_eq!(snapshot.pc, VirtAddr(0x2004));
        assert_eq!(snapshot.syscall_number, 172);
        assert_eq!(snapshot.syscall_args, [1, 2, 3, 4, 5, 6]);
        view.set_syscall_return(0x5a);
        TEST_SYSCALL_TRAPS.fetch_add(1, Ordering::AcqRel);
        TrapAction::Resume
    }

    fn on_timer_interrupt(_cpu: CpuId) -> TrapAction {
        panic!("unexpected timer")
    }

    fn on_external_irq(_cpu: CpuId) -> TrapAction {
        panic!("unexpected external irq")
    }

    fn on_ipi(_cpu: CpuId) -> TrapAction {
        panic!("unexpected ipi")
    }

    fn on_illegal_or_sync_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        panic!("unexpected sync fault")
    }
}

#[test]
fn boot_info_publishes_qemu_ram_and_kernel_image() {
    let info = Platform::boot_info();

    assert_eq!(info.initrd, None);
    assert_eq!(info.cmdline, None);
    assert_eq!(info.kernel_image.start, PhysAddr(0x0020_0000));
    assert!(info.kernel_image.size > 0);

    assert_eq!(info.memory_regions.len(), 2);
    assert_eq!(info.memory_regions[0].base, PhysAddr(0));
    assert_eq!(info.memory_regions[0].kind, MemoryRegionKind::Reserved);
    assert!(info.memory_regions[0].size >= info.kernel_image.end().0);

    assert_eq!(info.memory_regions[1].kind, MemoryRegionKind::Usable);
    assert_eq!(
        info.memory_regions[1].base,
        PhysAddr(info.memory_regions[0].size)
    );
    assert_eq!(
        info.memory_regions[1].base.0 + info.memory_regions[1].size,
        0x1000_0000
    );
}

#[test]
fn bootstrap_pmap_info_describes_dmw_direct_ram() {
    let info = Platform::boot_info();
    let pmap = Platform::bootstrap_pmap_info().expect("bootstrap pmap info");

    assert_eq!(pmap.root, PhysAddr(0));
    assert_eq!(
        pmap.mapped,
        PhysRange {
            start: PhysAddr(0),
            size: 0x1000_0000,
        }
    );
    assert_eq!(pmap.direct_map_base, VirtAddr(LA64_DMW_CACHED_BASE));
    assert_eq!(
        pmap.direct_map,
        VirtRange {
            start: VirtAddr(LA64_DMW_CACHED_BASE),
            size: 0x1000_0000,
        }
    );
    assert_eq!(pmap.identity, None);
    assert_eq!(
        pmap.kernel_image.start,
        VirtAddr(la64_cached_virt(info.kernel_image.start.0))
    );
    assert_eq!(pmap.kernel_image.size, info.kernel_image.size);
    assert_eq!(pmap.pt_node_pool, PhysRange::empty());
    assert!(pmap.reserved_page_tables.is_empty());
}

#[test]
fn substrate_smoke_gate_is_enabled_and_uart_mmio_is_published() {
    let substrate_ready = core::hint::black_box(<Platform as PlatformConfig>::SUBSTRATE_BOOT_READY);
    assert!(substrate_ready);
    assert_eq!(
        <Platform as PlatformConfig>::USER_TOP,
        VirtAddr(LA64_USER_TOP)
    );
    assert_eq!(<Platform as PlatformConfig>::PAGE_TABLE_LEVELS, 4);
    assert_eq!(<Platform as PlatformConfig>::ASID_BITS, 10);
    assert_eq!(Platform::platform_info().mmio_regions.len(), 1);
    assert_eq!(Platform::platform_info().mmio_regions[0].name, "uart0");
    assert_eq!(
        Platform::platform_info().mmio_regions[0].virt.start,
        VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_BASE))
    );
}

#[test]
fn auxv_facts_publish_loongarch64_platform() {
    let facts = <Platform as AuxvIf>::arch_auxv_facts();

    assert_eq!(facts.page_size, <Platform as PlatformConfig>::PAGE_SIZE);
    assert_eq!(facts.hwcap, 0);
    assert_eq!(facts.hwcap2, 0);
    assert_eq!(facts.platform, "loongarch64");
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn console_read_bytes_is_nonblocking_when_host_has_no_uart_input() {
    let mut buf = [0xaa; 4];

    assert_eq!(<Platform as ConsoleIf>::read_bytes(&mut buf), 0);
    assert_eq!(tx_hal::console_read_bytes::<Platform>(&mut buf), 0);
    assert_eq!(buf, [0xaa; 4]);
}

#[test]
fn trap_vector_addresses_are_written_as_physical_addresses() {
    assert_eq!(
        la64_kernel_addr_to_phys(la64_cached_virt(0x1234_5000)),
        0x1234_5000
    );
    assert_eq!(
        la64_kernel_addr_to_phys(la64_uncached_virt(0x1fe0_0000)),
        0x1fe0_0000
    );
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn trap_vector_install_is_host_noop() {
    <Platform as TrapIf>::install_minimal_trap_vector();
    <Platform as TrapIf>::install_kernel_trap_vector();
    <Platform as TrapIf>::install_user_trap_vector();
}

#[test]
fn la64_trap_classification_decodes_interrupts_and_sync_faults() {
    assert_eq!(
        classify_la64_trap(LA64_ESTAT_IS_TIMER),
        TrapClass::TimerInterrupt
    );
    assert_eq!(
        classify_la64_trap(LA64_ESTAT_IS_IPI),
        TrapClass::InterprocessorInterrupt
    );
    assert_eq!(classify_la64_trap(1 << 2), TrapClass::ExternalInterrupt);
    assert_eq!(
        classify_la64_trap(LA64_ECODE_SYS << LA64_ESTAT_ECODE_SHIFT),
        TrapClass::Syscall
    );
    assert_eq!(
        classify_la64_trap(LA64_ECODE_BRK << LA64_ESTAT_ECODE_SHIFT),
        TrapClass::Breakpoint
    );
    assert_eq!(
        classify_la64_trap(LA64_ECODE_INE << LA64_ESTAT_ECODE_SHIFT),
        TrapClass::IllegalInstruction
    );
    assert_eq!(
        classify_la64_trap(LA64_ECODE_PIS << LA64_ESTAT_ECODE_SHIFT),
        TrapClass::PageFault {
            write: true,
            instruction: false,
        }
    );
    assert_eq!(
        classify_la64_trap(LA64_ECODE_PIF << LA64_ESTAT_ECODE_SHIFT),
        TrapClass::PageFault {
            write: false,
            instruction: true,
        }
    );
}

#[test]
fn la64_platform_snapshot_projects_classified_fault() {
    let snapshot = TrapFrameSnapshot {
        scause: LA64_ECODE_PIL << LA64_ESTAT_ECODE_SHIFT,
        sepc: 0x2000,
        stval: 0x3000,
    };
    let portable = Platform::snapshot_trap(snapshot);

    assert_eq!(
        portable.class,
        TrapClass::PageFault {
            write: false,
            instruction: false,
        }
    );
    assert_eq!(portable.pc, VirtAddr(0x2000));
    assert_eq!(portable.fault_address, Some(VirtAddr(0x3000)));
}

#[test]
fn la64_trap_frame_view_uses_la64_abi_registers() {
    let mut frame = La64TrapFrame {
        r: [0; 32],
        estat: LA64_ECODE_SYS << LA64_ESTAT_ECODE_SHIFT,
        era: 0x2000,
        badv: 0,
        crmd: 0,
        prmd: LA64_PRMD_PPLV_USER | LA64_PRMD_PIE,
    };
    frame.r[LA64_R_SP] = 0x8000;
    frame.r[LA64_R_TLS] = 0x1234;
    frame.r[LA64_R_A7] = 93;
    frame.r[LA64_R_A0] = 1;
    frame.r[LA64_R_A1] = 2;
    frame.r[LA64_R_A2] = 3;
    frame.r[LA64_R_A3] = 4;
    frame.r[LA64_R_A4] = 5;
    frame.r[LA64_R_A5] = 6;

    let view = frame.view();

    assert_eq!(view.pc, VirtAddr(0x2000));
    assert_eq!(view.sp, VirtAddr(0x8000));
    assert_eq!(view.syscall_number, 93);
    assert_eq!(view.syscall_args, [1, 2, 3, 4, 5, 6]);
    assert_eq!(view.previous_mode, TrapPreviousMode::User);
    assert!(view.interrupts_enabled_before);
    assert_eq!(view.user_tls_register, 0x1234);
}

#[test]
fn la64_restore_user_context_refreshes_cached_view_with_la64_abi() {
    let mut frame = La64TrapFrame {
        r: [0; 32],
        estat: LA64_ECODE_SYS << LA64_ESTAT_ECODE_SHIFT,
        era: 0,
        badv: 0,
        crmd: 0,
        prmd: 0,
    };
    let mut context = UserTrapContext {
        regs: [0; 32],
        pc: 0x4000,
        status: 0,
    };
    context.regs[LA64_R_SP] = 0x9000;
    context.regs[LA64_R_TLS] = 0x55aa;
    context.regs[LA64_R_A7] = 221;
    context.regs[LA64_R_A0] = 10;
    context.regs[LA64_R_A1] = 11;
    context.regs[LA64_R_A2] = 12;
    context.regs[LA64_R_A3] = 13;
    context.regs[LA64_R_A4] = 14;
    context.regs[LA64_R_A5] = 15;

    {
        let mut view = frame.view_mut();
        view.restore_user_context(&context);
        let refreshed = view.view();

        assert_eq!(refreshed.pc, VirtAddr(0x4000));
        assert_eq!(refreshed.sp, VirtAddr(0x9000));
        assert_eq!(refreshed.syscall_number, 221);
        assert_eq!(refreshed.syscall_args, [10, 11, 12, 13, 14, 15]);
        assert_eq!(refreshed.user_tls_register, 0x55aa);
    }
    assert_eq!(frame.prmd & LA64_PRMD_PPLV_MASK, LA64_PRMD_PPLV_USER);
    assert_ne!(frame.prmd & LA64_PRMD_PIE, 0);
}

#[test]
fn dispatch_timer_trap_enters_irq_context_and_resumes() {
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    TEST_TIMER_TRAPS.store(0, Ordering::Release);
    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    assert!(!<Platform as IrqIf>::in_irq_context());

    let mut frame = La64TrapFrame {
        r: [0; 32],
        estat: LA64_ESTAT_IS_TIMER,
        era: 0x2000,
        badv: 0,
        crmd: 0,
        prmd: 0,
    };

    assert_eq!(
        dispatch_trap_frame::<RecordingTrapSink>(&mut frame),
        TrapAction::Resume
    );
    assert_eq!(TEST_TIMER_TRAPS.load(Ordering::Acquire), 1);
    assert!(!<Platform as IrqIf>::in_irq_context());

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn dispatch_syscall_advances_era_and_uses_la64_abi() {
    TEST_SYSCALL_TRAPS.store(0, Ordering::Release);
    let mut frame = La64TrapFrame {
        r: [0; 32],
        estat: LA64_ECODE_SYS << LA64_ESTAT_ECODE_SHIFT,
        era: 0x2000,
        badv: 0,
        crmd: 0,
        prmd: LA64_PRMD_PPLV_USER | LA64_PRMD_PIE,
    };
    frame.r[LA64_R_A7] = 172;
    frame.r[LA64_R_A0] = 1;
    frame.r[LA64_R_A1] = 2;
    frame.r[LA64_R_A2] = 3;
    frame.r[LA64_R_A3] = 4;
    frame.r[LA64_R_A4] = 5;
    frame.r[LA64_R_A5] = 6;

    assert_eq!(
        dispatch_trap_frame::<RecordingSyscallSink>(&mut frame),
        TrapAction::Resume
    );

    assert_eq!(TEST_SYSCALL_TRAPS.load(Ordering::Acquire), 1);
    assert_eq!(frame.era, 0x2004);
    assert_eq!(frame.r[LA64_R_A0], 0x5a);
}

#[test]
fn la64_user_access_host_paths_report_faults_for_nonzero_copies() {
    let mut kernel = [0u8; 4];
    let user = [1u8; 4];

    unsafe {
        assert_eq!(
            la64_copy_from_user_raw(kernel.as_mut_ptr(), UserPtr::new(user.as_ptr() as usize), 0),
            Ok(())
        );
        assert_eq!(
            la64_copy_to_user_raw(UserPtr::new(user.as_ptr() as usize), kernel.as_ptr(), 0),
            Ok(())
        );
        assert_eq!(
            la64_copy_from_user_raw(kernel.as_mut_ptr(), UserPtr::new(0x1234), 1),
            Err(FaultInfo {
                address: VirtAddr(0x1234),
                write: false,
                instruction: false,
                from_user: false,
            })
        );
        assert_eq!(
            la64_copy_to_user_raw(UserPtr::new(0x5678), kernel.as_ptr(), 1),
            Err(FaultInfo {
                address: VirtAddr(0x5678),
                write: true,
                instruction: false,
                from_user: false,
            })
        );
    }
}

#[test]
fn la64_signal_frame_layout_and_trampoline_are_stable() {
    assert_eq!(
        LA64_SIGRETURN_TRAMPOLINE,
        [LA64_ADDI_D_R11_ZERO_RT_SIGRETURN, LA64_SYSCALL_0]
    );
    assert_eq!(
        core::mem::size_of::<La64SignalFrame>() % LA64_SIGFRAME_ALIGN,
        0
    );
    assert_eq!(
        core::mem::offset_of!(La64SignalFrame, trampoline) % core::mem::align_of::<u32>(),
        0
    );
    assert_eq!(align_down(0x100f, LA64_SIGFRAME_ALIGN), 0x1000);
}

#[test]
fn la64_signal_frame_restore_uses_saved_user_context() {
    let mut frame = La64TrapFrame {
        r: [0; 32],
        estat: LA64_ECODE_SYS << LA64_ESTAT_ECODE_SHIFT,
        era: 0x1000,
        badv: 0,
        crmd: 0,
        prmd: 0,
    };
    let mut saved = SavedSignalFrame {
        saved_mask: UserSignalMaskAbi::EMPTY,
        user_context: UserTrapContext {
            regs: [0; 32],
            pc: 0x6000,
            status: 0,
        },
    };
    saved.user_context.regs[LA64_R_SP] = 0x7000;
    saved.user_context.regs[LA64_R_TLS] = 0x88;
    saved.user_context.regs[LA64_R_A7] = LA64_RT_SIGRETURN_SYSCALL as usize;

    <Platform as SignalFrameIf>::restore_signal_frame(frame.view_mut(), &saved);

    assert_eq!(frame.era, 0x6000);
    assert_eq!(frame.r[LA64_R_SP], 0x7000);
    assert_eq!(frame.r[LA64_R_TLS], 0x88);
    assert_eq!(frame.r[LA64_R_A7], LA64_RT_SIGRETURN_SYSCALL as usize);
    assert_eq!(frame.prmd & LA64_PRMD_PPLV_MASK, LA64_PRMD_PPLV_USER);
}

#[test]
fn la64_kernel_mmio_mapping_commits_through_pgdh_root() {
    let _guard = lock_test_pmap_state();
    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );
    let virt = VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_PAGE_BASE + 0x1000));
    let phys = PhysAddr(QEMU_LA64_UART0_PAGE_BASE + 0x1000);

    let reservation = Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K)
        .expect("reserve UART page")
        .expect("new kernel mapping");
    assert!(reservation.intermediates().l2.is_some());
    assert!(reservation.intermediates().l1.is_some());
    assert!(reservation.intermediates().l0.is_some());
    Platform::commit_kernel_mapping(reservation);

    let pgdh = PhysAddr(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire));
    let l0 = la64_l0_table_mut(pgdh, virt).expect("kernel L0");
    let leaf = l0[la64_l0_index(virt.0)];
    assert!(la64_pte_is_leaf(leaf));
    assert_eq!(la64_pte_phys(leaf), phys);
    assert_eq!(leaf & LA64_PTE_MAT_CC, LA64_PTE_MAT_SUC);
    assert_eq!(
        Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K),
        Ok(None)
    );
    assert_eq!(
        Platform::reserve_kernel_mapping(VirtAddr(0x2000), phys, PmapReserveKind::Page4K),
        Err(PmapError::InvalidRequest)
    );
    assert_eq!(
        Platform::reserve_kernel_mapping(
            VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_PAGE_BASE)),
            PhysAddr(QEMU_LA64_UART0_PAGE_BASE),
            PmapReserveKind::Page4K,
        ),
        Ok(None)
    );

    let invalidation =
        Platform::protect_kernel_mapping(virt, PmapReserveKind::Page4K, PmapPermissions::KERNEL_RO)
            .expect("protect")
            .expect("permissions changed");
    assert_eq!(invalidation, PmapInvalidation::new(virt, 4096));
    let protected = la64_l0_table_mut(pgdh, virt).expect("kernel L0")[la64_l0_index(virt.0)];
    assert_eq!(protected & LA64_PTE_W, 0);

    let unmapped = Platform::unmap_kernel_mapping(virt, PmapReserveKind::Page4K)
        .expect("unmap")
        .expect("mapping present");
    assert_eq!(unmapped.phys(), phys);
    assert_eq!(
        la64_page_table_mut_from_phys(pgdh)[la64_l3_index(virt.0)],
        0
    );
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 3);

    reset_pmap_test_state();
}

#[test]
fn la64_kernel_superpage_mappings_prune_committed_intermediates() {
    let _guard = lock_test_pmap_state();
    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );

    let virt_1g = VirtAddr(LA64_DMW_UNCACHED_BASE + 0x4000_0000);
    let phys_1g = PhysAddr(0x4000_0000);
    let reservation_1g =
        Platform::reserve_kernel_mapping(virt_1g, phys_1g, PmapReserveKind::Superpage1G)
            .expect("reserve 1G")
            .expect("new 1G mapping");
    assert!(reservation_1g.intermediates().l2.is_some());
    assert_eq!(reservation_1g.intermediates().l1, None);
    assert_eq!(reservation_1g.intermediates().l0, None);
    Platform::commit_kernel_mapping(reservation_1g);

    let pgdh = PhysAddr(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire));
    let leaf_1g =
        *la64_leaf_slot_mut(pgdh, virt_1g, PmapReserveKind::Superpage1G).expect("1G leaf slot");
    assert!(la64_pte_is_leaf(leaf_1g));
    assert_eq!(la64_pte_phys(leaf_1g), phys_1g);
    assert_eq!(
        Platform::reserve_kernel_mapping(virt_1g, phys_1g, PmapReserveKind::Superpage1G),
        Ok(None)
    );
    assert_eq!(
        Platform::reserve_kernel_mapping(
            virt_1g,
            PhysAddr(0x8000_0000),
            PmapReserveKind::Superpage1G,
        ),
        Err(PmapError::AlreadyMapped)
    );
    assert_eq!(
        Platform::unmap_kernel_mapping(virt_1g, PmapReserveKind::Superpage1G)
            .expect("unmap 1G")
            .expect("1G present")
            .phys(),
        phys_1g
    );
    assert_eq!(
        la64_page_table_mut_from_phys(pgdh)[la64_l3_index(virt_1g.0)],
        0
    );
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 1);

    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );

    let virt_2m = VirtAddr(LA64_DMW_UNCACHED_BASE + 0x200_0000);
    let phys_2m = PhysAddr(0x200_0000);
    let reservation_2m =
        Platform::reserve_kernel_mapping(virt_2m, phys_2m, PmapReserveKind::Superpage2M)
            .expect("reserve 2M")
            .expect("new 2M mapping");
    assert!(reservation_2m.intermediates().l2.is_some());
    assert!(reservation_2m.intermediates().l1.is_some());
    assert_eq!(reservation_2m.intermediates().l0, None);
    Platform::commit_kernel_mapping(reservation_2m);

    let pgdh = PhysAddr(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire));
    let leaf_2m =
        *la64_leaf_slot_mut(pgdh, virt_2m, PmapReserveKind::Superpage2M).expect("2M leaf slot");
    assert!(la64_pte_is_leaf(leaf_2m));
    assert_eq!(la64_pte_phys(leaf_2m), phys_2m);
    assert_eq!(
        Platform::unmap_kernel_mapping(virt_2m, PmapReserveKind::Superpage2M)
            .expect("unmap 2M")
            .expect("2M present")
            .phys(),
        phys_2m
    );
    assert_eq!(
        la64_page_table_mut_from_phys(pgdh)[la64_l3_index(virt_2m.0)],
        0
    );
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 2);

    reset_pmap_test_state();
}

#[test]
fn la64_kernel_mapping_rollback_releases_uncommitted_tables() {
    let _guard = lock_test_pmap_state();
    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );

    let virt = VirtAddr(LA64_DMW_UNCACHED_BASE + 0x6000_0000);
    let phys = PhysAddr(0x6000_0000);
    let reservation = Platform::reserve_kernel_mapping(virt, phys, PmapReserveKind::Page4K)
        .expect("reserve")
        .expect("new mapping");
    assert!(reservation.intermediates().l2.is_some());
    assert!(reservation.intermediates().l1.is_some());
    assert!(reservation.intermediates().l0.is_some());

    let pgdh = PhysAddr(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire));
    Platform::rollback_kernel_mapping(reservation);
    assert_eq!(
        la64_page_table_mut_from_phys(pgdh)[la64_l3_index(virt.0)],
        0
    );
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 3);

    reset_pmap_test_state();
}

#[test]
fn dmw_direct_map_reservation_is_precovered_for_ram() {
    assert_eq!(
        Platform::reserve_kernel_direct_map_1g(PhysAddr(0)),
        Ok(None)
    );
    assert_eq!(
        Platform::extend_direct_map(PhysAddr(QEMU_LA64_RAM_END)),
        Ok(())
    );
    assert_eq!(
        Platform::extend_direct_map(PhysAddr(QEMU_LA64_RAM_END + 1)),
        Err(PmapError::Unsupported)
    );
}

#[test]
fn process_pmap_root_allocates_asid_and_zeroed_pt_node() {
    let _guard = lock_test_pmap_state();
    reset_pt_node_allocator_for_test();
    reset_la64_asids_for_test();
    TEST_ROOT_ALLOCATIONS.store(0, Ordering::Release);
    TEST_ROOT_RELEASES.store(0, Ordering::Release);
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!(TEST_ROOT_PAGE).cast::<u8>(),
            0xaa,
            <Platform as PlatformConfig>::PAGE_SIZE,
        );
    }

    assert_eq!(
        Platform::install_pt_node_allocator(test_root_allocator),
        Ok(())
    );

    let root = Platform::create_pmap_root().expect("LA64 process root");

    assert_eq!(root.asid(), Asid(1));
    assert_eq!(TEST_ROOT_ALLOCATIONS.load(Ordering::Acquire), 1);
    let page = unsafe { &*core::ptr::addr_of!(TEST_ROOT_PAGE) };
    assert!(page.iter().all(|entry| *entry == 0));

    Platform::destroy_pmap_root(root);
    assert_eq!(TEST_ROOT_RELEASES.load(Ordering::Acquire), 1);

    let reused = Platform::create_pmap_root().expect("ASID reusable after destroy");
    assert_eq!(reused.asid(), Asid(1));
    Platform::destroy_pmap_root(reused);

    reset_pt_node_allocator_for_test();
    reset_la64_asids_for_test();
}

#[test]
fn activate_pmap_installs_pgdl_pgdh_and_asid() {
    let _guard = lock_test_pmap_state();
    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );

    let first = Platform::create_pmap_root().expect("first LA64 process root");
    let second = Platform::create_pmap_root().expect("second LA64 process root");
    assert_eq!(TEST_PMAP_ALLOCATIONS.load(Ordering::Acquire), 2);

    Platform::activate_user_pmap(&first);
    let pgdh = LA64_ACTIVE_PGDH.load(Ordering::Acquire);
    assert_ne!(pgdh, 0);
    assert_eq!(LA64_ACTIVE_PGDL.load(Ordering::Acquire), first.phys().0);
    assert_eq!(
        LA64_ACTIVE_ASID.load(Ordering::Acquire),
        first.asid().0 as usize
    );
    assert_eq!(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire), pgdh);
    assert_eq!(TEST_PMAP_ALLOCATIONS.load(Ordering::Acquire), 3);
    let pgdh_page = la64_page_table_mut_from_phys(PhysAddr(pgdh));
    assert!(pgdh_page.iter().all(|entry| *entry == 0));

    Platform::activate_user_pmap(&second);
    assert_eq!(LA64_ACTIVE_PGDL.load(Ordering::Acquire), second.phys().0);
    assert_eq!(
        LA64_ACTIVE_ASID.load(Ordering::Acquire),
        second.asid().0 as usize
    );
    assert_eq!(LA64_ACTIVE_PGDH.load(Ordering::Acquire), pgdh);
    assert_eq!(TEST_PMAP_ALLOCATIONS.load(Ordering::Acquire), 3);

    Platform::destroy_pmap_root(first);
    Platform::destroy_pmap_root(second);
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 2);

    reset_pmap_test_state();
}

#[test]
fn la64_user_page_mapping_reserve_commit_protect_and_unmap() {
    let _guard = lock_test_pmap_state();
    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );

    let root = Platform::create_pmap_root().expect("LA64 process root");
    let virt = VirtAddr(0x0000_1234_5000);
    let phys = PhysAddr(0x0080_0000);
    let permissions = PmapPermissions::READ
        .union(PmapPermissions::WRITE)
        .union(PmapPermissions::USER);

    let reservation = Platform::reserve_mapping(&root, virt, phys, PmapReserveKind::Page4K)
        .expect("reserve")
        .expect("new mapping");
    assert!(reservation.intermediates().l2.is_some());
    assert!(reservation.intermediates().l1.is_some());
    assert!(reservation.intermediates().l0.is_some());

    Platform::commit_mapping(&root, reservation, permissions);

    let l0 = la64_l0_table_mut(root.phys(), virt).expect("committed L0");
    let leaf = l0[la64_l0_index(virt.0)];
    assert!(la64_pte_is_leaf(leaf));
    assert_eq!(la64_pte_phys(leaf), phys);
    assert_eq!(
        Platform::reserve_mapping(&root, virt, phys, PmapReserveKind::Page4K),
        Err(PmapError::AlreadyMapped)
    );

    let read_only = PmapPermissions::READ.union(PmapPermissions::USER);
    let invalidation = Platform::protect_mapping(&root, virt, PmapReserveKind::Page4K, read_only)
        .expect("protect")
        .expect("changed permissions");
    assert_eq!(invalidation, PmapInvalidation::new(virt, 4096));
    let protected =
        la64_l0_table_mut(root.phys(), virt).expect("protected L0")[la64_l0_index(virt.0)];
    assert_eq!(la64_pte_phys(protected), phys);
    assert_eq!(protected & LA64_PTE_W, 0);
    assert_eq!(protected & LA64_PTE_NR, 0);

    let unmapped = Platform::unmap_mapping(&root, virt, PmapReserveKind::Page4K)
        .expect("unmap")
        .expect("present mapping");
    assert_eq!(unmapped.virt(), virt);
    assert_eq!(unmapped.phys(), phys);
    assert_eq!(unmapped.kind(), PmapReserveKind::Page4K);
    assert_eq!(
        la64_page_table_mut_from_phys(root.phys())[la64_l3_index(virt.0)],
        0
    );
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 3);

    Platform::destroy_pmap_root(root);
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 4);

    reset_pmap_test_state();
}

#[test]
fn la64_device_permission_selects_uncached_mat() {
    let ram = encode_la64_leaf_pte(PhysAddr(0x1000), PmapPermissions::KERNEL_RW);
    let device = encode_la64_leaf_pte(
        PhysAddr(0x1000),
        PmapPermissions::KERNEL_RW.union(PmapPermissions::DEVICE),
    );

    assert_eq!(ram & LA64_PTE_MAT_CC, LA64_PTE_MAT_CC);
    assert_eq!(device & LA64_PTE_MAT_CC, LA64_PTE_MAT_SUC);
    assert_eq!(la64_pte_phys(device), PhysAddr(0x1000));
}

#[test]
fn la64_user_mapping_rollback_uses_existing_root_path() {
    let _guard = lock_test_pmap_state();
    reset_pmap_test_state();
    assert_eq!(
        Platform::install_pt_node_allocator(test_pmap_allocator),
        Ok(())
    );

    let root = Platform::create_pmap_root().expect("LA64 process root");
    let first = VirtAddr(0x0000_2000_0000);
    let second = VirtAddr(first.0 + (1 << 21));
    let first_reservation =
        Platform::reserve_mapping(&root, first, PhysAddr(0x0090_0000), PmapReserveKind::Page4K)
            .expect("reserve first")
            .expect("first mapping");
    Platform::commit_mapping(
        &root,
        first_reservation,
        PmapPermissions::READ
            .union(PmapPermissions::WRITE)
            .union(PmapPermissions::USER),
    );

    let second_reservation = Platform::reserve_mapping(
        &root,
        second,
        PhysAddr(0x0090_1000),
        PmapReserveKind::Page4K,
    )
    .expect("reserve second")
    .expect("second mapping");
    assert_eq!(second_reservation.intermediates().l2, None);
    assert_eq!(second_reservation.intermediates().l1, None);
    assert!(second_reservation.intermediates().l0.is_some());

    Platform::rollback_mapping(&root, second_reservation);
    assert!(la64_l0_table_mut(root.phys(), second).is_none());
    assert!(la64_l0_table_mut(root.phys(), first).is_some());

    Platform::destroy_pmap_root(root);
    assert_eq!(TEST_PMAP_RELEASES.load(Ordering::Acquire), 5);

    reset_pmap_test_state();
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn pmap_shootdown_paths_are_host_noops() {
    let invalidation = PmapInvalidation::new(VirtAddr(LA64_DMW_CACHED_BASE), 4096);

    Platform::shootdown_kernel_mapping(invalidation);
    Platform::shootdown_mapping(Asid(1), invalidation);
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn cache_and_dma_paths_publish_qemu_coherent_defaults() {
    const { assert!(<Platform as PlatformConfig>::DMA_COHERENT) };
    const { assert!(<Platform as DmaIf>::DMA_COHERENT) };
    assert_eq!(
        <Platform as DmaIf>::phys_to_dma(PhysAddr(0x1234)),
        DmaAddr(0x1234)
    );
    assert_eq!(
        <Platform as DmaIf>::dma_to_phys(DmaAddr(0x5678)),
        PhysAddr(0x5678)
    );
    <Platform as CacheIf>::fence_all();
    <Platform as CacheIf>::fence_i_local();
    <Platform as CacheIf>::fence_i_all();
    <Platform as CacheIf>::flush_icache_range(VirtAddr(0x1000), 128);
    <Platform as DmaIf>::sync_for_device(PhysAddr(0x1000), 128, DmaDirection::Bidirectional);
    <Platform as DmaIf>::sync_for_cpu(PhysAddr(0x1000), 128, DmaDirection::Bidirectional);
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn irq_dispatch_table_routes_handlers_and_masks_spurious() {
    TEST_IRQ_DISPATCHES.store(0, Ordering::Release);
    LA64_IRQ_DISPATCH_TABLE.store(0, Ordering::Release);
    reset_la64_host_irq_controller_for_test();
    unsafe {
        core::ptr::write(
            core::ptr::addr_of_mut!(TEST_IRQ_TABLE),
            IrqDispatchTable::new(),
        );
        (*core::ptr::addr_of_mut!(TEST_IRQ_TABLE)).entries[QEMU_LA64_UART0_IRQ as usize] =
            Some(test_irq_handler);
        let table = &*core::ptr::addr_of!(TEST_IRQ_TABLE);
        <Platform as IrqIf>::install_dispatch_table(table);
    }

    assert_eq!(<Platform as IrqIf>::MAX_IRQ, 128);
    assert_eq!(<Platform as IrqIf>::claim(), 0);
    assert_eq!(
        <Platform as IrqIf>::dispatch_irq(QEMU_LA64_UART0_IRQ),
        IrqHandled::Wake
    );
    assert_eq!(TEST_IRQ_DISPATCHES.load(Ordering::Acquire), 1);
    assert_eq!(<Platform as IrqIf>::dispatch_irq(67), IrqHandled::Done);
    <Platform as IrqIf>::complete(QEMU_LA64_UART0_IRQ);
    <Platform as IrqIf>::mask(QEMU_LA64_UART0_IRQ);
    <Platform as IrqIf>::unmask(QEMU_LA64_UART0_IRQ);
    <Platform as IrqIf>::set_priority(QEMU_LA64_UART0_IRQ, 1);
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn la64_irq_claim_masks_and_completes_qemu_uart_gsi() {
    reset_la64_host_irq_controller_for_test();
    let ext_irq = QEMU_LA64_UART0_IRQ - QEMU_LA64_GSI_BASE;
    let bit = 1u64 << ext_irq;
    LA64_HOST_EIOINTC_COREISR0.store(bit, Ordering::Release);

    assert_eq!(<Platform as IrqIf>::claim(), 0);

    <Platform as IrqIf>::unmask(QEMU_LA64_UART0_IRQ);
    assert_eq!(LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire), bit);
    assert_eq!(LA64_HOST_PCH_PIC_MASK.load(Ordering::Acquire) & bit, 0);
    assert_eq!(<Platform as IrqIf>::claim(), QEMU_LA64_UART0_IRQ);

    <Platform as IrqIf>::complete(QEMU_LA64_UART0_IRQ);
    assert_eq!(LA64_HOST_EIOINTC_COREISR0.load(Ordering::Acquire) & bit, 0);

    LA64_HOST_EIOINTC_COREISR0.store(bit, Ordering::Release);
    <Platform as IrqIf>::mask(QEMU_LA64_UART0_IRQ);
    assert_eq!(LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire) & bit, 0);
    assert_ne!(LA64_HOST_PCH_PIC_MASK.load(Ordering::Acquire) & bit, 0);
    assert_eq!(<Platform as IrqIf>::claim(), 0);
}

fn test_irq_handler(irq: u32) -> IrqHandled {
    assert_eq!(irq, QEMU_LA64_UART0_IRQ);
    TEST_IRQ_DISPATCHES.fetch_add(1, Ordering::AcqRel);
    IrqHandled::Wake
}

#[test]
fn la64_timebase_frequency_uses_cpucfg_ratio() {
    assert_eq!(
        la64_cpucfg_timebase_frequency(LA64_CPUCFG2_LLFTP, 100_000_000, 3 | (2 << 16)),
        150_000_000
    );
    assert_eq!(
        la64_cpucfg_timebase_frequency(0, 100_000_000, 1 | (1 << 16)),
        0
    );
    assert_eq!(
        la64_cpucfg_timebase_frequency(LA64_CPUCFG2_LLFTP, 100_000_000, 0),
        0
    );
}

#[test]
fn la64_timer_deadline_rounds_up_to_tcfg_granule() {
    assert_eq!(round_up_to_tcfg_ticks(1), 4);
    assert_eq!(round_up_to_tcfg_ticks(4), 4);
    assert_eq!(round_up_to_tcfg_ticks(5), 8);
    assert_eq!(round_up_to_tcfg_ticks(u64::MAX), u64::MAX - 3);
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn timeif_paths_are_host_noops_without_cpu_counter() {
    assert_eq!(<Platform as TimeIf>::frequency_hz(), 0);
    assert_eq!(<Platform as TimeIf>::read_ns(), 0);
    <Platform as TimeIf>::set_deadline_ns(1_000_000);
    <Platform as TimeIf>::cancel_deadline();
    <Platform as TimeIf>::enable_timer_wakeups();
}

#[test]
#[cfg(not(target_arch = "loongarch64"))]
fn percpu_and_smp_publish_uniprocessor_state() {
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    assert_eq!(<Platform as PercpuIf>::current_cpu_id(), CpuId(0));
    assert_eq!(<Platform as SmpIf>::current_cpu_id(), CpuId(0));
    assert_eq!(
        <Platform as SmpIf>::possible_cpus(),
        CpuMask::single(CpuId(0))
    );

    <Platform as SmpIf>::mark_cpu_online(CpuId(0));
    assert_eq!(
        <Platform as SmpIf>::online_cpus(),
        CpuMask::single(CpuId(0))
    );
    assert_eq!(
        <Platform as SmpIf>::boot_secondary_cpus(test_secondary_entry),
        0
    );
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Reschedule));
    <Platform as SmpIf>::enable_ipi_wakeups();

    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::single(CpuId(0)));
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule),
        CpuMask::EMPTY
    );
    <Platform as SmpIf>::ack_ipi(IpiKind::Reschedule);
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule),
        CpuMask::single(CpuId(0))
    );

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn pt_node_allocator_handoff_is_one_shot_and_releases_typed_frames() {
    let _guard = lock_test_pmap_state();
    reset_pt_node_allocator_for_test();
    TEST_PT_ALLOCATIONS.store(0, Ordering::Release);
    TEST_PT_RELEASES.store(0, Ordering::Release);

    assert_eq!(Platform::alloc_pt_node(), Err(AllocError::Exhausted));
    assert_eq!(
        Platform::install_pt_node_allocator(test_pt_allocator),
        Ok(())
    );
    assert_eq!(
        Platform::install_pt_node_allocator(test_pt_allocator),
        Err(PmapError::AlreadyMapped)
    );

    let node = Platform::alloc_pt_node().expect("typed frame PT node");
    assert_eq!(node.phys, PhysAddr(0x0040_0000));
    assert_eq!(node.source_kind(), PtNodeSourceKind::TypedFrame);
    assert_eq!(TEST_PT_ALLOCATIONS.load(Ordering::Acquire), 1);

    Platform::free_pt_node(node);
    assert_eq!(TEST_PT_RELEASES.load(Ordering::Acquire), 1);

    reset_pt_node_allocator_for_test();
}

fn test_pt_allocator() -> Result<PtNode, AllocError> {
    TEST_PT_ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
    Ok(PtNode::typed_frame(PhysAddr(0x0040_0000), test_pt_release))
}

unsafe fn test_pt_release(phys: PhysAddr) {
    assert_eq!(phys, PhysAddr(0x0040_0000));
    TEST_PT_RELEASES.fetch_add(1, Ordering::AcqRel);
}

fn test_root_allocator() -> Result<PtNode, AllocError> {
    TEST_ROOT_ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
    Ok(PtNode::typed_frame(test_root_phys(), test_root_release))
}

unsafe fn test_root_release(phys: PhysAddr) {
    assert_eq!(phys, test_root_phys());
    TEST_ROOT_RELEASES.fetch_add(1, Ordering::AcqRel);
}

fn test_root_phys() -> PhysAddr {
    PhysAddr(core::ptr::addr_of_mut!(TEST_ROOT_PAGE) as *mut u8 as usize)
}

fn test_pmap_allocator() -> Result<PtNode, AllocError> {
    let index = TEST_PMAP_ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
    if index >= 8 {
        return Err(AllocError::Exhausted);
    }
    Ok(PtNode::typed_frame(
        test_pmap_phys(index),
        test_pmap_release,
    ))
}

unsafe fn test_pmap_release(_phys: PhysAddr) {
    TEST_PMAP_RELEASES.fetch_add(1, Ordering::AcqRel);
}

fn test_pmap_phys(index: usize) -> PhysAddr {
    unsafe {
        let base = core::ptr::addr_of_mut!(TEST_PMAP_PAGES) as *mut TestPmapPage;
        PhysAddr(base.add(index).cast::<u8>() as usize)
    }
}

unsafe extern "C" fn test_secondary_entry(_cpu_id: usize) -> ! {
    panic!("LA64 qemu virt is configured as uniprocessor in this HAL crate")
}

fn reset_pt_node_allocator_for_test() {
    INSTALLED_PT_NODE_ALLOCATOR.store(0, Ordering::Release);
}

fn reset_la64_asids_for_test() {
    LA64_ALLOCATED_ASIDS.store(1, Ordering::Release);
}

fn reset_pmap_test_state() {
    reset_pt_node_allocator_for_test();
    reset_la64_asids_for_test();
    TEST_PMAP_ALLOCATIONS.store(0, Ordering::Release);
    TEST_PMAP_RELEASES.store(0, Ordering::Release);
    LA64_KERNEL_PGDH_PHYS.store(0, Ordering::Release);
    LA64_ACTIVE_PGDL.store(0, Ordering::Release);
    LA64_ACTIVE_PGDH.store(0, Ordering::Release);
    LA64_ACTIVE_ASID.store(0, Ordering::Release);
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!(TEST_PMAP_PAGES).cast::<u8>(),
            0,
            core::mem::size_of::<[TestPmapPage; 8]>(),
        );
    }
    reset_la64_committed_pt_nodes_for_test();
}

fn lock_test_pmap_state() -> std::sync::MutexGuard<'static, ()> {
    TEST_PMAP_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reset_la64_committed_pt_nodes_for_test() {
    let _guard = lock_la64_committed_pt_node_registry();
    let nodes = unsafe { &mut *LA64_COMMITTED_PT_NODES.0.get() };
    nodes.fill(None);
}

#[cfg(not(target_arch = "loongarch64"))]
fn reset_la64_host_irq_controller_for_test() {
    LA64_HOST_EIOINTC_ENABLE0.store(0, Ordering::Release);
    LA64_HOST_EIOINTC_COREISR0.store(0, Ordering::Release);
    LA64_HOST_PCH_PIC_MASK.store(u64::MAX, Ordering::Release);
}
