use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_accept_target;
use crate::net::execution::{socket_accept_wait_token, yield_on_token};
use crate::net::structure::{AcceptWireSet, IpEndpoint, SocketIdentity};

#[derive(Clone)]
pub struct SocketAcceptOutcome {
    pub child: Cap<SocketIdentity>,
    pub local: IpEndpoint,
    pub peer: IpEndpoint,
}

pub fn step_accept(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<SocketAcceptOutcome> {
    let witness = match require_socket_accept_target(socket, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    let Some(pop) = payload.pop_accept_entry() else {
        return yield_on_token(socket_accept_wait_token(socket));
    };

    if pop.became_empty {
        socket.readiness.clear_accept(AcceptWireSet::HAS_PENDING);
    }

    StepOutcome::Done(SocketAcceptOutcome {
        child: pop.entry.child,
        local: pop.entry.local,
        peer: pop.entry.peer,
    })
}
