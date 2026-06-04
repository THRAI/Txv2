use tx_substrate::zone::{Cap, PayloadCap};

use crate::execution::{Guard, StepOutcome};
use crate::net::namespace::net_namespace_payloads_snapshot;
use crate::net::structure::table::SocketTable;
use crate::net::structure::SocketPayload;
use crate::net::structure::{
    AcceptWireSet, ConnectionKey, IpEndpoint, RdsState, RecvWireSet, SendWireSet, SocketIdentity,
    SocketProtocol, TcpState, UdpInner, UnixDatagramState, UnixStreamState,
};

use super::step_tcp_loopback::step_tcp_loopback_transfer;

const TCP_CLOSE_FLUSH_PASSES: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketCloseOutcome {
    pub payload_taken: bool,
    pub bindings_withdrawn: usize,
    pub tcp_flushed_bytes: usize,
    pub recv_woken: usize,
    pub send_woken: usize,
    pub accept_woken: usize,
}

pub fn step_socket_close(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<SocketCloseOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(payload) = socket.live_payload() else {
        return StepOutcome::Done(SocketCloseOutcome::default());
    };

    let mut bindings_withdrawn = 0;
    let mut tcp_flushed_bytes = 0;
    let mut peer_recv_woken = 0;
    let mut peer_send_woken = 0;
    let table = payload.socket_table();
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Bound { local }) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_bound(local));
        }
        SocketProtocol::Tcp(TcpState::Listening { local, .. }) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_listener(local));
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_bound(local));
        }
        SocketProtocol::Tcp(TcpState::Connecting { local, remote })
        | SocketProtocol::Tcp(TcpState::Connected { local, remote }) => {
            tcp_flushed_bytes += flush_tcp_tx_before_close(socket, guard);
            if let Some(peer) = lookup_tcp_peer_connection(table, remote, local, guard) {
                let peer_wakes = mark_tcp_peer_closed(&peer);
                peer_recv_woken += peer_wakes.recv_woken;
                peer_send_woken += peer_wakes.send_woken;
            }
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_tcp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_tcp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Tcp(TcpState::Init | TcpState::Closed) => {}
        SocketProtocol::Sctp(TcpState::Bound { local }) => {
            peer_recv_woken += notify_sctp_seqpacket_peers_closed(&payload, table, guard);
            bindings_withdrawn += withdraw_ok(table.withdraw_sctp_bound(local));
        }
        SocketProtocol::Sctp(TcpState::Listening { local, .. }) => {
            peer_recv_woken += notify_sctp_seqpacket_peers_closed(&payload, table, guard);
            bindings_withdrawn += withdraw_ok(table.withdraw_sctp_listener(local));
            bindings_withdrawn += withdraw_ok(table.withdraw_sctp_bound(local));
        }
        SocketProtocol::Sctp(TcpState::Connecting { local, remote })
        | SocketProtocol::Sctp(TcpState::Connected { local, remote }) => {
            if let Some(peer) =
                table.lookup_sctp_connection(ConnectionKey::new(remote, local), guard)
            {
                // Notify the peer's event subscription that the association is
                // going down (SCTP_SHUTDOWN_EVENT), queued ahead of the EOF mark.
                enqueue_sctp_shutdown_event(&peer);
                let peer_wakes = mark_sctp_peer_closed(&peer);
                peer_recv_woken += peer_wakes.recv_woken;
                peer_send_woken += peer_wakes.send_woken;
            }
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_sctp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_sctp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Sctp(TcpState::Init | TcpState::Closed) => {}
        SocketProtocol::Rds(RdsState::Bound { local }) => {
            bindings_withdrawn += withdraw_rds_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Rds(RdsState::Unbound | RdsState::Closed) => {}
        SocketProtocol::Udp(UdpInner::Bound { local }) => {
            bindings_withdrawn += withdraw_udp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => {
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_udp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_udp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Udp(UdpInner::Unbound | UdpInner::Closed) => {}
        SocketProtocol::RawIcmp(_) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_raw_icmp(socket.raw()));
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Bound { local }) => {
            bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local, .. }) => {
            if let Some(local) = local {
                bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
            }
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { peer_raw }) => {
            if let Some(peer) = table.lookup_unix_peer(socket.raw(), guard) {
                mark_unix_peer_broken(&peer);
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_peer(peer.raw()));
            } else if peer_raw != 0 {
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_peer(peer_raw));
            }
            bindings_withdrawn += withdraw_ok(table.withdraw_unix_peer(socket.raw()));
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Unbound) => {}
        SocketProtocol::UnixStream(UnixStreamState::Bound { local })
        | SocketProtocol::UnixStream(UnixStreamState::Listening { local, .. }) => {
            bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
        }
        SocketProtocol::UnixStream(UnixStreamState::Connected { peer_raw, .. }) => {
            if let Some(peer) = table.lookup_unix_stream_peer(socket.raw(), guard) {
                mark_unix_peer_broken(&peer);
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_stream_peer(peer.raw()));
            } else if peer_raw != 0 {
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_stream_peer(peer_raw));
            }
            bindings_withdrawn += withdraw_ok(table.withdraw_unix_stream_peer(socket.raw()));
        }
        SocketProtocol::UnixStream(UnixStreamState::Init | UnixStreamState::Closed) => {}
        SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => {}
    }

    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.abort();
    }
    if let Some(raw_udp) = payload.raw_udp_socket() {
        raw_udp.close();
    }
    let payload_taken = socket.take_payload().is_some();
    let recv_woken = peer_recv_woken + socket.readiness.fire_recv(RecvWireSet::BROKEN);
    let send_woken = peer_send_woken + socket.readiness.fire_send(SendWireSet::BROKEN);
    let accept_woken = socket.readiness.fire_accept(AcceptWireSet::BROKEN);

    StepOutcome::Done(SocketCloseOutcome {
        payload_taken,
        bindings_withdrawn,
        tcp_flushed_bytes,
        recv_woken,
        send_woken,
        accept_woken,
    })
}

fn flush_tcp_tx_before_close(socket: &Cap<SocketIdentity>, guard: &Guard<'_>) -> usize {
    if let Some(payload) = socket.acquire_operational() {
        if let Some(raw_tcp) = payload.raw_tcp_socket() {
            let _ = raw_tcp.flush_corked_tx();
        }
    }

    let mut moved_total = 0;
    for _ in 0..TCP_CLOSE_FLUSH_PASSES {
        let queued = socket
            .acquire_operational()
            .and_then(|payload| {
                payload
                    .raw_tcp_socket()
                    .map(|raw_tcp| raw_tcp.send_queued())
            })
            .unwrap_or(0);
        if queued == 0 {
            break;
        }

        let moved = match step_tcp_loopback_transfer(socket, queued, guard) {
            StepOutcome::Done(outcome) => outcome.bytes_moved,
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } | StepOutcome::Err(_) => 0,
        };
        if moved == 0 {
            break;
        }
        moved_total += moved;
    }
    moved_total
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PeerCloseWakes {
    recv_woken: usize,
    send_woken: usize,
}

fn mark_tcp_peer_closed(peer: &Cap<SocketIdentity>) -> PeerCloseWakes {
    let Some(payload) = peer.acquire_operational() else {
        return PeerCloseWakes::default();
    };
    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.mark_recv_closed_by_peer();
    }
    payload.refresh_io_from_raw();
    let recv_woken = peer.readiness.fire_recv(RecvWireSet::BROKEN);
    let send_woken = peer.readiness.fire_send(SendWireSet::BROKEN);
    PeerCloseWakes {
        recv_woken,
        send_woken,
    }
}

fn lookup_tcp_peer_connection(
    table: &SocketTable,
    remote: IpEndpoint,
    local: IpEndpoint,
    guard: &Guard<'_>,
) -> Option<Cap<SocketIdentity>> {
    let key = ConnectionKey::new(remote, local);
    table.lookup_tcp_connection(key, guard).or_else(|| {
        net_namespace_payloads_snapshot()
            .into_iter()
            .find_map(|namespace| namespace.socket_table().lookup_tcp_connection(key, guard))
    })
}

fn mark_sctp_peer_closed(peer: &Cap<SocketIdentity>) -> PeerCloseWakes {
    let recv_woken = peer.readiness.fire_recv(RecvWireSet::BROKEN);
    let send_woken = peer.readiness.fire_send(SendWireSet::BROKEN);
    PeerCloseWakes {
        recv_woken,
        send_woken,
    }
}

/// When a 1-to-many (SEQPACKET) socket closes, deliver SCTP_SHUTDOWN_COMP to each
/// peer association whose socket subscribed to association events. Returns the
/// number of peer recv waiters woken.
fn notify_sctp_seqpacket_peers_closed(
    payload: &PayloadCap<SocketPayload>,
    table: &SocketTable,
    guard: &Guard<'_>,
) -> usize {
    let mut woken = 0;
    for assoc in payload.sctp_peers() {
        let Some(peer) = table
            .lookup_sctp_listener_dual_stack_endpoint(assoc.peer, guard)
            .or_else(|| table.lookup_sctp_bound(assoc.peer, guard))
        else {
            continue;
        };
        let Some(peer_payload) = peer.acquire_operational() else {
            continue;
        };
        if !peer_payload.with_options(|o| o.sctp.event_assoc_change()) {
            continue;
        }
        let streams = peer_payload.with_options(|o| o.sctp.initmsg_num_ostreams);
        let bytes =
            crate::net::execution::sctp_assoc_change_bytes(3 /* SHUTDOWN_COMP */, streams);
        if peer_payload
            .record_sctp_message(bytes, true, 0, 0, None)
            .is_some()
        {
            woken += peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
        }
    }
    woken
}

/// Queue an SCTP_SHUTDOWN_EVENT notification on `peer`'s receive queue if it
/// subscribed to shutdown events, so its next recvmsg surfaces the teardown.
fn enqueue_sctp_shutdown_event(peer: &Cap<SocketIdentity>) {
    let Some(payload) = peer.acquire_operational() else {
        return;
    };
    if !payload.with_options(|o| o.sctp.event_shutdown()) {
        return;
    }
    let bytes = crate::net::execution::sctp_shutdown_event_bytes();
    if payload
        .record_sctp_message(bytes, true, 0, 0, None)
        .is_some()
    {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
}

fn withdraw_ok<T>(result: Result<T, tx_substrate::mutation::MutationError>) -> usize {
    usize::from(result.is_ok())
}

fn withdraw_unix_binding_on_close(
    table: &SocketTable,
    local: crate::net::structure::UnixSocketPath,
) -> usize {
    if local.is_abstract() {
        withdraw_ok(table.unlink_unix_path(local))
    } else {
        withdraw_ok(table.withdraw_unix_bound(local))
    }
}

fn withdraw_udp_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_udp_bound_exact(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_udp_bound(local))
}

fn withdraw_tcp_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_tcp_bound(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_tcp_bound(local))
}

fn withdraw_sctp_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_sctp_bound(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_sctp_bound(local))
}

fn withdraw_rds_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_rds_bound(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_rds_bound(local))
}

fn mark_unix_peer_broken(peer: &Cap<SocketIdentity>) {
    peer.readiness.fire_recv(RecvWireSet::BROKEN);
    peer.readiness.fire_send(SendWireSet::BROKEN);
}
