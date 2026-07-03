use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_shutdown_target;
use crate::net::structure::{
    RecvWireSet, SendWireSet, SockShutdownCmd, SocketIdentity, SocketProtocol, TcpState,
};

use super::step_tcp_cleanup::{cleanup_tcp_connection, TcpConnectionCleanupOutcome};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpCloseStagingOutcome {
    pub cleanup: TcpConnectionCleanupOutcome,
    pub recv_shutdown: bool,
    pub send_shutdown: bool,
    pub recv_broken_published: bool,
    pub send_broken_published: bool,
    pub recv_woken: usize,
    pub send_woken: usize,
}

pub fn step_tcp_close_staging(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<TcpCloseStagingOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_shutdown_target(socket, SockShutdownCmd::Both, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if !matches!(payload.protocol_snapshot(), SocketProtocol::Tcp(_)) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    let cleanup = match cleanup_tcp_connection(socket) {
        Ok(cleanup) => cleanup,
        Err(errno) => return StepOutcome::Err(errno),
    };
    payload.with_protocol_mut(|protocol| {
        if matches!(protocol, SocketProtocol::Tcp(_)) {
            *protocol = SocketProtocol::Tcp(TcpState::Closed);
        }
    });
    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.abort();
    }

    let mark = payload.mark_shutdown(witness.how);
    let recv_woken = if mark.recv {
        witness.identity.readiness.fire_recv(RecvWireSet::BROKEN)
    } else {
        0
    };
    let send_woken = if mark.send {
        witness.identity.readiness.fire_send(SendWireSet::BROKEN)
    } else {
        0
    };

    StepOutcome::Done(TcpCloseStagingOutcome {
        cleanup,
        recv_shutdown: mark.recv,
        send_shutdown: mark.send,
        recv_broken_published: mark.recv,
        send_broken_published: mark.send,
        recv_woken,
        send_woken,
    })
}
