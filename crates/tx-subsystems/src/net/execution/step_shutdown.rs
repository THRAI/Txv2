use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_shutdown_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::structure::{
    ConnectionKey, RecvWireSet, SendWireSet, SockShutdownCmd, SocketIdentity, SocketProtocol,
    TcpState,
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
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

    // SCTP: shutting down the write side tears down the (single) 1-to-1
    // association, so signal the peer's read side — its next recv with no
    // pending data sees EOF, like a peer close.
    if closes_write_side && mark.send {
        if let SocketProtocol::Sctp(TcpState::Connected { local, remote }) =
            payload.protocol_snapshot()
        {
            if let Some(peer) = payload
                .socket_table()
                .lookup_sctp_connection(ConnectionKey::new(remote, local), guard)
            {
                peer.readiness.fire_recv(RecvWireSet::BROKEN);
            }
            // If subscribed to association events, deliver SCTP_SHUTDOWN_COMP on
            // this socket once the (loopback-immediate) shutdown completes. Queued
            // after any pending data so recvmsg drains data first.
            if payload.with_options(|o| o.sctp.event_assoc_change()) {
                let streams = payload.with_options(|o| o.sctp.initmsg_num_ostreams);
                let bytes = crate::net::execution::sctp_assoc_change_bytes(
                    3, /* SCTP_SHUTDOWN_COMP */
                    streams, 0,
                );
                if payload
                    .record_sctp_message(bytes, true, 0, 0, None)
                    .is_some()
                {
                    socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
                }
            }
        }
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
