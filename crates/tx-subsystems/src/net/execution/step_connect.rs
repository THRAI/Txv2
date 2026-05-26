use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome, WaitToken};
use crate::net::checks::require::require_socket_connect_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::yield_on_token;
use crate::net::structure::registry;
use crate::net::structure::table::SocketTable;
use crate::net::structure::{
    AcceptWireSet, ConnectionKey, IpEndpoint, Ipv4Address, KernelSockAddr, SendWireSet,
    SocketAcceptEntry, SocketIdentity, SocketKind, SocketOperationalEvidence, SocketProtocol,
    TcpState, UdpInner, UnixDatagramState, UnixSocketPath, UnixStreamState,
};

pub fn step_connect(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if matches!(remote, KernelSockAddr::Unspec) {
        return step_connect_unspec(socket, &payload, guard);
    }

    let witness = match require_socket_connect_target(socket, remote, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    if matches!(remote, KernelSockAddr::Unix(_)) {
        return step_unix_connect(socket, &payload, remote, guard);
    }

    if let Err(errno) = update_udp_connection_index(
        payload.socket_table(),
        socket,
        &payload.protocol_snapshot(),
        witness.remote,
        guard,
    ) {
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

fn step_connect_unspec(
    socket: &Cap<SocketIdentity>,
    payload: &SocketOperationalEvidence,
    _guard: &Guard<'_>,
) -> StepOutcome<()> {
    if socket.kind != SocketKind::Tcp {
        return StepOutcome::Err(Errno::EAFNOSUPPORT);
    }

    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connected { local, remote }) => (local, remote),
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => (local, remote),
        SocketProtocol::Tcp(_) => return StepOutcome::Err(Errno::EINVAL),
        _ => return StepOutcome::Err(Errno::EAFNOSUPPORT),
    };

    let table = payload.socket_table();
    let _ = table.withdraw_tcp_connection(ConnectionKey::new(local, remote));
    let _ = table.withdraw_tcp_connection(ConnectionKey::new(remote, local));
    if let Err(errno) = payload.reset_raw_tcp_socket() {
        return StepOutcome::Err(errno);
    }
    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Tcp(TcpState::Init);
    });
    socket
        .readiness
        .clear_send(SendWireSet::SPACE | SendWireSet::BROKEN);
    socket
        .readiness
        .clear_recv(crate::net::structure::RecvWireSet::HAS_DATA);
    StepOutcome::Done(())
}

fn step_unix_connect(
    socket: &Cap<SocketIdentity>,
    payload: &crate::net::structure::SocketOperationalEvidence,
    remote: KernelSockAddr,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let KernelSockAddr::Unix(peer) = remote else {
        return StepOutcome::Err(Errno::EAFNOSUPPORT);
    };
    match socket.kind {
        SocketKind::UnixDatagram => connect_unix_datagram(payload, peer, guard),
        SocketKind::UnixStream => connect_unix_stream(socket, payload, peer, guard),
        _ => StepOutcome::Err(Errno::EAFNOSUPPORT),
    }
}

fn connect_unix_datagram(
    payload: &crate::net::structure::SocketOperationalEvidence,
    peer: UnixSocketPath,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let table = payload.socket_table();
    let Some(target) = table.lookup_unix_bound(peer, guard) else {
        return StepOutcome::Err(Errno::ENOENT);
    };
    if target.kind != SocketKind::UnixDatagram {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }

    let connected = payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::UnixDatagram(UnixDatagramState::Unbound) => {
            *protocol =
                SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local: None, peer });
            true
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Bound { local }) => {
            *protocol = SocketProtocol::UnixDatagram(UnixDatagramState::Connected {
                local: Some(*local),
                peer,
            });
            true
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local, .. }) => {
            *protocol = SocketProtocol::UnixDatagram(UnixDatagramState::Connected {
                local: *local,
                peer,
            });
            true
        }
        _ => false,
    });
    if connected {
        StepOutcome::Done(())
    } else {
        StepOutcome::Err(Errno::EINVAL)
    }
}

fn connect_unix_stream(
    socket: &Cap<SocketIdentity>,
    payload: &crate::net::structure::SocketOperationalEvidence,
    peer: UnixSocketPath,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let table = payload.socket_table();
    let Some(listener) = table.lookup_unix_bound(peer, guard) else {
        return StepOutcome::Err(Errno::ENOENT);
    };
    let Some(listener_payload) = listener.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let listener_local = match listener_payload.protocol_snapshot() {
        SocketProtocol::UnixStream(UnixStreamState::Listening { local, .. }) => local,
        _ => return StepOutcome::Err(Errno::ECONNREFUSED),
    };

    let options = listener_payload.with_options(Clone::clone);
    let child = match registry::create_socket_in_namespace(
        SocketKind::UnixStream,
        options,
        payload.net_namespace(),
    ) {
        Ok(child) => child,
        Err(_) => return StepOutcome::Err(Errno::ENOMEM),
    };

    let local = match payload.protocol_snapshot() {
        SocketProtocol::UnixStream(UnixStreamState::Init) => None,
        SocketProtocol::UnixStream(UnixStreamState::Bound { local }) => Some(local),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };

    if table
        .insert_unix_stream_peer(socket.raw(), child.clone())
        .is_err()
    {
        return StepOutcome::Err(Errno::ENOMEM);
    }
    if table
        .insert_unix_stream_peer(child.raw(), socket.clone())
        .is_err()
    {
        let _ = table.withdraw_unix_stream_peer(socket.raw());
        return StepOutcome::Err(Errno::ENOMEM);
    }

    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::UnixStream(UnixStreamState::Connected {
            local,
            peer_raw: child.raw(),
        });
    });
    if let Some(child_payload) = child.acquire_operational() {
        child_payload.with_protocol_mut(|protocol| {
            *protocol = SocketProtocol::UnixStream(UnixStreamState::Connected {
                local: Some(listener_local),
                peer_raw: socket.raw(),
            });
        });
    }

    let child_raw = child.raw();
    let entry = SocketAcceptEntry {
        child,
        local: unspecified_endpoint(),
        peer: unspecified_endpoint(),
        unix_peer: local,
    };
    if listener_payload.enqueue_accept_entry(entry).is_some() {
        listener.readiness.fire_accept(AcceptWireSet::HAS_PENDING);
        StepOutcome::Done(())
    } else {
        let _ = table.withdraw_unix_stream_peer(socket.raw());
        let _ = table.withdraw_unix_stream_peer(child_raw);
        StepOutcome::Err(Errno::ECONNREFUSED)
    }
}

const fn unspecified_endpoint() -> IpEndpoint {
    IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0)
}

fn update_udp_connection_index(
    table: &SocketTable,
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
        if let Some(existing) = table.lookup_udp_connection(key, guard) {
            if existing.raw() != socket.raw() {
                return Err(Errno::EADDRINUSE);
            }
        }
    }

    if let Some(key) = old_key {
        let _ = table.withdraw_udp_connection(key);
    }

    if let Some(key) = new_key {
        table
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
    if local.is_unspecified() && remote.is_loopback() {
        IpEndpoint::loopback_for_family(remote.family, local.port)
    } else {
        local
    }
}
