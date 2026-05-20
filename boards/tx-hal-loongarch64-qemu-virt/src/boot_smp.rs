//! LA64 SMP boot helpers.
//!
//! This module owns the mailbox and secondary-CPU bring-up helpers used by the
//! platform's `SmpIf` implementation.

use super::la64_irq_trap::{read_la64_csr, write_la64_csr};
use super::la64_pmap::la64_current_cpu_id;
use super::*;

#[cfg(target_arch = "loongarch64")]
const LA64_BOOT_STACK_STRIDE: usize = 128 * 1024;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_STATUS: usize = 0x1000;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_ENABLE: usize = 0x1004;
const LA64_IOCSR_IPI_CLEAR: usize = 0x100c;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_SEND: usize = 0x1040;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_MBUF_SEND: usize = 0x1048;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_SEND_CPU_SHIFT: usize = 16;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_SEND_BLOCKING: u32 = 1 << 31;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_VEC_SCHED: u32 = 0;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_MBUF_SEND_CPU_SHIFT: usize = 16;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_MBUF_SEND_BOX_SHIFT: usize = 2;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_MBUF_SEND_BUF_SHIFT: usize = 32;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_MBUF_SEND_BLOCKING: u64 = 1 << 31;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_MBUF_SEND_H32_MASK: u64 = 0xffff_ffff_0000_0000;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_AP_ENTRY_MAILBOX: usize = 0;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_AP_STACK_MAILBOX: usize = 1;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_AP_LOGICAL_ID_MAILBOX: usize = 2;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_ACTION_SCHED: u32 = 1;

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    static __tx_boot_stack_top: u8;
}

#[cfg(target_arch = "loongarch64")]
#[inline]
fn la64_iocsr_write_u32(addr: usize, value: u32) {
    unsafe {
        core::arch::asm!("iocsrwr.w {value}, {addr}", value = in(reg) value, addr = in(reg) addr);
    }
}

#[cfg(not(target_arch = "loongarch64"))]
#[inline]
fn la64_iocsr_write_u32(_addr: usize, _value: u32) {}

#[cfg(target_arch = "loongarch64")]
#[inline]
fn la64_iocsr_write_u64(addr: usize, value: u64) {
    unsafe {
        core::arch::asm!("iocsrwr.d {value}, {addr}", value = in(reg) value, addr = in(reg) addr);
    }
}

#[cfg(target_arch = "loongarch64")]
#[inline]
fn la64_iocsr_read_u32(addr: usize) -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("iocsrrd.w {value}, {addr}", value = out(reg) value, addr = in(reg) addr);
    }
    value
}

#[cfg(target_arch = "loongarch64")]
#[inline]
pub(crate) fn send_mail_u64(target_cpu: CpuId, mailbox: usize, value: u64) {
    let hi_box = mailbox * 2 + 1;
    let lo_box = mailbox * 2;
    let target = target_cpu.0 as u64;

    let hi = LA64_IOCSR_MBUF_SEND_BLOCKING
        | ((hi_box as u64) << LA64_IOCSR_MBUF_SEND_BOX_SHIFT)
        | (target << LA64_IOCSR_MBUF_SEND_CPU_SHIFT)
        | (value & LA64_IOCSR_MBUF_SEND_H32_MASK);
    let lo = LA64_IOCSR_MBUF_SEND_BLOCKING
        | ((lo_box as u64) << LA64_IOCSR_MBUF_SEND_BOX_SHIFT)
        | (target << LA64_IOCSR_MBUF_SEND_CPU_SHIFT)
        | (value << LA64_IOCSR_MBUF_SEND_BUF_SHIFT);
    la64_iocsr_write_u64(LA64_IOCSR_MBUF_SEND, hi);
    la64_iocsr_write_u64(LA64_IOCSR_MBUF_SEND, lo);
}

#[cfg(target_arch = "loongarch64")]
#[inline]
pub(crate) fn boot_stack_top_for_cpu(cpu: CpuId) -> usize {
    let top = core::ptr::addr_of!(__tx_boot_stack_top) as usize;
    top.saturating_sub(cpu.0.saturating_mul(LA64_BOOT_STACK_STRIDE))
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn start_secondary_cpu(cpu: CpuId, entry: SecondaryEntry) {
    let entry = entry as *const () as usize as u64;
    let stack_top = boot_stack_top_for_cpu(cpu) as u64;

    send_mail_u64(cpu, LA64_IOCSR_AP_ENTRY_MAILBOX, entry);
    send_mail_u64(cpu, LA64_IOCSR_AP_STACK_MAILBOX, stack_top);
    send_mail_u64(cpu, LA64_IOCSR_AP_LOGICAL_ID_MAILBOX, cpu.0 as u64);

    let value = LA64_IOCSR_IPI_SEND_BLOCKING
        | ((cpu.0 as u32) << LA64_IOCSR_IPI_SEND_CPU_SHIFT)
        | LA64_IOCSR_IPI_VEC_SCHED;
    la64_iocsr_write_u32(LA64_IOCSR_IPI_SEND, value);
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn wait_for_online_secondaries(target: CpuMask) -> usize {
    let target = target.bits();
    if target == 0 {
        return 0;
    }

    for _ in 0..500_000 {
        let online = LA64_ONLINE_CPUS.load(Ordering::Acquire) & target;
        if online == target {
            return online.count_ones() as usize;
        }
        core::hint::spin_loop();
    }
    (LA64_ONLINE_CPUS.load(Ordering::Acquire) & target).count_ones() as usize
}

pub(crate) fn boot_secondary_cpus(entry: SecondaryEntry) -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        let possible = CpuMask::first(
            LA64_POSSIBLE_CPU_COUNT
                .load(Ordering::Acquire)
                .clamp(1, LA64_MAX_BOOT_CPUS),
        );
        let current = la64_current_cpu_id();
        let target_mask = CpuMask::from_bits(possible.bits() & !CpuMask::single(current).bits());
        if target_mask.is_empty() {
            return 0;
        }

        LA64_IPI_ACKED_CPUS.store(0, Ordering::Release);

        let mut bits = target_mask.bits();
        while bits != 0 {
            let cpu = bits.trailing_zeros() as usize;
            start_secondary_cpu(CpuId(cpu), entry);
            bits &= bits - 1;
        }

        wait_for_online_secondaries(target_mask)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = entry;
        0
    }
}

pub(crate) fn enable_ipi_wakeups() {
    #[cfg(target_arch = "loongarch64")]
    la64_iocsr_write_u32(LA64_IOCSR_IPI_ENABLE, u32::MAX);

    let ecfg = read_la64_csr(LA64_CSR_ECFG) | LA64_ESTAT_IS_IPI;
    write_la64_csr(LA64_CSR_ECFG, ecfg);

    let crmd = read_la64_csr(LA64_CSR_CRMD) | LA64_CRMD_IE;
    write_la64_csr(LA64_CSR_CRMD, crmd);
}

pub(crate) fn pending_ipi() -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        la64_iocsr_read_u32(LA64_IOCSR_IPI_STATUS) & LA64_IOCSR_IPI_ACTION_SCHED != 0
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        false
    }
}

pub(crate) fn send_ipi(target: CpuId) {
    if target != la64_current_cpu_id() {
        #[cfg(target_arch = "loongarch64")]
        {
            let value = LA64_IOCSR_IPI_SEND_BLOCKING
                | ((target.0 as u32) << LA64_IOCSR_IPI_SEND_CPU_SHIFT)
                | LA64_IOCSR_IPI_VEC_SCHED;
            la64_iocsr_write_u32(LA64_IOCSR_IPI_SEND, value);
        }
        #[cfg(not(target_arch = "loongarch64"))]
        let _ = target;
    }
}

pub(crate) fn ack_ipi() {
    la64_iocsr_write_u32(LA64_IOCSR_IPI_CLEAR, u32::MAX);
    LA64_IPI_ACKED_CPUS.fetch_or(
        CpuMask::single(la64_current_cpu_id()).bits(),
        Ordering::AcqRel,
    );
}

pub(crate) fn clear_ipi_ack_cpus(mask: CpuMask) {
    LA64_IPI_ACKED_CPUS.fetch_and(!mask.bits(), Ordering::AcqRel);
}

pub(crate) fn ipi_ack_cpus() -> CpuMask {
    CpuMask::from_bits(LA64_IPI_ACKED_CPUS.load(Ordering::Acquire))
}
