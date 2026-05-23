use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome};
use crate::net::namespace::initial_loopback_iface;
use crate::net::protocol::{LoopbackIface, PollContext};
use crate::net::structure::SocketIdentity;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackIcmpTransferOutcome {
    pub tx_packets: usize,
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub bytes_moved: usize,
    pub source_wake_fired: bool,
    pub peer_wake_fired: bool,
}

pub fn step_process_loopback_icmp(
    source: &Cap<SocketIdentity>,
    budget: usize,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackIcmpTransferOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_icmp_on_iface(source, budget, initial_loopback_iface(), guard)
}

pub fn step_process_loopback_icmp_on_iface(
    source: &Cap<SocketIdentity>,
    budget: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackIcmpTransferOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(source_payload) = source.acquire_operational() else {
        return StepOutcome::Done(LoopbackIcmpTransferOutcome::default());
    };
    let mut ctx =
        PollContext::new_with_table(smoltcp::time::Instant::ZERO, source_payload.socket_table());
    let mut source_wake_fired = false;

    if let Some(publish) = ctx.poll_icmp_egress_one(source, iface, guard) {
        source_wake_fired = publish.publish.send_has_space;
        publish.publish();
    }

    let ingress = ctx.poll_icmp_ingress(iface, guard, budget);
    let mut peer_wake_fired = false;
    for publish in ingress.publishes {
        peer_wake_fired |= publish.publish.recv_has_data;
        publish.publish();
    }

    StepOutcome::Done(LoopbackIcmpTransferOutcome {
        tx_packets: ingress.tx_packets,
        packets_seen: ingress.packets_seen,
        sockets_touched: ingress.sockets_touched,
        bytes_moved: ingress.bytes_moved,
        source_wake_fired,
        peer_wake_fired,
    })
}
