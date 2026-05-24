use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome};
use crate::net::structure::table::SocketTable;
use crate::net::structure::{
    AcceptWireSet, ConnectionKey, RecvWireSet, SendWireSet, SocketIdentity, SocketProtocol,
    TcpState, UdpInner, UnixDatagramState, UnixStreamState,
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
            if let Some(peer) =
                table.lookup_tcp_connection(ConnectionKey::new(remote, local), guard)
            {
                let peer_wakes = mark_tcp_peer_closed(&peer);
                peer_recv_woken += peer_wakes.recv_woken;
                peer_send_woken += peer_wakes.send_woken;
            }
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_tcp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_tcp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Tcp(TcpState::Init | TcpState::Closed) => {}
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
            bindings_withdrawn += withdraw_ok(table.withdraw_unix_bound(local));
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local, .. }) => {
            if let Some(local) = local {
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_bound(local));
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
            bindings_withdrawn += withdraw_ok(table.withdraw_unix_bound(local));
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

fn withdraw_ok<T>(result: Result<T, tx_substrate::mutation::MutationError>) -> usize {
    usize::from(result.is_ok())
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

fn mark_unix_peer_broken(peer: &Cap<SocketIdentity>) {
    peer.readiness.fire_recv(RecvWireSet::BROKEN);
    peer.readiness.fire_send(SendWireSet::BROKEN);
}
