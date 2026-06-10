use super::boot_smp;
use super::la64_irq_trap::*;
use super::la64_pmap::*;
use super::*;

#[cfg(all(not(target_arch = "loongarch64"), test))]
use core::sync::atomic::AtomicU8;

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_qemu_activate_enter_userspace(
        resume_ctx: *mut KernelResumeCtx,
        frame: *const La64TrapFrame,
        trap_stack_top: usize,
        asid: usize,
        pgdl: usize,
        pgdh: usize,
        switch_required: usize,
    );
}

#[cfg(target_arch = "loongarch64")]
fn enable_uart_rx_irq() {
    unsafe {
        let base = la64_uncached_virt(QEMU_LA64_UART0_BASE) as *mut u8;
        core::ptr::write_volatile(base.add(UART_IER), UART_IER_ERBFI);
    }

    let ecfg = read_la64_csr(LA64_CSR_ECFG) | LA64_ESTAT_IS_HWI_MASK;
    write_la64_csr(LA64_CSR_ECFG, ecfg);

    let crmd = read_la64_csr(LA64_CSR_CRMD) | LA64_CRMD_IE;
    write_la64_csr(LA64_CSR_CRMD, crmd);
}

#[cfg(not(target_arch = "loongarch64"))]
fn enable_uart_rx_irq() {
    #[cfg(test)]
    LA64_HOST_UART_IER.store(UART_IER_ERBFI, Ordering::Release);
}

#[cfg(all(not(target_arch = "loongarch64"), test))]
static LA64_HOST_UART_IER: AtomicU8 = AtomicU8::new(0);

#[cfg(target_arch = "loongarch64")]
static LA64_USER_ENTRY_PROBE_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[cfg(all(not(target_arch = "loongarch64"), test))]
pub(crate) fn la64_reset_host_uart_ier_for_test() {
    LA64_HOST_UART_IER.store(0, Ordering::Release);
}

#[cfg(all(not(target_arch = "loongarch64"), test))]
pub(crate) fn la64_host_uart_ier_for_test() -> u8 {
    LA64_HOST_UART_IER.load(Ordering::Acquire)
}

pub(crate) unsafe fn la64_copy_from_user_raw(
    _dst: *mut u8,
    src: UserPtr<u8>,
    len: usize,
) -> Result<(), FaultInfo> {
    if len == 0 {
        return Ok(());
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let fault_va = unsafe { tx_la64_cfu_raw(_dst, src.as_ptr(), len) };
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

pub(crate) unsafe fn la64_copy_to_user_raw(
    dst: UserPtr<u8>,
    _src: *const u8,
    len: usize,
) -> Result<(), FaultInfo> {
    if len == 0 {
        return Ok(());
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let fault_va = unsafe { tx_la64_ctu_raw(dst.as_ptr(), _src as *mut u8, len) };
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

unsafe fn la64_write_user<T: Pod>(dst: UserPtr<T>, value: T) -> Result<(), FaultInfo> {
    let src = core::ptr::addr_of!(value) as *const u8;
    unsafe {
        la64_copy_to_user_raw(
            UserPtr::<u8>::new(dst.addr()),
            src,
            core::mem::size_of::<T>(),
        )
    }
}

unsafe fn la64_read_user<T: Pod>(src: UserPtr<T>) -> Result<T, FaultInfo> {
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    unsafe {
        la64_copy_from_user_raw(
            value.as_mut_ptr() as *mut u8,
            UserPtr::<u8>::new(src.addr()),
            core::mem::size_of::<T>(),
        )?;
        Ok(value.assume_init())
    }
}

impl PmapIf for Platform {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        // LA64 reaches generic boot through DMW, so the published bootstrap
        // facts describe that direct map rather than an early page-table root.
        boot_facts::bootstrap_pmap_info()
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
        // The cached DMW window covers the entire physical address space
        // (PALEN bits), so both the low 256 MiB and the high-memory region
        // above the MMIO hole are already direct-mapped — no page-table work
        // needed. Accept any phys end within the high-memory top.
        if phys_end.0 <= QEMU_LA64_HIGHMEM_END {
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

    fn commit_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
        commit_la64_kernel_mapping(reservation, permissions);
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

    fn activate_user_pmap(root: &PmapRoot) {
        let _ = activate_la64_pmap(root);
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

    fn enter_userspace_with_context(ctx: &UserTrapContext, root: &PmapRoot) {
        #[cfg(target_arch = "loongarch64")]
        unsafe {
            let cpu = <Platform as SmpIf>::current_cpu_id();
            let frame = la64_entry_trap_frame_ptr_for_cpu(cpu);
            (*frame).restore_user_context(ctx);
            let pmap_switch = prepare_la64_pmap_switch(root).expect("LA64 user pmap switch");
            if pmap_switch.switch_required {
                record_la64_pmap_switch(&pmap_switch);
            }
            let resume_ctx = la64_kernel_resume_ctx_ptr_for_cpu(cpu);
            let stack_top = la64_trap_stack_top_for_cpu(cpu);
            trace_la64_user_entry_probe(cpu, ctx, &*frame, &pmap_switch, stack_top);
            tx_la64_qemu_activate_enter_userspace(
                resume_ctx,
                frame,
                stack_top,
                pmap_switch.asid,
                pmap_switch.pgdl,
                pmap_switch.pgdh,
                pmap_switch.switch_required as usize,
            );
        }

        #[cfg(not(target_arch = "loongarch64"))]
        unsafe {
            let _ = root;
            let mut frame = La64TrapFrame::empty();
            frame.r = ctx.regs;
            frame.r[0] = 0;
            frame.era = ctx.pc;
            frame.prmd = ctx.status;
            frame.prepare_user_return();
            return_to_userspace(&frame)
        }
    }
}

#[cfg(target_arch = "loongarch64")]
fn trace_la64_user_entry_probe(
    cpu: CpuId,
    ctx: &UserTrapContext,
    frame: &La64TrapFrame,
    pmap_switch: &La64PmapSwitch,
    stack_top: usize,
) {
    let pc_is_user = (ctx.pc < LA64_USER_TOP) as usize;
    let frame_pc_is_user = (frame.era < LA64_USER_TOP) as usize;
    let probe_index =
        LA64_USER_ENTRY_PROBE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    if probe_index >= 8 && pc_is_user != 0 && frame_pc_is_user != 0 {
        return;
    }

    console_write_literal(b"txkernel:qemu-loongarch64-virt:probe:user-entry");
    console_write_literal(b":idx=0x");
    console_write_hex(probe_index);
    console_write_literal(b":cpu=0x");
    console_write_hex(cpu.0);
    console_write_literal(b":ctx_pc=0x");
    console_write_hex(ctx.pc);
    console_write_literal(b":ctx_sp=0x");
    console_write_hex(ctx.regs[LA64_R_SP]);
    console_write_literal(b":ctx_ra=0x");
    console_write_hex(ctx.regs[LA64_R_RA]);
    console_write_literal(b":ctx_prmd=0x");
    console_write_hex(ctx.status);
    console_write_literal(b":ctx_user=0x");
    console_write_hex(pc_is_user);
    console_write_literal(b":frame_pc=0x");
    console_write_hex(frame.era);
    console_write_literal(b":frame_sp=0x");
    console_write_hex(frame.r[LA64_R_SP]);
    console_write_literal(b":frame_ra=0x");
    console_write_hex(frame.r[LA64_R_RA]);
    console_write_literal(b":frame_prmd=0x");
    console_write_hex(frame.prmd);
    console_write_literal(b":frame_user=0x");
    console_write_hex(frame_pc_is_user);
    console_write_literal(b":asid=0x");
    console_write_hex(pmap_switch.asid);
    console_write_literal(b":pgdl=0x");
    console_write_hex(pmap_switch.pgdl);
    console_write_literal(b":pgdh=0x");
    console_write_hex(pmap_switch.pgdh);
    console_write_literal(b":switch=0x");
    console_write_hex(pmap_switch.switch_required as usize);
    console_write_literal(b":trap_stack=0x");
    console_write_hex(stack_top);
    console_write_literal(b"\n");
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
            la64_write_user(UserPtr::<La64SignalFrame>::new(frame_addr), frame)?;
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
        let frame = unsafe { la64_read_user(UserPtr::<La64SignalFrame>::new(user_sp.addr()))? };
        decode_la64_signal_frame(user_sp, frame)
    }

    fn signal_frame_size() -> usize {
        core::mem::size_of::<La64SignalFrame>()
    }

    fn decode_signal_frame_bytes(
        user_sp: UserPtr<u8>,
        bytes: &[u8],
    ) -> Result<SavedSignalFrame, FaultInfo> {
        if bytes.len() != core::mem::size_of::<La64SignalFrame>() {
            return Err(FaultInfo {
                address: VirtAddr(user_sp.addr()),
                write: false,
                instruction: false,
                from_user: false,
            });
        }
        let mut frame = core::mem::MaybeUninit::<La64SignalFrame>::uninit();
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                frame.as_mut_ptr().cast::<u8>(),
                core::mem::size_of::<La64SignalFrame>(),
            );
            decode_la64_signal_frame(user_sp, frame.assume_init())
        }
    }

    fn restore_signal_frame(mut tf: TrapFrameMut<'_>, frame: &SavedSignalFrame) {
        tf.restore_user_context(&frame.user_context);
    }

    fn prepare_signal_frame(
        ctx: &UserTrapContext,
        setup: &SignalFrameWrite,
    ) -> Result<(UserTrapContext, tx_hal::SignalFrameBytes), FaultInfo> {
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
        let frame = La64SignalFrame::new_from_context(ctx, setup);
        let frame_bytes = unsafe {
            core::slice::from_raw_parts(&frame as *const La64SignalFrame as *const u8, frame_size)
        };

        let siginfo_addr = frame_addr + core::mem::offset_of!(La64SignalFrame, siginfo);
        let ucontext_addr = frame_addr + core::mem::offset_of!(La64SignalFrame, user_context);
        let trampoline_pc = frame_addr + core::mem::offset_of!(La64SignalFrame, trampoline);

        let mut handler_ctx = *ctx;
        handler_ctx.pc = setup.handler_pc.addr();
        handler_ctx.regs[LA64_R_SP] = frame_addr;
        handler_ctx.regs[LA64_R_RA] = trampoline_pc;
        handler_ctx.regs[LA64_R_A0] = setup.sig_no as usize;
        handler_ctx.regs[LA64_R_A1] = siginfo_addr;
        handler_ctx.regs[LA64_R_A2] = ucontext_addr;

        Ok((
            handler_ctx,
            tx_hal::SignalFrameBytes::from_slice(frame_bytes),
        ))
    }

    fn rewind_syscall_pc(mut tf: TrapFrameMut<'_>) {
        tf.rewind_pc(4);
    }
}

fn decode_la64_signal_frame(
    user_sp: UserPtr<u8>,
    frame: La64SignalFrame,
) -> Result<SavedSignalFrame, FaultInfo> {
    frame.validate(user_sp)?;
    Ok(SavedSignalFrame {
        saved_mask: frame.saved_mask,
        user_context: frame.user_context,
    })
}

impl FpSimdIf for Platform {
    const SUPPORTED: bool = true;

    type State = UserFpContext;

    fn init_state() -> Self::State {
        let mut state = UserFpContext::empty();
        state.flags = UserFpContext::FLAG_VALID;
        state
    }

    fn enable_for_current() {
        la64_set_fpu_enabled(true);
    }

    fn disable_for_current() {
        la64_set_fpu_enabled(false);
    }

    fn save(state: &mut Self::State) {
        la64_save_fp_context(state);
    }

    fn restore(state: &Self::State) {
        la64_restore_fp_context_raw(state);
    }
}
impl IrqIf for Platform {
    const MAX_IRQ: u32 = QEMU_LA64_GSI_BASE + QEMU_LA64_PCH_PIC_IRQS;
    const UART_IRQ: u32 = QEMU_LA64_UART0_IRQ;

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
        enable_uart_rx_irq();
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
        write_la64_csr(
            LA64_CSR_TCFG,
            delta as usize | LA64_TCFG_ENABLE | LA64_TCFG_PERIODIC,
        );
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
        let possible = LA64_POSSIBLE_CPU_COUNT
            .load(Ordering::Acquire)
            .clamp(1, LA64_MAX_BOOT_CPUS);
        CpuMask::first(possible)
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
        boot_smp::boot_secondary_cpus(_entry)
    }

    fn enable_ipi_wakeups() {
        boot_smp::enable_ipi_wakeups();
    }

    fn wait_for_interrupt_once() {
        la64_wait_for_interrupt_once();
    }

    fn pending_ipi(_kind: IpiKind) -> bool {
        let _ = _kind;
        boot_smp::pending_ipi()
    }

    fn send_ipi(target: CpuId, _kind: IpiKind) {
        let _ = _kind;
        boot_smp::send_ipi(target);
    }

    fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
        let current = la64_current_cpu_id();
        let mut bits = mask.bits() & !CpuMask::single(current).bits();
        while bits != 0 {
            let cpu = bits.trailing_zeros() as usize;
            Self::send_ipi(CpuId(cpu), kind);
            bits &= bits - 1;
        }
    }

    fn ack_ipi(_kind: IpiKind) {
        let _ = _kind;
        boot_smp::ack_ipi();
    }

    fn clear_ipi_ack_cpus(_kind: IpiKind, mask: CpuMask) {
        let _ = _kind;
        boot_smp::clear_ipi_ack_cpus(mask);
    }

    fn ipi_ack_cpus(_kind: IpiKind) -> CpuMask {
        let _ = _kind;
        boot_smp::ipi_ack_cpus()
    }
}

impl PowerIf for Platform {
    fn system_off() -> ! {
        #[cfg(target_arch = "loongarch64")]
        unsafe {
            // QEMU loongarch virt exposes poweroff via the ACPI GED sleep
            // control byte in direct-kernel/FDT boots.
            let sleep_ctl = la64_uncached_virt(QEMU_LA64_GED_SLEEP_CTL) as *mut u8;
            core::ptr::write_volatile(sleep_ctl, QEMU_LA64_GED_SLEEP_VALUE_S5);
        }

        loop {
            #[cfg(target_arch = "loongarch64")]
            unsafe {
                core::arch::asm!("idle 0", options(nomem, nostack));
            }
            core::hint::spin_loop();
        }
    }
}

impl EntropyIf for Platform {}
