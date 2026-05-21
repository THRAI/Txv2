use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_listen_target;
use crate::net::execution::step_bind::table_error_to_errno;
use crate::net::execution::SOMAXCONN_STAGING;
use crate::net::structure::{SocketIdentity, SocketProtocol, TcpState, UnixStreamState};

pub fn step_listen(
    socket: &Cap<SocketIdentity>,
    backlog: usize,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let backlog_limit = backlog.clamp(1, SOMAXCONN_STAGING);
    let witness = match require_socket_listen_target(socket, backlog_limit, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if socket.kind == crate::net::structure::SocketKind::Tcp {
        if let Err(error) = payload
            .socket_table()
            .listen_tcp(witness.local, socket.clone())
        {
            return StepOutcome::Err(table_error_to_errno(error));
        }
    }

    let listening = payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Bound { local }) if *local == witness.local => {
            *protocol = SocketProtocol::Tcp(TcpState::Listening {
                local: witness.local,
                backlog_limit: witness.backlog_limit,
            });
            true
        }
        SocketProtocol::UnixStream(UnixStreamState::Bound { local }) => {
            *protocol = SocketProtocol::UnixStream(UnixStreamState::Listening {
                local: *local,
                backlog_limit: witness.backlog_limit,
            });
            true
        }
        _ => false,
    });
    if listening {
        if socket.kind == crate::net::structure::SocketKind::Tcp {
            if let Some(raw_tcp) = payload.raw_tcp_socket() {
                let _ = raw_tcp.listen_endpoint(witness.local);
            }
        }
        payload.set_accept_limit(witness.backlog_limit);
        StepOutcome::Done(())
    } else {
        StepOutcome::Err(Errno::EINVAL)
    }
}
