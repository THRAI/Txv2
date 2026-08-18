//! Loongson 2K1000 secondary-CPU release through the vendor U-Boot mailbox.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::la64_irq_trap::la64_dbar;
#[cfg(target_arch = "loongarch64")]
use super::la64_irq_trap::la64_timebase_frequency_hz;
#[cfg(target_arch = "loongarch64")]
use super::la64_percpu::{la64_current_cpu_id, la64_read_stable_counter};
use super::*;

const LA2K1000_CPU1_MAILBOX_BASE: usize = 0x1fe0_1100;
const LA2K1000_MAILBOX_FN: usize = 0x20;
const LA2K1000_MAILBOX_SP: usize = 0x28;
const LA2K1000_MAILBOX_TP: usize = 0x30;
const LA2K1000_MAILBOX_A1: usize = 0x38;

static LA2K1000_SECONDARY_ENTRY: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la2k1000_secondary_start() -> !;
    static __tx_ap_boot_stack_top: u8;
}

#[cfg(target_arch = "loongarch64")]
#[inline]
fn mailbox_ptr(offset: usize) -> *mut u64 {
    la64_uncached_virt(LA2K1000_CPU1_MAILBOX_BASE + offset) as *mut u64
}

#[cfg(target_arch = "loongarch64")]
fn start_secondary_cpu(cpu: CpuId, entry: SecondaryEntry) {
    assert_eq!(
        cpu,
        CpuId(1),
        "2K1000 firmware exposes only the CPU1 mailbox"
    );

    let stack_top = (&raw const __tx_ap_boot_stack_top) as usize & LA64_PHYS_ADDR_MASK;
    let trampoline = tx_la2k1000_secondary_start as *const () as usize;
    assert_eq!(trampoline & LA64_DMW_CACHED_BASE, LA64_DMW_CACHED_BASE);

    LA2K1000_SECONDARY_ENTRY.store(entry as usize, Ordering::Release);
    unsafe {
        // The vendor park loop ORs SP and its GP mailbox value into the cached
        // DMW. Its assembly actually installs the GP mailbox in $tp; txKernel
        // uses $r21 for kernel TLS, so the inherited $tp is not consumed.
        core::ptr::write_volatile(mailbox_ptr(LA2K1000_MAILBOX_SP), stack_top as u64);
        core::ptr::write_volatile(mailbox_ptr(LA2K1000_MAILBOX_TP), 0);
        core::ptr::write_volatile(mailbox_ptr(LA2K1000_MAILBOX_A1), 0);
    }
    la64_dbar();
    unsafe {
        // FN is the release flag and must be published last. U-Boot jumps to
        // it verbatim, so it is a cached high address rather than a physical
        // address.
        core::ptr::write_volatile(mailbox_ptr(LA2K1000_MAILBOX_FN), trampoline as u64);
    }
    la64_dbar();
}

#[no_mangle]
unsafe extern "C" fn tx_la2k1000_secondary_rust_entry(cpu_id: usize) -> ! {
    la64_dbar();
    let entry = LA2K1000_SECONDARY_ENTRY.load(Ordering::Acquire);
    if entry != 0 {
        let entry = unsafe { core::mem::transmute::<usize, SecondaryEntry>(entry) };
        unsafe { entry(cpu_id) }
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

#[cfg(target_arch = "loongarch64")]
fn wait_for_online_secondaries(target: CpuMask) -> usize {
    let target = target.bits();
    if target == 0 {
        return 0;
    }

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
        let current = la64_current_cpu_id();
        let target = CpuMask::from_bits(possible.bits() & !CpuMask::single(current).bits());
        if target.is_empty() {
            return 0;
        }

        crate::la64_ipi::clear_all_ipi_acks();
        let mut bits = target.bits();
        while bits != 0 {
            let cpu = bits.trailing_zeros() as usize;
            start_secondary_cpu(CpuId(cpu), entry);
            bits &= bits - 1;
        }
        wait_for_online_secondaries(target)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = (possible, entry);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_mailbox_offsets_do_not_overlap_ipi_registers() {
        assert_eq!(
            LA2K1000_CPU1_MAILBOX_BASE + LA2K1000_MAILBOX_FN,
            0x1fe0_1120
        );
        assert_eq!(
            LA2K1000_CPU1_MAILBOX_BASE + LA2K1000_MAILBOX_SP,
            0x1fe0_1128
        );
        assert_eq!(
            LA2K1000_CPU1_MAILBOX_BASE + LA2K1000_MAILBOX_TP,
            0x1fe0_1130
        );
        assert_eq!(
            LA2K1000_CPU1_MAILBOX_BASE + LA2K1000_MAILBOX_A1,
            0x1fe0_1138
        );
    }
}
