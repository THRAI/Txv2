use alloc::vec::Vec;
use smoltcp::time::Instant;
use tx_substrate::zone::{Cap, PayloadCap};

use crate::execution::{Guard, StepOutcome};
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::protocol::LoopbackIface;
use crate::net::structure::{Ipv4Address, SocketIdentity, SocketProtocol, TcpState, UdpInner};

use super::{
    step_process_loopback_icmp_on_iface, step_process_loopback_tcp,
    step_process_loopback_udp_on_iface, step_tcp_loopback_handshake_on_iface,
};

pub const LOOPBACK_POLL_BUDGET_DEFAULT: LoopbackPollBudget = LoopbackPollBudget {
    tcp_connecting: 16,
    tcp_connected: 32,
    udp_bound: 32,
    raw_icmp: 32,
    packet_budget: 32,
    tcp_transfer_bytes: 64 * 1024,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopbackPollBudget {
    pub tcp_connecting: usize,
    pub tcp_connected: usize,
    pub udp_bound: usize,
    pub raw_icmp: usize,
    pub packet_budget: usize,
    pub tcp_transfer_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackPendingOutcome {
    pub tcp_connect_attempted: usize,
    pub tcp_connected: usize,
    pub tcp_connect_failed: usize,
    pub tcp_transfer_attempted: usize,
    pub tcp_bytes_moved: usize,
    pub tcp_transfer_failed: usize,
    pub udp_transfer_attempted: usize,
    pub udp_bytes_moved: usize,
    pub udp_transfer_failed: usize,
    pub icmp_transfer_attempted: usize,
    pub icmp_bytes_moved: usize,
    pub icmp_transfer_failed: usize,
    pub tx_packets: usize,
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub wakes_fired: usize,
}

impl Default for LoopbackPollBudget {
    fn default() -> Self {
        LOOPBACK_POLL_BUDGET_DEFAULT
    }
}

impl LoopbackPendingOutcome {
    pub fn made_progress(&self) -> bool {
        self.tcp_connected != 0
            || self.tcp_bytes_moved != 0
            || self.udp_bytes_moved != 0
            || self.icmp_bytes_moved != 0
            || self.tx_packets != 0
            || self.packets_seen != 0
    }

    pub fn merge(&mut self, other: Self) {
        self.tcp_connect_attempted += other.tcp_connect_attempted;
        self.tcp_connected += other.tcp_connected;
        self.tcp_connect_failed += other.tcp_connect_failed;
        self.tcp_transfer_attempted += other.tcp_transfer_attempted;
        self.tcp_bytes_moved += other.tcp_bytes_moved;
        self.tcp_transfer_failed += other.tcp_transfer_failed;
        self.udp_transfer_attempted += other.udp_transfer_attempted;
        self.udp_bytes_moved += other.udp_bytes_moved;
        self.udp_transfer_failed += other.udp_transfer_failed;
        self.icmp_transfer_attempted += other.icmp_transfer_attempted;
        self.icmp_bytes_moved += other.icmp_bytes_moved;
        self.icmp_transfer_failed += other.icmp_transfer_failed;
        self.tx_packets += other.tx_packets;
        self.packets_seen += other.packets_seen;
        self.sockets_touched += other.sockets_touched;
        self.wakes_fired += other.wakes_fired;
    }
}

pub fn step_process_loopback_pending(
    now: Instant,
    iface: &LoopbackIface,
    budget: LoopbackPollBudget,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackPendingOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_pending_in_namespace(
        now,
        initial_net_namespace_payload(),
        iface,
        budget,
        guard,
    )
}

pub fn step_process_loopback_pending_in_namespace(
    _now: Instant,
    net_namespace: PayloadCap<NetNamespacePayload>,
    iface: &LoopbackIface,
    budget: LoopbackPollBudget,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackPendingOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let mut outcome = LoopbackPendingOutcome::default();
    let table = net_namespace.socket_table();

    for socket in table
        .snapshot_tcp_bound(guard)
        .into_iter()
        .filter(is_tcp_connecting)
        .take(budget.tcp_connecting)
    {
        outcome.tcp_connect_attempted += 1;
        match step_tcp_loopback_handshake_on_iface(&socket, iface, guard) {
            StepOutcome::Done(connect) => {
                outcome.tcp_connected += 1;
                outcome.tx_packets += connect.handshake.tx_packets;
                outcome.packets_seen += connect.handshake.packets_seen;
                outcome.sockets_touched += connect.handshake.sockets_touched;
                outcome.wakes_fired += connect.wakes_fired;
            }
            StepOutcome::Err(_) => outcome.tcp_connect_failed += 1,
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                outcome.tcp_connect_failed += 1
            }
        }
    }

    let mut tcp_connections_seen = Vec::new();
    for socket in table
        .snapshot_tcp_connections(guard)
        .into_iter()
        .filter(is_tcp_connected)
    {
        if !remember_socket(&mut tcp_connections_seen, &socket) {
            continue;
        }
        if outcome.tcp_transfer_attempted >= budget.tcp_connected {
            break;
        }

        outcome.tcp_transfer_attempted += 1;
        match step_process_loopback_tcp(&socket, budget.tcp_transfer_bytes, iface, guard) {
            StepOutcome::Done(transfer) => {
                outcome.tcp_bytes_moved += transfer.bytes_moved;
                outcome.tx_packets += transfer.tx_packets;
                outcome.packets_seen += transfer.packets_seen;
                outcome.sockets_touched += transfer.sockets_touched;
                outcome.wakes_fired +=
                    usize::from(transfer.source_wake_fired) + usize::from(transfer.peer_wake_fired);
                outcome.wakes_fired += usize::from(transfer.source_recv_broken)
                    + usize::from(transfer.source_send_broken)
                    + usize::from(transfer.peer_recv_broken)
                    + usize::from(transfer.peer_send_broken);
            }
            StepOutcome::Err(_) => outcome.tcp_transfer_failed += 1,
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                outcome.tcp_transfer_failed += 1
            }
        }
    }

    let mut udp_bound_seen = Vec::new();
    for socket in table
        .snapshot_udp_bound(guard)
        .into_iter()
        .chain(table.snapshot_udp_connections(guard))
        .filter(|socket| has_udp_loopback_tx_pending(socket, iface))
    {
        if !remember_socket(&mut udp_bound_seen, &socket) {
            continue;
        }
        if outcome.udp_transfer_attempted >= budget.udp_bound {
            break;
        }

        outcome.udp_transfer_attempted += 1;
        match step_process_loopback_udp_on_iface(&socket, budget.packet_budget, iface, guard) {
            StepOutcome::Done(transfer) => {
                outcome.udp_bytes_moved += transfer.bytes_moved;
                outcome.tx_packets += transfer.tx_packets;
                outcome.packets_seen += transfer.packets_seen;
                outcome.sockets_touched += transfer.sockets_touched;
                outcome.wakes_fired +=
                    usize::from(transfer.source_wake_fired) + usize::from(transfer.peer_wake_fired);
            }
            StepOutcome::Err(_) => outcome.udp_transfer_failed += 1,
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                outcome.udp_transfer_failed += 1
            }
        }
    }

    let mut raw_icmp_seen = Vec::new();
    for socket in table
        .snapshot_raw_icmp(guard)
        .into_iter()
        .filter(|socket| is_raw_icmp_loopback_pending(socket, iface))
    {
        if !remember_socket(&mut raw_icmp_seen, &socket) {
            continue;
        }
        if outcome.icmp_transfer_attempted >= budget.raw_icmp {
            break;
        }

        outcome.icmp_transfer_attempted += 1;
        match step_process_loopback_icmp_on_iface(&socket, budget.packet_budget, iface, guard) {
            StepOutcome::Done(transfer) => {
                outcome.icmp_bytes_moved += transfer.bytes_moved;
                outcome.tx_packets += transfer.tx_packets;
                outcome.packets_seen += transfer.packets_seen;
                outcome.sockets_touched += transfer.sockets_touched;
                outcome.wakes_fired +=
                    usize::from(transfer.source_wake_fired) + usize::from(transfer.peer_wake_fired);
            }
            StepOutcome::Err(_) => outcome.icmp_transfer_failed += 1,
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                outcome.icmp_transfer_failed += 1
            }
        }
    }

    StepOutcome::Done(outcome)
}

pub fn step_process_loopback_pending_zero(
    iface: &LoopbackIface,
    budget: LoopbackPollBudget,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackPendingOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_pending(Instant::ZERO, iface, budget, guard)
}

fn is_tcp_connecting(socket: &Cap<SocketIdentity>) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connecting { .. })
        )
    })
}

fn is_tcp_connected(socket: &Cap<SocketIdentity>) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connected { .. })
        )
    })
}

fn has_udp_loopback_tx_pending(socket: &Cap<SocketIdentity>, iface: &LoopbackIface) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. })
        ) && payload
            .peek_udp_tx_datagram()
            .is_some_and(|datagram| datagram.dst.addr == iface.local_ipv4())
    })
}

fn is_raw_icmp_loopback_pending(socket: &Cap<SocketIdentity>, iface: &LoopbackIface) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(payload.protocol_snapshot(), SocketProtocol::RawIcmp(_))
            && payload.peek_icmp_tx_echo().is_some_and(|packet| {
                packet.dst == iface.local_ipv4() || packet.dst == Ipv4Address::BROADCAST
            })
    })
}

fn remember_socket(seen: &mut Vec<u32>, socket: &Cap<SocketIdentity>) -> bool {
    let raw = socket.raw();
    if seen.contains(&raw) {
        return false;
    }
    seen.push(raw);
    true
}
