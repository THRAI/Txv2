use tx_substrate::zone::{Cap, PayloadCap};

use crate::execution::{Errno, Guard, StepOutcome, WaitToken};
use crate::net::checks::require::require_socket_connect_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::step_bind::table_error_to_errno;
use crate::net::execution::yield_on_token;
use crate::net::namespace::{net_namespace_payloads_snapshot, NetNamespacePayload};
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
    if socket.kind == SocketKind::Sctp {
        return step_sctp_connect(socket, &payload, witness.remote, guard);
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
            let selected_local = select_tcp_connect_local(&payload, *local, witness.remote);
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
        if let Some(outcome) = try_tcp_local_namespace_connect(socket, &payload, guard) {
            return outcome;
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

fn try_tcp_local_namespace_connect(
    socket: &Cap<SocketIdentity>,
    payload: &SocketOperationalEvidence,
    guard: &Guard<'_>,
) -> Option<StepOutcome<()>> {
    if socket.kind != SocketKind::Tcp {
        return None;
    }

    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => (local, remote),
        _ => return None,
    };
    if remote.is_loopback() {
        return None;
    }
    if local.is_unspecified() || local.port == 0 {
        return Some(StepOutcome::Err(Errno::EADDRNOTAVAIL));
    }

    let Some(target_namespace) = namespace_owning_endpoint(remote) else {
        return None;
    };
    let target_table = target_namespace.socket_table();
    let Some(listener) = target_table.lookup_tcp_listener_dual_stack_endpoint(remote, guard) else {
        return Some(StepOutcome::Err(Errno::ECONNREFUSED));
    };
    let Some(listener_payload) = listener.acquire_operational() else {
        return Some(StepOutcome::Err(Errno::ECONNREFUSED));
    };
    let listener_local = match listener_payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Listening {
            local: listener_local,
            ..
        }) if tcp_listener_accepts_local_endpoint(&listener_payload, listener_local, remote) => {
            concrete_listener_endpoint(listener_local, remote)
        }
        _ => return Some(StepOutcome::Err(Errno::ECONNREFUSED)),
    };

    let child_options = listener_payload.with_options(Clone::clone);
    let child = match registry::create_connected_stream_for_accept_in_namespace_with_family(
        listener_local,
        local,
        listener_local.family,
        child_options,
        listener_payload.net_namespace(),
    ) {
        Ok(child) => child,
        Err(_) => return Some(StepOutcome::Err(Errno::ENOMEM)),
    };

    let client_key = ConnectionKey::new(local, listener_local);
    let server_key = ConnectionKey::new(listener_local, local);
    let client_table = payload.socket_table();
    if let Err(error) = client_table.insert_tcp_connection(client_key, socket.clone()) {
        return Some(StepOutcome::Err(table_error_to_errno(error)));
    }
    if let Err(error) = target_table.insert_tcp_connection(server_key, child.clone()) {
        let _ = client_table.withdraw_tcp_connection(client_key);
        return Some(StepOutcome::Err(table_error_to_errno(error)));
    }

    let entry = SocketAcceptEntry {
        child,
        local: listener_local,
        peer: local,
        unix_peer: None,
    };
    let Some(accept_became_ready) = listener_payload.enqueue_accept_entry(entry) else {
        let _ = client_table.withdraw_tcp_connection(client_key);
        let _ = target_table.withdraw_tcp_connection(server_key);
        return Some(StepOutcome::Err(Errno::ECONNREFUSED));
    };

    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Tcp(TcpState::Connected {
            local,
            remote: listener_local,
        });
    });
    socket.readiness.fire_send(SendWireSet::SPACE);
    if accept_became_ready {
        listener.readiness.fire_accept(AcceptWireSet::HAS_PENDING);
    }
    Some(StepOutcome::Done(()))
}

fn namespace_owning_endpoint(remote: IpEndpoint) -> Option<PayloadCap<NetNamespacePayload>> {
    net_namespace_payloads_snapshot()
        .into_iter()
        .find(|namespace| match remote.ip_addr() {
            crate::net::structure::IpAddress::V4(addr) => namespace.owns_ipv4_addr(addr),
            crate::net::structure::IpAddress::V6(addr) => namespace.owns_ipv6_addr(addr),
        })
}

fn concrete_listener_endpoint(listener_local: IpEndpoint, remote: IpEndpoint) -> IpEndpoint {
    if listener_local.is_unspecified() && listener_local.port == remote.port {
        remote
    } else {
        listener_local
    }
}

fn tcp_listener_accepts_local_endpoint(
    listener_payload: &SocketOperationalEvidence,
    listener_local: IpEndpoint,
    dst: IpEndpoint,
) -> bool {
    let v6only = listener_payload.with_options(|options| options.ip.ipv6_v6only);
    listener_local.port == dst.port
        && ((listener_local.same_family(dst)
            && (listener_local.ip_addr() == dst.ip_addr() || listener_local.is_unspecified()))
            || (!v6only
                && listener_local.family == crate::net::structure::AddressFamily::Inet6
                && listener_local.is_unspecified()
                && dst.family == crate::net::structure::AddressFamily::Inet))
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

fn step_sctp_connect(
    socket: &Cap<SocketIdentity>,
    payload: &SocketOperationalEvidence,
    remote: IpEndpoint,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let local = match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Init) => unspecified_endpoint(),
        SocketProtocol::Sctp(TcpState::Bound { local }) => {
            select_tcp_connect_local(payload, local, remote)
        }
        SocketProtocol::Sctp(TcpState::Connecting { .. }) => {
            return StepOutcome::Err(Errno::EALREADY)
        }
        SocketProtocol::Sctp(TcpState::Connected { .. }) => {
            return StepOutcome::Err(Errno::EISCONN)
        }
        // Linux 1-to-1 (TCP-style) SCTP returns EISCONN for connect() on a
        // listening socket, the same as on an already-connected one.
        SocketProtocol::Sctp(TcpState::Listening { .. }) => {
            return StepOutcome::Err(Errno::EISCONN)
        }
        SocketProtocol::Sctp(TcpState::Closed) => return StepOutcome::Err(Errno::ENOTCONN),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    if local.is_unspecified() || local.port == 0 {
        return StepOutcome::Err(Errno::EADDRNOTAVAIL);
    }
    if !remote.is_loopback() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    let table = payload.socket_table();
    let Some(listener) = table.lookup_sctp_listener_dual_stack_endpoint(remote, guard) else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(listener_payload) = listener.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let listener_local = match listener_payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Listening {
            local: listener_local,
            ..
        }) if sctp_listener_accepts_incoming(&listener_payload, listener_local, remote) => {
            listener_local
        }
        _ => return StepOutcome::Err(Errno::ECONNREFUSED),
    };

    let child_options = listener_payload.with_options(Clone::clone);
    let child = match registry::create_connected_sctp_for_accept_in_namespace(
        listener_local,
        local,
        child_options,
        payload.net_namespace(),
    ) {
        Ok(child) => child,
        Err(_) => return StepOutcome::Err(Errno::ENOMEM),
    };

    if let Err(error) = table.insert_sctp_connection_pair(
        ConnectionKey::new(local, listener_local),
        socket.clone(),
        ConnectionKey::new(listener_local, local),
        child.clone(),
    ) {
        return StepOutcome::Err(table_error_to_errno(error));
    }

    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Sctp(TcpState::Connected {
            local,
            remote: listener_local,
        });
    });

    let entry = SocketAcceptEntry {
        child,
        local: listener_local,
        peer: local,
        unix_peer: None,
    };
    if listener_payload.enqueue_accept_entry(entry).is_some() {
        listener.readiness.fire_accept(AcceptWireSet::HAS_PENDING);
        StepOutcome::Done(())
    } else {
        let _ = table.withdraw_sctp_connection(ConnectionKey::new(local, listener_local));
        let _ = table.withdraw_sctp_connection(ConnectionKey::new(listener_local, local));
        StepOutcome::Err(Errno::ECONNREFUSED)
    }
}

fn sctp_listener_accepts_incoming(
    listener_payload: &SocketOperationalEvidence,
    listener_local: IpEndpoint,
    dst: IpEndpoint,
) -> bool {
    let v6only = listener_payload.with_options(|options| options.ip.ipv6_v6only);
    listener_local == dst
        || (listener_local.same_family(dst)
            && listener_local.is_unspecified()
            && listener_local.port == dst.port)
        || (!v6only
            && listener_local.family == crate::net::structure::AddressFamily::Inet6
            && listener_local.is_unspecified()
            && dst.family == crate::net::structure::AddressFamily::Inet
            && dst.is_loopback()
            && listener_local.port == dst.port)
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

fn select_tcp_connect_local(
    payload: &SocketOperationalEvidence,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> IpEndpoint {
    if !local.is_unspecified() {
        local
    } else if remote.is_loopback() {
        IpEndpoint::loopback_for_family(remote.family, local.port)
    } else {
        select_routed_local(payload, local, remote).unwrap_or(local)
    }
}

fn select_routed_local(
    payload: &SocketOperationalEvidence,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> Option<IpEndpoint> {
    match remote.ip_addr() {
        crate::net::structure::IpAddress::V4(dst) => payload
            .net_namespace()
            .best_ipv4_route(dst)
            .and_then(|route| route.preferred_src)
            .map(|src| IpEndpoint::new(src, local.port)),
        crate::net::structure::IpAddress::V6(_) => payload
            .net_namespace()
            .link_snapshot()
            .into_iter()
            .find(|link| link.is_up && !link.is_loopback && link.ipv6_addr.is_some())
            .and_then(|link| link.ipv6_addr)
            .map(|src| IpEndpoint::new_v6(src, local.port)),
    }
}
