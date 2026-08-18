use super::la64_percpu::la64_current_cpu_id;
#[cfg(target_arch = "loongarch64")]
use super::la64_pmap::deactivate_la64_user_pmap;
use super::la64_pmap::{
    dmw_covers_phys_range, la64_fixup_lookup, la64_kernel_addr_to_phys, la64_uncached_virt,
};
use super::*;

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
    let from_dtb = crate::boot_facts::platform_timebase_frequency_hz();
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

pub(crate) const fn la64_deadline_tcfg(delta_ticks: u64) -> usize {
    // Reactor deadlines are one-shot events. Setting PERIODIC here makes QEMU
    // re-arm the timer forever at the original (often very short) interval and
    // turns normal kernel interrupt enablement into a timer storm.
    round_up_to_tcfg_ticks(delta_ticks) as usize | LA64_TCFG_ENABLE
}

pub(crate) const fn la64_idle_interrupt_resume_pc(
    interrupted_pc: usize,
    region_start: usize,
    region_exit: usize,
) -> Option<usize> {
    if interrupted_pc >= region_start && interrupted_pc < region_exit {
        Some(region_exit)
    } else {
        None
    }
}

fn redirect_la64_idle_interrupt(frame: &mut La64TrapFrame) {
    #[cfg(target_arch = "loongarch64")]
    {
        unsafe extern "C" {
            fn tx_la64_idle_irq_region_start();
            fn tx_la64_idle_irq_region_exit();
        }

        let start = tx_la64_idle_irq_region_start as *const () as usize;
        let exit = tx_la64_idle_irq_region_exit as *const () as usize;
        if let Some(resume_pc) = la64_idle_interrupt_resume_pc(frame.era, start, exit) {
            frame.era = resume_pc;
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = frame;
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

    if !from_user
        && matches!(
            class,
            TrapClass::TimerInterrupt
                | TrapClass::ExternalInterrupt
                | TrapClass::InterprocessorInterrupt
                | TrapClass::UnknownInterrupt
        )
    {
        redirect_la64_idle_interrupt(frame);
    }

    // LA64 lazy-FPU first-use path: when user code traps with FPU
    // unavailable/disabled, materialise a clean per-thread FP context
    // and retry the same instruction.
    if from_user && (ecode == LA64_ECODE_IPE || ecode == LA64_ECODE_FPD) {
        let mut fp = UserFpContext::empty();
        fp.flags = UserFpContext::FLAG_VALID;
        la64_restore_fp_context(&fp);
        return TrapAction::Resume;
    }

    // LSX/LASX share their low lanes with the scalar FP register file.  The
    // first vector instruction traps while SXE/ASXE is clear; enable the
    // requested extension and retry it.  Subsequent trap-frame capture saves
    // the complete vector file into UserFpContext before another thread can
    // own the CPU.
    if from_user && (ecode == LA64_ECODE_SXD || ecode == LA64_ECODE_ASXD) {
        let mut euen = read_la64_csr(LA64_CSR_EUEN);
        euen |= LA64_EUEN_FPE | LA64_EUEN_SXE;
        if ecode == LA64_ECODE_ASXD {
            euen |= LA64_EUEN_ASXE;
        }
        write_la64_csr(LA64_CSR_EUEN, euen);
        return TrapAction::Resume;
    }

    if ecode == LA64_ECODE_ALE {
        if from_user {
            match super::la64_unaligned::emulate_user_unaligned(frame) {
                super::la64_unaligned::UnalignedOutcome::Emulated => return TrapAction::Resume,
                super::la64_unaligned::UnalignedOutcome::Unsupported => {}
                super::la64_unaligned::UnalignedOutcome::Fault(fault) => {
                    return K::on_page_fault(frame.view_mut(), fault);
                }
            }
        } else {
            // Kernel-mode ALE: real LA264 silicon has no hardware
            // unaligned access (QEMU emulates it silently). Emulate
            // and resume; unsupported encodings fall through to the
            // terminate dump so they still die loudly.
            match super::la64_unaligned::emulate_kernel_unaligned(frame) {
                super::la64_unaligned::UnalignedOutcome::Emulated => return TrapAction::Resume,
                super::la64_unaligned::UnalignedOutcome::Unsupported
                | super::la64_unaligned::UnalignedOutcome::Fault(_) => {}
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
            let cpu = <Platform as SmpIf>::current_cpu_id();
            write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
            let _irq_context = enter_la64_irq_context();
            K::on_timer_interrupt(cpu, frame.view_mut())
        }
        TrapClass::ExternalInterrupt => {
            let _irq_context = enter_la64_irq_context();
            K::on_external_irq(<Platform as SmpIf>::current_cpu_id(), frame.view_mut())
        }
        TrapClass::InterprocessorInterrupt => {
            let cpu = <Platform as SmpIf>::current_cpu_id();
            let _irq_context = enter_la64_irq_context();
            K::on_ipi(cpu, frame.view_mut())
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
    fn tx_la64_return_to_userspace(frame: *const La64TrapFrame) -> !;
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
    unsafe { tx_la64_return_to_userspace(frame as *const La64TrapFrame) }
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
        LA64_ECODE_ADE => {
            let esubcode = (estat >> LA64_ESTAT_ESUBCODE_SHIFT) & LA64_ESTAT_ESUBCODE_MASK;
            TrapClass::AlignmentFault {
                write: false,
                instruction: esubcode == LA64_ESUBCODE_ADEF,
            }
        }
        LA64_ECODE_SYS => TrapClass::Syscall,
        LA64_ECODE_BRK => TrapClass::Breakpoint,
        LA64_ECODE_INE | LA64_ECODE_IPE | LA64_ECODE_FPD | LA64_ECODE_SXD | LA64_ECODE_ASXD => {
            TrapClass::IllegalInstruction
        }
        _ => TrapClass::UnknownSync,
    }
}

pub(crate) struct La64IrqContextGuard {
    depth: &'static AtomicUsize,
}

impl Drop for La64IrqContextGuard {
    fn drop(&mut self) {
        let previous = self
            .depth
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                Some(depth.saturating_sub(1))
            })
            .unwrap_or(0);
        debug_assert!(previous > 0);
    }
}

pub(crate) fn enter_la64_irq_context() -> La64IrqContextGuard {
    let depth = current_la64_irq_depth_cell();
    depth.fetch_add(1, Ordering::AcqRel);
    La64IrqContextGuard { depth }
}

pub(crate) fn la64_irq_context_depth() -> usize {
    current_la64_irq_depth_cell().load(Ordering::Acquire)
}

fn current_la64_irq_depth_cell() -> &'static AtomicUsize {
    let cpu = la64_current_cpu_id();
    LA64_IRQ_CONTEXT_DEPTHS
        .get(cpu.0)
        .unwrap_or(&LA64_IRQ_CONTEXT_DEPTHS[0])
}

pub(crate) fn install_la64_trap_vectors() {
    let exception = la64_exception_vector_addr();
    let tlb_refill = la64_kernel_addr_to_phys(la64_tlb_refill_vector_addr());

    write_la64_csr(LA64_CSR_EENTRY, exception);
    write_la64_csr(LA64_CSR_TLBRENTRY, tlb_refill);
    write_la64_csr(LA64_CSR_MERRENTRY, exception);
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn la64_exception_vector_addr() -> usize {
    unsafe extern "C" {
        fn tx_la64_exception_vector();
    }

    tx_la64_exception_vector as *const () as usize
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn la64_exception_vector_addr() -> usize {
    0
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn la64_tlb_refill_vector_addr() -> usize {
    unsafe extern "C" {
        fn tx_la64_tlb_refill_vector();
    }

    tx_la64_tlb_refill_vector as *const () as usize
}

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn la64_tlb_refill_vector_addr() -> usize {
    0
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_kernel_loongarch64_trap_dispatch(frame: *mut La64TrapFrame) -> TrapAction;
    fn tx_la64_resume_kernel_after_reschedule(resume_ctx: *const KernelResumeCtx) -> !;
}

#[cfg(target_arch = "loongarch64")]
#[no_mangle]
extern "C" fn tx_la64_kernel_trap_entry(frame: &mut La64TrapFrame) {
    let action = unsafe { tx_kernel_loongarch64_trap_dispatch(frame) };
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
            deactivate_la64_user_pmap();
            write_la64_csr(LA64_CSR_KSAVE0, stack_top);
            let resume_ctx = la64_kernel_resume_ctx_ptr_for_cpu(cpu) as *const KernelResumeCtx;
            tx_la64_resume_kernel_after_reschedule(resume_ctx);
        },
        TrapAction::Resume | TrapAction::Reschedule | TrapAction::DeliverSignal => {}
        TrapAction::Terminate => tx_la64_trap_panic(frame),
    }
}

pub(crate) fn write_la64_csr(csr: usize, value: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        match csr {
            LA64_CSR_CRMD => {
                core::arch::asm!("csrwr {value}, 0x00", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_EUEN => {
                core::arch::asm!("csrwr {value}, 0x02", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_EENTRY => {
                core::arch::asm!("csrwr {value}, 0x0c", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_ECFG => {
                core::arch::asm!("csrwr {value}, 0x04", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_KSAVE0 => {
                core::arch::asm!("csrwr {value}, 0x30", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_ASID => {
                core::arch::asm!("csrwr {value}, 0x18", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_PGDL => {
                core::arch::asm!("csrwr {value}, 0x19", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_PGDH => {
                core::arch::asm!("csrwr {value}, 0x1a", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_PWCL => {
                core::arch::asm!("csrwr {value}, 0x1c", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_PWCH => {
                core::arch::asm!("csrwr {value}, 0x1d", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_STLBPS => {
                core::arch::asm!("csrwr {value}, 0x1e", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_TLBRENTRY => {
                core::arch::asm!("csrwr {value}, 0x88", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_TLBREHI => {
                core::arch::asm!("csrwr {value}, 0x8e", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_MERRENTRY => {
                core::arch::asm!("csrwr {value}, 0x93", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_TCFG => {
                core::arch::asm!("csrwr {value}, 0x41", value = inout(reg) value => _, options(nomem, nostack));
            }
            LA64_CSR_TICLR => {
                core::arch::asm!("csrwr {value}, 0x44", value = inout(reg) value => _, options(nomem, nostack));
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
            LA64_CSR_EUEN => {
                core::arch::asm!("csrrd {value}, 0x02", value = out(reg) value, options(nomem, nostack));
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
pub(crate) fn tx_la64_trap_panic(frame: &La64TrapFrame) -> ! {
    console_write_literal(b"txkernel:loongarch64:trap\nreason=trap-action-terminate\n");
    console_write_trap_summary(frame);
    console_write_trapframe(frame);
    // Kernel-mode traps (CRMD.PLV == 0) carry a usable kernel stack; scan it
    // for return-address candidates so a wild-jump/corrupted-ra crash can be
    // back-traced offline (objdump-resolve each printed addr). Userspace
    // traps (PLV != 0) have a user stack here — skip.
    if frame.crmd & 0x3 == 0 {
        console_write_kernel_stack_scan(frame.r[3]);
    }

    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack));
        }
        core::hint::spin_loop();
    }
}

/// Scan the kernel stack from `sp` upward for words that look like kernel
/// `.text` return addresses and print them. Offline, objdump-resolving each
/// gives the call chain that led to a wild jump / corrupted-ra crash.
#[cfg(target_arch = "loongarch64")]
pub(crate) fn console_write_kernel_stack_scan(sp: usize) {
    unsafe extern "C" {
        static __kernel_start: u8;
        static __text_end: u8;
    }
    let text_lo = (&raw const __kernel_start) as usize;
    let text_hi = (&raw const __text_end) as usize;
    // sp must be a plausible kernel direct-map address; bail if obviously bad.
    if sp < 0x9000_0000_0000_0000 || sp & 0x7 != 0 {
        console_write_literal(b"kstack: <unusable sp>\n");
        return;
    }
    console_write_literal(b"kstack-ra-candidates (sp=0x");
    console_write_hex(sp);
    console_write_literal(b"):\n");
    // Walk up to 512 words (4 KiB) of stack.
    let mut printed = 0usize;
    for i in 0..512usize {
        let addr = sp + i * 8;
        // SAFETY: kernel direct-map read; sp validated above. A bad page
        // would re-trap, but kernel stacks are mapped for this depth.
        let word = unsafe { core::ptr::read_volatile(addr as *const usize) };
        if (text_lo..text_hi).contains(&word) {
            console_write_literal(b"  +0x");
            console_write_hex(i * 8);
            console_write_literal(b": 0x");
            console_write_hex(word);
            console_write_literal(b"\n");
            printed += 1;
            if printed >= 40 {
                break;
            }
        }
    }
    if printed == 0 {
        console_write_literal(b"  <none>\n");
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

#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn console_write_decimal(_value: usize) {}

#[no_mangle]
#[cfg(target_arch = "loongarch64")]
extern "C" fn tx_la64_unhandled_exception() -> ! {
    Platform::write_bytes(b"txkernel:loongarch64:trap\n");
    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack));
        }
        core::hint::spin_loop();
    }
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
