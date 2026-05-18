use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome, WaitToken};
use crate::net::checks::require::require_socket_connect_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::yield_on_token;
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::structure::{
    ConnectionKey, IpEndpoint, Ipv4Address, KernelSockAddr, SendWireSet, SocketIdentity,
    SocketProtocol, TcpState, UdpInner,
};

pub fn step_connect(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let witness = match require_socket_connect_target(socket, remote, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    if let Err(errno) =
        update_udp_connection_index(socket, &payload.protocol_snapshot(), witness.remote, guard)
    {
        return StepOutcome::Err(errno);
    }

    let mut advanced = false;
    let blocked = payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Init) => {
            *protocol = SocketProtocol::Tcp(TcpState::Connecting {
                local: unspecified_endpoint(),
                remote: witness.remote,
            });
            advanced = true;
            true
        }
        SocketProtocol::Tcp(TcpState::Bound { local }) => {
            let selected_local = select_tcp_connect_local(*local, witness.remote);
            *protocol = SocketProtocol::Tcp(TcpState::Connecting {
                local: selected_local,
                remote: witness.remote,
            });
            advanced = true;
            true
        }
        SocketProtocol::Tcp(TcpState::Connecting { .. }) => true,
        SocketProtocol::Udp(UdpInner::Unbound) => {
            *protocol = SocketProtocol::Udp(UdpInner::Connected {
                local: unspecified_endpoint(),
                remote: witness.remote,
            });
            false
        }
        SocketProtocol::Udp(UdpInner::Bound { local }) => {
            *protocol = SocketProtocol::Udp(UdpInner::Connected {
                local: *local,
                remote: witness.remote,
            });
            false
        }
        SocketProtocol::Udp(UdpInner::Connected { local, .. }) => {
            *protocol = SocketProtocol::Udp(UdpInner::Connected {
                local: *local,
                remote: witness.remote,
            });
            false
        }
        SocketProtocol::RawIcmp(_) => false,
        _ => false,
    });

    if blocked {
        if advanced {
            net_delegate_kick_poll();
        }
        let wait = WaitToken::new(
            socket.wait_carriers.send,
            SendWireSet::SPACE.bits() | SendWireSet::BROKEN.bits(),
        );
        yield_on_token(wait)
    } else {
        net_delegate_kick_poll();
        StepOutcome::Done(())
    }
}

const fn unspecified_endpoint() -> IpEndpoint {
    IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0)
}

fn update_udp_connection_index(
    socket: &Cap<SocketIdentity>,
    protocol: &SocketProtocol,
    remote: IpEndpoint,
    guard: &Guard<'_>,
) -> Result<(), Errno> {
    let (old_key, new_key) = match protocol {
        SocketProtocol::Udp(UdpInner::Unbound) => (None, None),
        SocketProtocol::Udp(UdpInner::Bound { local }) => {
            (None, udp_connection_key(*local, remote))
        }
        SocketProtocol::Udp(UdpInner::Connected {
            local,
            remote: old_remote,
        }) => (
            udp_connection_key(*local, *old_remote),
            udp_connection_key(*local, remote),
        ),
        _ => return Ok(()),
    };

    if old_key == new_key {
        return Ok(());
    }

    if let Some(key) = new_key {
        if let Some(existing) = SOCKET_TABLE.lookup_udp_connection(key, guard) {
            if existing.raw() != socket.raw() {
                return Err(Errno::EADDRINUSE);
            }
        }
    }

    if let Some(key) = old_key {
        let _ = SOCKET_TABLE.withdraw_udp_connection(key);
    }

    if let Some(key) = new_key {
        SOCKET_TABLE
            .insert_udp_connection(key, socket.clone())
            .map_err(udp_table_error_to_errno)?;
    }

    Ok(())
}

fn udp_connection_key(local: IpEndpoint, remote: IpEndpoint) -> Option<ConnectionKey> {
    if local.port == 0 || remote.port == 0 {
        return None;
    }
    Some(ConnectionKey::new(local, remote))
}

fn udp_table_error_to_errno(error: tx_substrate::index::IndexError) -> Errno {
    match error {
        tx_substrate::index::IndexError::Duplicate => Errno::EADDRINUSE,
        tx_substrate::index::IndexError::Full => Errno::ENOMEM,
        tx_substrate::index::IndexError::Busy => Errno::EBUSY,
        tx_substrate::index::IndexError::Missing => Errno::EINVAL,
    }
}

fn select_tcp_connect_local(local: IpEndpoint, remote: IpEndpoint) -> IpEndpoint {
    if local.addr == Ipv4Address::UNSPECIFIED && remote.addr == Ipv4Address::LOOPBACK {
        IpEndpoint::new(Ipv4Address::LOOPBACK, local.port)
    } else {
        local
    }
}
