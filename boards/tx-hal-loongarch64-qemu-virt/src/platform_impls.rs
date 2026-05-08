use super::la64_irq_trap::*;
use super::la64_pmap::*;
use super::*;

impl PmapIf for Platform {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        ensure_static_boot_facts();

        unsafe { Some(&*core::ptr::addr_of!(BOOTSTRAP_PMAP_INFO)) }
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        let Some(allocator) = installed_pt_node_allocator() else {
            return Err(AllocError::Exhausted);
        };

        allocator()
    }

    fn free_pt_node(node: PtNode) {
        unsafe {
            let _ = node.release_typed_frame();
        }
    }

    fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
        let value = allocator as usize;
        INSTALLED_PT_NODE_ALLOCATOR
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| PmapError::AlreadyMapped)
    }

    fn reserve_kernel_direct_map_1g(phys: PhysAddr) -> Result<Option<PmapReservation>, PmapError> {
        if dmw_covers_phys_range(phys, PmapReserveKind::Superpage1G.size()) {
            Ok(None)
        } else {
            Err(PmapError::Unsupported)
        }
    }

    fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError> {
        if phys_end.0 <= QEMU_LA64_RAM_END {
            Ok(())
        } else {
            Err(PmapError::Unsupported)
        }
    }

    fn reserve_kernel_mapping(
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        if dmw_mmio_page_is_precovered(virt, phys, kind) {
            return Ok(None);
        }

        reserve_la64_kernel_mapping(virt, phys, kind)
    }

    fn rollback_kernel_mapping(reservation: PmapReservation) {
        rollback_la64_kernel_mapping(reservation);
    }

    fn commit_kernel_mapping(reservation: PmapReservation) {
        commit_la64_kernel_mapping(reservation);
    }

    fn unmap_kernel_mapping(
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        unmap_la64_kernel_mapping(virt, kind)
    }

    fn protect_kernel_mapping(
        virt: VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        protect_la64_kernel_mapping(virt, kind, permissions)
    }

    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let asid = alloc_la64_asid()?;
        let node = match Self::alloc_pt_node() {
            Ok(node) => node,
            Err(_) => {
                free_la64_asid(asid);
                return Err(PmapError::Exhausted);
            }
        };

        zero_la64_pt_node(node.phys);
        Ok(PmapRoot::new(node, asid))
    }

    fn destroy_pmap_root(root: PmapRoot) {
        release_la64_user_page_table_tree(root.phys(), 3);
        free_la64_asid(root.asid());
        Self::free_pt_node(root.into_node());
    }

    fn activate_pmap(root: &PmapRoot) -> Result<(), PmapError> {
        activate_la64_pmap(root)
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        reserve_la64_user_mapping(root, virt, phys, kind)
    }

    fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
        rollback_la64_user_mapping(root, reservation);
    }

    fn commit_mapping(root: &PmapRoot, reservation: PmapReservation, permissions: PmapPermissions) {
        commit_la64_user_mapping(root, reservation, permissions);
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        unmap_la64_user_mapping(root, virt, kind)
    }

    fn protect_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        protect_la64_user_mapping(root, virt, kind, permissions)
    }

    fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
        la64_invtlb_global(invalidation.virt());
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        la64_invtlb_asid(asid, invalidation.virt());
    }
}
impl TrapIf for Platform {
    fn install_minimal_trap_vector() {
        install_la64_trap_vectors();
    }

    fn install_kernel_trap_vector() {
        install_la64_trap_vectors();
    }

    fn install_user_trap_vector() {
        install_la64_trap_vectors();
    }

    fn classify_trap(snapshot: TrapFrameSnapshot) -> TrapClass {
        classify_la64_trap(snapshot.scause)
    }
}
impl UserAccessIf for Platform {
    unsafe fn copy_from_user(
        dst: KernelPtr<u8>,
        src: UserPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo> {
        let _ = dst;
        if len == 0 {
            return Ok(());
        }

        #[cfg(target_arch = "loongarch64")]
        {
            let fault_va = unsafe { tx_la64_cfu_raw(dst.as_ptr(), src.as_ptr(), len) };
            if fault_va == 0 {
                Ok(())
            } else {
                Err(FaultInfo {
                    address: VirtAddr(fault_va),
                    write: false,
                    instruction: false,
                    from_user: false,
                })
            }
        }

        #[cfg(not(target_arch = "loongarch64"))]
        Err(FaultInfo {
            address: VirtAddr(src.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }

    unsafe fn copy_to_user(
        dst: UserPtr<u8>,
        src: KernelPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo> {
        let _ = src;
        if len == 0 {
            return Ok(());
        }

        #[cfg(target_arch = "loongarch64")]
        {
            let fault_va = unsafe { tx_la64_ctu_raw(dst.as_ptr(), src.as_ptr(), len) };
            if fault_va == 0 {
                Ok(())
            } else {
                Err(FaultInfo {
                    address: VirtAddr(fault_va),
                    write: true,
                    instruction: false,
                    from_user: false,
                })
            }
        }

        #[cfg(not(target_arch = "loongarch64"))]
        Err(FaultInfo {
            address: VirtAddr(dst.addr()),
            write: true,
            instruction: false,
            from_user: false,
        })
    }
}

impl SignalFrameIf for Platform {
    fn write_signal_frame(
        mut tf: TrapFrameMut<'_>,
        setup: SignalFrameWrite,
    ) -> Result<SignalFramePlacement, FaultInfo> {
        let frame_size = core::mem::size_of::<La64SignalFrame>();
        let Some(unrounded_frame_addr) = setup.stack_top.addr().checked_sub(frame_size) else {
            return Err(FaultInfo {
                address: VirtAddr(setup.stack_top.addr()),
                write: true,
                instruction: false,
                from_user: false,
            });
        };
        let frame_addr = align_down(unrounded_frame_addr, LA64_SIGFRAME_ALIGN);
        let frame = La64SignalFrame::new(&tf, &setup);

        unsafe {
            <Platform as UserAccessIf>::write_user(
                UserPtr::<La64SignalFrame>::new(frame_addr),
                frame,
            )?;
        }

        let siginfo_addr = frame_addr + core::mem::offset_of!(La64SignalFrame, siginfo);
        let ucontext_addr = frame_addr + core::mem::offset_of!(La64SignalFrame, user_context);
        let trampoline_pc = frame_addr + core::mem::offset_of!(La64SignalFrame, trampoline);

        tf.set_pc(VirtAddr(setup.handler_pc.addr()));
        tf.set_sp(VirtAddr(frame_addr));
        tf.set_signal_handler_regs(SignalHandlerRegs {
            return_pc: VirtAddr(trampoline_pc),
            args: [setup.sig_no as usize, siginfo_addr, ucontext_addr],
        });

        Ok(SignalFramePlacement {
            frame_addr: UserPtr::new(frame_addr),
            trampoline_pc: UserPtr::new(trampoline_pc),
        })
    }

    fn read_signal_frame(user_sp: UserPtr<u8>) -> Result<SavedSignalFrame, FaultInfo> {
        let frame = unsafe {
            <Platform as UserAccessIf>::read_user(UserPtr::<La64SignalFrame>::new(user_sp.addr()))?
        };
        frame.validate(user_sp)?;
        Ok(SavedSignalFrame {
            saved_mask: frame.saved_mask,
            user_context: frame.user_context,
        })
    }

    fn restore_signal_frame(mut tf: TrapFrameMut<'_>, frame: &SavedSignalFrame) {
        tf.restore_user_context(&frame.user_context);
    }

    fn rewind_syscall_pc(mut tf: TrapFrameMut<'_>) {
        tf.rewind_pc(4);
    }
}
impl IrqIf for Platform {
    const MAX_IRQ: u32 = QEMU_LA64_GSI_BASE + QEMU_LA64_PCH_PIC_IRQS;

    fn in_irq_context() -> bool {
        la64_irq_context_depth() != 0
    }

    fn interrupts_enabled() -> bool {
        read_la64_csr(LA64_CSR_CRMD) & LA64_CRMD_IE != 0
    }

    fn claim() -> u32 {
        la64_extioi_claim()
    }

    fn complete(irq: u32) {
        la64_complete_external_irq(irq);
    }

    fn mask(irq: u32) {
        la64_mask_external_irq(irq);
    }

    fn unmask(irq: u32) {
        la64_unmask_external_irq(irq);
    }

    fn set_priority(_irq: u32, _priority: u8) {}

    fn install_dispatch_table(table: &'static IrqDispatchTable) {
        LA64_IRQ_DISPATCH_TABLE.store(table as *const IrqDispatchTable as usize, Ordering::Release);
    }

    fn dispatch_irq(irq: u32) -> IrqHandled {
        let table = LA64_IRQ_DISPATCH_TABLE.load(Ordering::Acquire);
        let Some(handler) = (table != 0)
            .then(|| unsafe { &*(table as *const IrqDispatchTable) })
            .and_then(|table| table.entries.get(irq as usize).copied().flatten())
        else {
            Self::mask(irq);
            return IrqHandled::Done;
        };

        handler(irq)
    }
}
impl TimeIf for Platform {
    fn read_ns() -> u64 {
        tx_hal::time::ticks_to_ns(la64_read_stable_counter(), Self::frequency_hz())
    }

    fn set_deadline_ns(deadline: u64) {
        let frequency_hz = Self::frequency_hz();
        if frequency_hz == 0 {
            return;
        }

        let now = la64_read_stable_counter();
        let target = tx_hal::time::deadline_ns_to_ticks(deadline, frequency_hz);
        let delta = target.saturating_sub(now).max(1);
        let delta = round_up_to_tcfg_ticks(delta);

        write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
        write_la64_csr(LA64_CSR_TCFG, delta as usize | LA64_TCFG_ENABLE);
    }

    fn cancel_deadline() {
        write_la64_csr(LA64_CSR_TCFG, 0);
        write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
    }

    fn enable_timer_wakeups() {
        let ecfg = read_la64_csr(LA64_CSR_ECFG) | LA64_ESTAT_IS_TIMER;
        write_la64_csr(LA64_CSR_ECFG, ecfg);

        let crmd = read_la64_csr(LA64_CSR_CRMD) | LA64_CRMD_IE;
        write_la64_csr(LA64_CSR_CRMD, crmd);
    }

    fn frequency_hz() -> u64 {
        la64_timebase_frequency_hz()
    }
}
impl PercpuIf for Platform {
    fn current_cpu_id() -> CpuId {
        la64_current_cpu_id()
    }

    fn install_early_percpu(cpu_id: CpuId) {
        la64_write_kernel_tls(cpu_id.0);
    }

    fn read_kernel_tls() -> u64 {
        la64_read_kernel_tls() as u64
    }

    fn write_kernel_tls(value: u64) {
        la64_write_kernel_tls(value as usize);
    }

    unsafe fn install_kernel_stack(top: VirtAddr) {
        unsafe { la64_install_kernel_stack(top) };
    }
}
impl CacheIf for Platform {
    fn fence_all() {
        la64_dbar();
    }

    fn fence_i_local() {
        la64_ibar();
    }

    fn fence_i_all() {
        la64_ibar();
    }

    fn flush_icache_range(_start: VirtAddr, _len: usize) {
        la64_ibar();
    }
}
impl DmaIf for Platform {}
impl SmpIf for Platform {
    fn current_cpu_id() -> CpuId {
        la64_current_cpu_id()
    }

    fn possible_cpus() -> CpuMask {
        CpuMask::first(PLATFORM_INFO.possible_cpu_count.min(LA64_MAX_BOOT_CPUS))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(LA64_ONLINE_CPUS.load(Ordering::Acquire) & Self::possible_cpus().bits())
    }

    fn mark_cpu_online(cpu: CpuId) {
        if Self::possible_cpus().contains(cpu) {
            LA64_ONLINE_CPUS.fetch_or(CpuMask::single(cpu).bits(), Ordering::AcqRel);
        }
    }

    fn boot_secondary_cpus(_entry: SecondaryEntry) -> usize {
        0
    }

    fn enable_ipi_wakeups() {}

    fn wait_for_interrupt_once() {
        la64_wait_for_interrupt_once();
    }

    fn pending_ipi(_kind: IpiKind) -> bool {
        false
    }

    fn send_ipi(target: CpuId, _kind: IpiKind) {
        assert_eq!(target, la64_current_cpu_id());
    }

    fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
        let current = la64_current_cpu_id();
        if mask.contains(current) {
            Self::send_ipi(current, kind);
        }
    }

    fn ack_ipi(_kind: IpiKind) {
        LA64_IPI_ACKED_CPUS.fetch_or(
            CpuMask::single(la64_current_cpu_id()).bits(),
            Ordering::AcqRel,
        );
    }

    fn clear_ipi_ack_cpus(_kind: IpiKind, mask: CpuMask) {
        LA64_IPI_ACKED_CPUS.fetch_and(!mask.bits(), Ordering::AcqRel);
    }

    fn ipi_ack_cpus(_kind: IpiKind) -> CpuMask {
        CpuMask::from_bits(LA64_IPI_ACKED_CPUS.load(Ordering::Acquire))
    }
}

impl PowerIf for Platform {
    fn system_off() -> ! {
        loop {
            #[cfg(target_arch = "loongarch64")]
            unsafe {
                core::arch::asm!("idle 0", options(nomem, nostack));
            }
            core::hint::spin_loop();
        }
    }
}
