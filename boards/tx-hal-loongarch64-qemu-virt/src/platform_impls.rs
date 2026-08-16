use super::boot_smp;
use super::la64_irq_trap::*;
use super::la64_percpu::{
    la64_current_cpu_id, la64_install_kernel_stack, la64_read_kernel_tls, la64_read_stable_counter,
    la64_wait_for_interrupt_once, la64_write_kernel_tls,
};
use super::la64_pmap::*;
use super::*;

const LA64_IPI_ACK_TIMEOUT_NS: u64 = 2_000_000_000;

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
        // The cached DMW window statically covers the whole physical address
        // space, so no page-table work is needed to reach any RAM the firmware
        // reports — including high RAM above the MMIO hole. Succeed for anything
        // inside the DMW-mapped limit.
        if phys_end.0 <= QEMU_LA64_RAM_BASE.saturating_add(QEMU_LA64_DIRECT_MAP_SIZE) {
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

    fn commit_new_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
        commit_new_la64_kernel_mapping(reservation, permissions);
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
        deactivate_la64_user_pmap_if_matches(root.asid(), root.phys());
        wait_for_la64_asid_quiescence(root.asid(), root.phys());
        invalidate_la64_asid_before_reuse();
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
        let targets = CpuMask::from_bits(
            <Platform as SmpIf>::online_cpus().bits()
                & !CpuMask::single(la64_current_cpu_id()).bits(),
        );
        la64_remote_tlb_shootdown(targets);
    }

    fn shootdown_kernel_mappings(invalidations: &[PmapInvalidation]) {
        if !invalidations.is_empty() {
            // One architecturally defined all-TLB invalidation is cheaper than
            // issuing INVTLB op 0x6 once for every unmapped vmalloc page.
            la64_invtlb_all();
            let targets = CpuMask::from_bits(
                <Platform as SmpIf>::online_cpus().bits()
                    & !CpuMask::single(la64_current_cpu_id()).bits(),
            );
            la64_remote_tlb_shootdown(targets);
        }
    }

    fn service_pending_tlb_shootdown() {
        service_la64_pending_tlb_shootdown();
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        la64_invtlb_asid(asid, invalidation.virt());
        la64_remote_tlb_shootdown(la64_asid_residency_mask(asid));
    }

    fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        if invalidations.is_empty() {
            return;
        }
        for invalidation in invalidations {
            la64_invtlb_asid(asid, invalidation.virt());
        }
        la64_remote_tlb_shootdown(la64_asid_residency_mask(asid));
    }

    fn synchronize_new_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        for invalidation in invalidations {
            la64_invtlb_asid(asid, invalidation.virt());
        }
    }
}
impl TrapIf for Platform {
    fn install_minimal_trap_vector() {
        install_la64_trap_vectors();
    }

    fn install_kernel_trap_vector() {
        install_la64_trap_vectors();
        // The full kernel trap vector is now live, so this hart can safely
        // accept runtime IPIs.  This must happen here for both the BSP and
        // APs: the generic AP entry enables IPIs again before publishing
        // itself online, while the BSP has no later per-CPU enable step.
        boot_smp::enable_ipi_wakeups();
    }

    fn install_user_trap_vector() {
        install_la64_trap_vectors();
    }

    fn classify_trap(snapshot: TrapFrameSnapshot) -> TrapClass {
        classify_la64_trap(snapshot.scause)
    }

    fn enter_userspace_with_context(ctx: &UserTrapContext, root: &PmapRoot) {
        assert_eq!(
            <Platform as PercpuIf>::cpu_pin_depth(),
            0,
            "LA64 CPU pin escaped across a reactor/userspace boundary"
        );
        #[cfg(target_arch = "loongarch64")]
        unsafe {
            let cpu = <Platform as SmpIf>::current_cpu_id();
            let frame = la64_entry_trap_frame_ptr_for_cpu(cpu);
            (*frame).restore_user_context(ctx);
            let pmap_switch = prepare_la64_pmap_switch(root).expect("LA64 user pmap switch");
            let resume_ctx = la64_kernel_resume_ctx_ptr_for_cpu(cpu);
            let stack_top = la64_trap_stack_top_for_cpu(cpu);
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

impl SignalFrameIf for Platform {
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
    const RTC_IRQ: u32 = QEMU_LA64_RTC_IRQ;

    fn in_irq_context() -> bool {
        la64_irq_context_depth() != 0
    }

    fn in_trap_context() -> bool {
        la64_current_stack_is_trap_stack()
    }

    fn interrupts_enabled() -> bool {
        read_la64_csr(LA64_CSR_CRMD) & LA64_CRMD_IE != 0
    }

    fn exclude_local_execution() -> LocalExecutionGuard {
        exclude_la64_interrupts()
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

fn exclude_la64_interrupts() -> LocalExecutionGuard {
    #[cfg(target_arch = "loongarch64")]
    {
        let mut saved = 0usize;
        let mask = LA64_CRMD_IE;
        unsafe {
            core::arch::asm!(
                "csrxchg {saved}, {mask}, 0x00",
                saved = inout(reg) saved,
                mask = in(reg) mask,
                options(nostack)
            );
            LocalExecutionGuard::new(saved & LA64_CRMD_IE, restore_la64_interrupts)
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    unsafe {
        LocalExecutionGuard::new(0, restore_la64_interrupts)
    }
}

unsafe fn restore_la64_interrupts(saved: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let restored = saved & LA64_CRMD_IE;
        let mask = LA64_CRMD_IE;
        core::arch::asm!(
            "csrxchg {restored}, {mask}, 0x00",
            restored = inout(reg) restored => _,
            mask = in(reg) mask,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = saved;
}
impl MonotonicCounterIf for Platform {
    fn read_ns() -> u64 {
        tx_hal::time::ticks_to_ns(
            la64_read_stable_counter(),
            <Self as MonotonicCounterIf>::frequency_hz(),
        )
    }

    fn frequency_hz() -> u64 {
        la64_timebase_frequency_hz()
    }
}

impl DeadlineTimerIf for Platform {
    fn set_deadline_ns(deadline: u64) {
        let frequency_hz = <Self as MonotonicCounterIf>::frequency_hz();
        if frequency_hz == 0 {
            return;
        }

        let now = la64_read_stable_counter();
        let target = tx_hal::time::deadline_ns_to_ticks(deadline, frequency_hz);
        let delta = target.saturating_sub(now).max(1);

        write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
        write_la64_csr(LA64_CSR_TCFG, la64_deadline_tcfg(delta));
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
}

fn ls7a_rtc_ensure_toy_enabled() {
    let ctrl = ls7a_rtc_read_u32(LS7A_RTC_CTRL);
    let required = LS7A_RTC_CTRL_EO | LS7A_RTC_CTRL_TOYEN;
    if ctrl & required != required {
        ls7a_rtc_write_u32(LS7A_RTC_CTRL, ctrl | required);
    }
}

#[cfg(target_arch = "loongarch64")]
fn ls7a_rtc_read_u32(offset: usize) -> u32 {
    unsafe { ((la64_uncached_virt(QEMU_LA64_RTC_BASE) + offset) as *const u32).read_volatile() }
}

#[cfg(target_arch = "loongarch64")]
fn ls7a_rtc_write_u32(offset: usize, value: u32) {
    unsafe { ((la64_uncached_virt(QEMU_LA64_RTC_BASE) + offset) as *mut u32).write_volatile(value) }
}

#[cfg(all(not(target_arch = "loongarch64"), not(test)))]
fn ls7a_rtc_read_u32(_offset: usize) -> u32 {
    0
}

#[cfg(all(not(target_arch = "loongarch64"), not(test)))]
fn ls7a_rtc_write_u32(_offset: usize, _value: u32) {}

#[cfg(all(not(target_arch = "loongarch64"), test))]
fn ls7a_rtc_read_u32(offset: usize) -> u32 {
    LA64_HOST_LS7A_RTC_STATE
        .lock()
        .expect("host ls7a rtc state")
        .read_u32(offset)
}

#[cfg(all(not(target_arch = "loongarch64"), test))]
fn ls7a_rtc_write_u32(offset: usize, value: u32) {
    LA64_HOST_LS7A_RTC_STATE
        .lock()
        .expect("host ls7a rtc state")
        .write_u32(offset, value);
}

impl PersistentClockIf for Platform {
    fn read_realtime_ns() -> Result<u64, PersistentClockError> {
        ls7a_rtc_ensure_toy_enabled();
        let toy0 = ls7a_rtc_read_u32(LS7A_RTC_TOYREAD0);
        let toy1 = ls7a_rtc_read_u32(LS7A_RTC_TOYREAD1);
        ls7a_unix_ns_from_toy_registers(toy0, toy1)
    }

    fn set_realtime_ns(ns: u64) -> Result<(), PersistentClockError> {
        let (toy0, toy1) = ls7a_toy_registers_from_unix_ns(ns)?;
        ls7a_rtc_ensure_toy_enabled();
        ls7a_rtc_write_u32(LS7A_RTC_TOYWRITE1, toy1);
        ls7a_rtc_write_u32(LS7A_RTC_TOYWRITE0, toy0);
        Ok(())
    }

    fn set_wake_alarm_ns(ns: u64) -> Result<(), PersistentClockError> {
        let toymatch = ls7a_toymatch_from_unix_ns(ns)?;
        ls7a_rtc_ensure_toy_enabled();
        ls7a_rtc_write_u32(LS7A_RTC_TOYMATCH0, toymatch);
        Self::set_priority(QEMU_LA64_RTC_IRQ, 1);
        Self::unmask(QEMU_LA64_RTC_IRQ);
        Ok(())
    }

    fn clear_wake_alarm() -> Result<(), PersistentClockError> {
        ls7a_rtc_write_u32(LS7A_RTC_TOYMATCH0, 0);
        Self::mask(QEMU_LA64_RTC_IRQ);
        Ok(())
    }

    fn acknowledge_wake_alarm_irq() -> Result<(), PersistentClockError> {
        Ok(())
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

    fn pin_current_cpu() -> CpuPinGuard {
        let cpu = la64_current_cpu_id();
        LA64_CPU_PIN_DEPTHS[cpu.0].fetch_add(1, Ordering::Relaxed);
        CpuPinGuard::with_unpin(cpu, la64_unpin_cpu)
    }

    fn cpu_pin_depth() -> usize {
        LA64_CPU_PIN_DEPTHS[la64_current_cpu_id().0].load(Ordering::Relaxed)
    }

    unsafe fn install_kernel_stack(top: VirtAddr) {
        unsafe { la64_install_kernel_stack(top) };
    }
}

fn la64_unpin_cpu(cpu: CpuId) {
    debug_assert_eq!(cpu, la64_current_cpu_id());
    let previous = LA64_CPU_PIN_DEPTHS[cpu.0].fetch_sub(1, Ordering::Release);
    assert!(previous != 0, "LA64 CPU pin nesting underflow");
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
        // QEMU publishes its `-smp` count through the firmware FDT. Use that
        // topology by default and treat `tx.maxcpus=N` as an explicit upper
        // bound. Missing firmware topology falls back to one CPU in
        // `LA64_DEFAULT_POSSIBLE_CPUS`.
        let discovered = LA64_POSSIBLE_CPU_COUNT
            .load(Ordering::Acquire)
            .clamp(1, LA64_MAX_BOOT_CPUS);
        let requested = crate::boot_facts::max_cpus_from_cmdline()
            .unwrap_or(discovered)
            .clamp(1, LA64_MAX_BOOT_CPUS);
        CpuMask::first(discovered.min(requested))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(LA64_ONLINE_CPUS.load(Ordering::Acquire) & Self::possible_cpus().bits())
    }

    fn mark_cpu_online(cpu: CpuId) {
        if Self::possible_cpus().contains(cpu) {
            // Accept synchronous work before scheduler-visible online
            // publication, so no observer can target a CPU that is unable to
            // acquire a shootdown pin.
            mark_la64_tlb_cpu_online(cpu);
            LA64_ONLINE_CPUS.fetch_or(CpuMask::single(cpu).bits(), Ordering::AcqRel);
        }
    }

    fn prepare_cpu_offline() {
        prepare_la64_tlb_cpu_offline();
    }

    fn boot_secondary_cpus(entry: SecondaryEntry) -> usize {
        boot_smp::boot_secondary_cpus(Self::possible_cpus(), entry)
    }

    fn enable_ipi_wakeups() {
        boot_smp::enable_ipi_wakeups();
    }

    fn wait_for_interrupt_once() {
        la64_wait_for_interrupt_once();
    }

    fn prepare_interrupt_wait() -> InterruptWaitState {
        let crmd = read_la64_csr(LA64_CSR_CRMD);
        write_la64_csr(LA64_CSR_CRMD, crmd & !LA64_CRMD_IE);
        InterruptWaitState::from_raw(crmd)
    }

    fn cancel_interrupt_wait(state: InterruptWaitState) {
        let crmd = read_la64_csr(LA64_CSR_CRMD);
        let restored = if state.raw() & LA64_CRMD_IE != 0 {
            crmd | LA64_CRMD_IE
        } else {
            crmd & !LA64_CRMD_IE
        };
        write_la64_csr(LA64_CSR_CRMD, restored);
    }

    fn wait_for_interrupt_prepared(state: InterruptWaitState) {
        // `prepare_interrupt_wait` left IE clear while the kernel performed
        // its final runnable-work check. The assembly helper below enables IE
        // adjacent to `idle`; the trap dispatcher redirects an interrupt from
        // that tiny window to the instruction after `idle`, so the just-served
        // wake cannot be followed by an indefinite sleep.
        la64_wait_for_interrupt_once();
        if state.raw() & LA64_CRMD_IE == 0 {
            let crmd = read_la64_csr(LA64_CSR_CRMD);
            write_la64_csr(LA64_CSR_CRMD, crmd & !LA64_CRMD_IE);
        }
    }

    fn pending_ipi(kind: IpiKind) -> bool {
        boot_smp::pending_ipi(kind)
    }

    fn quiesce_this_cpu() -> ! {
        <Self as DeadlineTimerIf>::cancel_deadline();
        debug_assert_eq!(
            LA64_TLB_TARGET_USERS[la64_current_cpu_id().0].load(Ordering::Acquire),
            0
        );
        #[cfg(target_arch = "loongarch64")]
        {
            // Disable every local interrupt source before publishing a
            // permanently parked AP to the shutdown coordinator.
            write_la64_csr(LA64_CSR_ECFG, 0);
            let crmd = read_la64_csr(LA64_CSR_CRMD) & !LA64_CRMD_IE;
            write_la64_csr(LA64_CSR_CRMD, crmd);
        }
        loop {
            #[cfg(target_arch = "loongarch64")]
            unsafe {
                core::arch::asm!("idle 0", options(nomem, nostack));
            }
            #[cfg(not(target_arch = "loongarch64"))]
            core::hint::spin_loop();
        }
    }

    fn send_ipi(target: CpuId, kind: IpiKind) {
        boot_smp::send_ipi(target, kind);
    }

    fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
        boot_smp::broadcast_ipi(mask, kind);
    }

    fn ack_ipi(kind: IpiKind) {
        boot_smp::ack_ipi(kind);
    }

    fn clear_ipi_ack_cpus(kind: IpiKind, mask: CpuMask) {
        boot_smp::clear_ipi_ack_cpus(kind, mask);
    }

    fn ipi_ack_cpus(kind: IpiKind) -> CpuMask {
        boot_smp::ipi_ack_cpus(kind)
    }

    fn wait_for_ipi_ack_cpus(mask: CpuMask, kind: IpiKind) -> usize {
        let target = mask.bits();
        if target == 0 {
            return 0;
        }

        // Multi-threaded TCG does not schedule every vCPU within a fixed
        // number of BSP spin iterations. Use the architectural counter, as
        // the RV64 board does, so the final healthy AP gets a real-time
        // acknowledgement window independent of host scheduling jitter.
        let start_ns = <Platform as MonotonicCounterIf>::read_ns();
        let deadline_ns = start_ns.saturating_add(LA64_IPI_ACK_TIMEOUT_NS);
        loop {
            let acked = Self::ipi_ack_cpus(kind).bits() & target;
            if acked == target {
                return mask.count();
            }
            if <Platform as MonotonicCounterIf>::read_ns() >= deadline_ns {
                return acked.count_ones() as usize;
            }
            core::hint::spin_loop();
        }
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
