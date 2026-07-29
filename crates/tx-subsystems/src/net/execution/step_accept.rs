use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_accept_target;
use crate::net::execution::{socket_accept_wait_token, yield_on_token};
use crate::net::structure::{
    AcceptWireSet, IpEndpoint, SocketIdentity, SocketProtocol, TcpState, UnixSocketPath,
};

#[derive(Clone)]
pub struct SocketAcceptOutcome {
    pub child: Cap<SocketIdentity>,
    pub local: IpEndpoint,
    pub peer: IpEndpoint,
    pub unix_peer: Option<UnixSocketPath>,
}

pub fn step_accept(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<SocketAcceptOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_accept_target(socket, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    let Some(pop) = payload.pop_accept_entry() else {
        clear_accept_pending_if_empty(socket, &payload);
        return yield_on_token(socket_accept_wait_token(socket));
    };

    if pop.became_empty {
        clear_accept_pending_if_empty(socket, &payload);
    }
    if let Some(child_payload) = pop.entry.child.acquire_operational() {
        child_payload.with_protocol_mut(|protocol| {
            if matches!(protocol, SocketProtocol::Tcp(TcpState::Connecting { .. })) {
                *protocol = SocketProtocol::Tcp(TcpState::Connected {
                    local: pop.entry.local,
                    remote: pop.entry.peer,
                });
            } else if matches!(protocol, SocketProtocol::Sctp(TcpState::Connecting { .. })) {
                *protocol = SocketProtocol::Sctp(TcpState::Connected {
                    local: pop.entry.local,
                    remote: pop.entry.peer,
                });
            }
        });
    }

    StepOutcome::Done(SocketAcceptOutcome {
        child: pop.entry.child,
        local: pop.entry.local,
        peer: pop.entry.peer,
        unix_peer: pop.entry.unix_peer,
    })
}

fn clear_accept_pending_if_empty(
    socket: &Cap<SocketIdentity>,
    payload: &crate::net::structure::SocketPayload,
) {
    socket.readiness.clear_accept(AcceptWireSet::HAS_PENDING);
    // Promotion can enqueue a child after pop/empty-check released the
    // backlog lock but before HAS_PENDING is cleared.  Re-read the real queue
    // after clearing so that incoming connection cannot lose its wake.
    if payload.accept_queue_len() != 0 {
        socket.readiness.fire_accept(AcceptWireSet::HAS_PENDING);
    }
}
