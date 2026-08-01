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
    AcceptWireSet, ConnectionKey, IpEndpoint, Ipv4Address, KernelSockAddr, RecvWireSet,
    SendWireSet, SocketAcceptEntry, SocketIdentity, SocketKind, SocketOperationalEvidence,
    SocketProtocol, SocketType, TcpState, UdpInner, UnixDatagramState, UnixSocketPath,
    UnixStreamState,
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
        // No in-kernel namespace owns the remote: this is an EXTERNAL TCP
        // connect over the real device. Emit the SYN into smoltcp and
        // register the client in the connection table so the inbound
        // SYN-ACK matches in process_tcp_event. Kept after the
        // local-namespace short-circuit so loopback / intra-namespace
        // connects are unaffected.
        try_tcp_external_connect(socket, &payload);
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

/// Emit the SYN for an EXTERNAL TCP connect (no in-kernel namespace owns the
/// remote) and register the client in the connection table.
///
/// Drives smoltcp into SynSent via `connect_endpoint` (mirroring the loopback
/// path in step_tcp_loopback.rs) so the device-TX scan ships the SYN, and
/// inserts the client under `ConnectionKey::new(local, remote)` so the inbound
/// SYN-ACK (src=remote, dst=local) matches `lookup_tcp_connection(dst, src)`
/// in process_tcp_event. Both the smoltcp connect and the table insert are
/// best-effort: a smoltcp Err means it is already connecting, and a table
/// Duplicate means the client is already registered (re-entrant connect/poll)
/// — neither is fatal, so connect() still parks on its send carrier.
fn try_tcp_external_connect(socket: &Cap<SocketIdentity>, payload: &SocketOperationalEvidence) {
    if socket.kind != SocketKind::Tcp {
        return;
    }
    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => (local, remote),
        _ => return,
    };
    if remote.is_loopback() || local.is_unspecified() || local.port == 0 {
        return;
    }

    if let Some(raw) = payload.raw_tcp_socket() {
        // Ignore Err: smoltcp is already in a connecting state.
        let _ = raw.connect_endpoint(local, remote);
    }
    // Ignore a Duplicate: the client is already registered for this 4-tuple.
    let _ = payload
        .socket_table()
        .insert_tcp_connection(ConnectionKey::new(local, remote), socket.clone());
    net_delegate_kick_poll();
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
        // A 1-to-many (SEQPACKET) listening socket may also initiate new
        // associations; a 1-to-1 (TCP-style) listening socket cannot, so connect()
        // on it is EISCONN, the same as on an already-connected one.
        SocketProtocol::Sctp(TcpState::Listening { local, .. }) => {
            if payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket) {
                select_tcp_connect_local(payload, local, remote)
            } else {
                return StepOutcome::Err(Errno::EISCONN);
            }
        }
        SocketProtocol::Sctp(TcpState::Closed) => return StepOutcome::Err(Errno::ENOTCONN),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    if local.is_unspecified() || local.port == 0 {
        return StepOutcome::Err(Errno::EADDRNOTAVAIL);
    }

    let table = payload.socket_table();
    // A 1-to-many socket cannot re-create an association that has been peeled off
    // (its (local, remote) connection slot is owned by the peeled 1-to-1 socket).
    // Checked before the loopback gate so a peeled multi-homed peer address (e.g.
    // 127.0.1.x) still reports EADDRNOTAVAIL rather than EOPNOTSUPP.
    if payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket)
        && table
            .lookup_sctp_connection(ConnectionKey::new(local, remote), guard)
            .is_some()
    {
        return StepOutcome::Err(Errno::EADDRNOTAVAIL);
    }
    if !remote.is_loopback() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
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

    // 1-to-many (SEQPACKET): connect() establishes an association directly on the
    // listener socket — no accept queue, no child socket. Both ends record the
    // association and receive COMM_UP; the connecting socket remains a 1-to-many
    // socket (subsequent sends route by msg_name or association id). `local` is
    // already a specific address (the wildcard case is rejected above).
    if payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket) {
        // Re-connecting to an endpoint we already have an association with is
        // EISCONN; otherwise establish it now.
        let (client_assoc_id, is_new) = payload
            .sctp_ensure_assoc(listener_local)
            .unwrap_or((0, false));
        if !is_new {
            return StepOutcome::Err(Errno::EISCONN);
        }
        let server_assoc_id = listener_payload
            .sctp_ensure_assoc(local)
            .map_or(0, |(id, _)| id);
        enqueue_sctp_comm_up(&listener, Some(local), server_assoc_id);
        listener.readiness.fire_recv(RecvWireSet::HAS_DATA);
        enqueue_sctp_comm_up(socket, Some(listener_local), client_assoc_id);
        return StepOutcome::Done(());
    }

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
    // The accepted (server-side) association is established immediately on our
    // loopback: deliver its COMM_UP to the child if it subscribed to events
    // (subscription inherited from the listener).
    enqueue_sctp_comm_up(&child, Some(local), 0);

    if let Err(error) = table.insert_sctp_connection_pair(
        ConnectionKey::new(local, listener_local),
        socket.clone(),
        ConnectionKey::new(listener_local, local),
        child.clone(),
    ) {
        return StepOutcome::Err(table_error_to_errno(error));
    }

    // Enqueue onto the listener's accept queue FIRST; only commit this socket to
    // the Connected state once the association is actually accepted. Otherwise a
    // refused connect (accept queue full) would leave the socket wedged in
    // Connected, breaking a later connect() with a spurious EISCONN.
    let entry = SocketAcceptEntry {
        child,
        local: listener_local,
        peer: local,
        unix_peer: None,
    };
    if listener_payload.enqueue_accept_entry(entry).is_none() {
        let _ = table.withdraw_sctp_connection(ConnectionKey::new(local, listener_local));
        let _ = table.withdraw_sctp_connection(ConnectionKey::new(listener_local, local));
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }
    listener.readiness.fire_accept(AcceptWireSet::HAS_PENDING);

    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Sctp(TcpState::Connected {
            local,
            remote: listener_local,
        });
    });
    // Deliver COMM_UP to the connecting side if it subscribed to events.
    enqueue_sctp_comm_up(socket, Some(listener_local), 0);
    StepOutcome::Done(())
}

/// SCTP_SOCKOPT_PEELOFF: split the 1-to-many association named by `assoc_id` off
/// into a new 1-to-1 (TCP-style) socket, returning its identity for the caller
/// to install as a file descriptor. The new socket is Connected to the peer and
/// inherits the parent socket's options.
pub fn step_sctp_peeloff(
    socket: &Cap<SocketIdentity>,
    assoc_id: u32,
    guard: &Guard<'_>,
) -> StepOutcome<Cap<SocketIdentity>> {
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    // Peel-off is a 1-to-many (SEQPACKET) operation; the assoc_id must name one
    // of this socket's associations.
    if !payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket) {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let Some(peer) = payload.sctp_peer_addr_by_assoc(assoc_id) else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let local = match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Bound { local })
        | SocketProtocol::Sctp(TcpState::Listening { local, .. })
        | SocketProtocol::Sctp(TcpState::Connecting { local, .. })
        | SocketProtocol::Sctp(TcpState::Connected { local, .. }) => local,
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    // The peeled-off socket is a 1-to-1 (TCP-style) socket, Connected to the
    // association's peer.
    let mut options = payload.with_options(Clone::clone);
    options.socket.sock_type = SocketType::Stream;
    let child = match registry::create_connected_sctp_for_accept_in_namespace(
        local,
        peer,
        options,
        payload.net_namespace(),
    ) {
        Ok(child) => child,
        Err(_) => return StepOutcome::Err(Errno::ENOMEM),
    };
    // Migrate the association onto the peeled-off socket: register it under
    // (local, peer) so the peer's future messages route to it (the peer is a
    // 1-to-many socket, so only this end is a connection-table entry) and so its
    // close notifies the peer; then drop the association from the 1-to-many parent
    // (a subsequent SCTP_STATUS on this assoc id reports EINVAL).
    let table = payload.socket_table();
    if let Err(error) = table.insert_sctp_connection(ConnectionKey::new(local, peer), child.clone())
    {
        return StepOutcome::Err(table_error_to_errno(error));
    }
    payload.sctp_remove_assoc(assoc_id);
    StepOutcome::Done(child)
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

/// Enqueue an SCTP_ASSOC_CHANGE / SCTP_COMM_UP notification on `socket`'s own
/// receive queue if it subscribed to association events. Delivered ahead of any
/// data so recvmsg surfaces COMM_UP first (with MSG_NOTIFICATION). `peer` is the
/// remote endpoint (recvmsg msg_name); `assoc_id` identifies the association.
pub(crate) fn enqueue_sctp_comm_up(
    socket: &Cap<SocketIdentity>,
    peer: Option<IpEndpoint>,
    assoc_id: u32,
) {
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    if !payload.with_options(|o| o.sctp.event_assoc_change()) {
        return;
    }
    let streams = payload.with_options(|o| o.sctp.initmsg_num_ostreams);
    let bytes = crate::net::execution::sctp_assoc_change_bytes(
        0, /* SCTP_COMM_UP */
        streams, assoc_id,
    );
    if payload
        .record_sctp_message(bytes, true, 0, 0, peer)
        .is_some()
    {
        socket
            .readiness
            .fire_recv(crate::net::structure::RecvWireSet::HAS_DATA);
    }
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
