use smoltcp::time::Instant;

use crate::execution::{Guard, StepOutcome};
use crate::net::protocol::EtherIface;

pub use crate::net::protocol::ArpFlushOutcome;

pub const ARP_FLUSH_BUDGET_DEFAULT: usize = 8;

pub fn step_flush_pending_arp(
    iface: &EtherIface,
    now: Instant,
    budget: usize,
    guard: &Guard<'_>,
) -> StepOutcome<ArpFlushOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    StepOutcome::Done(iface.flush_pending_arp_at(now, budget, guard))
}
