use tx_substrate::index::IndexError;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_bind_target;
use crate::net::structure::table::SocketTable;
use crate::net::structure::{
    IpEndpoint, Ipv4Address, KernelSockAddr, SocketIdentity, SocketKind, SocketProtocol, TcpState,
    UdpInner, UnixDatagramState, UnixStreamState,
};

pub fn step_bind(
    socket: &Cap<SocketIdentity>,
    addr: KernelSockAddr,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_bind_target(socket, addr, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let table = payload.socket_table();

    let table_result = match witness.identity.kind {
        SocketKind::UnixDatagram | SocketKind::UnixStream => match witness.addr {
            KernelSockAddr::Unix(path) => table.bind_unix(path, socket.clone()),
            _ => Err(IndexError::Missing),
        },
        SocketKind::Tcp => bind_tcp_no_wildcard_overlap(table, socket, witness.local, guard),
        SocketKind::Udp => bind_udp_maybe_reuseaddr(table, socket, witness.local, guard),
        SocketKind::RawIcmp => Ok(()),
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter | SocketKind::Packet => Ok(()),
    };
    if let Err(error) = table_result {
        return StepOutcome::Err(table_error_to_errno(error));
    }

    let bound = payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::UnixDatagram(UnixDatagramState::Unbound) => {
            if let KernelSockAddr::Unix(local) = witness.addr {
                *protocol = SocketProtocol::UnixDatagram(UnixDatagramState::Bound { local });
                true
            } else {
                false
            }
        }
        SocketProtocol::UnixStream(UnixStreamState::Init) => {
            if let KernelSockAddr::Unix(local) = witness.addr {
                *protocol = SocketProtocol::UnixStream(UnixStreamState::Bound { local });
                true
            } else {
                false
            }
        }
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

fn bind_tcp_no_wildcard_overlap(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: IpEndpoint,
    guard: &Guard<'_>,
) -> Result<(), IndexError> {
    if tcp_bind_conflict(table, socket, local, guard).is_some() {
        return Err(IndexError::Duplicate);
    }
    table.bind_tcp(local, socket.clone())
}

fn tcp_bind_conflict(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: IpEndpoint,
    guard: &Guard<'_>,
) -> Option<Cap<SocketIdentity>> {
    if let Some(existing) = table.lookup_tcp_bound(local, guard) {
        if existing.raw() != socket.raw() {
            return Some(existing);
        }
    }

    if local.addr == Ipv4Address::UNSPECIFIED {
        for existing in table.snapshot_tcp_bound(guard) {
            if existing.raw() == socket.raw() {
                continue;
            }
            if tcp_socket_local(&existing).is_some_and(|endpoint| endpoint.port == local.port) {
                return Some(existing);
            }
        }
        return None;
    }

    let wildcard = IpEndpoint::new(Ipv4Address::UNSPECIFIED, local.port);
    table
        .lookup_tcp_bound(wildcard, guard)
        .filter(|existing| existing.raw() != socket.raw())
}

fn bind_udp_maybe_reuseaddr(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> Result<(), IndexError> {
    if let Some(existing) = udp_bind_conflict(table, socket, local, guard) {
        if !(socket_reuse_addr(socket) && socket_reuse_addr(&existing)) {
            return Err(IndexError::Duplicate);
        }
        if udp_socket_local(&existing) != Some(local) {
            return Err(IndexError::Duplicate);
        }
    }

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

fn udp_bind_conflict(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: IpEndpoint,
    guard: &Guard<'_>,
) -> Option<Cap<SocketIdentity>> {
    if let Some(existing) = table.lookup_udp_bound_exact(local, guard) {
        if existing.raw() != socket.raw() {
            return Some(existing);
        }
    }

    if local.addr == Ipv4Address::UNSPECIFIED {
        for existing in table.snapshot_udp_bound(guard) {
            if existing.raw() == socket.raw() {
                continue;
            }
            if udp_socket_local(&existing).is_some_and(|endpoint| endpoint.port == local.port) {
                return Some(existing);
            }
        }
        return None;
    }

    let wildcard = IpEndpoint::new(Ipv4Address::UNSPECIFIED, local.port);
    table
        .lookup_udp_bound_exact(wildcard, guard)
        .filter(|existing| existing.raw() != socket.raw())
}

fn tcp_socket_local(socket: &Cap<SocketIdentity>) -> Option<IpEndpoint> {
    socket
        .acquire_operational()
        .and_then(|payload| match payload.protocol_snapshot() {
            SocketProtocol::Tcp(TcpState::Bound { local })
            | SocketProtocol::Tcp(TcpState::Listening { local, .. })
            | SocketProtocol::Tcp(TcpState::Connecting { local, .. })
            | SocketProtocol::Tcp(TcpState::Connected { local, .. }) => Some(local),
            _ => None,
        })
}

fn udp_socket_local(socket: &Cap<SocketIdentity>) -> Option<IpEndpoint> {
    socket
        .acquire_operational()
        .and_then(|payload| match payload.protocol_snapshot() {
            SocketProtocol::Udp(UdpInner::Bound { local })
            | SocketProtocol::Udp(UdpInner::Connected { local, .. }) => Some(local),
            _ => None,
        })
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
