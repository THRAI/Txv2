use alloc::vec::Vec;
use smoltcp::time::{Duration, Instant};
use tx_substrate::zone::PayloadCap;

use crate::execution::{Guard, StepOutcome};
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::packet::{
    NetworkPublish, NetworkPublishTarget, PacketDispatch, PacketSource, TcpPacketEvent,
    UdpPacketEvent,
};
use crate::net::protocol::{
    is_first_syn, listener_accepts_incoming, promote_connected_stream_and_publish_accept,
    Icmpv4Event, LoopbackIface, SmoltcpTcpSegment,
};
use crate::net::structure::registry;
use crate::net::structure::table::SocketTable;
use crate::net::structure::{
    ConnectionKey, Ipv4Address, RecvWireSet, SocketIdentity, SocketKind, SocketProtocol,
    TcpBacklogEntry, TcpBacklogRetransmitOutcome, TcpState, TCP_BACKLOG_TIMEOUT_STAGING_MILLIS,
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
                if let Some(targets) = process_tcp_event(table, &net_namespace, event, now, guard) {
                    outcome.sockets_touched += 1;
                    for target in targets {
                        outcome.wakes_fired += target.publish();
                    }
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
    _net_namespace: &PayloadCap<NetNamespacePayload>,
    event: TcpPacketEvent,
    now: Instant,
    guard: &Guard<'_>,
) -> Option<Vec<NetworkPublishTarget>> {
    let key = ConnectionKey::new(event.dst, event.src);
    if let Some(socket) = table.lookup_tcp_connection(key, guard) {
        let payload = socket.acquire_operational()?;
        // Established-connection RX feeds smoltcp: seq/ack/checksum are the
        // state machine's verdict, not hand bookkeeping. Events without a
        // parsed segment (hand-built) cannot enter an established
        // connection and are dropped.
        let segment = event.segment.as_ref()?;
        return Some(feed_tcp_segment(
            table,
            &socket,
            &payload,
            segment,
            event.urgent,
            guard,
        ));
    }

    // No connection matched: only checksum-verified parsed segments may
    // participate in handshakes (hand-built events cannot).
    let segment = event.segment.as_ref()?;

    // P2-S3: real inbound handshake (mirror of the loopback
    // `process_first_syn_for_listener` flow). The child is NOT inserted
    // into the connections table here — it lives in the listener's
    // connecting backlog until the final ACK promotes it (loopback
    // parity); mid-handshake segments route via the backlog below.
    let listener = table.lookup_tcp_listener_dual_stack_endpoint(event.dst, guard)?;
    let listener_payload = listener.acquire_operational()?;
    if !listener_accepts_incoming(&listener_payload, event.dst) {
        return None;
    }

    if let Some(child) = listener_payload.connecting_child(event.dst, event.src) {
        // Half-open child exists: the final ACK completes the handshake
        // (feed → connected edge → accept promotion); a retransmitted SYN
        // re-queues the SYN-ACK inside smoltcp. Either way, feed it.
        let child_payload = child.acquire_operational()?;
        return Some(feed_tcp_segment(
            table,
            &child,
            &child_payload,
            segment,
            event.urgent,
            guard,
        ));
    }

    if !is_first_syn(segment) {
        return None;
    }

    let options = listener_payload.with_options(Clone::clone);
    let child = registry::create_socket_in_namespace_with_family(
        SocketKind::Tcp,
        listener_payload.family(),
        options,
        listener_payload.net_namespace(),
    )
    .ok()?;
    let child_payload = child.acquire_operational()?;
    child_payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Tcp(TcpState::Connecting {
            local: event.dst,
            remote: event.src,
        });
    });
    child_payload.raw_tcp_socket()?.listen_endpoint(event.dst).ok()?;
    // Backlog full ⇒ enqueue fails ⇒ drop the SYN (Linux semantics: the
    // client retries; the just-created child is reclaimed with its Cap).
    listener_payload.enqueue_connecting_entry(TcpBacklogEntry {
        child: child.clone(),
        local: event.dst,
        peer: event.src,
        created_at: now,
        deadline: now + Duration::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS as u64),
        attempts: 1,
    })?;
    // Feed the SYN: smoltcp moves to SynReceived and queues the SYN-ACK.
    // The device-TX half-open lane emits it (and its RTO retransmits).
    Some(feed_tcp_segment(
        table,
        &child,
        &child_payload,
        segment,
        event.urgent,
        guard,
    ))
}

/// Feed one checksum-verified segment into a socket's smoltcp state
/// machine and derive the publish set. Shared by the established branch,
/// the half-open backlog branch, and the first-SYN feed.
fn feed_tcp_segment(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    payload: &crate::net::structure::SocketOperationalEvidence,
    segment: &SmoltcpTcpSegment,
    urgent: bool,
    guard: &Guard<'_>,
) -> Vec<NetworkPublishTarget> {
    let Some(raw) = payload.raw_tcp_socket() else {
        return Vec::new();
    };
    let bits = raw.process_segment(segment);
    payload.refresh_io_from_raw();

    let mut publishes = Vec::new();
    if bits.connected {
        // Inbound child: flip Connecting→Connected, register the
        // connection in the table, move the backlog entry to the accept
        // queue, and publish accept-readiness to the listener. Outbound
        // client: the enum flip still happens inside, but no listener
        // matches its ephemeral local port, so this returns None and the
        // SPACE publish below resumes the parked connect().
        if let Some(accept_publish) =
            promote_connected_stream_and_publish_accept(table, socket, payload, guard)
        {
            publishes.push(accept_publish);
        }
    }

    let publish = NetworkPublish {
        recv_has_data: bits.recv_readable
            || socket.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() == 0
                && raw.recv_available() > 0,
        send_has_space: bits.connected || bits.send_writable,
        recv_broken: bits.broken || bits.recv_closed,
        send_broken: bits.broken || bits.send_closed,
        urgent,
        ..NetworkPublish::none()
    };
    if publish.has_any() {
        publishes.push(NetworkPublishTarget::new(socket.clone(), publish));
    }
    publishes
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
