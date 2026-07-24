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

const LA64_IPI_KIND_COUNT: usize = 5;
const LA64_IPI_CPU_SLOTS: usize = u64::BITS as usize;
static LA64_IPI_STATE_LOCKS: [AtomicBool; LA64_IPI_CPU_SLOTS] =
    [const { AtomicBool::new(false) }; LA64_IPI_CPU_SLOTS];
static LA64_IPI_PENDING_CPUS: [AtomicU64; LA64_IPI_KIND_COUNT] =
    [const { AtomicU64::new(0) }; LA64_IPI_KIND_COUNT];
static LA64_IPI_ACKED_CPUS: [AtomicU64; LA64_IPI_KIND_COUNT] =
    [const { AtomicU64::new(0) }; LA64_IPI_KIND_COUNT];
#[cfg(test)]
static TEST_IPI_TRANSPORT_MASK: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_LOCAL_MEMBARRIER_ACTIONS: AtomicUsize = AtomicUsize::new(0);

const fn ipi_kind_index(kind: IpiKind) -> usize {
    match kind {
        IpiKind::Reschedule => 0,
        IpiKind::TlbShootdown => 1,
        IpiKind::Membarrier => 2,
        IpiKind::Maintenance => 3,
        IpiKind::Stop => 4,
    }
}

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

        for pending in &LA64_IPI_PENDING_CPUS {
            pending.store(0, Ordering::Release);
        }
        for acked in &LA64_IPI_ACKED_CPUS {
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

pub(crate) fn pending_ipi(kind: IpiKind) -> bool {
    ipi_pending_on_cpu(la64_current_cpu_id(), kind)
}

fn ipi_pending_on_cpu(cpu: CpuId, kind: IpiKind) -> bool {
    let bit = CpuMask::single(cpu).bits();
    bit != 0 && LA64_IPI_PENDING_CPUS[ipi_kind_index(kind)].load(Ordering::Acquire) & bit != 0
}

pub(crate) fn send_ipi(target: CpuId, kind: IpiKind) {
    send_ipi_from(la64_current_cpu_id(), target, kind);
}

fn send_ipi_from(current: CpuId, target: CpuId, kind: IpiKind) {
    if target == current {
        return;
    }

    let locked = lock_ipi_targets(CpuMask::single(target));
    publish_ipi_pending(CpuMask::single(target), kind);
    #[cfg(target_arch = "loongarch64")]
    {
        let value = LA64_IOCSR_IPI_SEND_BLOCKING
            | ((target.0 as u32) << LA64_IOCSR_IPI_SEND_CPU_SHIFT)
            | LA64_IOCSR_IPI_VEC_SCHED;
        la64_iocsr_write_u32(LA64_IOCSR_IPI_SEND, value);
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        #[cfg(test)]
        TEST_IPI_TRANSPORT_MASK.fetch_or(CpuMask::single(target).bits(), Ordering::AcqRel);
        #[cfg(not(test))]
        let _ = target;
    }
    unlock_ipi_targets(locked);
}

pub(crate) fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
    broadcast_ipi_from(la64_current_cpu_id(), mask, kind);
}

fn broadcast_ipi_from(current: CpuId, mask: CpuMask, kind: IpiKind) {
    let current_mask = CpuMask::single(current);
    let mut remote_bits = mask.bits() & !current_mask.bits();
    while remote_bits != 0 {
        let cpu = CpuId(remote_bits.trailing_zeros() as usize);
        send_ipi_from(current, cpu, kind);
        remote_bits &= remote_bits - 1;
    }
    if mask.contains(current) {
        process_local_ipi(current, kind);
    }
}

fn process_local_ipi(cpu: CpuId, kind: IpiKind) {
    let _irq_guard = <Platform as IrqIf>::exclude_local_execution();
    let cpu_mask = CpuMask::single(cpu);
    let locked = lock_ipi_targets(cpu_mask);
    publish_ipi_pending(cpu_mask, kind);
    execute_local_ipi_action(kind);
    if acknowledge_ipi_on_cpu(cpu, kind) {
        la64_iocsr_write_u32(LA64_IOCSR_IPI_CLEAR, u32::MAX);
    }
    unlock_ipi_targets(locked);
}

fn execute_local_ipi_action(kind: IpiKind) {
    if kind == IpiKind::Membarrier {
        core::sync::atomic::fence(Ordering::SeqCst);
        #[cfg(test)]
        TEST_LOCAL_MEMBARRIER_ACTIONS.fetch_add(1, Ordering::AcqRel);
    }
}

pub(crate) fn ack_ipi(kind: IpiKind) {
    let cpu = la64_current_cpu_id();
    let locked = lock_ipi_targets(CpuMask::single(cpu));
    if acknowledge_ipi_on_cpu(cpu, kind) {
        la64_iocsr_write_u32(LA64_IOCSR_IPI_CLEAR, u32::MAX);
    }
    unlock_ipi_targets(locked);
}

pub(crate) fn clear_ipi_ack_cpus(kind: IpiKind, mask: CpuMask) {
    LA64_IPI_ACKED_CPUS[ipi_kind_index(kind)].fetch_and(!mask.bits(), Ordering::AcqRel);
}

pub(crate) fn ipi_ack_cpus(kind: IpiKind) -> CpuMask {
    CpuMask::from_bits(LA64_IPI_ACKED_CPUS[ipi_kind_index(kind)].load(Ordering::Acquire))
}

fn lock_ipi_targets(mask: CpuMask) -> u64 {
    let mut bits = mask.bits();
    let mut locked = 0;
    while bits != 0 {
        let cpu = bits.trailing_zeros() as usize;
        while LA64_IPI_STATE_LOCKS[cpu]
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let bit = 1u64 << cpu;
        locked |= bit;
        bits &= bits - 1;
    }
    locked
}

fn unlock_ipi_targets(mut bits: u64) {
    while bits != 0 {
        let cpu = bits.trailing_zeros() as usize;
        LA64_IPI_STATE_LOCKS[cpu].store(false, Ordering::Release);
        bits &= bits - 1;
    }
}

fn publish_ipi_pending(mask: CpuMask, kind: IpiKind) {
    LA64_IPI_PENDING_CPUS[ipi_kind_index(kind)].fetch_or(mask.bits(), Ordering::Release);
}

fn acknowledge_ipi_on_cpu(cpu: CpuId, kind: IpiKind) -> bool {
    let bit = CpuMask::single(cpu).bits();
    LA64_IPI_PENDING_CPUS[ipi_kind_index(kind)].fetch_and(!bit, Ordering::AcqRel);
    LA64_IPI_ACKED_CPUS[ipi_kind_index(kind)].fetch_or(bit, Ordering::AcqRel);
    !LA64_IPI_PENDING_CPUS
        .iter()
        .any(|pending| pending.load(Ordering::Acquire) & bit != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_self_and_remote_runs_local_membarrier_and_isolates_kind_state() {
        let current = CpuId(3);
        let remote = CpuId(63);
        let targets =
            CpuMask::from_bits(CpuMask::single(current).bits() | CpuMask::single(remote).bits());

        for pending in &LA64_IPI_PENDING_CPUS {
            pending.store(0, Ordering::Release);
        }
        for acked in &LA64_IPI_ACKED_CPUS {
            acked.store(0, Ordering::Release);
        }
        TEST_IPI_TRANSPORT_MASK.store(0, Ordering::Release);
        TEST_LOCAL_MEMBARRIER_ACTIONS.store(0, Ordering::Release);
        broadcast_ipi_from(current, targets, IpiKind::Membarrier);

        assert_eq!(
            TEST_IPI_TRANSPORT_MASK.load(Ordering::Acquire),
            CpuMask::single(remote).bits()
        );
        assert_eq!(TEST_LOCAL_MEMBARRIER_ACTIONS.load(Ordering::Acquire), 1);
        assert_eq!(ipi_ack_cpus(IpiKind::Membarrier), CpuMask::single(current));
        assert_eq!(ipi_ack_cpus(IpiKind::Maintenance), CpuMask::EMPTY);
        assert!(!ipi_pending_on_cpu(current, IpiKind::Membarrier));
        assert!(ipi_pending_on_cpu(remote, IpiKind::Membarrier));
        assert!(!ipi_pending_on_cpu(remote, IpiKind::Maintenance));

        let locked = lock_ipi_targets(CpuMask::single(remote));
        assert!(acknowledge_ipi_on_cpu(remote, IpiKind::Membarrier));
        unlock_ipi_targets(locked);
        assert_eq!(ipi_ack_cpus(IpiKind::Membarrier), targets);

        for pending in &LA64_IPI_PENDING_CPUS {
            pending.store(0, Ordering::Release);
        }
        for acked in &LA64_IPI_ACKED_CPUS {
            acked.store(0, Ordering::Release);
        }
        TEST_IPI_TRANSPORT_MASK.store(0, Ordering::Release);
        TEST_LOCAL_MEMBARRIER_ACTIONS.store(0, Ordering::Release);
        broadcast_ipi_from(current, targets, IpiKind::Maintenance);
        assert_eq!(TEST_LOCAL_MEMBARRIER_ACTIONS.load(Ordering::Acquire), 0);
        assert_eq!(ipi_ack_cpus(IpiKind::Maintenance), CpuMask::single(current));
        assert_eq!(ipi_ack_cpus(IpiKind::Membarrier), CpuMask::EMPTY);
        assert!(ipi_pending_on_cpu(remote, IpiKind::Maintenance));
        assert!(!ipi_pending_on_cpu(remote, IpiKind::Membarrier));
        let locked = lock_ipi_targets(CpuMask::single(remote));
        assert!(acknowledge_ipi_on_cpu(remote, IpiKind::Maintenance));
        unlock_ipi_targets(locked);
        assert_eq!(ipi_ack_cpus(IpiKind::Maintenance), targets);

        for pending in &LA64_IPI_PENDING_CPUS {
            pending.store(0, Ordering::Release);
        }
        for acked in &LA64_IPI_ACKED_CPUS {
            acked.store(0, Ordering::Release);
        }
    }

    #[test]
    fn ipi_pending_and_ack_state_are_isolated_by_cpu_and_kind() {
        let sender = CpuId(0);
        let target = CpuId(63);
        let target_mask = CpuMask::single(target);

        for pending in &LA64_IPI_PENDING_CPUS {
            pending.fetch_and(!target_mask.bits(), Ordering::AcqRel);
        }
        for acked in &LA64_IPI_ACKED_CPUS {
            acked.fetch_and(!target_mask.bits(), Ordering::AcqRel);
        }

        publish_ipi_pending(target_mask, IpiKind::Reschedule);
        publish_ipi_pending(target_mask, IpiKind::Maintenance);
        assert!(!ipi_pending_on_cpu(sender, IpiKind::Reschedule));
        assert!(!ipi_pending_on_cpu(sender, IpiKind::Maintenance));
        assert!(ipi_pending_on_cpu(target, IpiKind::Reschedule));
        assert!(ipi_pending_on_cpu(target, IpiKind::Maintenance));
        assert!(!ipi_pending_on_cpu(target, IpiKind::TlbShootdown));

        assert!(!acknowledge_ipi_on_cpu(target, IpiKind::Reschedule));
        assert!(!ipi_pending_on_cpu(target, IpiKind::Reschedule));
        assert!(ipi_pending_on_cpu(target, IpiKind::Maintenance));
        assert_eq!(ipi_ack_cpus(IpiKind::Reschedule), CpuMask::single(target));
        assert_eq!(ipi_ack_cpus(IpiKind::Maintenance), CpuMask::EMPTY);

        assert!(acknowledge_ipi_on_cpu(target, IpiKind::Maintenance));
        assert!(!ipi_pending_on_cpu(target, IpiKind::Maintenance));
        assert_eq!(ipi_ack_cpus(IpiKind::Maintenance), CpuMask::single(target));

        clear_ipi_ack_cpus(IpiKind::Reschedule, target_mask);
        clear_ipi_ack_cpus(IpiKind::Maintenance, target_mask);
    }
}
