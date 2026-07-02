use smoltcp::time::Instant;
use tx_substrate::zone::PayloadCap;

use crate::execution::{Guard, StepOutcome};
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::packet::{
    NetworkPublish, PacketDispatch, PacketSource, TcpPacketEvent, UdpPacketEvent,
};
use crate::net::protocol::{Icmpv4Event, LoopbackIface};
use crate::net::structure::registry;
use crate::net::structure::table::SocketTable;
use crate::net::structure::{
    ConnectionKey, Ipv4Address, RecvWireSet, SocketAcceptEntry, SocketIdentity, SocketProtocol,
    TcpBacklogRetransmitOutcome,
};
use tx_substrate::zone::Cap;

use super::step_tcp_backlog_cleanup::{cleanup_tcp_backlog_for_listener, TcpBacklogCleanupOutcome};
use super::step_tcp_backlog_poll::poll_tcp_backlog_for_listener_loopback;

pub const NET_EVENT_BUDGET: usize = 32;
pub const NET_BACKLOG_SCAN_BUDGET: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkStepOutcome {
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub wakes_fired: usize,
    pub backlog: NetworkBacklogTickOutcome,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkBacklogTickOutcome {
    pub listeners_seen: usize,
    pub listeners_touched: usize,
    pub half_open_scanned: usize,
    pub half_open_retransmitted: usize,
    pub half_open_expired: usize,
    pub half_open_failed: usize,
    pub remaining_connecting: usize,
    pub next_deadline: Option<Instant>,
}

pub fn step_process_network_events(
    source: &dyn PacketSource,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkStepOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_network_events_in_namespace_at(
        source,
        initial_net_namespace_payload(),
        Instant::ZERO,
        guard,
    )
}

pub fn step_process_network_events_at(
    source: &dyn PacketSource,
    now: Instant,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkStepOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_network_events_in_namespace_at(source, initial_net_namespace_payload(), now, guard)
}

pub fn step_process_network_events_in_namespace_at(
    source: &dyn PacketSource,
    net_namespace: PayloadCap<NetNamespacePayload>,
    now: Instant,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkStepOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let mut outcome = NetworkStepOutcome::default();
    let table = net_namespace.socket_table();

    for _ in 0..NET_EVENT_BUDGET {
        let Some(packet) = source.next_packet_at(now, guard) else {
            break;
        };
        outcome.packets_seen += 1;

        match packet {
            PacketDispatch::Tcp(event) => {
                if let Some((socket, publish)) =
                    process_tcp_event(table, &net_namespace, event, guard)
                {
                    outcome.sockets_touched += 1;
                    outcome.wakes_fired += publish.publish_to(&socket);
                }
            }
            PacketDispatch::Udp(event) => {
                if let Some((socket, publish)) = process_udp_event(table, event, guard) {
                    outcome.sockets_touched += 1;
                    outcome.wakes_fired += publish.publish_to(&socket);
                }
            }
            PacketDispatch::Icmp(event) => {
                if let Some((socket, publish)) = process_icmp_event(table, event, guard) {
                    outcome.sockets_touched += 1;
                    outcome.wakes_fired += publish.publish_to(&socket);
                }
            }
            PacketDispatch::Unsupported | PacketDispatch::Malformed => {}
        }
    }

    outcome.backlog = process_tcp_backlog_tick_in_namespace(now, table, guard);
    StepOutcome::Done(outcome)
}

pub fn step_process_network_tick(
    now: Instant,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkBacklogTickOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    StepOutcome::Done(process_tcp_backlog_tick_in_namespace(
        now,
        initial_net_namespace_payload().socket_table(),
        guard,
    ))
}

pub fn step_process_network_tick_in_namespace(
    now: Instant,
    net_namespace: PayloadCap<NetNamespacePayload>,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkBacklogTickOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    StepOutcome::Done(process_tcp_backlog_tick_in_namespace(
        now,
        net_namespace.socket_table(),
        guard,
    ))
}

pub fn step_process_network_tick_loopback(
    now: Instant,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkBacklogTickOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    StepOutcome::Done(process_tcp_backlog_tick_loopback_in_namespace(
        now,
        initial_net_namespace_payload().socket_table(),
        iface,
        guard,
    ))
}

pub fn step_process_network_tick_loopback_in_namespace(
    now: Instant,
    net_namespace: PayloadCap<NetNamespacePayload>,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkBacklogTickOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    StepOutcome::Done(process_tcp_backlog_tick_loopback_in_namespace(
        now,
        net_namespace.socket_table(),
        iface,
        guard,
    ))
}

fn process_tcp_backlog_tick_in_namespace(
    now: Instant,
    table: &SocketTable,
    guard: &Guard<'_>,
) -> NetworkBacklogTickOutcome {
    let mut outcome = NetworkBacklogTickOutcome::default();
    let listeners = table.snapshot_tcp_listeners(guard);

    for listener in listeners.into_iter().take(NET_BACKLOG_SCAN_BUDGET) {
        outcome.listeners_seen += 1;
        let cleanup = match cleanup_tcp_backlog_for_listener(&listener, now) {
            Ok(cleanup) => cleanup,
            Err(_) => continue,
        };
        record_backlog_cleanup(&mut outcome, cleanup);

        let Some(payload) = listener.acquire_operational() else {
            continue;
        };
        if let Some(deadline) = payload.tcp_backlog_next_deadline() {
            outcome.next_deadline = earliest_deadline(outcome.next_deadline, deadline);
        }
    }

    outcome
}

fn process_tcp_backlog_tick_loopback_in_namespace(
    now: Instant,
    table: &SocketTable,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> NetworkBacklogTickOutcome {
    let mut outcome = NetworkBacklogTickOutcome::default();
    let listeners = table.snapshot_tcp_listeners(guard);

    for listener in listeners.into_iter().take(NET_BACKLOG_SCAN_BUDGET) {
        outcome.listeners_seen += 1;
        let poll = match poll_tcp_backlog_for_listener_loopback(&listener, now, iface) {
            Ok(poll) => poll,
            Err(_) => continue,
        };
        record_backlog_retransmit(&mut outcome, poll);
    }

    outcome
}

fn record_backlog_cleanup(
    outcome: &mut NetworkBacklogTickOutcome,
    cleanup: TcpBacklogCleanupOutcome,
) {
    if cleanup.scanned != 0 {
        outcome.listeners_touched += 1;
    }
    outcome.half_open_scanned += cleanup.scanned;
    outcome.half_open_expired += cleanup.expired;
    outcome.half_open_failed += cleanup.failed;
    outcome.remaining_connecting += cleanup.remaining_connecting;
}

fn record_backlog_retransmit(
    outcome: &mut NetworkBacklogTickOutcome,
    poll: TcpBacklogRetransmitOutcome,
) {
    if poll.scanned != 0 {
        outcome.listeners_touched += 1;
    }
    outcome.half_open_scanned += poll.scanned;
    outcome.half_open_retransmitted += poll.retransmitted;
    outcome.half_open_expired += poll.expired;
    outcome.half_open_failed += poll.failed;
    outcome.remaining_connecting += poll.remaining_connecting;
    if let Some(deadline) = poll.next_deadline {
        outcome.next_deadline = earliest_deadline(outcome.next_deadline, deadline);
    }
}

fn earliest_deadline(current: Option<Instant>, candidate: Instant) -> Option<Instant> {
    Some(current.map_or(candidate, |current| current.min(candidate)))
}

fn process_tcp_event(
    table: &SocketTable,
    net_namespace: &PayloadCap<NetNamespacePayload>,
    event: TcpPacketEvent,
    guard: &Guard<'_>,
) -> Option<(Cap<SocketIdentity>, NetworkPublish)> {
    let key = ConnectionKey::new(event.dst, event.src);
    if let Some(socket) = table.lookup_tcp_connection(key, guard) {
        let payload = socket.acquire_operational()?;
        // Established-connection RX feeds smoltcp: seq/ack/checksum are the
        // state machine's verdict, not hand bookkeeping. Events without a
        // parsed segment (hand-built) cannot enter an established
        // connection and are dropped.
        let segment = event.segment.as_ref()?;
        let raw = payload.raw_tcp_socket()?;
        let bits = raw.process_segment(segment);
        payload.refresh_io_from_raw();
        let publish = NetworkPublish {
            recv_has_data: bits.recv_readable
                || socket.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() == 0
                    && raw.recv_available() > 0,
            send_has_space: bits.send_writable,
            recv_broken: bits.broken || bits.recv_closed,
            send_broken: bits.broken || bits.send_closed,
            urgent: event.urgent,
            ..NetworkPublish::none()
        };
        return Some((socket, publish));
    }

    if event.flags.syn && !event.flags.ack {
        let socket = table.lookup_tcp_listener_endpoint(event.dst, guard)?;
        let payload = socket.acquire_operational()?;
        let options = payload.with_options(Clone::clone);
        let child = registry::create_connected_stream_for_accept_in_namespace(
            event.dst,
            event.src,
            options,
            net_namespace.clone(),
        )
        .ok()?;
        table.insert_tcp_connection(key, child.clone()).ok()?;
        let entry = SocketAcceptEntry {
            child,
            local: event.dst,
            peer: event.src,
            unix_peer: None,
        };
        let mut publish = NetworkPublish::none();
        if payload.enqueue_accept_entry(entry)? {
            publish.accept_has_pending = true;
        }
        return Some((socket, publish));
    }

    None
}

fn process_icmp_event(
    table: &SocketTable,
    event: Icmpv4Event,
    guard: &Guard<'_>,
) -> Option<(Cap<SocketIdentity>, NetworkPublish)> {
    let Icmpv4Event::EchoReply(reply) = event else {
        return None;
    };

    for socket in table.snapshot_raw_icmp(guard) {
        let payload = socket.acquire_operational()?;
        if !raw_icmp_accepts_reply(&payload.protocol_snapshot(), reply.dst) {
            continue;
        }

        let mut publish = NetworkPublish::none();
        if payload.record_icmp_recv_echo_reply(reply.clone()) {
            publish.recv_has_data = true;
        }
        if publish.has_any() {
            return Some((socket, publish));
        }
    }

    None
}

fn raw_icmp_accepts_reply(protocol: &SocketProtocol, dst: Ipv4Address) -> bool {
    match protocol {
        SocketProtocol::RawIcmp(state) => state.accepts_ipv4_reply_to(dst),
        _ => false,
    }
}

fn process_udp_event(
    table: &SocketTable,
    event: UdpPacketEvent,
    guard: &Guard<'_>,
) -> Option<(Cap<SocketIdentity>, NetworkPublish)> {
    let socket = table.lookup_udp_ingress(event.src, event.dst, guard)?;
    let payload = socket.acquire_operational()?;
    let mut publish = NetworkPublish::none();
    if payload.record_recv_payload(event.src, event.dst, event.payload) {
        publish.recv_has_data = true;
    }
    Some((socket, publish))
}
