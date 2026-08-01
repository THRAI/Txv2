use smoltcp::time::Instant;
use tx_reactor::wait::{Channel, Mask, WaitOutcome, WaitProtocol};

use super::net_delegate_kick_tick;

/// Convert a smoltcp-relative deadline into the reactor's absolute
/// nanosecond clock domain.
pub fn smoltcp_instant_to_reactor_deadline_ns(
    base: Instant,
    deadline: Instant,
    base_ns: u64,
) -> Option<u64> {
    let delta_micros = deadline
        .total_micros()
        .saturating_sub(base.total_micros())
        .max(0);
    let delta_ns = u64::try_from(delta_micros).ok()?.checked_mul(1_000)?;
    base_ns.checked_add(delta_ns)
}

/// Arm a reactor timeout that publishes a delegate TICK when it expires.
pub async fn net_delegate_wait_tick_deadline(
    timer_channel: Channel,
    deadline_ns: u64,
) -> WaitOutcome {
    let outcome = timer_channel
        .wait_event(
            Mask::from_bits(0),
            WaitProtocol::InterruptibleTimeout(deadline_ns),
            || false,
        )
        .await;
    if matches!(outcome, WaitOutcome::TimedOut) {
        net_delegate_kick_tick();
    }
    outcome
}
