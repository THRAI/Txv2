const NANOS_PER_SECOND: u128 = 1_000_000_000;

pub fn ticks_to_ns(ticks: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }

    let ns = u128::from(ticks).saturating_mul(NANOS_PER_SECOND) / u128::from(frequency_hz);
    ns.min(u128::from(u64::MAX)) as u64
}

pub fn deadline_ns_to_ticks(deadline_ns: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return u64::MAX;
    }

    let numerator = u128::from(deadline_ns).saturating_mul(u128::from(frequency_hz));
    let ticks = numerator.saturating_add(NANOS_PER_SECOND - 1) / NANOS_PER_SECOND;
    ticks.min(u128::from(u64::MAX)) as u64
}
