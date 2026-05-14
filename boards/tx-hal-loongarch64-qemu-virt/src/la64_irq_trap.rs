use super::la64_pmap::{
    dmw_covers_phys_range, la64_cached_virt, la64_fixup_lookup, la64_kernel_addr_to_phys,
    la64_uncached_virt,
};
use super::*;
use crate::dtb::{parse_boot_info_from_fdt, DtbBootInfo};

pub(crate) fn la64_extioi_claim() -> u32 {
    for word in 0..(LA64_EIOINTC_IRQS / u64::BITS) {
        let offset = word as usize * core::mem::size_of::<u64>();
        let pending = la64_eiointc_read_u64(LA64_EIOINTC_COREISR_START + offset)
            & la64_eiointc_read_enable_u64(word);
        if pending == 0 {
            continue;
        }

        let ext_irq = word * u64::BITS + pending.trailing_zeros();
        return la64_public_irq_from_extioi(ext_irq);
    }

    0
}

pub(crate) fn la64_complete_external_irq(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    la64_eiointc_write_bit(LA64_EIOINTC_COREISR_START, ext_irq);
    if let Some(pin) = la64_pch_pic_pin_from_public_irq(irq) {
        la64_pch_pic_write_bit(LA64_PCH_PIC_CLEAR_START, pin);
    }
}

pub(crate) fn la64_mask_external_irq(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    la64_eiointc_clear_bit(LA64_EIOINTC_ENABLE_START, ext_irq);
    if let Some(pin) = la64_pch_pic_pin_from_public_irq(irq) {
        la64_pch_pic_set_bit(LA64_PCH_PIC_MASK_START, pin);
    }
}

pub(crate) fn la64_unmask_external_irq(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    if let Some(pin) = la64_pch_pic_pin_from_public_irq(irq) {
        la64_pch_pic_clear_bit(LA64_PCH_PIC_MASK_START, pin);
    }
    la64_eiointc_set_bit(LA64_EIOINTC_ENABLE_START, ext_irq);
}

pub(crate) const fn la64_public_irq_from_extioi(ext_irq: u32) -> u32 {
    if ext_irq < QEMU_LA64_PCH_PIC_IRQS {
        QEMU_LA64_GSI_BASE + ext_irq
    } else {
        ext_irq
    }
}

pub(crate) const fn la64_extioi_irq_from_public_irq(irq: u32) -> Option<u32> {
    if irq >= QEMU_LA64_GSI_BASE && irq < QEMU_LA64_GSI_BASE + QEMU_LA64_PCH_PIC_IRQS {
        Some(irq - QEMU_LA64_GSI_BASE)
    } else if irq < LA64_EIOINTC_IRQS {
        Some(irq)
    } else {
        None
    }
}

pub(crate) const fn la64_pch_pic_pin_from_public_irq(irq: u32) -> Option<u32> {
    if irq >= QEMU_LA64_GSI_BASE && irq < QEMU_LA64_GSI_BASE + QEMU_LA64_PCH_PIC_IRQS {
        Some(irq - QEMU_LA64_GSI_BASE)
    } else {
        None
    }
}

pub(crate) fn la64_eiointc_set_bit(start: usize, irq: u32) {
    let offset = start + (irq as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (irq % u32::BITS);
    let value = la64_eiointc_read_u32(offset) | bit;
    la64_eiointc_write_u32(offset, value);
}

pub(crate) fn la64_eiointc_clear_bit(start: usize, irq: u32) {
    let offset = start + (irq as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (irq % u32::BITS);
    let value = la64_eiointc_read_u32(offset) & !bit;
    la64_eiointc_write_u32(offset, value);
}

pub(crate) fn la64_eiointc_write_bit(start: usize, irq: u32) {
    let offset = start + (irq as usize / u64::BITS as usize) * core::mem::size_of::<u64>();
    let bit = 1u64 << (irq % u64::BITS);
    la64_eiointc_write_u64(offset, bit);
}

pub(crate) fn la64_eiointc_read_enable_u64(word: u32) -> u64 {
    let offset = LA64_EIOINTC_ENABLE_START + word as usize * core::mem::size_of::<u64>();
    let lo = la64_eiointc_read_u32(offset) as u64;
    let hi = la64_eiointc_read_u32(offset + core::mem::size_of::<u32>()) as u64;
    lo | (hi << u32::BITS)
}

pub(crate) fn la64_eiointc_read_u32(offset: usize) -> u32 {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u32;
        unsafe {
            core::arch::asm!(
                "iocsrrd.w {value}, {address}",
                value = out(reg) value,
                address = in(reg) address,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire) as u32,
            offset if offset == LA64_EIOINTC_ENABLE_START + core::mem::size_of::<u32>() => {
                (LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire) >> u32::BITS) as u32
            }
            _ => {
                let _ = address;
                0
            }
        }
    }
}

pub(crate) fn la64_eiointc_write_u32(offset: usize, value: u32) {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "iocsrwr.w {value}, {address}",
            value = in(reg) value,
            address = in(reg) address,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => {
                LA64_HOST_EIOINTC_ENABLE0.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & !(u32::MAX as u64)) | value as u64)
                })
            }
            offset if offset == LA64_EIOINTC_ENABLE_START + core::mem::size_of::<u32>() => {
                LA64_HOST_EIOINTC_ENABLE0.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & (u32::MAX as u64)) | ((value as u64) << u32::BITS))
                })
            }
            _ => {
                let _ = (address, value);
                Ok(0)
            }
        }
        .ok();
    }
}

pub(crate) fn la64_eiointc_read_u64(offset: usize) -> u64 {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u64;
        unsafe {
            core::arch::asm!(
                "iocsrrd.d {value}, {address}",
                value = out(reg) value,
                address = in(reg) address,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire),
            LA64_EIOINTC_COREISR_START => LA64_HOST_EIOINTC_COREISR0.load(Ordering::Acquire),
            _ => {
                let _ = address;
                0
            }
        }
    }
}

pub(crate) fn la64_eiointc_write_u64(offset: usize, value: u64) {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "iocsrwr.d {value}, {address}",
            value = in(reg) value,
            address = in(reg) address,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => LA64_HOST_EIOINTC_ENABLE0.store(value, Ordering::Release),
            LA64_EIOINTC_COREISR_START => {
                LA64_HOST_EIOINTC_COREISR0.fetch_and(!value, Ordering::AcqRel);
            }
            _ => {
                let _ = (address, value);
            }
        }
    }
}

pub(crate) fn la64_pch_pic_set_bit(start: usize, pin: u32) {
    let offset = start + (pin as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (pin % u32::BITS);
    let value = la64_pch_pic_read_u32(offset) | bit;
    la64_pch_pic_write_u32(offset, value);
}

pub(crate) fn la64_pch_pic_clear_bit(start: usize, pin: u32) {
    let offset = start + (pin as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (pin % u32::BITS);
    let value = la64_pch_pic_read_u32(offset) & !bit;
    la64_pch_pic_write_u32(offset, value);
}

pub(crate) fn la64_pch_pic_write_bit(start: usize, pin: u32) {
    let offset = start + (pin as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (pin % u32::BITS);
    la64_pch_pic_write_u32(offset, bit);
}

pub(crate) fn la64_pch_pic_read_u32(offset: usize) -> u32 {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::ptr::read_volatile(la64_uncached_virt(QEMU_LA64_PCH_PIC_BASE + offset) as *const u32)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_PCH_PIC_MASK_START => LA64_HOST_PCH_PIC_MASK.load(Ordering::Acquire) as u32,
            offset if offset == LA64_PCH_PIC_MASK_START + core::mem::size_of::<u32>() => {
                (LA64_HOST_PCH_PIC_MASK.load(Ordering::Acquire) >> u32::BITS) as u32
            }
            _ => 0,
        }
    }
}

pub(crate) fn la64_pch_pic_write_u32(offset: usize, value: u32) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::ptr::write_volatile(
            la64_uncached_virt(QEMU_LA64_PCH_PIC_BASE + offset) as *mut u32,
            value,
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    match offset {
        LA64_PCH_PIC_MASK_START => {
            LA64_HOST_PCH_PIC_MASK
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & !(u32::MAX as u64)) | value as u64)
                })
                .ok();
        }
        offset if offset == LA64_PCH_PIC_MASK_START + core::mem::size_of::<u32>() => {
            LA64_HOST_PCH_PIC_MASK
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & (u32::MAX as u64)) | ((value as u64) << u32::BITS))
                })
                .ok();
        }
        LA64_PCH_PIC_CLEAR_START => {
            LA64_HOST_EIOINTC_COREISR0.fetch_and(!(value as u64), Ordering::AcqRel);
        }
        offset if offset == LA64_PCH_PIC_CLEAR_START + core::mem::size_of::<u32>() => {
            LA64_HOST_EIOINTC_COREISR0.fetch_and(!((value as u64) << u32::BITS), Ordering::AcqRel);
        }
        _ => {
            let _ = value;
        }
    }
}

pub(crate) fn la64_dbar() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("dbar 0", options(nostack));
    }
}

pub(crate) fn la64_ibar() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("ibar 0", options(nostack));
    }
}

pub(crate) fn la64_timebase_frequency_hz() -> u64 {
    let cached = LA64_TIMEBASE_HZ.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }

    let detected = la64_detect_timebase_frequency_hz();
    if detected != 0 {
        LA64_TIMEBASE_HZ.store(detected, Ordering::Release);
        return detected;
    }

    // CPUCFG LLFTP bit not set (common in QEMU); fall back to the
    // timebase frequency parsed from the DTB during early boot.
    let from_dtb = unsafe { PLATFORM_INFO.timebase_frequency_hz };
    if from_dtb != 0 {
        LA64_TIMEBASE_HZ.store(from_dtb, Ordering::Release);
    }
    from_dtb
}

pub(crate) fn la64_detect_timebase_frequency_hz() -> u64 {
    la64_cpucfg_timebase_frequency(
        read_la64_cpucfg(LA64_CPUCFG2),
        read_la64_cpucfg(LA64_CPUCFG4),
        read_la64_cpucfg(LA64_CPUCFG5),
    )
}

pub(crate) const fn la64_cpucfg_timebase_frequency(
    cpucfg2: u32,
    cpucfg4: u32,
    cpucfg5: u32,
) -> u64 {
    if cpucfg2 & LA64_CPUCFG2_LLFTP == 0 {
        return 0;
    }

    let base_hz = cpucfg4 as u64;
    let multiplier = (cpucfg5 & 0xffff) as u64;
    let divisor = (cpucfg5 >> 16) as u64;

    if base_hz == 0 || multiplier == 0 || divisor == 0 {
        return 0;
    }

    base_hz.saturating_mul(multiplier) / divisor
}

pub(crate) const fn round_up_to_tcfg_ticks(ticks: u64) -> u64 {
    let mask = LA64_TCFG_TICK_MASK as u64;
    ticks.saturating_add(3) & mask
}

pub(crate) fn read_la64_cpucfg(index: usize) -> u32 {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: usize;
        unsafe {
            core::arch::asm!(
                "cpucfg {value}, {index}",
                value = out(reg) value,
                index = in(reg) index,
                options(nomem, nostack)
            );
        }
        value as u32
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = index;
        0
    }
}

pub fn dispatch_trap_frame<K>(frame: &mut La64TrapFrame) -> TrapAction
where
    K: KernelTrapSink<Platform>,
{
    let class = classify_la64_trap(frame.estat);
    let from_user = frame.previous_mode() == TrapPreviousMode::User;
    let ecode = (frame.estat >> LA64_ESTAT_ECODE_SHIFT) & LA64_ESTAT_ECODE_MASK;

    // LA64 lazy-FPU first-use path: when user code traps with FPU
    // unavailable/disabled, materialise a clean per-thread FP context
    // and retry the same instruction.
    if from_user && (ecode == LA64_ECODE_IPE || ecode == LA64_ECODE_FPD) {
        let mut fp = UserFpContext::empty();
        fp.flags = UserFpContext::FLAG_VALID;
        la64_restore_fp_context(&fp);
        return TrapAction::Resume;
    }

    if from_user && ecode == LA64_ECODE_ALE {
        match super::la64_unaligned::emulate_user_unaligned(frame) {
            super::la64_unaligned::UnalignedOutcome::Emulated => return TrapAction::Resume,
            super::la64_unaligned::UnalignedOutcome::Unsupported => {}
            super::la64_unaligned::UnalignedOutcome::Fault(fault) => {
                return K::on_page_fault(frame.view_mut(), fault);
            }
        }
    }

    match class {
        TrapClass::PageFault { write, instruction } => {
            if !from_user {
                if let Some(recovery_pc) = la64_fixup_lookup(frame.era) {
                    frame.era = recovery_pc;
                    frame.r[LA64_R_A0] = frame.badv;
                    return TrapAction::Resume;
                }
            }

            let fault = FaultInfo {
                address: VirtAddr(frame.badv),
                write,
                instruction,
                from_user,
            };
            K::on_page_fault(frame.view_mut(), fault)
        }
        TrapClass::Syscall => K::on_syscall(frame.view_mut()),
        TrapClass::TimerInterrupt => {
            write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
            let _irq_context = enter_la64_irq_context();
            K::on_timer_interrupt(<Platform as SmpIf>::current_cpu_id())
        }
        TrapClass::ExternalInterrupt => {
            let _irq_context = enter_la64_irq_context();
            K::on_external_irq(<Platform as SmpIf>::current_cpu_id())
        }
        TrapClass::InterprocessorInterrupt => {
            let _irq_context = enter_la64_irq_context();
            K::on_ipi(<Platform as SmpIf>::current_cpu_id())
        }
        TrapClass::IllegalInstruction
        | TrapClass::Breakpoint
        | TrapClass::AlignmentFault { .. }
        | TrapClass::UnknownSync
        | TrapClass::UnknownInterrupt => {
            let fault = FaultInfo {
                address: VirtAddr(frame.era),
                write: false,
                instruction: true,
                from_user,
            };
            K::on_illegal_or_sync_fault(frame.view_mut(), fault)
        }
    }
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_qemu_return_to_userspace(frame: *const La64TrapFrame) -> !;
}

/// Restore a prepared LA64 trap frame and enter user mode with `ertn`.
///
/// # Safety
///
/// `frame` must describe a valid user context for the currently active LA64
/// page table. Its `ERA` and `PRMD` fields must already be prepared for a PLV3
/// return, and the saved GPRs must be safe to expose to user mode. This
/// function never returns.
#[cfg(target_arch = "loongarch64")]
pub unsafe fn return_to_userspace(frame: &La64TrapFrame) -> ! {
    unsafe { tx_la64_qemu_return_to_userspace(frame as *const La64TrapFrame) }
}

/// Host-build placeholder for the LA64 user-mode restore primitive.
///
/// # Safety
///
/// This function never returns and exists only so host builds typecheck the
/// platform surface. It does not enter user mode.
#[cfg(not(target_arch = "loongarch64"))]
pub unsafe fn return_to_userspace(_frame: &La64TrapFrame) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

pub(crate) const fn classify_la64_trap(estat: usize) -> TrapClass {
    let ecode = (estat >> LA64_ESTAT_ECODE_SHIFT) & LA64_ESTAT_ECODE_MASK;

    match ecode {
        LA64_ECODE_INT => {
            if estat & LA64_ESTAT_IS_TIMER != 0 {
                TrapClass::TimerInterrupt
            } else if estat & LA64_ESTAT_IS_IPI != 0 {
                TrapClass::InterprocessorInterrupt
            } else if estat & LA64_ESTAT_IS_HWI_MASK != 0 {
                TrapClass::ExternalInterrupt
            } else {
                TrapClass::UnknownInterrupt
            }
        }
        LA64_ECODE_PIL | LA64_ECODE_PNR | LA64_ECODE_PPI => TrapClass::PageFault {
            write: false,
            instruction: false,
        },
        LA64_ECODE_PIS | LA64_ECODE_PME => TrapClass::PageFault {
            write: true,
            instruction: false,
        },
        LA64_ECODE_PIF | LA64_ECODE_PNX => TrapClass::PageFault {
            write: false,
            instruction: true,
        },
        LA64_ECODE_ALE => TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        },
        LA64_ECODE_ADEF => TrapClass::AlignmentFault {
            write: false,
            instruction: true,
        },
        LA64_ECODE_ADEM => TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        },
        LA64_ECODE_SYS => TrapClass::Syscall,
        LA64_ECODE_BRK => TrapClass::Breakpoint,
        LA64_ECODE_INE | LA64_ECODE_IPE | LA64_ECODE_FPD => TrapClass::IllegalInstruction,
        _ => TrapClass::UnknownSync,
    }
}

pub(crate) struct La64IrqContextGuard;

impl Drop for La64IrqContextGuard {
    fn drop(&mut self) {
        let previous = LA64_IRQ_CONTEXT_DEPTH
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                Some(depth.saturating_sub(1))
            })
            .unwrap_or(0);
        debug_assert!(previous > 0);
    }
}

pub(crate) fn enter_la64_irq_context() -> La64IrqContextGuard {
    LA64_IRQ_CONTEXT_DEPTH.fetch_add(1, Ordering::AcqRel);
    La64IrqContextGuard
}

pub(crate) fn la64_irq_context_depth() -> usize {
    LA64_IRQ_CONTEXT_DEPTH.load(Ordering::Acquire)
}

pub(crate) fn install_la64_trap_vectors() {
    let exception = la64_kernel_addr_to_phys(la64_exception_vector_addr());
    let tlb_refill = la64_kernel_addr_to_phys(la64_tlb_refill_vector_addr());

    write_la64_csr(LA64_CSR_EENTRY, exception);
    write_la64_csr(LA64_CSR_TLBRENTRY, tlb_refill);
    write_la64_csr(LA64_CSR_MERRENTRY, exception);
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn la64_exception_vector_addr() -> usize {
    unsafe extern "C" {
        fn tx_la64_qemu_exception_vector();
    }

    tx_la64_qemu_exception_vector as *const () as usize
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn la64_exception_vector_addr() -> usize {
    0
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn la64_tlb_refill_vector_addr() -> usize {
    unsafe extern "C" {
        fn tx_la64_qemu_tlb_refill_vector();
    }

    tx_la64_qemu_tlb_refill_vector as *const () as usize
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn la64_tlb_refill_vector_addr() -> usize {
    0
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_kernel_loongarch64_qemu_trap_dispatch(frame: *mut La64TrapFrame) -> TrapAction;
    fn tx_la64_resume_kernel_after_reschedule(resume_ctx: *const KernelResumeCtx) -> !;
}

#[cfg(target_arch = "loongarch64")]
#[no_mangle]
extern "C" fn tx_la64_qemu_kernel_trap_entry(frame: &mut La64TrapFrame) {
    let action = unsafe { tx_kernel_loongarch64_qemu_trap_dispatch(frame) };
    apply_la64_trap_action(frame, action);
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn apply_la64_trap_action(frame: &La64TrapFrame, action: TrapAction) {
    let from_user = frame.previous_mode() == TrapPreviousMode::User;
    match action {
        #[cfg(target_arch = "loongarch64")]
        TrapAction::Reschedule if from_user => unsafe {
            let cpu = <Platform as SmpIf>::current_cpu_id();
            let stack_top = la64_trap_stack_top_for_cpu(cpu);
            write_la64_csr(LA64_CSR_KSAVE0, stack_top);
            let resume_ctx = la64_kernel_resume_ctx_ptr_for_cpu(cpu) as *const KernelResumeCtx;
            tx_la64_resume_kernel_after_reschedule(resume_ctx);
        },
        TrapAction::Resume | TrapAction::Reschedule | TrapAction::DeliverSignal => {}
        TrapAction::Terminate => tx_la64_qemu_trap_panic(frame),
    }
}

pub(crate) fn write_la64_csr(csr: usize, value: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        match csr {
            LA64_CSR_CRMD => {
                core::arch::asm!("csrwr {value}, 0x00", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_EENTRY => {
                core::arch::asm!("csrwr {value}, 0x0c", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_ECFG => {
                core::arch::asm!("csrwr {value}, 0x04", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_ASID => {
                core::arch::asm!("csrwr {value}, 0x18", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_PGDL => {
                core::arch::asm!("csrwr {value}, 0x19", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_PGDH => {
                core::arch::asm!("csrwr {value}, 0x1a", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_PWCL => {
                core::arch::asm!("csrwr {value}, 0x1c", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_PWCH => {
                core::arch::asm!("csrwr {value}, 0x1d", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_STLBPS => {
                core::arch::asm!("csrwr {value}, 0x1e", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TLBRENTRY => {
                core::arch::asm!("csrwr {value}, 0x88", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TLBREHI => {
                core::arch::asm!("csrwr {value}, 0x8e", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_MERRENTRY => {
                core::arch::asm!("csrwr {value}, 0x93", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TCFG => {
                core::arch::asm!("csrwr {value}, 0x41", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TICLR => {
                core::arch::asm!("csrwr {value}, 0x44", value = in(reg) value, options(nomem, nostack));
            }
            _ => {}
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = (csr, value);
}

pub(crate) fn read_la64_csr(csr: usize) -> usize {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let value: usize;
        match csr {
            LA64_CSR_CRMD => {
                core::arch::asm!("csrrd {value}, 0x00", value = out(reg) value, options(nomem, nostack));
                value
            }
            LA64_CSR_ECFG => {
                core::arch::asm!("csrrd {value}, 0x04", value = out(reg) value, options(nomem, nostack));
                value
            }
            _ => 0,
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = csr;
        0
    }
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn tx_la64_qemu_trap_panic(frame: &La64TrapFrame) -> ! {
    console_write_literal(b"txkernel:qemu-loongarch64-virt:trap\nreason=trap-action-terminate\n");
    console_write_trap_summary(frame);
    console_write_trapframe(frame);

    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack));
        }
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_trap_summary(frame: &La64TrapFrame) {
    console_write_literal(b"estat=0x");
    console_write_hex(frame.estat);
    console_write_literal(b" era=0x");
    console_write_hex(frame.era);
    console_write_literal(b" badv=0x");
    console_write_hex(frame.badv);
    console_write_literal(b"\n");
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_trapframe(frame: &La64TrapFrame) {
    console_write_literal(b"trapframe:\n");
    for index in 0..32 {
        if index % 4 == 0 {
            console_write_literal(b"  ");
        } else {
            console_write_literal(b" ");
        }
        console_write_literal(b"r");
        console_write_decimal(index);
        console_write_literal(b"=0x");
        console_write_hex(frame.r[index]);
        if index % 4 == 3 {
            console_write_literal(b"\n");
        }
    }
    console_write_literal(b"  estat=0x");
    console_write_hex(frame.estat);
    console_write_literal(b" era=0x");
    console_write_hex(frame.era);
    console_write_literal(b" badv=0x");
    console_write_hex(frame.badv);
    console_write_literal(b" crmd=0x");
    console_write_hex(frame.crmd);
    console_write_literal(b" prmd=0x");
    console_write_hex(frame.prmd);
    console_write_literal(b"\n");
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_literal(bytes: &[u8]) {
    Platform::write_bytes(bytes);
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn console_write_literal(_bytes: &[u8]) {}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_hex(value: usize) {
    for shift in (0..usize::BITS).step_by(4).rev() {
        let digit = ((value >> shift) & 0xf) as u8;
        let byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        Platform::write_bytes(&[byte]);
    }
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn console_write_hex(_value: usize) {}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_decimal(mut value: usize) {
    let mut buf = [0u8; 20];
    let mut cursor = buf.len();
    loop {
        cursor -= 1;
        buf[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    Platform::write_bytes(&buf[cursor..]);
}

#[no_mangle]
#[cfg(target_arch = "loongarch64")]
extern "C" fn tx_la64_qemu_unhandled_exception() -> ! {
    Platform::write_bytes(b"txkernel:qemu-loongarch64-virt:trap\n");
    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack));
        }
        core::hint::spin_loop();
    }
}

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

pub(crate) fn publish_static_boot_facts() {
    let kernel_image = linked_kernel_image();
    let reserved_end = align_up(
        kernel_image.end().0,
        <Platform as PlatformConfig>::PAGE_SIZE,
    )
    .min(QEMU_LA64_RAM_END);
    // DIAGNOSTIC: log the raw LoongArch direct-boot registers. QEMU
    // places the Linux boot flag in a0, the command-line physical
    // address in a1, and the EFI system-table physical address in a2.
    #[cfg(target_arch = "loongarch64")]
    {
        let fw = LA64_BOOT_FIRMWARE_ARG.load(Ordering::Acquire);
        let efi_boot = LA64_BOOT_EFI_BOOT.load(Ordering::Acquire);
        let cmdline = LA64_BOOT_CMDLINE_PTR.load(Ordering::Acquire);
        let system_table = LA64_BOOT_SYSTEM_TABLE.load(Ordering::Acquire);
        console_write_literal(b"txkernel:qemu-loongarch64-virt:bootarg:a0=0x");
        console_write_hex(efi_boot);
        console_write_literal(b":a1=0x");
        console_write_hex(cmdline);
        console_write_literal(b":a2=0x");
        console_write_hex(system_table);
        console_write_literal(b":fw=0x");
        console_write_hex(fw);
        console_write_literal(b"\n");
    }
    let parsed_from_firmware = parse_firmware_boot_info(reserved_end);
    let (memory_region_count, initrd, cmdline_len, timebase_frequency_hz, possible_cpu_count) =
        parsed_from_firmware.unwrap_or_else(|| {
            let usable_size = QEMU_LA64_RAM_END.saturating_sub(reserved_end);
            unsafe {
                let regions = core::ptr::addr_of_mut!(BOOT_MEMORY_REGIONS) as *mut MemoryRegion;
                core::ptr::write(
                    regions,
                    MemoryRegion {
                        base: PhysAddr(QEMU_LA64_RAM_BASE),
                        size: reserved_end - QEMU_LA64_RAM_BASE,
                        kind: MemoryRegionKind::Reserved,
                    },
                );
                core::ptr::write(
                    regions.add(1),
                    MemoryRegion {
                        base: PhysAddr(reserved_end),
                        size: usable_size,
                        kind: MemoryRegionKind::Usable,
                    },
                );
            }

            (
                2usize,
                None,
                0usize,
                la64_detect_timebase_frequency_hz(),
                LA64_DEFAULT_POSSIBLE_CPUS,
            )
        });

    LA64_TIMEBASE_HZ.store(timebase_frequency_hz, Ordering::Release);
    LA64_POSSIBLE_CPU_COUNT.store(possible_cpu_count, Ordering::Release);

    let direct_map = VirtRange {
        start: VirtAddr(la64_cached_virt(QEMU_LA64_RAM_BASE)),
        size: QEMU_LA64_RAM_SIZE,
    };
    let cmdline = if cmdline_len > 0 {
        unsafe {
            Some(core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                core::ptr::addr_of!(BOOT_CMDLINE) as *const u8,
                cmdline_len,
            )))
        }
    } else {
        None
    };

    unsafe {
        let regions = core::ptr::addr_of!(BOOT_MEMORY_REGIONS) as *const MemoryRegion;

        core::ptr::write(
            core::ptr::addr_of_mut!(BOOT_INFO),
            BootInfo {
                memory_regions: core::slice::from_raw_parts(regions, memory_region_count),
                kernel_image,
                initrd,
                cmdline,
            },
        );

        core::ptr::write(
            core::ptr::addr_of_mut!(BOOTSTRAP_PMAP_INFO),
            BootstrapPmapInfo {
                // LA64 publishes DMW-backed direct-map facts before the
                // board-owned page-table root exists. Fine-grained mapping
                // mutation stays unsupported until real pmap work lands.
                root: PhysAddr(0),
                mapped: PhysRange {
                    start: PhysAddr(QEMU_LA64_RAM_BASE),
                    size: QEMU_LA64_RAM_SIZE,
                },
                direct_map_base: VirtAddr(LA64_DMW_CACHED_BASE),
                direct_map,
                kernel_image: VirtRange {
                    start: VirtAddr(la64_cached_virt(kernel_image.start.0)),
                    size: kernel_image.size,
                },
                identity: None,
                pt_node_pool: PhysRange::empty(),
                reserved_page_tables: &[],
            },
        );

        (*core::ptr::addr_of_mut!(PLATFORM_INFO)).timebase_frequency_hz = timebase_frequency_hz;
        (*core::ptr::addr_of_mut!(PLATFORM_INFO)).possible_cpu_count = possible_cpu_count;
    }

    write_boot_facts_summary(
        parsed_from_firmware.is_some(),
        memory_region_count,
        timebase_frequency_hz,
        possible_cpu_count,
        initrd,
        cmdline,
    );
}

fn parse_firmware_boot_info(
    reserved_end: usize,
) -> Option<(usize, Option<PhysRange>, usize, u64, usize)> {
    let system_table_phys = LA64_BOOT_SYSTEM_TABLE.load(Ordering::Acquire);
    let legacy_firmware_arg = LA64_BOOT_FIRMWARE_ARG.load(Ordering::Acquire);

    unsafe {
        let mut dtb_memory_regions = [MemoryRegion {
            base: PhysAddr(0),
            size: 0,
            kind: MemoryRegionKind::Reserved,
        }; LA64_BOOT_MEMORY_REGION_CAPACITY];
        let cmdline = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BOOT_CMDLINE) as *mut u8,
            LA64_BOOT_CMDLINE_CAPACITY,
        );

        if system_table_phys != 0 {
            let efi = parse_efi_boot_info(system_table_phys);
            let parsed_dtb = efi.and_then(|info| info.fdt).and_then(|fdt| {
                parse_dtb_boot_info_with_fallbacks(fdt, &mut dtb_memory_regions, cmdline)
            });
            let dtb_cmdline_len = parsed_dtb.map_or(0, |parsed| parsed.cmdline_len);
            let boot_cmdline_len =
                copy_cmdline_from_phys(LA64_BOOT_CMDLINE_PTR.load(Ordering::Acquire), cmdline);
            let cmdline_len = if boot_cmdline_len > 0 {
                boot_cmdline_len
            } else {
                dtb_cmdline_len
            }
            .min(cmdline.len());
            let initrd = efi
                .and_then(|info| info.initrd)
                .or_else(|| parsed_dtb.and_then(|parsed| parsed.initrd));

            if parsed_dtb.is_some() || initrd.is_some() || cmdline_len > 0 {
                let memory_region_count = if let Some(parsed) = parsed_dtb {
                    populate_boot_memory_regions_from_dtb(
                        &dtb_memory_regions,
                        parsed.memory_region_count,
                        reserved_end,
                    )
                } else {
                    populate_fallback_boot_memory_regions(reserved_end)
                };
                let timebase_frequency_hz = parsed_dtb
                    .and_then(|parsed| parsed.timebase_frequency_hz)
                    .unwrap_or_else(la64_detect_timebase_frequency_hz);
                let possible_cpu_count = parsed_dtb
                    .map_or(LA64_DEFAULT_POSSIBLE_CPUS, |parsed| {
                        parsed.possible_cpu_count
                    })
                    .clamp(1, LA64_MAX_BOOT_CPUS);

                return Some((
                    memory_region_count,
                    initrd,
                    cmdline_len,
                    timebase_frequency_hz,
                    possible_cpu_count,
                ));
            }
        }

        if let Some(parsed) = parse_fw_cfg_boot_info(reserved_end, &mut dtb_memory_regions, cmdline)
        {
            return Some(parsed);
        }

        if legacy_firmware_arg != 0 {
            if let Some(parsed) = parse_dtb_boot_info_with_fallbacks(
                legacy_firmware_arg,
                &mut dtb_memory_regions,
                cmdline,
            ) {
                let memory_region_count = populate_boot_memory_regions_from_dtb(
                    &dtb_memory_regions,
                    parsed.memory_region_count,
                    reserved_end,
                );
                let cmdline_len = parsed.cmdline_len.min(cmdline.len());
                let timebase_frequency_hz = parsed
                    .timebase_frequency_hz
                    .unwrap_or_else(la64_detect_timebase_frequency_hz);
                let possible_cpu_count = parsed.possible_cpu_count.clamp(1, LA64_MAX_BOOT_CPUS);

                return Some((
                    memory_region_count,
                    parsed.initrd,
                    cmdline_len,
                    timebase_frequency_hz,
                    possible_cpu_count,
                ));
            }
        }
    }

    None
}

unsafe fn parse_fw_cfg_boot_info(
    reserved_end: usize,
    dtb_memory_regions: &mut [MemoryRegion],
    cmdline: &mut [u8],
) -> Option<(usize, Option<PhysRange>, usize, u64, usize)> {
    if !fw_cfg_available() {
        return None;
    }

    let parsed_dtb =
        parse_dtb_boot_info_with_fallbacks(QEMU_LA64_FDT_BASE, dtb_memory_regions, cmdline);
    let dtb_cmdline_len = parsed_dtb.map_or(0, |parsed| parsed.cmdline_len);
    let fw_cfg_files = fw_cfg_find_boot_files();
    let boot_cmdline_len = fw_cfg_copy_cmdline(cmdline, fw_cfg_files.cmdline);
    let cmdline_len = if boot_cmdline_len > 0 {
        boot_cmdline_len
    } else {
        dtb_cmdline_len
    }
    .min(cmdline.len());
    let initrd = fw_cfg_copy_initrd(fw_cfg_files.initrd);

    if parsed_dtb.is_none() && initrd.is_none() && cmdline_len == 0 {
        return None;
    }

    let memory_region_count = if let Some(parsed) = parsed_dtb {
        populate_boot_memory_regions_from_dtb(
            dtb_memory_regions,
            parsed.memory_region_count,
            reserved_end,
        )
    } else {
        populate_fallback_boot_memory_regions(reserved_end)
    };
    let timebase_frequency_hz = parsed_dtb
        .and_then(|parsed| parsed.timebase_frequency_hz)
        .unwrap_or_else(la64_detect_timebase_frequency_hz);
    let possible_cpu_count = parsed_dtb
        .map_or(LA64_DEFAULT_POSSIBLE_CPUS, |parsed| {
            parsed.possible_cpu_count
        })
        .clamp(1, LA64_MAX_BOOT_CPUS);

    Some((
        memory_region_count,
        initrd.or_else(|| parsed_dtb.and_then(|parsed| parsed.initrd)),
        cmdline_len,
        timebase_frequency_hz,
        possible_cpu_count,
    ))
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_SIGNATURE: u16 = 0x0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_KERNEL_CMDLINE: u16 = 0x0009;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_INITRD_SIZE: u16 = 0x000b;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_INITRD_DATA: u16 = 0x0012;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_CMDLINE_SIZE: u16 = 0x0014;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_FILE_DIR: u16 = 0x0019;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_DATA_OFFSET: usize = 0x00;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_SELECTOR_OFFSET: usize = 0x08;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_MAX_FILE_ENTRIES: usize = 64;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const FW_CFG_FILE_NAME_LEN: usize = 56;

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
#[derive(Clone, Copy)]
struct FwCfgFile {
    selector: u16,
    size: usize,
}

#[derive(Clone, Copy)]
struct FwCfgBootFiles {
    cmdline: Option<FwCfgFile>,
    initrd: Option<FwCfgFile>,
}

fn fw_cfg_available() -> bool {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        fw_cfg_select(FW_CFG_SIGNATURE);
        let sig = [
            fw_cfg_read_u8(),
            fw_cfg_read_u8(),
            fw_cfg_read_u8(),
            fw_cfg_read_u8(),
        ];
        sig == *b"QEMU"
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        false
    }
}

fn fw_cfg_copy_cmdline(dst: &mut [u8], file: Option<FwCfgFile>) -> usize {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let (selector, size) = if let Some(file) = file {
            (file.selector, file.size.min(dst.len()))
        } else {
            let size = fw_cfg_read_u32_be(FW_CFG_CMDLINE_SIZE)
                .map(|value| value as usize)
                .unwrap_or(dst.len())
                .min(dst.len());
            (FW_CFG_KERNEL_CMDLINE, size)
        };
        if size == 0 {
            return 0;
        }

        fw_cfg_select(selector);
        let mut len = 0usize;
        while len < size {
            let byte = fw_cfg_read_u8();
            if byte == 0 {
                break;
            }
            if len == 0 {
                dst.fill(0);
            }
            dst[len] = byte;
            len += 1;
        }
        len
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = (dst, file);
        0
    }
}

fn fw_cfg_copy_initrd(file: Option<FwCfgFile>) -> Option<PhysRange> {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let (selector, size) = if let Some(file) = file {
            (file.selector, file.size)
        } else {
            (
                FW_CFG_INITRD_DATA,
                fw_cfg_read_u32_be(FW_CFG_INITRD_SIZE)? as usize,
            )
        };
        if size == 0 || size > LA64_FW_CFG_INITRD_CAPACITY {
            return None;
        }

        let dst = fw_cfg_initrd_buffer_ptr();
        fw_cfg_select(selector);
        for offset in 0..size {
            core::ptr::write_volatile(dst.add(offset), fw_cfg_read_u8());
        }

        Some(PhysRange {
            start: PhysAddr(la64_kernel_addr_to_phys(dst as usize)),
            size,
        })
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = file;
        None
    }
}

fn fw_cfg_find_boot_files() -> FwCfgBootFiles {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        fw_cfg_select(FW_CFG_FILE_DIR);
        let count = fw_cfg_read_stream_u32_be().min(FW_CFG_MAX_FILE_ENTRIES as u32) as usize;
        let mut out = FwCfgBootFiles {
            cmdline: None,
            initrd: None,
        };

        for _ in 0..count {
            let size = fw_cfg_read_stream_u32_be() as usize;
            let selector = fw_cfg_read_stream_u16_be();
            let _reserved = fw_cfg_read_stream_u16_be();
            let mut name = [0u8; FW_CFG_FILE_NAME_LEN];
            for byte in &mut name {
                *byte = fw_cfg_read_u8();
            }

            if out.cmdline.is_none()
                && (name_contains(&name, b"cmdline") || name_contains(&name, b"bootargs"))
            {
                out.cmdline = Some(FwCfgFile { selector, size });
            } else if out.initrd.is_none()
                && (name_contains(&name, b"initrd") || name_contains(&name, b"ramdisk"))
            {
                out.initrd = Some(FwCfgFile { selector, size });
            }
        }

        out
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        FwCfgBootFiles {
            cmdline: None,
            initrd: None,
        }
    }
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
fn name_contains(name: &[u8; FW_CFG_FILE_NAME_LEN], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > name.len() {
        return false;
    }
    name.windows(needle.len()).any(|window| window == needle)
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_u32_be(selector: u16) -> Option<u32> {
    fw_cfg_select(selector);
    Some(fw_cfg_read_stream_u32_be())
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_stream_u32_be() -> u32 {
    u32::from_be_bytes([
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
    ])
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_stream_u16_be() -> u16 {
    u16::from_be_bytes([fw_cfg_read_u8(), fw_cfg_read_u8()])
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_select(selector: u16) {
    let ptr = la64_uncached_virt(QEMU_LA64_FW_CFG_BASE + FW_CFG_SELECTOR_OFFSET) as *mut u16;
    core::ptr::write_volatile(ptr, selector.to_be());
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_read_u8() -> u8 {
    let ptr = la64_uncached_virt(QEMU_LA64_FW_CFG_BASE + FW_CFG_DATA_OFFSET) as *const u8;
    core::ptr::read_volatile(ptr)
}

#[cfg(target_arch = "loongarch64")]
unsafe fn fw_cfg_initrd_buffer_ptr() -> *mut u8 {
    core::ptr::addr_of_mut!(LA64_FW_CFG_INITRD_BUFFER.bytes) as *mut u8
}

#[derive(Clone, Copy)]
struct EfiBootInfo {
    initrd: Option<PhysRange>,
    fdt: Option<usize>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiTableHeader {
    signature: u64,
    revision: u32,
    header_size: u32,
    crc32: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiSystemTable {
    hdr: EfiTableHeader,
    fw_vendor: u64,
    fw_revision: u32,
    _pad: u32,
    con_in_handle: u64,
    con_in: u64,
    con_out_handle: u64,
    con_out: u64,
    stderr_handle: u64,
    stderr: u64,
    runtime: u64,
    boottime: u64,
    nr_tables: u64,
    tables: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiConfigurationTable {
    guid: [u8; 16],
    table: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EfiInitrd {
    base: u64,
    size: u64,
}

const EFI_SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
const EFI_MAX_CONFIG_TABLES: usize = 16;
const LINUX_EFI_INITRD_MEDIA_GUID: [u8; 16] = [
    0x27, 0xe4, 0x68, 0x55, 0xfc, 0x68, 0x3d, 0x4f, 0xac, 0x74, 0xca, 0x55, 0x52, 0x31, 0xcc, 0x68,
];
const DEVICE_TREE_GUID: [u8; 16] = [
    0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0,
];

unsafe fn parse_efi_boot_info(system_table_phys: usize) -> Option<EfiBootInfo> {
    let systab = read_boot_phys::<EfiSystemTable>(system_table_phys)?;
    if systab.hdr.signature != EFI_SYSTEM_TABLE_SIGNATURE {
        return None;
    }

    let tables_phys = usize::try_from(systab.tables).ok()?;
    let table_count = usize::try_from(systab.nr_tables)
        .ok()?
        .min(EFI_MAX_CONFIG_TABLES);
    let mut out = EfiBootInfo {
        initrd: None,
        fdt: None,
    };

    for index in 0..table_count {
        let entry_phys =
            tables_phys.checked_add(index * core::mem::size_of::<EfiConfigurationTable>())?;
        let entry = read_boot_phys::<EfiConfigurationTable>(entry_phys)?;
        let table_phys = usize::try_from(entry.table).ok()?;
        if entry.guid == LINUX_EFI_INITRD_MEDIA_GUID {
            let initrd = read_boot_phys::<EfiInitrd>(table_phys)?;
            let base = usize::try_from(initrd.base).ok()?;
            let size = usize::try_from(initrd.size).ok()?;
            if size > 0 {
                out.initrd = Some(PhysRange {
                    start: PhysAddr(base),
                    size,
                });
            }
        } else if entry.guid == DEVICE_TREE_GUID && table_phys != 0 {
            out.fdt = Some(table_phys);
        }
    }

    Some(out)
}

unsafe fn read_boot_phys<T: Copy>(phys: usize) -> Option<T> {
    let ptr = boot_phys_to_ptr::<T>(phys)?;
    Some(unsafe { core::ptr::read_unaligned(ptr) })
}

fn boot_phys_to_ptr<T>(phys: usize) -> Option<*const T> {
    let phys = la64_kernel_addr_to_phys(phys);
    if (QEMU_LA64_RAM_END..QEMU_LA64_PCIE_ECAM_BASE).contains(&phys) {
        return None;
    }

    #[cfg(target_arch = "loongarch64")]
    {
        Some(la64_cached_virt(phys) as *const T)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        Some(phys as *const T)
    }
}

unsafe fn copy_cmdline_from_phys(cmdline_phys: usize, dst: &mut [u8]) -> usize {
    let Some(src) = boot_phys_to_ptr::<u8>(cmdline_phys) else {
        return 0;
    };

    let mut len = 0usize;
    while len < dst.len() {
        let byte = unsafe { core::ptr::read_volatile(src.add(len)) };
        if byte == 0 {
            break;
        }
        if len == 0 {
            dst.fill(0);
        }
        dst[len] = byte;
        len += 1;
    }
    len
}

unsafe fn parse_dtb_boot_info_with_fallbacks(
    firmware_arg: usize,
    memory_regions: &mut [MemoryRegion],
    cmdline: &mut [u8],
) -> Option<DtbBootInfo> {
    let parse = |addr: usize, memory: &mut [MemoryRegion], out: &mut [u8]| unsafe {
        parse_boot_info_from_fdt(addr, memory, out)
    };

    // Derive the physical address and both DMW aliases. We always probe
    // the cached-alias first (safe: DMW window bypasses TLB, no fault
    // risk at early boot). Reading a raw physical address — what a QEMU
    // direct-boot loader places in `a1` — causes a kernel-mode TLB fault
    // and Terminate before page tables exist, so we check whether
    // `firmware_arg` is already in a DMW window before trying it directly.
    let phys = la64_kernel_addr_to_phys(firmware_arg);
    let cached_alias = la64_cached_virt(phys);
    let uncached_alias = la64_uncached_virt(phys);

    // 1. Cached DMW alias — always safe.
    if let Some(parsed) = parse(cached_alias, memory_regions, cmdline) {
        return Some(parsed);
    }

    // 2. firmware_arg itself, only if it is a DMW virtual address (i.e.
    //    QEMU / firmware already wrapped the physical in a window tag).
    if firmware_arg != cached_alias && firmware_arg != uncached_alias {
        let fw_tag = firmware_arg >> 48;
        let cached_tag = LA64_DMW_CACHED_BASE >> 48;
        let uncached_tag = LA64_DMW_UNCACHED_BASE >> 48;
        if fw_tag == cached_tag || fw_tag == uncached_tag {
            if let Some(parsed) = parse(firmware_arg, memory_regions, cmdline) {
                return Some(parsed);
            }
        }
    }

    // 3. Uncached DMW alias.
    if uncached_alias != cached_alias {
        return parse(uncached_alias, memory_regions, cmdline);
    }

    None
}

unsafe fn populate_boot_memory_regions_from_dtb(
    dtb_regions: &[MemoryRegion],
    dtb_region_count: usize,
    reserved_end: usize,
) -> usize {
    let out = core::ptr::addr_of_mut!(BOOT_MEMORY_REGIONS) as *mut MemoryRegion;
    let mut out_count = 0usize;

    core::ptr::write(
        out.add(out_count),
        MemoryRegion {
            base: PhysAddr(QEMU_LA64_RAM_BASE),
            size: reserved_end.saturating_sub(QEMU_LA64_RAM_BASE),
            kind: MemoryRegionKind::Reserved,
        },
    );
    out_count += 1;

    for region in dtb_regions.iter().take(dtb_region_count) {
        if out_count >= LA64_BOOT_MEMORY_REGION_CAPACITY {
            break;
        }

        if region.kind != MemoryRegionKind::Usable || region.size == 0 {
            continue;
        }

        let start = region.base.0;
        let end = region
            .base
            .0
            .saturating_add(region.size)
            .min(QEMU_LA64_RAM_END);
        if end <= start {
            continue;
        }

        if end <= reserved_end {
            continue;
        }

        let usable_start = start.max(reserved_end);
        if usable_start >= end {
            continue;
        }

        core::ptr::write(
            out.add(out_count),
            MemoryRegion {
                base: PhysAddr(usable_start),
                size: end - usable_start,
                kind: MemoryRegionKind::Usable,
            },
        );
        out_count += 1;
    }

    if out_count == 1
        && out_count < LA64_BOOT_MEMORY_REGION_CAPACITY
        && reserved_end < QEMU_LA64_RAM_END
    {
        core::ptr::write(
            out.add(out_count),
            MemoryRegion {
                base: PhysAddr(reserved_end),
                size: QEMU_LA64_RAM_END - reserved_end,
                kind: MemoryRegionKind::Usable,
            },
        );
        out_count += 1;
    }

    out_count
}

unsafe fn populate_fallback_boot_memory_regions(reserved_end: usize) -> usize {
    let usable_size = QEMU_LA64_RAM_END.saturating_sub(reserved_end);
    let out = core::ptr::addr_of_mut!(BOOT_MEMORY_REGIONS) as *mut MemoryRegion;
    unsafe {
        core::ptr::write(
            out,
            MemoryRegion {
                base: PhysAddr(QEMU_LA64_RAM_BASE),
                size: reserved_end - QEMU_LA64_RAM_BASE,
                kind: MemoryRegionKind::Reserved,
            },
        );
        core::ptr::write(
            out.add(1),
            MemoryRegion {
                base: PhysAddr(reserved_end),
                size: usable_size,
                kind: MemoryRegionKind::Usable,
            },
        );
    }
    2
}

fn write_boot_facts_summary(
    parsed_from_dtb: bool,
    memory_region_count: usize,
    timebase_frequency_hz: u64,
    possible_cpu_count: usize,
    initrd: Option<PhysRange>,
    cmdline: Option<&str>,
) {
    #[cfg(target_arch = "loongarch64")]
    {
        console_write_literal(b"txkernel:qemu-loongarch64-virt:bootinfo:");
        if parsed_from_dtb {
            console_write_literal(b"dtb");
        } else {
            console_write_literal(b"fallback");
        }
        console_write_literal(b":regions=");
        console_write_decimal(memory_region_count);
        console_write_literal(b":timebase-hz=");
        console_write_decimal(timebase_frequency_hz as usize);
        console_write_literal(b":cpus=");
        console_write_decimal(possible_cpu_count);

        if let Some(range) = initrd {
            console_write_literal(b":initrd=0x");
            console_write_hex(range.start.0);
            console_write_literal(b"+0x");
            console_write_hex(range.size);
        } else {
            console_write_literal(b":initrd=none");
        }

        if let Some(value) = cmdline {
            console_write_literal(b":cmdline=\"");
            Platform::write_bytes(value.as_bytes());
            console_write_literal(b"\"");
        } else {
            console_write_literal(b":cmdline=none");
        }
        console_write_literal(b"\n");
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = (
        parsed_from_dtb,
        memory_region_count,
        timebase_frequency_hz,
        possible_cpu_count,
        initrd,
        cmdline,
    );
}

pub(crate) fn linked_kernel_image() -> PhysRange {
    let start = linked_kernel_start();
    let end = linked_kernel_end();

    PhysRange {
        start: PhysAddr(start),
        size: end.saturating_sub(start),
    }
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn linked_kernel_start() -> usize {
    la64_kernel_addr_to_phys(core::ptr::addr_of!(__kernel_start) as usize)
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn linked_kernel_start() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn linked_kernel_end() -> usize {
    la64_kernel_addr_to_phys(core::ptr::addr_of!(__kernel_end) as usize)
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn linked_kernel_end() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE + 128 * 1024
}

pub(crate) fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

pub(crate) fn installed_pt_node_allocator() -> Option<PtNodeAllocator> {
    let value = INSTALLED_PT_NODE_ALLOCATOR.load(Ordering::Acquire);
    if value == 0 {
        return None;
    }

    Some(unsafe { core::mem::transmute::<usize, PtNodeAllocator>(value) })
}

pub(crate) fn dmw_mmio_page_is_precovered(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> bool {
    virt == VirtAddr(la64_uncached_virt(phys.0)) && dmw_covers_phys_range(phys, kind.size())
}
