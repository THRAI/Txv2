//! LA64 SMP boot helpers.
//!
//! This module owns the mailbox and secondary-CPU bring-up helpers used by the
//! platform's `SmpIf` implementation.

use super::la64_irq_trap::{la64_timebase_frequency_hz, read_la64_csr, write_la64_csr};
use super::la64_percpu::{la64_current_cpu_id, la64_read_stable_counter};
use super::*;

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
const LA64_IOCSR_AP_RUST_ENTRY_MAILBOX: usize = 1;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_ACTION_SCHED: u32 = 1;
const LA64_IPI_KIND_COUNT: usize = 4;
static LA64_IPI_PENDING: [AtomicU8; LA64_MAX_BOOT_CPUS] =
    [const { AtomicU8::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_IPI_ACKED: [AtomicU64; LA64_IPI_KIND_COUNT] =
    [const { AtomicU64::new(0) }; LA64_IPI_KIND_COUNT];

const fn ipi_kind_index(kind: IpiKind) -> usize {
    match kind {
        IpiKind::Reschedule => 0,
        IpiKind::TlbShootdown => 1,
        IpiKind::Membarrier => 2,
        IpiKind::Stop => 3,
    }
}

const fn ipi_kind_bit(kind: IpiKind) -> u8 {
    1u8 << ipi_kind_index(kind)
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_secondary_start() -> !;
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
pub(crate) fn start_secondary_cpu(cpu: CpuId, entry: SecondaryEntry) {
    // QEMU's slave boot ROM only consumes mailbox 0 and jumps to it; it
    // neither installs a stack nor passes the CPU id in a0. Enter our
    // physical trampoline first. The trampoline configures DMW, derives the
    // per-hart boot stack from CSR.CPUID, then loads the real Rust entry from
    // mailbox 1 and calls it with a0 = CPUID.
    let trampoline = tx_la64_secondary_start as *const () as usize as u64;
    let rust_entry = entry as *const () as usize as u64;
    send_mail_u64(cpu, LA64_IOCSR_AP_RUST_ENTRY_MAILBOX, rust_entry);
    send_mail_u64(cpu, LA64_IOCSR_AP_ENTRY_MAILBOX, trampoline);

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

    // AP initialization includes per-hart substrate/reactor setup. A fixed
    // iteration count is not a real timeout and expires far too early under
    // multi-threaded TCG. Use the architectural stable counter so an 8-hart
    // QEMU boot gets a deterministic five-second window even on a busy judge.
    let frequency = la64_timebase_frequency_hz();
    let start = la64_read_stable_counter();
    let timeout_ticks = frequency.saturating_mul(5);
    if timeout_ticks != 0 {
        loop {
            let online = LA64_ONLINE_CPUS.load(Ordering::Acquire) & target;
            if online == target {
                return online.count_ones() as usize;
            }
            if la64_read_stable_counter().wrapping_sub(start) >= timeout_ticks {
                break;
            }
            core::hint::spin_loop();
        }
    } else {
        // Firmware without a usable timebase still receives a bounded
        // best-effort wait.
        for _ in 0..10_000_000 {
            let online = LA64_ONLINE_CPUS.load(Ordering::Acquire) & target;
            if online == target {
                return online.count_ones() as usize;
            }
            core::hint::spin_loop();
        }
    }
    (LA64_ONLINE_CPUS.load(Ordering::Acquire) & target).count_ones() as usize
}

pub(crate) fn boot_secondary_cpus(possible: CpuMask, entry: SecondaryEntry) -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        // Wake set = SmpIf policy mask (firmware topology capped by
        // tx.maxcpus) ∩ the raw discovered CPU count. Missing firmware
        // topology is deliberately one CPU, so boards cannot accidentally
        // wake nonexistent harts.
        let discovered = CpuMask::first(
            LA64_POSSIBLE_CPU_COUNT
                .load(Ordering::Acquire)
                .clamp(1, LA64_MAX_BOOT_CPUS),
        );
        let current = la64_current_cpu_id();
        let target_mask = CpuMask::from_bits(
            possible.bits() & discovered.bits() & !CpuMask::single(current).bits(),
        );
        if target_mask.is_empty() {
            return 0;
        }

        for acked in &LA64_IPI_ACKED {
            acked.store(0, Ordering::Release);
        }

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
        let _ = (possible, entry);
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

pub(crate) fn pending_ipi(kind: IpiKind) -> bool {
    let cpu = la64_current_cpu_id();
    cpu.0 < LA64_IPI_PENDING.len()
        && LA64_IPI_PENDING[cpu.0].load(Ordering::Acquire) & ipi_kind_bit(kind) != 0
}

pub(crate) fn send_ipi(target: CpuId, kind: IpiKind) {
    if target != la64_current_cpu_id() {
        if target.0 >= LA64_IPI_PENDING.len() {
            return;
        }
        LA64_IPI_PENDING[target.0].fetch_or(ipi_kind_bit(kind), Ordering::AcqRel);
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

pub(crate) fn ack_ipi(kind: IpiKind) {
    let cpu = la64_current_cpu_id();
    if cpu.0 >= LA64_IPI_PENDING.len() {
        return;
    }
    if matches!(kind, IpiKind::TlbShootdown) {
        crate::la64_pmap::la64_invtlb_all();
    }
    LA64_IPI_PENDING[cpu.0].fetch_and(!ipi_kind_bit(kind), Ordering::AcqRel);
    LA64_IPI_ACKED[ipi_kind_index(kind)].fetch_or(CpuMask::single(cpu).bits(), Ordering::AcqRel);
    if LA64_IPI_PENDING[cpu.0].load(Ordering::Acquire) == 0 {
        la64_iocsr_write_u32(LA64_IOCSR_IPI_CLEAR, u32::MAX);
    }
}

pub(crate) fn clear_ipi_ack_cpus(kind: IpiKind, mask: CpuMask) {
    LA64_IPI_ACKED[ipi_kind_index(kind)].fetch_and(!mask.bits(), Ordering::AcqRel);
}

pub(crate) fn ipi_ack_cpus(kind: IpiKind) -> CpuMask {
    CpuMask::from_bits(LA64_IPI_ACKED[ipi_kind_index(kind)].load(Ordering::Acquire))
}
