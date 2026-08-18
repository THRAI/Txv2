//! Architectural LoongArch IOCSR IPI transport.
//!
//! Runtime IPI actions are shared by LoongArch platforms. CPU release and
//! external interrupt-controller setup remain board-specific.

#[cfg(all(test, not(target_arch = "loongarch64")))]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

#[cfg(target_arch = "loongarch64")]
use super::la64_irq_trap::la64_dbar;
use super::la64_irq_trap::{read_la64_csr, write_la64_csr};
use super::la64_percpu::la64_current_cpu_id;
use tx_hal::{CpuId, CpuMask, IpiKind};

#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_FEATURES: usize = 0x0008;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_FEATURES_IPI: u32 = 1 << 4;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_STATUS: usize = 0x1000;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_ENABLE: usize = 0x1004;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_CLEAR: usize = 0x100c;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_SEND: usize = 0x1040;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_SEND_CPU_SHIFT: usize = 16;
#[cfg(target_arch = "loongarch64")]
const LA64_IOCSR_IPI_SEND_BLOCKING: u32 = 1 << 31;

const LA64_IPI_KIND_COUNT: usize = 5;
#[cfg(target_arch = "loongarch64")]
const LA64_IPI_RUNTIME_ENABLE_MASK: u32 = 0x3e;
static LA64_IPI_PENDING: [AtomicU8; crate::LA64_MAX_BOOT_CPUS] =
    [const { AtomicU8::new(0) }; crate::LA64_MAX_BOOT_CPUS];
static LA64_IPI_ACKED: [AtomicU64; LA64_IPI_KIND_COUNT] =
    [const { AtomicU64::new(0) }; LA64_IPI_KIND_COUNT];

#[cfg(all(test, not(target_arch = "loongarch64")))]
type La64TestAfterIpiClearHook = fn(CpuId, IpiKind);

#[cfg(all(test, not(target_arch = "loongarch64")))]
static LA64_TEST_AFTER_IPI_CLEAR_HOOK: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, not(target_arch = "loongarch64")))]
pub(crate) fn install_la64_test_after_ipi_clear_hook(hook: Option<La64TestAfterIpiClearHook>) {
    LA64_TEST_AFTER_IPI_CLEAR_HOOK.store(hook.map_or(0, |hook| hook as usize), Ordering::Release);
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
fn run_la64_test_after_ipi_clear_hook(cpu: CpuId, kind: IpiKind) {
    let hook = LA64_TEST_AFTER_IPI_CLEAR_HOOK.load(Ordering::Acquire);
    if hook != 0 {
        let hook = unsafe { core::mem::transmute::<usize, La64TestAfterIpiClearHook>(hook) };
        hook(cpu, kind);
    }
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
pub(crate) fn reset_la64_ipi_state_for_test() {
    install_la64_test_after_ipi_clear_hook(None);
    for pending in &LA64_IPI_PENDING {
        pending.store(0, Ordering::Release);
    }
    clear_all_ipi_acks();
}

pub(crate) fn clear_all_ipi_acks() {
    for acked in &LA64_IPI_ACKED {
        acked.store(0, Ordering::Release);
    }
}

const fn ipi_kind_index(kind: IpiKind) -> usize {
    match kind {
        IpiKind::Reschedule => 0,
        IpiKind::TlbShootdown => 1,
        IpiKind::Membarrier => 2,
        IpiKind::Maintenance => 3,
        IpiKind::Stop => 4,
    }
}

#[cfg(not(target_arch = "loongarch64"))]
const fn ipi_kind_bit(kind: IpiKind) -> u8 {
    1u8 << ipi_kind_index(kind)
}

/// IOCSR.IPI_SEND carries a vector index, while STATUS/CLEAR use its bit.
/// Vector zero is reserved for firmware/QEMU secondary-CPU release.
#[cfg(target_arch = "loongarch64")]
const fn ipi_kind_vector(kind: IpiKind) -> u32 {
    match kind {
        IpiKind::Reschedule => 1,
        IpiKind::TlbShootdown => 2,
        IpiKind::Membarrier => 3,
        IpiKind::Maintenance => 4,
        IpiKind::Stop => 5,
    }
}

#[cfg(target_arch = "loongarch64")]
const fn ipi_kind_action(kind: IpiKind) -> u32 {
    1u32 << ipi_kind_vector(kind)
}

#[cfg(target_arch = "loongarch64")]
#[inline]
fn la64_iocsr_write_u32(addr: usize, value: u32) {
    unsafe {
        core::arch::asm!("iocsrwr.w {value}, {addr}", value = in(reg) value, addr = in(reg) addr);
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

pub(crate) fn transport_available() -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        return la64_iocsr_read_u32(LA64_IOCSR_FEATURES) & LA64_IOCSR_FEATURES_IPI != 0;
    }

    #[cfg(not(target_arch = "loongarch64"))]
    true
}

pub(crate) fn enable_ipi_wakeups() -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        if !transport_available() {
            return false;
        }
        // Admit only the five runtime actions understood by SmpIf. Firmware
        // boot-vector residue and unknown actions stay masked. Do not clear
        // status here: callers may use this operation to restore wakeups while
        // a valid runtime action is pending.
        la64_iocsr_write_u32(LA64_IOCSR_IPI_ENABLE, LA64_IPI_RUNTIME_ENABLE_MASK);
    }

    let ecfg = read_la64_csr(crate::LA64_CSR_ECFG) | crate::LA64_ESTAT_IS_IPI;
    write_la64_csr(crate::LA64_CSR_ECFG, ecfg);

    let crmd = read_la64_csr(crate::LA64_CSR_CRMD) | crate::LA64_CRMD_IE;
    write_la64_csr(crate::LA64_CSR_CRMD, crmd);
    true
}

pub(crate) fn pending_ipi(kind: IpiKind) -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        la64_iocsr_read_u32(LA64_IOCSR_IPI_STATUS) & ipi_kind_action(kind) != 0
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let cpu = la64_current_cpu_id();
        cpu.0 < LA64_IPI_PENDING.len()
            && LA64_IPI_PENDING[cpu.0].load(Ordering::Acquire) & ipi_kind_bit(kind) != 0
    }
}

pub(crate) fn send_raw_ipi_vector(target: CpuId, vector: u32) {
    if target.0 >= crate::LA64_MAX_BOOT_CPUS {
        return;
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let value = LA64_IOCSR_IPI_SEND_BLOCKING
            | ((target.0 as u32) << LA64_IOCSR_IPI_SEND_CPU_SHIFT)
            | vector;
        la64_iocsr_write_u32(LA64_IOCSR_IPI_SEND, value);
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = vector;
}

pub(crate) fn send_ipi(target: CpuId, kind: IpiKind) {
    if target == la64_current_cpu_id() || target.0 >= LA64_IPI_PENDING.len() {
        return;
    }

    #[cfg(not(target_arch = "loongarch64"))]
    LA64_IPI_PENDING[target.0].fetch_or(ipi_kind_bit(kind), Ordering::AcqRel);

    #[cfg(target_arch = "loongarch64")]
    send_raw_ipi_vector(target, ipi_kind_vector(kind));
}

pub(crate) fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
    let mut bits = mask.bits();
    while bits != 0 {
        let cpu = bits.trailing_zeros() as usize;
        send_ipi(CpuId(cpu), kind);
        bits &= bits - 1;
    }
}

pub(crate) fn ack_ipi(kind: IpiKind) {
    let cpu = la64_current_cpu_id();
    if cpu.0 >= LA64_IPI_PENDING.len() {
        return;
    }

    #[cfg(target_arch = "loongarch64")]
    {
        la64_iocsr_write_u32(LA64_IOCSR_IPI_CLEAR, ipi_kind_action(kind));
        // IOCSR writes may be posted on real Loongson interconnects. Complete
        // the clear before publishing the software ack or another same-vector
        // round could race the retiring hardware action.
        la64_dbar();
    }

    #[cfg(not(target_arch = "loongarch64"))]
    LA64_IPI_PENDING[cpu.0].fetch_and(!ipi_kind_bit(kind), Ordering::AcqRel);

    #[cfg(all(test, not(target_arch = "loongarch64")))]
    run_la64_test_after_ipi_clear_hook(cpu, kind);

    if matches!(kind, IpiKind::TlbShootdown)
        && !crate::la64_pmap::service_la64_pending_tlb_shootdown()
    {
        crate::la64_pmap::la64_invtlb_all();
    }
    LA64_IPI_ACKED[ipi_kind_index(kind)].fetch_or(CpuMask::single(cpu).bits(), Ordering::AcqRel);
}

pub(crate) fn clear_ipi_ack_cpus(kind: IpiKind, mask: CpuMask) {
    LA64_IPI_ACKED[ipi_kind_index(kind)].fetch_and(!mask.bits(), Ordering::AcqRel);
}

pub(crate) fn ipi_ack_cpus(kind: IpiKind) -> CpuMask {
    CpuMask::from_bits(LA64_IPI_ACKED[ipi_kind_index(kind)].load(Ordering::Acquire))
}
