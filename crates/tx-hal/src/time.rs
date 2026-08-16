const NANOS_PER_SECOND: u128 = 1_000_000_000;
const NANOS_PER_SECOND_U64: u64 = 1_000_000_000;

pub fn ticks_to_ns(ticks: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }

    // Keep the exact 1 GHz identity case branch-free for platforms whose
    // architectural counter already advances in nanoseconds. Other common
    // frequencies, including QEMU LA64's 100 MHz timer, use the exact native-
    // width divisibility paths below.
    if frequency_hz == NANOS_PER_SECOND_U64 {
        return ticks;
    }

    // Most supported boards expose a timebase that divides one second
    // exactly (QEMU virt uses 10 MHz). Keep that overwhelmingly common path
    // in native-width arithmetic: the generic u128 division lowers to the
    // very expensive `__udivti3` helper on RV64.
    if NANOS_PER_SECOND_U64.is_multiple_of(frequency_hz) {
        let nanos_per_tick = NANOS_PER_SECOND_U64 / frequency_hz;
        return ticks.saturating_mul(nanos_per_tick);
    }
    if frequency_hz.is_multiple_of(NANOS_PER_SECOND_U64) {
        let ticks_per_nano = frequency_hz / NANOS_PER_SECOND_U64;
        return ticks / ticks_per_nano;
    }

    let frequency = u128::from(frequency_hz);
    let ns = u128::from(ticks).saturating_mul(NANOS_PER_SECOND) / frequency;
    ns.min(u128::from(u64::MAX)) as u64
}

pub fn deadline_ns_to_ticks(deadline_ns: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return u64::MAX;
    }

    if frequency_hz == NANOS_PER_SECOND_U64 {
        return deadline_ns;
    }

    if NANOS_PER_SECOND_U64.is_multiple_of(frequency_hz) {
        let nanos_per_tick = NANOS_PER_SECOND_U64 / frequency_hz;
        let whole = deadline_ns / nanos_per_tick;
        return whole.saturating_add(u64::from(deadline_ns % nanos_per_tick != 0));
    }
    if frequency_hz.is_multiple_of(NANOS_PER_SECOND_U64) {
        let ticks_per_nano = frequency_hz / NANOS_PER_SECOND_U64;
        return deadline_ns.saturating_mul(ticks_per_nano);
    }

    let frequency = u128::from(frequency_hz);
    let numerator = u128::from(deadline_ns).saturating_mul(frequency);
    let ticks = numerator.saturating_add(NANOS_PER_SECOND - 1) / NANOS_PER_SECOND;
    ticks.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::{deadline_ns_to_ticks, ticks_to_ns};

    #[test]
    fn qemu_ten_mhz_uses_exact_native_width_conversion() {
        assert_eq!(ticks_to_ns(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(deadline_ns_to_ticks(1_000_000_000, 10_000_000), 10_000_000);
        assert_eq!(deadline_ns_to_ticks(101, 10_000_000), 2);
    }

    #[test]
    fn sub_nanosecond_timebase_uses_exact_native_width_conversion() {
        assert_eq!(ticks_to_ns(2_000_000_000, 2_000_000_000), 1_000_000_000);
        assert_eq!(deadline_ns_to_ticks(7, 2_000_000_000), 14);
    }

    #[test]
    fn one_ghz_timebase_is_identity() {
        assert_eq!(ticks_to_ns(1_234_567_890, 1_000_000_000), 1_234_567_890);
        assert_eq!(
            deadline_ns_to_ticks(1_234_567_890, 1_000_000_000),
            1_234_567_890
        );
    }

    #[test]
    fn fractional_timebase_preserves_generic_rounding() {
        assert_eq!(ticks_to_ns(32_768, 32_768), 1_000_000_000);
        assert_eq!(deadline_ns_to_ticks(1, 32_768), 1);
        assert_eq!(deadline_ns_to_ticks(1_000_000_000, 32_768), 32_768);
    }

    #[test]
    fn conversions_preserve_zero_frequency_and_saturation_contracts() {
        assert_eq!(ticks_to_ns(123, 0), 0);
        assert_eq!(deadline_ns_to_ticks(123, 0), u64::MAX);
        assert_eq!(ticks_to_ns(u64::MAX, 1), u64::MAX);
        assert_eq!(deadline_ns_to_ticks(u64::MAX, 2_000_000_000), u64::MAX);
    }
}
