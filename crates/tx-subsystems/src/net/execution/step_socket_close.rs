use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome};
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::structure::{
    AcceptWireSet, ConnectionKey, RecvWireSet, SendWireSet, SocketIdentity, SocketProtocol,
    TcpState, UdpInner,
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
    _guard: &Guard<'_>,
) -> StepOutcome<SocketCloseOutcome> {
    let Some(payload) = socket.live_payload() else {
        return StepOutcome::Done(SocketCloseOutcome::default());
    };

    let mut bindings_withdrawn = 0;
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Bound { local }) => {
            bindings_withdrawn += withdraw_ok(SOCKET_TABLE.withdraw_tcp_bound(local));
        }
        SocketProtocol::Tcp(TcpState::Listening { local, .. }) => {
            bindings_withdrawn += withdraw_ok(SOCKET_TABLE.withdraw_tcp_listener(local));
            bindings_withdrawn += withdraw_ok(SOCKET_TABLE.withdraw_tcp_bound(local));
        }
        SocketProtocol::Tcp(TcpState::Connecting { local, remote })
        | SocketProtocol::Tcp(TcpState::Connected { local, remote }) => {
            bindings_withdrawn += withdraw_ok(
                SOCKET_TABLE.withdraw_tcp_connection(ConnectionKey::new(local, remote)),
            );
            bindings_withdrawn += withdraw_ok(
                SOCKET_TABLE.withdraw_tcp_connection(ConnectionKey::new(remote, local)),
            );
            bindings_withdrawn += withdraw_ok(SOCKET_TABLE.withdraw_tcp_bound(local));
        }
        SocketProtocol::Tcp(TcpState::Init | TcpState::Closed) => {}
        SocketProtocol::Udp(UdpInner::Bound { local } | UdpInner::Connected { local, .. }) => {
            bindings_withdrawn += withdraw_ok(SOCKET_TABLE.withdraw_udp_bound(local));
        }
        SocketProtocol::Udp(UdpInner::Unbound | UdpInner::Closed) => {}
        SocketProtocol::RawIcmp(_) => {
            bindings_withdrawn += withdraw_ok(SOCKET_TABLE.withdraw_raw_icmp(socket.raw()));
        }
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

fn withdraw_ok<T>(result: Result<T, tx_substrate::mutation::MutationError>) -> usize {
    usize::from(result.is_ok())
}
