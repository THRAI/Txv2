use tx_hal::{
    AuxvIf, CacheIf, ConsoleIf, CpuId, CpuMask, DmaAddr, DmaDirection, DmaIf, FaultInfo, IpiKind,
    IrqDispatchTable, IrqHandled, IrqIf, KernelTrapSink, PercpuIf, PersistentClockIf, PhysAddr,
    SmpIf, TrapAction, TrapClass, TrapFrameMut, TrapFrameSnapshot, TrapIf, TrapPreviousMode,
    VirtAddr,
};

use crate::{
    asid_residency_mask, asid_tlb_hart_mask, begin_asid_switch_on_current_cpu,
    clear_asid_residency, deactivate_current_user_pmap, dispatch_trap_frame, enter_irq_context,
    finish_asid_switch_on_current_cpu, for_each_console_byte_for_sbi, limit_cpus,
    mark_asid_resident_on_current_cpu, mark_ipi_ack, percpu_tls_for_cpu, remote_ipi_targets_from,
    remote_sfence_targets_for_asid_from, remote_sfence_targets_from, trap::classify_rv64_trap,
    Platform, Rv64TrapFrame, MAX_BOOT_CPUS, RV64_PERCPU_AREAS,
};

static RV64_HAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static IRQ_HANDLER_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn wake_irq_handler(irq: u32) -> IrqHandled {
    assert_eq!(irq, 8);
    IRQ_HANDLER_COUNT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    IrqHandled::Wake
}

struct RecordingTrapSink;

impl KernelTrapSink<Platform> for RecordingTrapSink {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        assert_eq!(view.view().previous_mode, TrapPreviousMode::User);
        assert_eq!(view.view().fault_address, Some(VirtAddr(0xfeed_cafe)));
        assert_eq!(view.view().faulting_instruction, Some(VirtAddr(0x3000)));
        assert_eq!(fault.address, VirtAddr(0xfeed_cafe));
        assert!(fault.write);
        assert!(!fault.instruction);
        assert!(fault.from_user);
        TrapAction::Terminate
    }

    fn on_syscall(mut view: TrapFrameMut<'_>) -> TrapAction {
        assert_eq!(view.view().syscall_number, 64);
        assert_eq!(view.view().syscall_args, [1, 2, 3, 4, 5, 6]);
        view.set_syscall_return(123);
        TrapAction::Resume
    }

    fn on_timer_interrupt(_cpu: CpuId, _view: TrapFrameMut<'_>) -> TrapAction {
        assert!(<Platform as IrqIf>::in_irq_context());
        TrapAction::Reschedule
    }

    fn on_external_irq(_cpu: CpuId, _view: TrapFrameMut<'_>) -> TrapAction {
        assert!(<Platform as IrqIf>::in_irq_context());
        TrapAction::Resume
    }

    fn on_ipi(_cpu: CpuId, _view: TrapFrameMut<'_>) -> TrapAction {
        assert!(<Platform as IrqIf>::in_irq_context());
        TrapAction::Resume
    }

    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        assert_eq!(view.view().pc, VirtAddr(0x4040));
        assert_eq!(fault.address, VirtAddr(0x4040));
        assert!(fault.instruction);
        TrapAction::Terminate
    }
}

#[test]
fn rv64_trap_classification_decodes_sync_faults_and_interrupts() {
    assert_eq!(classify_rv64_trap(2), TrapClass::IllegalInstruction);
    assert_eq!(classify_rv64_trap(3), TrapClass::Breakpoint);
    assert_eq!(
        classify_rv64_trap(4),
        TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        }
    );
    assert_eq!(
        classify_rv64_trap(6),
        TrapClass::AlignmentFault {
            write: true,
            instruction: false,
        }
    );
    assert_eq!(
        classify_rv64_trap(0),
        TrapClass::AlignmentFault {
            write: false,
            instruction: true,
        }
    );
    assert_eq!(classify_rv64_trap(8), TrapClass::Syscall);
    assert_eq!(
        classify_rv64_trap(12),
        TrapClass::PageFault {
            write: false,
            instruction: true,
        }
    );
    assert_eq!(
        classify_rv64_trap(13),
        TrapClass::PageFault {
            write: false,
            instruction: false,
        }
    );
    assert_eq!(
        classify_rv64_trap(15),
        TrapClass::PageFault {
            write: true,
            instruction: false,
        }
    );

    let interrupt_bit = 1usize << (usize::BITS as usize - 1);
    assert_eq!(
        classify_rv64_trap(interrupt_bit | 1),
        TrapClass::InterprocessorInterrupt
    );
    assert_eq!(
        classify_rv64_trap(interrupt_bit | 5),
        TrapClass::TimerInterrupt
    );
    assert_eq!(
        Platform::classify_trap(TrapFrameSnapshot {
            scause: interrupt_bit | 9,
            sepc: 0x1000,
            stval: 0,
        }),
        TrapClass::ExternalInterrupt
    );
}

#[test]
fn rv64_trap_classification_distinguishes_unknown_sync_and_interrupt() {
    let interrupt_bit = 1usize << (usize::BITS as usize - 1);

    assert_eq!(classify_rv64_trap(63), TrapClass::UnknownSync);
    assert_eq!(
        classify_rv64_trap(interrupt_bit | 63),
        TrapClass::UnknownInterrupt
    );
}

#[test]
fn trap_class_legacy_names_remain_compatible() {
    assert_eq!(
        TrapClass::InstructionPageFault,
        TrapClass::PageFault {
            write: false,
            instruction: true,
        }
    );
    assert_eq!(
        TrapClass::LoadPageFault,
        TrapClass::PageFault {
            write: false,
            instruction: false,
        }
    );
    assert_eq!(
        TrapClass::StorePageFault,
        TrapClass::PageFault {
            write: true,
            instruction: false,
        }
    );
    assert_eq!(TrapClass::UserEnvCall, TrapClass::Syscall);
    assert_eq!(TrapClass::SupervisorTimer, TrapClass::TimerInterrupt);
    assert_eq!(TrapClass::SupervisorExternal, TrapClass::ExternalInterrupt);
    assert_eq!(TrapClass::Unknown, TrapClass::UnknownSync);
}

#[test]
fn platform_trap_snapshot_projects_portable_fault_fields() {
    let snapshot = TrapFrameSnapshot {
        scause: 15,
        sepc: 0x2000,
        stval: 0xfeed_cafe,
    };

    let portable = Platform::snapshot_trap(snapshot);

    assert_eq!(
        portable.class,
        TrapClass::PageFault {
            write: true,
            instruction: false,
        }
    );
    assert_eq!(portable.pc, VirtAddr(0x2000));
    assert_eq!(portable.fault_address, Some(VirtAddr(0xfeed_cafe)));
}

#[test]
fn percpu_install_sets_kernel_tls_pointer_and_current_cpu() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

    <Platform as PercpuIf>::install_early_percpu(CpuId(7));

    let kernel_tls = <Platform as PercpuIf>::read_kernel_tls() as usize;
    assert_eq!(MAX_BOOT_CPUS, 8);
    assert_eq!(RV64_PERCPU_AREAS.len(), 8);
    assert_eq!(Some(kernel_tls), percpu_tls_for_cpu(CpuId(7)));
    assert_eq!(<Platform as PercpuIf>::current_cpu_id(), CpuId(7));
    assert_eq!(<Platform as SmpIf>::current_cpu_id(), CpuId(7));

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn cpu_limit_keeps_boot_hart_and_caps_discovered_topology() {
    let discovered = CpuMask::first(8);
    assert_eq!(
        limit_cpus(discovered, 1, CpuId(3)),
        CpuMask::single(CpuId(3))
    );
    assert_eq!(limit_cpus(discovered, 4, CpuId(3)).count(), 4);
    assert!(limit_cpus(discovered, 4, CpuId(3)).contains(CpuId(3)));

    let sparse = CpuMask::from_bits((1 << 1) | (1 << 4) | (1 << 7));
    assert_eq!(
        limit_cpus(sparse, 2, CpuId(4)),
        CpuMask::from_bits((1 << 1) | (1 << 4))
    );
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn percpu_install_kernel_stack_records_stack_top() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

    <Platform as PercpuIf>::install_early_percpu(CpuId(1));
    unsafe {
        <Platform as PercpuIf>::install_kernel_stack(VirtAddr(0x8000_4000));
    }

    assert_eq!(
        RV64_PERCPU_AREAS[1].kernel_stack_top(),
        VirtAddr(0x8000_4000)
    );

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn irq_context_guard_tracks_nested_interrupt_depth() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

    <Platform as PercpuIf>::install_early_percpu(CpuId(3));
    assert!(!<Platform as IrqIf>::in_irq_context());

    {
        let _outer = enter_irq_context();
        assert!(<Platform as IrqIf>::in_irq_context());
        assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 1);

        {
            let _inner = enter_irq_context();
            assert!(<Platform as IrqIf>::in_irq_context());
            assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 2);
        }

        assert!(<Platform as IrqIf>::in_irq_context());
        assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 1);
    }

    assert!(!<Platform as IrqIf>::in_irq_context());
    assert_eq!(RV64_PERCPU_AREAS[3].irq_depth(), 0);

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn plic_priority_enable_claim_and_complete_use_current_context() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    super::HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .reset();

    <Platform as PercpuIf>::install_early_percpu(CpuId(1));
    let context = super::plic_context_for_cpu(CpuId(1));

    <Platform as IrqIf>::set_priority(8, 3);
    assert_eq!(
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .read_u32(super::plic_priority_offset(8)),
        3
    );

    <Platform as IrqIf>::unmask(8);
    let enable_offset = super::plic_enable_word_offset(context, 0);
    assert_ne!(
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .read_u32(enable_offset)
            & (1 << 8),
        0
    );

    super::HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .write_u32(super::plic_claim_complete_offset(context), 8);
    assert_eq!(<Platform as IrqIf>::claim(), 8);

    <Platform as IrqIf>::complete(8);
    assert_eq!(
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .read_u32(super::plic_claim_complete_offset(context)),
        8
    );

    <Platform as IrqIf>::mask(8);
    assert_eq!(
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .read_u32(enable_offset)
            & (1 << 8),
        0
    );

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn plic_dispatch_table_invokes_handler_and_masks_unhandled_irq() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    super::HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .reset();
    IRQ_HANDLER_COUNT.store(0, std::sync::atomic::Ordering::Release);

    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    let context = super::plic_context_for_cpu(CpuId(0));
    let mut table = IrqDispatchTable::new();
    table.entries[8] = Some(wake_irq_handler);
    let table = std::boxed::Box::leak(std::boxed::Box::new(table));
    <Platform as IrqIf>::install_dispatch_table(table);

    assert_eq!(<Platform as IrqIf>::dispatch_irq(8), IrqHandled::Wake);
    assert_eq!(
        IRQ_HANDLER_COUNT.load(std::sync::atomic::Ordering::Acquire),
        1
    );

    <Platform as IrqIf>::unmask(9);
    let enable_offset = super::plic_enable_word_offset(context, 0);
    assert_ne!(
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .read_u32(enable_offset)
            & (1 << 9),
        0
    );

    assert_eq!(<Platform as IrqIf>::dispatch_irq(9), IrqHandled::Done);
    assert_eq!(
        super::HOST_PLIC_STATE
            .lock()
            .expect("host plic state")
            .read_u32(enable_offset)
            & (1 << 9),
        0
    );

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn qemu_mmio_regions_include_goldfish_rtc() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let regions = crate::boot_static::qemu_mmio_regions();
    let rtc = regions
        .iter()
        .find(|region| region.name == "goldfish-rtc")
        .expect("goldfish rtc mmio region");

    assert_eq!(rtc.phys.start, PhysAddr(super::GOLDFISH_RTC_PHYS_BASE));
    assert_eq!(rtc.phys.size, 0x1000);
    assert_eq!(rtc.virt.start, VirtAddr(0xffff_ffc0_0010_1000));
    assert_eq!(rtc.virt.size, 0x1000);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn goldfish_persistent_clock_reads_time_low_then_high() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let expected = 0x0123_4567_89ab_cdef;
    {
        let mut state = super::HOST_GOLDFISH_RTC_STATE
            .lock()
            .expect("host goldfish rtc state");
        state.reset();
        state.set_time_ns(expected);
    }

    assert_eq!(
        <Platform as PersistentClockIf>::read_realtime_ns(),
        Ok(expected)
    );

    let state = super::HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state");
    assert_eq!(
        state.read_log(),
        [super::GOLDFISH_RTC_TIME_LOW, super::GOLDFISH_RTC_TIME_HIGH]
    );
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn goldfish_persistent_clock_writes_time_high_then_low() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let ns = 0x1111_2222_3333_4444;
    {
        let mut state = super::HOST_GOLDFISH_RTC_STATE
            .lock()
            .expect("host goldfish rtc state");
        state.reset();
    }

    assert_eq!(<Platform as PersistentClockIf>::set_realtime_ns(ns), Ok(()));

    let state = super::HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state");
    assert_eq!(
        state.write_log(),
        [super::GOLDFISH_RTC_TIME_HIGH, super::GOLDFISH_RTC_TIME_LOW]
    );
    assert_eq!(state.write_values(), [0x1111_2222, 0x3333_4444]);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn goldfish_persistent_clock_programs_alarm_and_unmasks_irq() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    super::HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .reset();
    {
        let mut state = super::HOST_GOLDFISH_RTC_STATE
            .lock()
            .expect("host goldfish rtc state");
        state.reset();
    }

    let ns = 0xaaaa_bbbb_cccc_dddd;
    assert_eq!(
        <Platform as PersistentClockIf>::set_wake_alarm_ns(ns),
        Ok(())
    );

    let state = super::HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state");
    assert_eq!(
        state.write_log(),
        [
            super::GOLDFISH_RTC_ALARM_HIGH,
            super::GOLDFISH_RTC_ALARM_LOW,
            super::GOLDFISH_RTC_IRQ_ENABLED
        ]
    );
    assert_eq!(state.write_values(), [0xaaaa_bbbb, 0xcccc_dddd, 1]);
    drop(state);

    let plic = super::HOST_PLIC_STATE.lock().expect("host plic state");
    assert_eq!(
        plic.read_u32(super::plic_priority_offset(super::GOLDFISH_RTC_IRQ)),
        1
    );
    assert_ne!(
        plic.read_u32(super::plic_enable_word_offset(
            super::plic_context_for_cpu(CpuId(0)),
            0
        )) & (1 << super::GOLDFISH_RTC_IRQ),
        0
    );

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn goldfish_persistent_clock_clear_alarm_disables_and_clears_pending_irq() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    super::HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .reset();
    <Platform as IrqIf>::unmask(super::GOLDFISH_RTC_IRQ);
    {
        let mut state = super::HOST_GOLDFISH_RTC_STATE
            .lock()
            .expect("host goldfish rtc state");
        state.reset();
        state.set_alarm_status(true);
    }

    assert_eq!(<Platform as PersistentClockIf>::clear_wake_alarm(), Ok(()));

    let state = super::HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state");
    assert_eq!(
        state.write_log(),
        [
            super::GOLDFISH_RTC_IRQ_ENABLED,
            super::GOLDFISH_RTC_CLEAR_ALARM,
            super::GOLDFISH_RTC_CLEAR_INTERRUPT
        ]
    );
    assert_eq!(state.write_values(), [0, 1, 1]);
    drop(state);

    let plic = super::HOST_PLIC_STATE.lock().expect("host plic state");
    assert_eq!(
        plic.read_u32(super::plic_enable_word_offset(
            super::plic_context_for_cpu(CpuId(0)),
            0
        )) & (1 << super::GOLDFISH_RTC_IRQ),
        0
    );

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn goldfish_persistent_clock_acknowledges_alarm_irq_without_disabling_alarm() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    {
        let mut state = super::HOST_GOLDFISH_RTC_STATE
            .lock()
            .expect("host goldfish rtc state");
        state.reset();
    }

    assert_eq!(
        <Platform as PersistentClockIf>::acknowledge_wake_alarm_irq(),
        Ok(())
    );

    let state = super::HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state");
    assert_eq!(state.write_log(), [super::GOLDFISH_RTC_CLEAR_INTERRUPT]);
    assert_eq!(state.write_values(), [1]);
}

#[test]
fn cache_methods_are_callable_on_qemu_coherent_platform() {
    <Platform as CacheIf>::fence_all();
    <Platform as CacheIf>::fence_i_local();
    <Platform as CacheIf>::fence_i_all();
    <Platform as CacheIf>::flush_icache_range(VirtAddr(0x8020_0000), 4096);
    <Platform as CacheIf>::dcache_clean_range(PhysAddr(0x8020_0000), 4096);
    <Platform as CacheIf>::dcache_invalidate_range(PhysAddr(0x8020_0000), 4096);
    <Platform as CacheIf>::dcache_clean_invalidate_range(PhysAddr(0x8020_0000), 4096);
}

#[test]
fn dma_identity_mapping_and_sync_are_qemu_coherent() {
    assert!(core::hint::black_box(<Platform as DmaIf>::DMA_COHERENT));
    assert_eq!(
        <Platform as DmaIf>::phys_to_dma(PhysAddr(0x8020_1000)),
        DmaAddr(0x8020_1000)
    );
    assert_eq!(
        <Platform as DmaIf>::dma_to_phys(DmaAddr(0x8020_2000)),
        PhysAddr(0x8020_2000)
    );

    <Platform as DmaIf>::sync_for_device(PhysAddr(0x8020_3000), 512, DmaDirection::ToDevice);
    <Platform as DmaIf>::sync_for_cpu(PhysAddr(0x8020_3000), 512, DmaDirection::FromDevice);
    <Platform as DmaIf>::sync_for_device(PhysAddr(0x8020_3000), 512, DmaDirection::Bidirectional);
    <Platform as DmaIf>::sync_for_cpu(PhysAddr(0x8020_3000), 512, DmaDirection::Bidirectional);
}

#[test]
fn auxv_facts_publish_riscv64_platform_and_hwcap() {
    let facts = <Platform as AuxvIf>::arch_auxv_facts();

    assert_eq!(
        facts.page_size,
        <Platform as tx_hal::PlatformConfig>::PAGE_SIZE
    );
    assert_eq!(facts.hwcap, tx_hal::RISCV_HWCAP_IMAFDC);
    assert_eq!(facts.hwcap2, 0);
    assert_eq!(facts.platform, "riscv64");
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn console_read_bytes_is_nonblocking_when_host_has_no_sbi_input() {
    let mut buf = [0xaa; 4];

    assert_eq!(<Platform as ConsoleIf>::read_bytes(&mut buf), 0);
    assert_eq!(tx_hal::console_read_bytes::<Platform>(&mut buf), 0);
    assert_eq!(buf, [0xaa; 4]);
}

#[test]
fn trap_frame_view_projects_rv64_trap_metadata() {
    let mut frame = test_trap_frame(15, 0x3000, 0xfeed_cafe);
    frame.sstatus &= !(1 << 8);
    frame.sstatus |= 1 << 5;
    frame.x[2] = 0x7000;
    frame.x[4] = 0x1234_5678;

    let view = frame.view();

    assert_eq!(view.pc, VirtAddr(0x3000));
    assert_eq!(view.sp, VirtAddr(0x7000));
    assert_eq!(view.fault_address, Some(VirtAddr(0xfeed_cafe)));
    assert_eq!(view.faulting_instruction, Some(VirtAddr(0x3000)));
    assert_eq!(view.previous_mode, TrapPreviousMode::User);
    assert!(view.interrupts_enabled_before);
    assert_eq!(view.user_tls_register, 0x1234_5678);
}

#[test]
fn trap_frame_view_omits_fault_fields_for_interrupts_and_syscalls() {
    let interrupt_bit = 1usize << (usize::BITS as usize - 1);

    let syscall = test_trap_frame(8, 0x1000, 0xaaaa);
    let syscall_view = syscall.view();
    assert_eq!(syscall_view.fault_address, None);
    assert_eq!(syscall_view.faulting_instruction, Some(VirtAddr(0x1000)));

    let interrupt = test_trap_frame(interrupt_bit | 5, 0x2000, 0xbbbb);
    let interrupt_view = interrupt.view();
    assert_eq!(interrupt_view.fault_address, None);
    assert_eq!(interrupt_view.faulting_instruction, None);
}

#[test]
fn trap_frame_view_projects_syscall_fields() {
    let mut frame = test_trap_frame(8, 0x1000, 0);
    frame.x[10] = 1;
    frame.x[11] = 2;
    frame.x[12] = 3;
    frame.x[13] = 4;
    frame.x[14] = 5;
    frame.x[15] = 6;
    frame.x[17] = 64;

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

    assert_eq!(action, TrapAction::Resume);
    assert_eq!(frame.x[10], 123);
}

#[test]
fn trap_frame_mutators_write_saved_registers() {
    let mut frame = test_trap_frame(8, 0x1000, 0);

    {
        let mut view = frame.view_mut();
        view.set_pc(VirtAddr(0x1111));
        view.set_sp(VirtAddr(0x2222));
        view.set_syscall_return(7);
        view.set_user_tls_register(0x3333);

        assert_eq!(view.view().pc, VirtAddr(0x1111));
        assert_eq!(view.view().sp, VirtAddr(0x2222));
        assert_eq!(view.view().syscall_args[0], 7);
        assert_eq!(view.view().user_tls_register, 0x3333);
    }

    assert_eq!(frame.sepc, 0x1111);
    assert_eq!(frame.x[2], 0x2222);
    assert_eq!(frame.x[10], 7);
    assert_eq!(frame.x[4], 0x3333);
}

#[test]
fn trap_frame_signal_context_round_trips_user_registers() {
    let mut frame = test_trap_frame(8, 0x1000, 0);
    for (idx, reg) in frame.x.iter_mut().enumerate() {
        *reg = 0x1000 + idx;
    }
    frame.x[0] = 0;
    frame.x[2] = 0x8000;
    frame.sstatus &= !(1 << 8);

    let saved = frame.view_mut().capture_user_context();

    frame.x[1] = 0xaaaa;
    frame.x[2] = 0xbbbb;
    frame.sepc = 0xcccc;
    frame.sstatus |= 1 << 8;

    frame.view_mut().restore_user_context(&saved);

    assert_eq!(frame.x, saved.regs);
    assert_eq!(frame.sepc, 0x1000);
    assert_eq!(frame.x[2], 0x8000);
    assert_eq!(frame.sstatus & (1 << 8), 0);
    assert_ne!(frame.sstatus & (1 << 5), 0);
}

#[test]
fn trap_frame_signal_handler_regs_write_entry_arguments() {
    let mut frame = test_trap_frame(8, 0x4000, 0);

    {
        let mut view = frame.view_mut();
        view.set_signal_handler_regs(tx_hal::SignalHandlerRegs {
            return_pc: VirtAddr(0x7000),
            args: [9, 0x7100, 0x7200],
        });
        view.set_pc(VirtAddr(0x6000));
        view.set_sp(VirtAddr(0x5ff0));
    }

    assert_eq!(frame.sepc, 0x6000);
    assert_eq!(frame.x[1], 0x7000);
    assert_eq!(frame.x[2], 0x5ff0);
    assert_eq!(frame.x[10], 9);
    assert_eq!(frame.x[11], 0x7100);
    assert_eq!(frame.x[12], 0x7200);
}

#[test]
fn trap_frame_rewind_pc_steps_back_one_rv64_instruction() {
    let mut frame = test_trap_frame(8, 0x4004, 0);

    frame.view_mut().rewind_pc(4);

    assert_eq!(frame.sepc, 0x4000);
}

#[test]
fn trap_frame_mutators_encode_syscall_error() {
    let mut frame = test_trap_frame(8, 0x1000, 0);

    frame.view_mut().set_syscall_error(5);

    assert_eq!(frame.x[10], (-5isize) as usize);
}

#[test]
fn trap_frame_prepare_user_return_sets_sret_mode_bits() {
    let mut frame = test_trap_frame(8, 0x1000, 0);

    frame.prepare_user_return();

    assert_eq!(frame.sstatus & (1 << 8), 0, "SPP must be clear (user mode)");
    assert_ne!(frame.sstatus & (1 << 5), 0, "SPIE must be set");
    assert_eq!(frame.previous_mode(), TrapPreviousMode::User);
    // FP starts lazily disabled; the first user FP instruction enables
    // a zeroed FP image through the illegal-instruction path.
    assert_eq!((frame.sstatus >> 13) & 3, 0, "FS must be Off");
}

#[test]
fn trap_dispatch_routes_timer_to_sink_action() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    let interrupt_bit = 1usize << (usize::BITS as usize - 1);
    let mut frame = test_trap_frame(interrupt_bit | 5, 0x2000, 0);

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

    assert_eq!(action, TrapAction::Reschedule);
    assert!(!<Platform as IrqIf>::in_irq_context());
    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn trap_dispatch_marks_external_and_ipi_as_irq_context() {
    let _guard = RV64_HAL_TEST_LOCK.lock().expect("rv64 hal test lock");
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    let interrupt_bit = 1usize << (usize::BITS as usize - 1);

    let mut external = test_trap_frame(interrupt_bit | 9, 0x2000, 0);
    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut external);
    assert_eq!(action, TrapAction::Resume);
    assert!(!<Platform as IrqIf>::in_irq_context());

    let mut ipi = test_trap_frame(interrupt_bit | 1, 0x2000, 0);
    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut ipi);
    assert_eq!(action, TrapAction::Resume);
    assert!(!<Platform as IrqIf>::in_irq_context());

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
fn trap_dispatch_routes_page_fault_with_user_flag() {
    let mut frame = test_trap_frame(15, 0x3000, 0xfeed_cafe);
    frame.sstatus &= !(1 << 8);

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

    assert_eq!(action, TrapAction::Terminate);
}

#[test]
fn trap_dispatch_routes_sync_fault_to_illegal_or_sync_sink() {
    let mut frame = test_trap_frame(2, 0x4040, 0);
    frame.sstatus |= 1 << 13;

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

    assert_eq!(action, TrapAction::Terminate);
}

#[test]
fn trap_dispatch_lazily_enables_user_fp_once() {
    let mut frame = test_trap_frame(2, 0x4040, 0);
    frame.sstatus &= !(1 << 8);
    frame.f = [0xdead_beef_dead_beef; 32];
    frame.fcsr = 7;

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

    assert_eq!(action, TrapAction::Resume);
    assert_eq!(
        (frame.sstatus >> 13) & 3,
        1,
        "first FP trap should retry with FS=Initial"
    );
    assert_eq!(frame.f, [0u64; 32], "lazy FP must publish zero regs");
    assert_eq!(frame.fcsr, 0, "lazy FP must clear fcsr");

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);
    assert_eq!(
        action,
        TrapAction::Terminate,
        "non-FP illegal instruction is only retried once"
    );
}

#[test]
fn remote_sfence_targets_exclude_current_hart() {
    let targets = remote_sfence_targets_from(CpuMask::from_bits(0b1111), CpuId(2));

    assert_eq!(targets.bits(), 0b1011);
}

#[test]
fn remote_sfence_targets_are_empty_for_uniprocessor_online_mask() {
    let targets = remote_sfence_targets_from(CpuMask::single(CpuId(0)), CpuId(0));

    assert!(targets.is_empty());
}

#[test]
fn remote_ipi_targets_exclude_current_hart() {
    assert_eq!(
        remote_ipi_targets_from(CpuMask::from_bits(0b1111), CpuId(2)).bits(),
        0b1011
    );
    assert!(remote_ipi_targets_from(CpuMask::single(CpuId(0)), CpuId(0)).is_empty());
}

#[test]
fn remote_sfence_targets_include_harts_with_stale_asid_tlb_history() {
    let _guard = RV64_HAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let asid = tx_hal::Asid(9);
    clear_asid_residency(asid);

    <Platform as PercpuIf>::install_early_percpu(CpuId(1));
    mark_asid_resident_on_current_cpu(asid);
    <Platform as PercpuIf>::install_early_percpu(CpuId(3));
    mark_asid_resident_on_current_cpu(asid);

    let targets = remote_sfence_targets_for_asid_from(asid, CpuMask::from_bits(0b1111), CpuId(1));

    assert_eq!(targets.bits(), 0b1000);
    assert_eq!(asid_residency_mask(asid).bits(), 0b1010);
    deactivate_current_user_pmap();
    assert_eq!(
        asid_tlb_hart_mask(asid).bits(),
        0b1010,
        "switching away must retain the ASID TLB-history bit"
    );
    assert_eq!(
        remote_sfence_targets_for_asid_from(asid, CpuMask::from_bits(0b1111), CpuId(0)).bits(),
        0b1010,
        "later invalidation must still reach every hart that ran the ASID"
    );
    clear_asid_residency(asid);
}

#[test]
fn asid_switch_retains_old_residency_until_hardware_transition_finishes() {
    let _guard = RV64_HAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = tx_hal::Asid(10);
    let new = tx_hal::Asid(11);
    let cpu = CpuId(2);
    clear_asid_residency(old);
    clear_asid_residency(new);

    <Platform as PercpuIf>::install_early_percpu(cpu);
    mark_asid_resident_on_current_cpu(old);
    let switch = begin_asid_switch_on_current_cpu(new);

    assert_eq!(asid_residency_mask(old).bits(), CpuMask::single(cpu).bits());
    assert_eq!(asid_residency_mask(new).bits(), CpuMask::single(cpu).bits());
    assert_eq!(
        RV64_PERCPU_AREAS[cpu.0]
            .active_user_asid
            .load(core::sync::atomic::Ordering::Acquire),
        old.0 as usize,
        "software active root must remain old until satp changes"
    );

    finish_asid_switch_on_current_cpu(switch);

    assert!(asid_residency_mask(old).is_empty());
    assert_eq!(asid_residency_mask(new).bits(), CpuMask::single(cpu).bits());
    assert_eq!(
        asid_tlb_hart_mask(old).bits(),
        CpuMask::single(cpu).bits(),
        "old tagged translations may survive the context switch"
    );
    assert_eq!(
        RV64_PERCPU_AREAS[cpu.0]
            .active_user_asid
            .load(core::sync::atomic::Ordering::Acquire),
        new.0 as usize
    );

    deactivate_current_user_pmap();
    clear_asid_residency(old);
    clear_asid_residency(new);
}

#[test]
fn ipi_ack_observation_can_be_cleared_by_mask() {
    let _guard = RV64_HAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::from_bits(u64::MAX));
    mark_ipi_ack(CpuId(1), IpiKind::Reschedule);
    mark_ipi_ack(CpuId(3), IpiKind::Reschedule);

    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule).bits(),
        0b1010
    );

    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::single(CpuId(1)));

    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule).bits(),
        0b1000
    );
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn broadcast_excludes_self_and_keeps_kind_state_isolated() {
    let _guard = RV64_HAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    let current = CpuId(0);
    let remote = CpuId(1);
    let targets =
        CpuMask::from_bits(CpuMask::single(current).bits() | CpuMask::single(remote).bits());

    super::reset_ipi_software_state();
    super::TEST_IPI_TRANSPORT_MASK.store(0, std::sync::atomic::Ordering::Release);
    <Platform as PercpuIf>::install_early_percpu(current);

    <Platform as SmpIf>::broadcast_ipi(targets, IpiKind::Membarrier);

    assert_eq!(
        super::TEST_IPI_TRANSPORT_MASK.load(std::sync::atomic::Ordering::Acquire),
        CpuMask::single(remote).bits()
    );
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Membarrier),
        CpuMask::EMPTY
    );
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Maintenance),
        CpuMask::EMPTY
    );
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Membarrier));

    <Platform as PercpuIf>::install_early_percpu(remote);
    assert!(<Platform as SmpIf>::pending_ipi(IpiKind::Membarrier));
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Maintenance));
    <Platform as SmpIf>::ack_ipi(IpiKind::Membarrier);
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Membarrier),
        CpuMask::single(remote)
    );

    super::reset_ipi_software_state();
    super::TEST_IPI_TRANSPORT_MASK.store(0, std::sync::atomic::Ordering::Release);
    <Platform as PercpuIf>::install_early_percpu(current);
    <Platform as SmpIf>::broadcast_ipi(targets, IpiKind::Maintenance);
    assert_eq!(
        super::TEST_IPI_TRANSPORT_MASK.load(std::sync::atomic::Ordering::Acquire),
        CpuMask::single(remote).bits()
    );
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Maintenance),
        CpuMask::EMPTY
    );
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Membarrier),
        CpuMask::EMPTY
    );
    <Platform as PercpuIf>::install_early_percpu(remote);
    assert!(<Platform as SmpIf>::pending_ipi(IpiKind::Maintenance));
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Membarrier));
    <Platform as SmpIf>::ack_ipi(IpiKind::Maintenance);
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Maintenance),
        CpuMask::single(remote)
    );

    super::reset_ipi_software_state();
    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

#[test]
#[cfg(not(target_arch = "riscv64"))]
fn ipi_pending_and_ack_state_are_isolated_by_kind() {
    let _guard = RV64_HAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved_tls = <Platform as PercpuIf>::read_kernel_tls();
    let target = CpuId(1);

    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::from_bits(u64::MAX));
    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Maintenance, CpuMask::from_bits(u64::MAX));
    <Platform as PercpuIf>::install_early_percpu(CpuId(0));
    <Platform as SmpIf>::send_ipi(target, IpiKind::Reschedule);
    <Platform as SmpIf>::send_ipi(target, IpiKind::Maintenance);
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Reschedule));
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Maintenance));

    <Platform as PercpuIf>::install_early_percpu(target);
    assert!(<Platform as SmpIf>::pending_ipi(IpiKind::Reschedule));
    assert!(<Platform as SmpIf>::pending_ipi(IpiKind::Maintenance));
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::TlbShootdown));

    <Platform as SmpIf>::ack_ipi(IpiKind::Reschedule);
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Reschedule));
    assert!(<Platform as SmpIf>::pending_ipi(IpiKind::Maintenance));
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule),
        CpuMask::single(target)
    );
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Maintenance),
        CpuMask::EMPTY
    );

    <Platform as SmpIf>::ack_ipi(IpiKind::Maintenance);
    assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Maintenance));
    assert_eq!(
        <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Maintenance),
        CpuMask::single(target)
    );

    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::from_bits(u64::MAX));
    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Maintenance, CpuMask::from_bits(u64::MAX));
    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
}

fn test_trap_frame(scause: usize, sepc: usize, stval: usize) -> Rv64TrapFrame {
    Rv64TrapFrame {
        x: [0; 32],
        scause,
        sepc,
        stval,
        sstatus: 1 << 8,
        f: [0u64; 32],
        fcsr: 0,
        _fp_state_flags: 0,
        _trap_tmp_sscratch: 0,
    }
}

#[test]
fn trap_frame_fp_context_round_trips_through_capture_restore() {
    // FS=Dirty (3<<13): FP was used since last sret; SPIE set; SPP=0 (user mode).
    let mut frame = test_trap_frame(8, 0x1000, 0);
    frame.sstatus = (1 << 5) | (3 << 13);
    for (i, r) in frame.f.iter_mut().enumerate() {
        *r = 0xf000_0000_0000_0000 | i as u64;
    }
    frame.fcsr = 0x05;
    frame._fp_state_flags = 1;

    let ctx = frame.view_mut().capture_user_context();

    assert!(
        ctx.fp.is_valid(),
        "FP context must be valid when FS=Clean/Dirty"
    );
    assert_ne!(
        ctx.fp.flags & tx_hal::UserFpContext::FLAG_DIRTY,
        0,
        "FLAG_DIRTY must be set when FS=Dirty"
    );
    assert_eq!(ctx.fp.regs, frame.f, "FP regs must round-trip");
    assert_eq!(ctx.fp.fcsr, 0x05, "fcsr must round-trip");
    assert_eq!(
        (frame.sstatus >> 13) & 3,
        3,
        "capturing a Dirty image must not demote a direct return to Clean"
    );

    // Restore into a fresh frame and verify FP state is recovered.
    let mut frame2 = test_trap_frame(8, 0x2000, 0);
    frame2.sstatus = 0;
    frame2.view_mut().restore_user_context(&ctx);

    assert_eq!(frame2.f, frame.f, "restored FP regs must match original");
    assert_eq!(frame2.fcsr, 0x05, "restored fcsr must match original");
    // Restoring a saved image establishes a clean hardware copy. A later user
    // FP write, rather than the kernel return itself, marks it dirty again.
    assert_eq!(
        (frame2.sstatus >> 13) & 3,
        2,
        "FS must be Clean after restoring valid FP state"
    );
    assert_ne!(frame2._fp_state_flags & 2, 0);
}

#[test]
fn clean_fp_trap_reuses_authoritative_payload_image() {
    let mut frame = test_trap_frame(8, 0x1000, 0);
    frame.sstatus = (1 << 5) | (2 << 13);
    frame.f.fill(0xdead_beef_dead_beef);
    frame._fp_state_flags = 0;

    let ctx = frame.view_mut().capture_user_context();

    assert!(
        !ctx.fp.is_valid(),
        "Clean FS must reuse the already-published payload image"
    );
}

#[test]
fn trap_frame_fp_context_empty_when_fs_off() {
    // FS=Off (bits 14:13 = 00): FP disabled; captured FP state must be empty.
    let mut frame = test_trap_frame(8, 0x1000, 0);
    frame.sstatus = 0;
    for (i, r) in frame.f.iter_mut().enumerate() {
        *r = i as u64 + 1;
    }
    frame.fcsr = 0x03;

    let ctx = frame.view_mut().capture_user_context();

    assert!(
        !ctx.fp.is_valid(),
        "FP context must not be valid when FS=Off"
    );
    assert_eq!(ctx.fp.flags, 0);
}

#[test]
fn trap_frame_fp_context_zeroed_when_restored_without_valid_fp() {
    // restore_user_context with fp.is_valid()==false must zero the frame's FP slots.
    let mut frame = test_trap_frame(8, 0x1000, 0);
    frame.sstatus = 0; // FS=Off → capture returns empty fp
    for (i, r) in frame.f.iter_mut().enumerate() {
        *r = i as u64 + 1; // non-zero to verify zeroing
    }
    frame.fcsr = 0x07;

    let ctx = frame.view_mut().capture_user_context(); // fp.is_valid() == false
    assert!(!ctx.fp.is_valid());

    let mut frame2 = test_trap_frame(8, 0x2000, 0);
    for r in frame2.f.iter_mut() {
        *r = 0xdead_beef_dead_beef;
    }
    frame2.view_mut().restore_user_context(&ctx);

    assert_eq!(
        frame2.f, [0u64; 32],
        "FP regs must be zeroed when fp not valid"
    );
    assert_eq!(frame2.fcsr, 0, "fcsr must be zeroed when fp not valid");
}

#[test]
fn sbi_console_helper_collapses_crlf_pairs() {
    let mut out = std::vec::Vec::new();
    for_each_console_byte_for_sbi(b"hi\r\nthere\r\n", |byte| out.push(byte));
    assert_eq!(out, b"hi\nthere\n");
}

#[test]
fn sbi_console_helper_preserves_non_crlf_bytes() {
    let mut out = std::vec::Vec::new();
    for_each_console_byte_for_sbi(b"\rlead\nmid\rtrail", |byte| out.push(byte));
    assert_eq!(out, b"\rlead\nmid\rtrail");
}
