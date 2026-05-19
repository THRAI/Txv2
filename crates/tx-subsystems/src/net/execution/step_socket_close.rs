use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome};
use crate::net::structure::table::SocketTable;
use crate::net::structure::{
    AcceptWireSet, ConnectionKey, RecvWireSet, SendWireSet, SockShutdownCmd, SocketIdentity,
    SocketProtocol, TcpState, UdpInner,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketCloseOutcome {
    pub payload_taken: bool,
    pub bindings_withdrawn: usize,
    pub recv_woken: usize,
    pub send_woken: usize,
    pub accept_woken: usize,
}

pub fn step_socket_close(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<SocketCloseOutcome> {
    let Some(payload) = socket.live_payload() else {
        return StepOutcome::Done(SocketCloseOutcome::default());
    };

    let mut bindings_withdrawn = 0;
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
            if let Some(peer) =
                table.lookup_tcp_connection(ConnectionKey::new(remote, local), guard)
            {
                mark_tcp_peer_broken(&peer);
            }
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_tcp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_tcp_connection(ConnectionKey::new(remote, local)));
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_bound(local));
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
        SocketProtocol::UnixDatagram => {}
        SocketProtocol::NetlinkRoute(_) => {}
    }

    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.abort();
    }
    if let Some(raw_udp) = payload.raw_udp_socket() {
        raw_udp.close();
    }
    let payload_taken = socket.take_payload().is_some();
    let recv_woken = socket.readiness.fire_recv(RecvWireSet::BROKEN);
    let send_woken = socket.readiness.fire_send(SendWireSet::BROKEN);
    let accept_woken = socket.readiness.fire_accept(AcceptWireSet::BROKEN);

    StepOutcome::Done(SocketCloseOutcome {
        payload_taken,
        bindings_withdrawn,
        recv_woken,
        send_woken,
        accept_woken,
    })
}

fn mark_tcp_peer_broken(peer: &Cap<SocketIdentity>) {
    let Some(payload) = peer.acquire_operational() else {
        return;
    };
    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.abort();
    }
    payload.mark_shutdown(SockShutdownCmd::Both);
    payload.refresh_io_from_raw();
    peer.readiness.fire_recv(RecvWireSet::BROKEN);
    peer.readiness.fire_send(SendWireSet::BROKEN);
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
