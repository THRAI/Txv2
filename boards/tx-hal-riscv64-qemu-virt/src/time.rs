// QEMU virt DTBs normally publish `/timebase-frequency`; this fallback matches
// qemu's virt default and is used only when the firmware DTB is absent or lacks
// the property.
pub(crate) const QEMU_VIRT_FALLBACK_TIMEBASE_HZ: u64 = 10_000_000;

pub(crate) fn read_ns(frequency_hz: u64) -> u64 {
    tx_hal::time::ticks_to_ns(read_time_ticks(), frequency_hz)
}

pub(crate) fn set_deadline_ns(deadline_ns: u64, frequency_hz: u64) {
    crate::sbi::sbi_set_timer(tx_hal::time::deadline_ns_to_ticks(
        deadline_ns,
        frequency_hz,
    ));
}

pub(crate) fn cancel_deadline() {
    crate::sbi::sbi_set_timer(u64::MAX);
}

pub(crate) fn enable_timer_wakeups() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        let stie = 1usize << 5;
        core::arch::asm!(
            "csrs sie, {stie}",
            "csrsi sstatus, 2",
            stie = in(reg) stie,
            options(nomem, nostack)
        );
    }
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn read_time_ticks() -> u64 {
    let ticks: u64;
    unsafe {
        core::arch::asm!("rdtime {ticks}", ticks = out(reg) ticks, options(nomem, nostack));
    }
    ticks
}

#[cfg(not(target_arch = "riscv64"))]
pub(crate) fn read_time_ticks() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use tx_hal::time::{deadline_ns_to_ticks, ticks_to_ns};

    #[test]
    fn ticks_to_ns_uses_wide_math_and_saturates() {
        assert_eq!(ticks_to_ns(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(ticks_to_ns(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn deadline_ns_to_ticks_rounds_up_to_avoid_early_deadlines() {
        assert_eq!(deadline_ns_to_ticks(1, 3), 1);
        assert_eq!(deadline_ns_to_ticks(999_999_999, 10_000_000), 10_000_000);
        assert_eq!(deadline_ns_to_ticks(1_000_000_000, 10_000_000), 10_000_000);
        assert_eq!(deadline_ns_to_ticks(u64::MAX, u64::MAX), u64::MAX);
    }
}
