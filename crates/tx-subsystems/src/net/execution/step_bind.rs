use tx_substrate::index::IndexError;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_bind_target;
use crate::net::structure::table::SocketTable;
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

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let table = payload.socket_table();

    let table_result = match witness.identity.kind {
        SocketKind::UnixDatagram => Ok(()),
        SocketKind::Tcp => table.bind_tcp(witness.local, socket.clone()),
        SocketKind::Udp => bind_udp_maybe_reuseaddr(table, socket, witness.local, guard),
        SocketKind::RawIcmp => Ok(()),
        SocketKind::NetlinkRoute => Ok(()),
    };
    let _requested_addr = witness.addr;
    if let Err(error) = table_result {
        return StepOutcome::Err(table_error_to_errno(error));
    }

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

fn bind_udp_maybe_reuseaddr(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> Result<(), IndexError> {
    match table.bind_udp(local, socket.clone()) {
        Ok(()) => Ok(()),
        Err(IndexError::Duplicate) if socket_reuse_addr(socket) => {
            let Some(existing) = table.lookup_udp_bound_exact(local, guard) else {
                return Err(IndexError::Duplicate);
            };
            if !socket_reuse_addr(&existing) {
                return Err(IndexError::Duplicate);
            }
            table
                .withdraw_udp_bound(local)
                .map_err(|_| IndexError::Busy)?;
            table.bind_udp(local, socket.clone())
        }
        Err(error) => Err(error),
    }
}

fn socket_reuse_addr(socket: &Cap<SocketIdentity>) -> bool {
    socket
        .acquire_operational()
        .is_some_and(|payload| payload.with_options(|options| options.socket.reuse_addr))
}

pub(crate) fn table_error_to_errno(error: IndexError) -> Errno {
    match error {
        IndexError::Duplicate => Errno::EADDRINUSE,
        IndexError::Full => Errno::ENOMEM,
        IndexError::Busy => Errno::EBUSY,
        IndexError::Missing => Errno::EINVAL,
    }
}
