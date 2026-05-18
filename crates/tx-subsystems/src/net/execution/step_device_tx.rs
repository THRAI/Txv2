use alloc::vec::Vec;
use smoltcp::time::Instant;
use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome};
use crate::net::packet::{PacketTxReadiness, PacketTxResult, PacketTxSink};
use crate::net::protocol::build_icmpv4_echo_request;
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::structure::{
    IpEndpoint, Ipv4Address, SendWireSet, SocketIdentity, SocketProtocol, TcpState, UdpInner,
};

pub const DEVICE_TX_BUDGET_DEFAULT: DeviceTxBudget = DeviceTxBudget {
    tcp_connecting: 16,
    tcp_connected: 32,
    udp_bound: 32,
    raw_icmp: 32,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceTxBudget {
    pub tcp_connecting: usize,
    pub tcp_connected: usize,
    pub udp_bound: usize,
    pub raw_icmp: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceTxOutcome {
    pub tcp_attempted: usize,
    pub tcp_packets: usize,
    pub tcp_busy: usize,
    pub tcp_resolution_pending: usize,
    pub tcp_failed: usize,
    pub udp_attempted: usize,
    pub udp_packets: usize,
    pub udp_busy: usize,
    pub udp_resolution_pending: usize,
    pub udp_failed: usize,
    pub raw_icmp_attempted: usize,
    pub raw_icmp_packets: usize,
    pub raw_icmp_busy: usize,
    pub raw_icmp_resolution_pending: usize,
    pub raw_icmp_failed: usize,
    pub tx_bytes: usize,
    pub sockets_touched: usize,
    pub wakes_fired: usize,
}

impl Default for DeviceTxBudget {
    fn default() -> Self {
        DEVICE_TX_BUDGET_DEFAULT
    }
}

impl DeviceTxOutcome {
    pub fn merge(&mut self, other: Self) {
        self.tcp_attempted += other.tcp_attempted;
        self.tcp_packets += other.tcp_packets;
        self.tcp_busy += other.tcp_busy;
        self.tcp_resolution_pending += other.tcp_resolution_pending;
        self.tcp_failed += other.tcp_failed;
        self.udp_attempted += other.udp_attempted;
        self.udp_packets += other.udp_packets;
        self.udp_busy += other.udp_busy;
        self.udp_resolution_pending += other.udp_resolution_pending;
        self.udp_failed += other.udp_failed;
        self.raw_icmp_attempted += other.raw_icmp_attempted;
        self.raw_icmp_packets += other.raw_icmp_packets;
        self.raw_icmp_busy += other.raw_icmp_busy;
        self.raw_icmp_resolution_pending += other.raw_icmp_resolution_pending;
        self.raw_icmp_failed += other.raw_icmp_failed;
        self.tx_bytes += other.tx_bytes;
        self.sockets_touched += other.sockets_touched;
        self.wakes_fired += other.wakes_fired;
    }
}

pub fn step_process_device_tx_pending(
    sink: &dyn PacketTxSink,
    budget: DeviceTxBudget,
    guard: &Guard<'_>,
) -> StepOutcome<DeviceTxOutcome> {
    step_process_device_tx_pending_at(sink, Instant::ZERO, budget, guard)
}

pub fn step_process_device_tx_pending_at(
    sink: &dyn PacketTxSink,
    now: Instant,
    budget: DeviceTxBudget,
    guard: &Guard<'_>,
) -> StepOutcome<DeviceTxOutcome> {
    let mut outcome = DeviceTxOutcome::default();

    for socket in SOCKET_TABLE
        .snapshot_tcp_bound(guard)
        .into_iter()
        .filter(is_tcp_connecting)
        .take(budget.tcp_connecting)
    {
        process_tcp_tx_socket(&socket, sink, now, guard, &mut outcome);
    }

    let mut tcp_connections_seen = Vec::new();
    for socket in SOCKET_TABLE
        .snapshot_tcp_connections(guard)
        .into_iter()
        .filter(is_tcp_connected)
    {
        if !remember_socket(&mut tcp_connections_seen, &socket) {
            continue;
        }
        if outcome.tcp_attempted >= budget.tcp_connected {
            break;
        }
        process_tcp_tx_socket(&socket, sink, now, guard, &mut outcome);
    }

    let mut udp_bound_seen = Vec::new();
    for socket in SOCKET_TABLE
        .snapshot_udp_bound(guard)
        .into_iter()
        .chain(SOCKET_TABLE.snapshot_udp_connections(guard))
        .filter(is_udp_bound_or_connected)
    {
        if !remember_socket(&mut udp_bound_seen, &socket) {
            continue;
        }
        if outcome.udp_attempted >= budget.udp_bound {
            break;
        }
        process_udp_tx_socket(&socket, sink, now, guard, &mut outcome);
    }

    let mut raw_icmp_seen = Vec::new();
    for socket in SOCKET_TABLE
        .snapshot_raw_icmp(guard)
        .into_iter()
        .filter(is_raw_icmp)
    {
        if !remember_socket(&mut raw_icmp_seen, &socket) {
            continue;
        }
        if outcome.raw_icmp_attempted >= budget.raw_icmp {
            break;
        }
        process_raw_icmp_tx_socket(&socket, sink, now, guard, &mut outcome);
    }

    StepOutcome::Done(outcome)
}

fn process_tcp_tx_socket(
    socket: &Cap<SocketIdentity>,
    sink: &dyn PacketTxSink,
    now: Instant,
    guard: &Guard<'_>,
    outcome: &mut DeviceTxOutcome,
) {
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    let Some(raw_tcp) = payload.raw_tcp_socket() else {
        return;
    };
    if sink.readiness_at(now, guard) == PacketTxReadiness::Busy {
        outcome.tcp_busy += 1;
        return;
    }
    let Some(packet) = raw_tcp
        .dispatch_segment()
        .and_then(|segment| segment.emit_ipv4_packet())
    else {
        return;
    };

    outcome.tcp_attempted += 1;
    match sink.transmit_at(packet.as_bytes(), now, guard) {
        PacketTxResult::Accepted { frame_len } => {
            outcome.tcp_packets += 1;
            outcome.tx_bytes += frame_len;
            outcome.sockets_touched += 1;
        }
        PacketTxResult::Busy => {
            outcome.tcp_busy += 1;
        }
        PacketTxResult::PendingResolution { .. } => {
            outcome.tcp_resolution_pending += 1;
        }
        PacketTxResult::Failed { .. } => {
            outcome.tcp_failed += 1;
        }
    }
}

fn process_udp_tx_socket(
    socket: &Cap<SocketIdentity>,
    sink: &dyn PacketTxSink,
    now: Instant,
    guard: &Guard<'_>,
    outcome: &mut DeviceTxOutcome,
) {
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    let Some(local) = udp_local_endpoint(&payload.protocol_snapshot()) else {
        return;
    };
    let Some(datagram) = payload.peek_udp_tx_datagram() else {
        return;
    };
    if sink.readiness_at(now, guard) == PacketTxReadiness::Busy {
        outcome.udp_busy += 1;
        return;
    }
    let Some(packet) = datagram.emit_ipv4_packet(local) else {
        return;
    };

    outcome.udp_attempted += 1;
    match sink.transmit_at(packet.as_bytes(), now, guard) {
        PacketTxResult::Accepted { frame_len } => {
            let Some(drain) = payload.commit_udp_tx_datagram_sent() else {
                outcome.udp_failed += 1;
                return;
            };
            outcome.udp_packets += 1;
            outcome.tx_bytes += frame_len;
            outcome.sockets_touched += 1;
            if drain.became_available {
                outcome.wakes_fired += socket.readiness.fire_send(SendWireSet::SPACE);
            }
        }
        PacketTxResult::Busy => {
            outcome.udp_busy += 1;
        }
        PacketTxResult::PendingResolution { .. } => {
            outcome.udp_resolution_pending += 1;
        }
        PacketTxResult::Failed { .. } => {
            outcome.udp_failed += 1;
        }
    }
}

fn process_raw_icmp_tx_socket(
    socket: &Cap<SocketIdentity>,
    sink: &dyn PacketTxSink,
    now: Instant,
    guard: &Guard<'_>,
    outcome: &mut DeviceTxOutcome,
) {
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    let Some(mut echo) = payload.peek_icmp_tx_echo() else {
        return;
    };
    if sink.readiness_at(now, guard) == PacketTxReadiness::Busy {
        outcome.raw_icmp_busy += 1;
        return;
    }
    if is_external_ipv4(echo.dst)
        && (echo.src == Ipv4Address::LOOPBACK || echo.src == Ipv4Address::UNSPECIFIED)
    {
        if let Some(src) = sink.source_ipv4() {
            echo.src = src;
        }
    }
    let packet = build_icmpv4_echo_request(&echo);

    outcome.raw_icmp_attempted += 1;
    match sink.transmit_at(packet.as_bytes(), now, guard) {
        PacketTxResult::Accepted { frame_len } => {
            let Some(drain) = payload.commit_icmp_tx_echo_sent() else {
                outcome.raw_icmp_failed += 1;
                return;
            };
            outcome.raw_icmp_packets += 1;
            outcome.tx_bytes += frame_len;
            outcome.sockets_touched += 1;
            if drain.became_available {
                outcome.wakes_fired += socket.readiness.fire_send(SendWireSet::SPACE);
            }
        }
        PacketTxResult::Busy => {
            outcome.raw_icmp_busy += 1;
        }
        PacketTxResult::PendingResolution { .. } => {
            outcome.raw_icmp_resolution_pending += 1;
        }
        PacketTxResult::Failed { .. } => {
            outcome.raw_icmp_failed += 1;
        }
    }
}

fn is_external_ipv4(addr: Ipv4Address) -> bool {
    addr != Ipv4Address::LOOPBACK && addr != Ipv4Address::BROADCAST
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

fn is_udp_bound_or_connected(socket: &Cap<SocketIdentity>) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. })
        )
    })
}

fn udp_local_endpoint(protocol: &SocketProtocol) -> Option<IpEndpoint> {
    match protocol {
        SocketProtocol::Udp(UdpInner::Bound { local })
        | SocketProtocol::Udp(UdpInner::Connected { local, .. }) => Some(*local),
        _ => None,
    }
}

fn is_raw_icmp(socket: &Cap<SocketIdentity>) -> bool {
    socket
        .acquire_operational()
        .is_some_and(|payload| matches!(payload.protocol_snapshot(), SocketProtocol::RawIcmp(_)))
}

fn remember_socket(seen: &mut Vec<u32>, socket: &Cap<SocketIdentity>) -> bool {
    let raw = socket.raw();
    if seen.contains(&raw) {
        return false;
    }
    seen.push(raw);
    true
}
