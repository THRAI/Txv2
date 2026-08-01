//! LA64 per-CPU state and low-level CPU control.
//!
//! Split out of `la64_pmap.rs` (which should own only page tables / DMW / ASID /
//! TLB): the stable counter, CPU-id resolution, kernel-TLS register (`$r21`),
//! kernel-stack install, and the `idle` wait live here because they are about
//! per-hart CPU state, not the page-table machinery.

use core::sync::atomic::Ordering;

use super::*;

pub(crate) fn la64_read_stable_counter() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let ticks: u64;
        unsafe {
            core::arch::asm!(
                "rdtime.d {ticks}, $zero",
                ticks = out(reg) ticks,
                options(nomem, nostack)
            );
        }
        ticks
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        0
    }
}

pub(crate) fn la64_current_cpu_id() -> CpuId {
    #[cfg(target_arch = "loongarch64")]
    {
        let kernel_tls = la64_read_kernel_tls();
        let csr_cpu: usize;
        unsafe {
            core::arch::asm!(
                "csrrd {cpu}, 0x20",
                cpu = out(reg) csr_cpu,
                options(nomem, nostack)
            );
        }
        let csr_cpu = csr_cpu.min(LA64_MAX_BOOT_CPUS - 1);
        if LA64_KERNEL_TLS_VALID[csr_cpu].load(Ordering::Acquire) && kernel_tls < LA64_MAX_BOOT_CPUS
        {
            return CpuId(kernel_tls);
        }
        return CpuId(csr_cpu);
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let kernel_tls = la64_read_kernel_tls();
        if kernel_tls < LA64_MAX_BOOT_CPUS {
            return CpuId(kernel_tls);
        }
        CpuId(0)
    }
}

pub(crate) fn la64_read_kernel_tls() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: usize;
        unsafe {
            core::arch::asm!(
                "move {value}, $r21",
                value = out(reg) value,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        LA64_HOST_KERNEL_TLS.load(Ordering::Acquire)
    }
}

pub(crate) fn la64_write_kernel_tls(value: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("move $r21, {value}", value = in(reg) value, options(nomem, nostack));
        let csr_cpu: usize;
        core::arch::asm!(
            "csrrd {cpu}, 0x20",
            cpu = out(reg) csr_cpu,
            options(nomem, nostack)
        );
        LA64_KERNEL_TLS_VALID[csr_cpu.min(LA64_MAX_BOOT_CPUS - 1)].store(true, Ordering::Release);
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        LA64_HOST_KERNEL_TLS.store(value, Ordering::Release);
    }
}

pub(crate) unsafe fn la64_install_kernel_stack(top: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("move $sp, {top}", top = in(reg) top.0, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = top;
}

pub(crate) fn la64_wait_for_interrupt_once() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        unsafe extern "C" {
            fn tx_la64_idle_prepared();
        }

        // The helper enables CRMD.IE and executes `idle` inside the rollback
        // region understood by the trap dispatcher. A plain Rust
        // "enable; idle" sequence loses a wake when the interrupt lands
        // between those two operations.
        tx_la64_idle_prepared();
    }

    #[cfg(not(target_arch = "loongarch64"))]
    core::hint::spin_loop();
}
