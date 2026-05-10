use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_shutdown_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::structure::{
    RecvWireSet, SendWireSet, SockShutdownCmd, SocketIdentity, SocketProtocol, TcpState,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownOutcome {
    pub recv_shutdown: bool,
    pub send_shutdown: bool,
    pub recv_woken: usize,
    pub send_woken: usize,
    pub delegate_kicked: bool,
}

pub fn step_shutdown(
    socket: &Cap<SocketIdentity>,
    how: SockShutdownCmd,
    guard: &Guard<'_>,
) -> StepOutcome<ShutdownOutcome> {
    let witness = match require_socket_shutdown_target(socket, how, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    let mark = payload.mark_shutdown(witness.how);
    let closes_write_side = matches!(witness.how, SockShutdownCmd::Send | SockShutdownCmd::Both);
    let tcp_graceful_close = closes_write_side
        && mark.send
        && matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connected { .. } | TcpState::Connecting { .. })
        );
    if tcp_graceful_close {
        if let Some(raw_tcp) = payload.raw_tcp_socket() {
            raw_tcp.close();
        }
        payload.refresh_io_from_raw();
    }

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
    let delegate_kicked = if tcp_graceful_close {
        net_delegate_kick_poll();
        true
    } else {
        false
    };

    StepOutcome::Done(ShutdownOutcome {
        recv_shutdown: mark.recv,
        send_shutdown: mark.send,
        recv_woken,
        send_woken,
        delegate_kicked,
    })
}
