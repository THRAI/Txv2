use alloc::vec::Vec;
use smoltcp::time::Instant;
use tx_substrate::zone::{Cap, PayloadCap};

use crate::execution::{Guard, StepOutcome};
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::packet::{PacketTxReadiness, PacketTxResult, PacketTxSink};
use crate::net::protocol::build_icmpv4_echo_request;
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_device_tx_pending_in_namespace_at(
        sink,
        initial_net_namespace_payload(),
        Instant::ZERO,
        budget,
        guard,
    )
}

pub fn step_process_device_tx_pending_at(
    sink: &dyn PacketTxSink,
    now: Instant,
    budget: DeviceTxBudget,
    guard: &Guard<'_>,
) -> StepOutcome<DeviceTxOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_device_tx_pending_in_namespace_at(
        sink,
        initial_net_namespace_payload(),
        now,
        budget,
        guard,
    )
}

pub fn step_process_device_tx_pending_in_namespace_at(
    sink: &dyn PacketTxSink,
    net_namespace: PayloadCap<NetNamespacePayload>,
    now: Instant,
    budget: DeviceTxBudget,
    guard: &Guard<'_>,
) -> StepOutcome<DeviceTxOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let mut outcome = DeviceTxOutcome::default();
    let table = net_namespace.socket_table();

    for socket in table
        .snapshot_tcp_bound(guard)
        .into_iter()
        .filter(|socket| is_tcp_connecting(socket, guard))
        .take(budget.tcp_connecting)
    {
        process_tcp_tx_socket(&socket, sink, now, guard, &mut outcome);
    }

    // Half-open inbound children (P2-S3): their SYN-ACK (initial send and
    // RTO retransmits) is queued inside smoltcp by the SYN feed, but they
    // are not in the connections table until the final ACK promotes them,
    // so the lanes above cannot see them. Walk the listeners' connecting
    // backlogs; `dispatch_segment` inside `process_tcp_tx_socket` is the
    // only gate needed (it emits nothing unless smoltcp wants to send).
    let mut half_open_seen = Vec::new();
    for listener in table
        .snapshot_tcp_listeners(guard)
        .into_iter()
        .take(budget.tcp_connecting)
    {
        let Some(listener_ident) = listener.downgrade().observe(guard) else {
            continue;
        };
        let Some(listener_payload) = listener_ident.acquire_operational() else {
            continue;
        };
        for child in listener_payload.connecting_children() {
            if !remember_socket(&mut half_open_seen, &child) {
                continue;
            }
            process_tcp_tx_socket(&child, sink, now, guard, &mut outcome);
        }
    }

    let mut tcp_connections_seen = Vec::new();
    for socket in table
        .snapshot_tcp_connections(guard)
        .into_iter()
        .filter(|socket| is_tcp_connected(socket, guard))
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
    for socket in table
        .snapshot_udp_bound(guard)
        .into_iter()
        .chain(table.snapshot_udp_connections(guard))
        .filter(|socket| is_udp_bound_or_connected(socket, guard))
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
    for socket in table
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

/// Per-socket drain bound for one device-TX pass (P2-S4). Keeps a single
/// bulk sender from monopolising the delegate round while still letting a
/// multi-MSS send queue empty in one pass instead of one-segment-per-wake.
const TCP_TX_SOCKET_DRAIN_BUDGET: usize = 16;

fn process_tcp_tx_socket(
    socket: &Cap<SocketIdentity>,
    sink: &dyn PacketTxSink,
    now: Instant,
    guard: &Guard<'_>,
    outcome: &mut DeviceTxOutcome,
) {
    // P2-S7 hardening (P1-S4 race family): the socket may be concurrently
    // close-retired; observe(guard) instead of bare Cap deref (which
    // panics on a retired slot).
    let Some(ident) = socket.downgrade().observe(guard) else {
        return;
    };
    let Some(payload) = ident.acquire_operational() else {
        return;
    };
    let Some(raw_tcp) = payload.raw_tcp_socket() else {
        return;
    };
    // P2-S4 drain loop: keep dispatching until smoltcp has nothing to send,
    // the sink backpressures, or the per-socket budget is spent. A segment
    // popped by `dispatch_segment` that the sink then refuses is recovered
    // by smoltcp's RTO (same exposure as the previous single-shot shape);
    // the readiness probe before each dispatch keeps that window small.
    let mut sent = false;
    for _ in 0..TCP_TX_SOCKET_DRAIN_BUDGET {
        if sink.readiness_at(now, guard) == PacketTxReadiness::Busy {
            outcome.tcp_busy += 1;
            break;
        }
        let Some(packet) = raw_tcp
            .dispatch_segment()
            .and_then(|segment| segment.emit_ipv4_packet())
        else {
            break;
        };

        outcome.tcp_attempted += 1;
        match sink.transmit_at(packet.as_bytes(), now, guard) {
            PacketTxResult::Accepted { frame_len } => {
                outcome.tcp_packets += 1;
                outcome.tx_bytes += frame_len;
                sent = true;
            }
            PacketTxResult::Busy => {
                outcome.tcp_busy += 1;
                break;
            }
            PacketTxResult::PendingResolution { .. } => {
                outcome.tcp_resolution_pending += 1;
                break;
            }
            PacketTxResult::Failed { .. } => {
                outcome.tcp_failed += 1;
                break;
            }
        }
    }
    if sent {
        outcome.sockets_touched += 1;
    }
}

fn process_udp_tx_socket(
    socket: &Cap<SocketIdentity>,
    sink: &dyn PacketTxSink,
    now: Instant,
    guard: &Guard<'_>,
    outcome: &mut DeviceTxOutcome,
) {
    // P2-S7 hardening (P1-S4 race family): observe(guard), no bare deref.
    let Some(ident) = socket.downgrade().observe(guard) else {
        return;
    };
    let Some(payload) = ident.acquire_operational() else {
        return;
    };
    let Some(local) = udp_local_endpoint(&payload.protocol_snapshot()) else {
        return;
    };
    if payload.peek_udp_tx_datagram().is_none() {
        return;
    }
    if sink.readiness_at(now, guard) == PacketTxReadiness::Busy {
        outcome.udp_busy += 1;
        return;
    }
    // P2-S6: pop through smoltcp dispatch so the wire source is the
    // dispatch-resolved endpoint (bound address or enqueue-time hint) —
    // sockets autobound to 0.0.0.0 must not emit src-unspecified packets.
    // The pop is destructive; a sink refusal drops the datagram (UDP is
    // best-effort and the readiness probe above keeps that window small).
    let Some(drain) = payload.take_udp_tx_datagram() else {
        return;
    };
    let packet_src = if !drain.src.is_unspecified() && drain.src.port != 0 {
        drain.src
    } else {
        local
    };
    let Some(packet) = drain.datagram.emit_ipv4_packet(packet_src) else {
        return;
    };

    outcome.udp_attempted += 1;
    match sink.transmit_at(packet.as_bytes(), now, guard) {
        PacketTxResult::Accepted { frame_len } => {
            outcome.udp_packets += 1;
            outcome.tx_bytes += frame_len;
            outcome.sockets_touched += 1;
            if drain.became_available {
                outcome.wakes_fired += ident.readiness.fire_send(SendWireSet::SPACE);
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
    // P2-S7 hardening (P1-S4 race family): observe(guard), no bare deref.
    let Some(ident) = socket.downgrade().observe(guard) else {
        return;
    };
    let Some(payload) = ident.acquire_operational() else {
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
                outcome.wakes_fired += ident.readiness.fire_send(SendWireSet::SPACE);
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

fn is_tcp_connecting(socket: &Cap<SocketIdentity>, guard: &Guard<'_>) -> bool {
    socket
        .downgrade()
        .observe(guard)
        .and_then(|ident| ident.acquire_operational())
        .is_some_and(|payload| {
            matches!(
                payload.protocol_snapshot(),
                SocketProtocol::Tcp(TcpState::Connecting { .. })
            )
        })
}

fn is_tcp_connected(socket: &Cap<SocketIdentity>, guard: &Guard<'_>) -> bool {
    socket
        .downgrade()
        .observe(guard)
        .and_then(|ident| ident.acquire_operational())
        .is_some_and(|payload| {
            matches!(
                payload.protocol_snapshot(),
                SocketProtocol::Tcp(TcpState::Connected { .. })
            )
        })
}

fn is_udp_bound_or_connected(socket: &Cap<SocketIdentity>, guard: &Guard<'_>) -> bool {
    socket
        .downgrade()
        .observe(guard)
        .and_then(|ident| ident.acquire_operational())
        .is_some_and(|payload| {
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
