use super::la64_pmap::{
    la64_cached_virt, la64_fixup_lookup, la64_kernel_addr_to_phys, la64_uncached_virt,
};
use super::*;

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
    }

    detected
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
        TrapClass::Syscall => {
            frame.era = frame.era.saturating_add(4);
            K::on_syscall(frame.view_mut())
        }
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
        LA64_ECODE_INE | LA64_ECODE_IPE => TrapClass::IllegalInstruction,
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
}

#[cfg(target_arch = "loongarch64")]
#[no_mangle]
extern "C" fn tx_la64_qemu_kernel_trap_entry(frame: &mut La64TrapFrame) {
    let action = unsafe { tx_kernel_loongarch64_qemu_trap_dispatch(frame) };
    apply_la64_trap_action(frame, action);
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn apply_la64_trap_action(frame: &La64TrapFrame, action: TrapAction) {
    match action {
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

#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_hex(value: usize) {
    for shift in (0..usize::BITS).rev().step_by(4) {
        let digit = ((value >> shift) & 0xf) as u8;
        let byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        Platform::write_bytes(&[byte]);
    }
}

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
    let usable_size = QEMU_LA64_RAM_END.saturating_sub(reserved_end);
    let direct_map = VirtRange {
        start: VirtAddr(la64_cached_virt(QEMU_LA64_RAM_BASE)),
        size: QEMU_LA64_RAM_SIZE,
    };

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

        core::ptr::write(
            core::ptr::addr_of_mut!(BOOT_INFO),
            BootInfo {
                memory_regions: core::slice::from_raw_parts(regions, 2),
                kernel_image,
                initrd: None,
                cmdline: None,
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
    }
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
    core::ptr::addr_of!(__kernel_start) as usize
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn linked_kernel_start() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn linked_kernel_end() -> usize {
    core::ptr::addr_of!(__kernel_end) as usize
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
    kind == PmapReserveKind::Page4K
        && virt == VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_PAGE_BASE))
        && phys == PhysAddr(QEMU_LA64_UART0_PAGE_BASE)
}
