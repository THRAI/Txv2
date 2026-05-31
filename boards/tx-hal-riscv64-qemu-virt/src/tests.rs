use tx_hal::{
    AuxvIf, CacheIf, ConsoleIf, CpuId, CpuMask, DmaAddr, DmaDirection, DmaIf, FaultInfo, IpiKind,
    IrqDispatchTable, IrqHandled, IrqIf, KernelTrapSink, PercpuIf, PhysAddr, SmpIf, TrapAction,
    TrapClass, TrapFrameMut, TrapFrameSnapshot, TrapIf, TrapPreviousMode, VirtAddr,
};

use crate::{
    dispatch_trap_frame, enter_irq_context, mark_ipi_ack, percpu_tls_for_cpu,
    remote_sfence_targets_from, trap::classify_rv64_trap, Platform, Rv64TrapFrame,
    RV64_PERCPU_AREAS,
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

    fn on_external_irq(_cpu: CpuId) -> TrapAction {
        assert!(<Platform as IrqIf>::in_irq_context());
        TrapAction::Resume
    }

    fn on_ipi(_cpu: CpuId) -> TrapAction {
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

    <Platform as PercpuIf>::install_early_percpu(CpuId(2));

    let kernel_tls = <Platform as PercpuIf>::read_kernel_tls() as usize;
    assert_eq!(Some(kernel_tls), percpu_tls_for_cpu(CpuId(2)));
    assert_eq!(<Platform as PercpuIf>::current_cpu_id(), CpuId(2));
    assert_eq!(<Platform as SmpIf>::current_cpu_id(), CpuId(2));

    <Platform as PercpuIf>::write_kernel_tls(saved_tls);
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
    // FS must be Initial (01) so FP instructions don't trap on re-entry.
    assert_eq!((frame.sstatus >> 13) & 3, 1, "FS must be Initial");
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

    let action = dispatch_trap_frame::<RecordingTrapSink>(&mut frame);

    assert_eq!(action, TrapAction::Terminate);
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
fn ipi_ack_observation_can_be_cleared_by_mask() {
    <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::from_bits(u64::MAX));
    mark_ipi_ack(CpuId(1));
    mark_ipi_ack(CpuId(3));

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

fn test_trap_frame(scause: usize, sepc: usize, stval: usize) -> Rv64TrapFrame {
    Rv64TrapFrame {
        x: [0; 32],
        scause,
        sepc,
        stval,
        sstatus: 1 << 8,
        f: [0u64; 32],
        fcsr: 0,
        _pad_fp: 0,
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

    let ctx = frame.view_mut().capture_user_context();

    assert!(ctx.fp.is_valid(), "FP context must be valid when FS != Off");
    assert_ne!(
        ctx.fp.flags & tx_hal::UserFpContext::FLAG_DIRTY,
        0,
        "FLAG_DIRTY must be set when FS=Dirty"
    );
    assert_eq!(ctx.fp.regs, frame.f, "FP regs must round-trip");
    assert_eq!(ctx.fp.fcsr, 0x05, "fcsr must round-trip");

    // Restore into a fresh frame and verify FP state is recovered.
    let mut frame2 = test_trap_frame(8, 0x2000, 0);
    frame2.sstatus = 0;
    frame2.view_mut().restore_user_context(&ctx);

    assert_eq!(frame2.f, frame.f, "restored FP regs must match original");
    assert_eq!(frame2.fcsr, 0x05, "restored fcsr must match original");
    // prepare_user_return always sets FS=Initial (01).
    assert_eq!(
        (frame2.sstatus >> 13) & 3,
        1,
        "FS must be Initial after restore_user_context"
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
