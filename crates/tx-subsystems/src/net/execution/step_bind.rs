use tx_substrate::index::IndexError;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_bind_target;
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::structure::{
    KernelSockAddr, SocketIdentity, SocketKind, SocketProtocol, TcpState, UdpInner,
};

pub fn step_bind(
    socket: &Cap<SocketIdentity>,
    addr: KernelSockAddr,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let witness = match require_socket_bind_target(socket, addr, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };

    let table_result = match witness.identity.kind {
        SocketKind::Tcp => SOCKET_TABLE.bind_tcp(witness.local, socket.clone()),
        SocketKind::Udp => SOCKET_TABLE.bind_udp(witness.local, socket.clone()),
        SocketKind::RawIcmp => Ok(()),
    };
    let _requested_addr = witness.addr;
    if let Err(error) = table_result {
        return StepOutcome::Err(table_error_to_errno(error));
    }

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let bound = payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Init) => {
            *protocol = SocketProtocol::Tcp(TcpState::Bound {
                local: witness.local,
            });
            true
        }
        SocketProtocol::Udp(UdpInner::Unbound) => {
            *protocol = SocketProtocol::Udp(UdpInner::Bound {
                local: witness.local,
            });
            true
        }
        SocketProtocol::RawIcmp(state) if state.bound_local.is_none() => {
            state.bound_local = Some(witness.local.addr);
            true
        }
        _ => false,
    });
    if bound {
        StepOutcome::Done(())
    } else {
        StepOutcome::Err(Errno::EINVAL)
    }
}

pub(crate) fn table_error_to_errno(error: IndexError) -> Errno {
    match error {
        IndexError::Duplicate => Errno::EADDRINUSE,
        IndexError::Full => Errno::ENOMEM,
        IndexError::Busy => Errno::EBUSY,
        IndexError::Missing => Errno::EINVAL,
    }
}
