use smoltcp::time::Instant;

use crate::execution::{Guard, StepOutcome};
use crate::net::packet::{
    NetworkPublish, PacketDispatch, PacketSource, TcpPacketEvent, UdpPacketEvent,
};
use crate::net::protocol::LoopbackIface;
use crate::net::structure::registry;
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::structure::{
    ConnectionKey, SocketAcceptEntry, SocketIdentity, TcpBacklogRetransmitOutcome,
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
    step_process_network_events_at(source, Instant::ZERO, guard)
}

pub fn step_process_network_events_at(
    source: &dyn PacketSource,
    now: Instant,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkStepOutcome> {
    let mut outcome = NetworkStepOutcome::default();

    for _ in 0..NET_EVENT_BUDGET {
        let Some(packet) = source.next_packet_at(now, guard) else {
            break;
        };
        outcome.packets_seen += 1;

        match packet {
            PacketDispatch::Tcp(event) => {
                if let Some((socket, publish)) = process_tcp_event(event, guard) {
                    outcome.sockets_touched += 1;
                    outcome.wakes_fired += publish.publish_to(&socket);
                }
            }
            PacketDispatch::Udp(event) => {
                if let Some((socket, publish)) = process_udp_event(event, guard) {
                    outcome.sockets_touched += 1;
                    outcome.wakes_fired += publish.publish_to(&socket);
                }
            }
            PacketDispatch::Icmp(_) | PacketDispatch::Unsupported | PacketDispatch::Malformed => {}
        }
    }

    outcome.backlog = process_tcp_backlog_tick(now, guard);
    StepOutcome::Done(outcome)
}

pub fn step_process_network_tick(
    now: Instant,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkBacklogTickOutcome> {
    StepOutcome::Done(process_tcp_backlog_tick(now, guard))
}

pub fn step_process_network_tick_loopback(
    now: Instant,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<NetworkBacklogTickOutcome> {
    StepOutcome::Done(process_tcp_backlog_tick_loopback(now, iface, guard))
}

fn process_tcp_backlog_tick(now: Instant, guard: &Guard<'_>) -> NetworkBacklogTickOutcome {
    let mut outcome = NetworkBacklogTickOutcome::default();
    let listeners = SOCKET_TABLE.snapshot_tcp_listeners(guard);

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

fn process_tcp_backlog_tick_loopback(
    now: Instant,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> NetworkBacklogTickOutcome {
    let mut outcome = NetworkBacklogTickOutcome::default();
    let listeners = SOCKET_TABLE.snapshot_tcp_listeners(guard);

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
    event: TcpPacketEvent,
    guard: &Guard<'_>,
) -> Option<(Cap<SocketIdentity>, NetworkPublish)> {
    let key = ConnectionKey::new(event.dst, event.src);
    if let Some(socket) = SOCKET_TABLE.lookup_tcp_connection(key, guard) {
        let payload = socket.acquire_operational()?;
        let mut publish = NetworkPublish::none();
        let flags = event.flags;
        let urgent = event.urgent;
        let ack_bytes = core::cmp::max(1, event.payload_len());
        if payload.record_recv_payload(event.src, event.dst, event.payload) {
            publish.recv_has_data = true;
        }
        if flags.ack && payload.record_send_space(ack_bytes) {
            publish.send_has_space = true;
        }
        publish.urgent = urgent;
        return Some((socket, publish));
    }

    if event.flags.syn && !event.flags.ack {
        let socket =
            SOCKET_TABLE.lookup_tcp_listener_addr(event.dst.addr, event.dst.port, guard)?;
        let payload = socket.acquire_operational()?;
        let options = payload.with_options(Clone::clone);
        let child =
            registry::create_connected_stream_for_accept(event.dst, event.src, options).ok()?;
        SOCKET_TABLE
            .insert_tcp_connection(key, child.clone())
            .ok()?;
        let entry = SocketAcceptEntry {
            child,
            local: event.dst,
            peer: event.src,
        };
        let mut publish = NetworkPublish::none();
        if payload.enqueue_accept_entry(entry)? {
            publish.accept_has_pending = true;
        }
        return Some((socket, publish));
    }

    None
}

fn process_udp_event(
    event: UdpPacketEvent,
    guard: &Guard<'_>,
) -> Option<(Cap<SocketIdentity>, NetworkPublish)> {
    let socket = SOCKET_TABLE.lookup_udp_bound(event.dst, guard)?;
    let payload = socket.acquire_operational()?;
    let mut publish = NetworkPublish::none();
    if payload.record_recv_payload(event.src, event.dst, event.payload) {
        publish.recv_has_data = true;
    }
    Some((socket, publish))
}
