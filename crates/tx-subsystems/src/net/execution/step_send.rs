use tx_substrate::step::{ByteProgress, StepOutcome};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::checks::require::require_socket_write_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::{socket_send_wait_token, yield_bytes_on_token, ByteStepOutcome};
use crate::net::namespace::{net_namespace_payloads_snapshot, NetNamespacePayload};
use crate::net::protocol::{
    build_icmpv6_echo_reply_message, parse_icmpv4_payload, parse_icmpv6_payload_unchecked,
    parse_raw_icmpv4_echo_payload_unchecked, Icmpv4Event, Icmpv6Event, RawIpv6Packet,
    UDP_IPV4_MAX_PAYLOAD_BYTES,
};
use crate::net::structure::{
    AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address, Ipv6Address, ProtocolNumber, RdsState,
    RecvWireSet, SendRecvFlags, SendWireSet, SocketIdentity, SocketKind, SocketPayload,
    SocketProtocol, TcpState, UnixDatagramState, UnixSocketPath, UnixStreamState,
};
use crate::net::NetAdminAuthority;
use tx_substrate::zone::PayloadCap;

pub fn step_send(
    socket: &Cap<SocketIdentity>,
    len: usize,
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if let Some(errno) = send_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if let Some(errno) = udp_payload_len_error(socket.kind, payload.udp_corked_send_len() + len) {
        return StepOutcome::Err(errno);
    }
    if let Some(errno) = tcp_connected_peer_error(&payload, guard) {
        return StepOutcome::Err(errno);
    }
    if len == 0 {
        return StepOutcome::Done(0);
    }

    let Some(reserve) = payload.reserve_send_space(len) else {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };

    if reserve.bytes == 0 {
        if reserve.needs_poll_kick {
            net_delegate_kick_poll();
        }
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    }

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    if !flags.contains(SendRecvFlags::MSG_MORE) || reserve.needs_poll_kick {
        net_delegate_kick_poll();
    }
    StepOutcome::Done(reserve.bytes)
}

pub fn step_send_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if let Some(errno) = send_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if let Some(errno) =
        udp_payload_len_error(socket.kind, payload.udp_corked_send_len() + bytes.len())
    {
        return StepOutcome::Err(errno);
    }
    if let Some(errno) = tcp_connected_peer_error(&payload, guard) {
        return StepOutcome::Err(errno);
    }
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::Tcp && tcp_uses_direct_stream(&payload) {
        return send_tcp_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::Sctp {
        return send_sctp_stream_bytes(socket, &payload, bytes, 0, 0, guard);
    }
    if socket.kind == SocketKind::UnixDatagram {
        return send_unix_datagram_connected(socket, &payload, bytes, guard);
    }

    let Some(reserve) = payload.reserve_send_bytes_with_flags(bytes, flags) else {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };

    if reserve.bytes == 0 {
        if reserve.needs_poll_kick {
            net_delegate_kick_poll();
        }
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    }

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    if !flags.contains(SendRecvFlags::MSG_MORE) || reserve.needs_poll_kick {
        net_delegate_kick_poll();
    }
    StepOutcome::Done(reserve.bytes)
}

pub fn step_send_to_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_send_to_kernel_bytes_with_poll_kick(socket, dst, bytes, flags, guard, true)
}

pub fn step_send_to_unix_path_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    dst: UnixSocketPath,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if let Some(errno) = send_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if socket.kind != SocketKind::UnixDatagram {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    send_unix_datagram_to_path(socket, &payload, dst, bytes, guard)
}

pub fn step_send_to_kernel_bytes_with_poll_kick(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
    kick_poll: bool,
) -> ByteStepOutcome<usize> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if let Some(errno) = send_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if let Some(errno) = stream_send_state_error(socket, &payload) {
        return StepOutcome::Err(errno);
    }
    if let Some(errno) = tcp_connected_peer_error(&payload, guard) {
        return StepOutcome::Err(errno);
    }
    if let Some(errno) =
        udp_payload_len_error(socket.kind, payload.udp_corked_send_len() + bytes.len())
    {
        return StepOutcome::Err(errno);
    }
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet {
        if let Some(destination) = dst {
            if destination.family != AddressFamily::Inet {
                return StepOutcome::Err(Errno::EAFNOSUPPORT);
            }
            if !destination.is_loopback() && !destination.is_unspecified() {
                return send_configured_icmpv4_echo(&payload, destination.addr, bytes, guard);
            }
        }
    }
    if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet6 {
        return send_raw_ipv6(socket, &payload, dst, bytes, guard);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::Tcp && tcp_uses_direct_stream(&payload) {
        return send_tcp_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::Sctp {
        return send_sctp_stream_bytes(socket, &payload, bytes, 0, 0, guard);
    }
    if socket.kind == SocketKind::RdsSeqPacket {
        return send_rds_packet(socket, &payload, dst, bytes, guard);
    }
    if socket.kind == SocketKind::UnixDatagram && dst.is_none() {
        return send_unix_datagram_connected(socket, &payload, bytes, guard);
    }

    let reserve = match payload.reserve_send_bytes_to_with_flags(dst, bytes, flags) {
        Ok(Some(reserve)) => reserve,
        Ok(None) => {
            socket.readiness.clear_send(SendWireSet::SPACE);
            return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
        }
        Err(errno) => return StepOutcome::Err(errno),
    };

    if reserve.bytes == 0 {
        if kick_poll && reserve.needs_poll_kick {
            net_delegate_kick_poll();
        }
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    }

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    if kick_poll && (!flags.contains(SendRecvFlags::MSG_MORE) || reserve.needs_poll_kick) {
        net_delegate_kick_poll();
    }
    StepOutcome::Done(reserve.bytes)
}

pub(super) fn send_flags_error(flags: SendRecvFlags) -> Option<Errno> {
    if flags.contains(SendRecvFlags::MSG_OOB) {
        Some(Errno::EOPNOTSUPP)
    } else if flags.contains(SendRecvFlags::MSG_ERRQUEUE) {
        Some(Errno::EINVAL)
    } else {
        None
    }
}

pub(super) fn udp_payload_len_error(kind: SocketKind, len: usize) -> Option<Errno> {
    if kind == SocketKind::Udp && len > UDP_IPV4_MAX_PAYLOAD_BYTES {
        Some(Errno::EMSGSIZE)
    } else {
        None
    }
}

fn stream_send_state_error(socket: &Cap<SocketIdentity>, payload: &SocketPayload) -> Option<Errno> {
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connected { .. }) => None,
        SocketProtocol::Tcp(TcpState::Closed) => Some(Errno::ENOTCONN),
        SocketProtocol::Sctp(TcpState::Connected { .. }) => None,
        SocketProtocol::Sctp(TcpState::Closed) => Some(Errno::ENOTCONN),
        SocketProtocol::UnixStream(UnixStreamState::Connected { .. }) => None,
        SocketProtocol::UnixStream(UnixStreamState::Closed) => Some(Errno::ENOTCONN),
        _ if matches!(
            socket.kind,
            SocketKind::Tcp | SocketKind::Sctp | SocketKind::UnixStream
        ) =>
        {
            Some(Errno::EPIPE)
        }
        _ => None,
    }
}

fn tcp_connected_peer_error(payload: &SocketPayload, guard: &Guard<'_>) -> Option<Errno> {
    let SocketProtocol::Tcp(TcpState::Connected { local, remote }) = payload.protocol_snapshot()
    else {
        return None;
    };
    let Some(peer) = lookup_tcp_connected_peer(payload, local, remote, guard) else {
        return Some(Errno::EPIPE);
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return Some(Errno::EPIPE);
    };
    match peer_payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connected {
            local: peer_local,
            remote: peer_remote,
        }) if peer_local == remote && peer_remote == local => None,
        _ => Some(Errno::EPIPE),
    }
}

fn tcp_uses_direct_stream(payload: &SocketPayload) -> bool {
    matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { .. })
    ) && payload
        .raw_tcp_socket()
        .is_some_and(|raw| !raw.protocol_runtime_state().has_connected)
}

fn lookup_tcp_connected_peer(
    payload: &SocketPayload,
    local: IpEndpoint,
    remote: IpEndpoint,
    guard: &Guard<'_>,
) -> Option<Cap<SocketIdentity>> {
    let key = ConnectionKey::new(remote, local);
    payload
        .socket_table()
        .lookup_tcp_connection(key, guard)
        .or_else(|| {
            net_namespace_payloads_snapshot()
                .into_iter()
                .find_map(|namespace| namespace.socket_table().lookup_tcp_connection(key, guard))
        })
}

fn send_tcp_stream_bytes(
    _socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connected { local, remote }) => (local, remote),
        SocketProtocol::Tcp(TcpState::Closed) => return StepOutcome::Err(Errno::ENOTCONN),
        SocketProtocol::Tcp(_) => return StepOutcome::Err(Errno::EPIPE),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    let Some(peer) = lookup_tcp_connected_peer(payload, local, remote, guard) else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    if !matches!(
        peer_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: peer_local,
            remote: peer_remote,
        }) if peer_local == remote && peer_remote == local
    ) {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if peer_payload.shutdown_rd() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    let Some(became_readable) = peer_payload.record_tcp_stream_bytes(bytes) else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    if became_readable {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}

fn send_unix_datagram_connected(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let peer = match payload.protocol_snapshot() {
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { peer, .. }) => peer,
        SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { peer_raw }) => {
            return send_unix_datagram_to_peer_raw(socket, payload, peer_raw, bytes, guard);
        }
        SocketProtocol::UnixDatagram(_) => return StepOutcome::Err(Errno::EDESTADDRREQ),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    send_unix_datagram_to_path(socket, payload, peer, bytes, guard)
}

fn send_unix_datagram_to_peer_raw(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    peer_raw: u32,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let table = payload.socket_table();
    let Some(target) = table.lookup_unix_peer(socket.raw(), guard) else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    if target.raw() != peer_raw || target.kind != SocketKind::UnixDatagram {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }
    let Some(target_payload) = target.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(became_readable) = target_payload.record_unix_datagram(None, bytes.to_vec()) else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        target.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}

fn send_unix_datagram_to_path(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    dst: UnixSocketPath,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let table = payload.socket_table();
    let Some(target) = table.lookup_unix_bound(dst, guard) else {
        return StepOutcome::Err(Errno::ENOENT);
    };
    if target.kind != SocketKind::UnixDatagram {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }
    let source = match payload.protocol_snapshot() {
        SocketProtocol::UnixDatagram(UnixDatagramState::Bound { local }) => Some(local),
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local, .. }) => local,
        SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { .. }) => None,
        SocketProtocol::UnixDatagram(UnixDatagramState::Unbound) => None,
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    let Some(target_payload) = target.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(became_readable) = target_payload.record_unix_datagram(source, bytes.to_vec()) else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        target.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}

fn send_rds_packet(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let source = match payload.protocol_snapshot() {
        SocketProtocol::Rds(RdsState::Bound { local }) => local,
        SocketProtocol::Rds(RdsState::Unbound) => return StepOutcome::Err(Errno::EDESTADDRREQ),
        SocketProtocol::Rds(RdsState::Closed) => return StepOutcome::Err(Errno::ENOTCONN),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    let Some(destination) = dst else {
        return StepOutcome::Err(Errno::EDESTADDRREQ);
    };
    if destination.family != crate::net::structure::AddressFamily::Inet {
        return StepOutcome::Err(Errno::EAFNOSUPPORT);
    }
    if !destination.is_loopback() && !destination.is_unspecified() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    let table = payload.socket_table();
    let Some(target) = table.lookup_rds_bound(destination, guard) else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    if target.raw() == socket.raw() || target.kind != SocketKind::RdsSeqPacket {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }
    let Some(target_payload) = target.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(became_readable) =
        target_payload.record_rds_packet(source, destination, bytes.to_vec())
    else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        target.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}

fn send_configured_icmpv4_echo(
    payload: &SocketPayload,
    dst_addr: Ipv4Address,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    if payload.raw_icmp_protocol() != Some(ProtocolNumber(1)) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    let multicast_local = ipv4_multicast_group_is_joined_locally(dst_addr);
    if !multicast_local && !ipv4_addr_is_configured(dst_addr) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    let Some(src_addr) = payload
        .raw_icmp_bound_local()
        .filter(|addr| *addr != Ipv4Address::UNSPECIFIED)
        .or_else(|| preferred_ipv4_source_for(&payload.net_namespace(), dst_addr))
        .or_else(|| {
            multicast_local
                .then(|| first_configured_ipv4_source(&payload.net_namespace()))
                .flatten()
        })
    else {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    };
    let request = match parse_icmpv4_payload(src_addr, dst_addr, bytes) {
        Icmpv4Event::EchoRequest(request) => request,
        Icmpv4Event::Malformed => {
            match parse_raw_icmpv4_echo_payload_unchecked(src_addr, dst_addr, bytes) {
                Icmpv4Event::EchoRequest(request) => request,
                Icmpv4Event::Malformed => return StepOutcome::Err(Errno::EINVAL),
                Icmpv4Event::EchoReply(_) | Icmpv4Event::Unsupported => {
                    return StepOutcome::Err(Errno::EOPNOTSUPP);
                }
            }
        }
        Icmpv4Event::EchoReply(_) | Icmpv4Event::Unsupported => {
            return StepOutcome::Err(Errno::EOPNOTSUPP);
        }
    };
    // A multicast echo is answered from the responding host's own unicast
    // address (never from the group address) and addressed back to the sender.
    let reply = if multicast_local {
        crate::net::protocol::Icmpv4EchoPacket {
            src: src_addr,
            dst: request.src,
            ident: request.ident,
            seq_no: request.seq_no,
            payload: request.payload.clone(),
        }
    } else {
        request.reply_packet()
    };
    deliver_icmpv4_reply_to_table(payload, reply, guard);
    StepOutcome::Done(bytes.len())
}

fn deliver_icmpv4_reply_to_table(
    payload: &SocketPayload,
    reply: crate::net::protocol::Icmpv4EchoPacket,
    guard: &Guard<'_>,
) {
    for target in payload.socket_table().snapshot_raw_icmp(guard) {
        if target.kind != SocketKind::RawIcmp {
            continue;
        }
        let Some(target_payload) = target.acquire_operational() else {
            continue;
        };
        if target_payload.family() != AddressFamily::Inet {
            continue;
        }
        let accepts_destination = match target_payload.protocol_snapshot() {
            SocketProtocol::RawIcmp(state) => {
                state.protocol == ProtocolNumber(1) && state.accepts_ipv4_reply_to(reply.dst)
            }
            _ => false,
        };
        if !accepts_destination {
            continue;
        }
        if target_payload.record_icmp_recv_echo_reply(reply.clone()) {
            target.readiness.fire_recv(RecvWireSet::HAS_DATA);
        }
    }
}

fn send_raw_ipv6(
    _socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let Some(destination) = dst else {
        return StepOutcome::Err(Errno::EDESTADDRREQ);
    };
    if destination.family != AddressFamily::Inet6 {
        return StepOutcome::Err(Errno::EAFNOSUPPORT);
    }

    let checksum_offset = payload.with_options(|options| options.ip.ipv6_checksum);
    if checksum_offset >= 0 {
        let offset = checksum_offset as usize;
        if offset.checked_add(2).is_none_or(|end| end > bytes.len()) {
            return StepOutcome::Err(Errno::EINVAL);
        }
    }

    let protocol = payload.raw_icmp_protocol().unwrap_or(ProtocolNumber(58));
    let dst_addr = if destination.addr6.is_unspecified() {
        Ipv6Address::LOOPBACK
    } else {
        destination.addr6
    };
    let src_addr = payload
        .raw_icmp_bound_local6()
        .filter(|addr| !addr.is_unspecified())
        .or_else(|| preferred_ipv6_source_for(&payload.net_namespace(), dst_addr))
        .unwrap_or(Ipv6Address::LOOPBACK);

    if !destination.is_loopback() && !destination.is_unspecified() {
        return send_configured_icmpv6_echo(payload, protocol, src_addr, dst_addr, bytes, guard);
    }

    let packet = RawIpv6Packet {
        src: src_addr,
        dst: dst_addr,
        next_header: protocol,
        payload: bytes.to_vec(),
    };

    deliver_raw_ipv6_packet_to_table(
        payload,
        protocol,
        dst_addr,
        bytes.first().copied(),
        packet,
        guard,
    );
    StepOutcome::Done(bytes.len())
}

fn send_configured_icmpv6_echo(
    payload: &SocketPayload,
    protocol: ProtocolNumber,
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    if protocol != ProtocolNumber(58) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    if !ipv6_addr_is_configured(dst_addr) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    let request = match parse_icmpv6_payload_unchecked(src_addr, dst_addr, bytes) {
        Icmpv6Event::EchoRequest(request) => request,
        Icmpv6Event::Malformed => return StepOutcome::Err(Errno::EINVAL),
        Icmpv6Event::EchoReply(_) | Icmpv6Event::Unsupported => {
            return StepOutcome::Err(Errno::EOPNOTSUPP)
        }
    };
    learn_configured_icmpv6_neighbor(payload, src_addr, dst_addr);
    let reply = request.reply_packet();
    let reply_payload = build_icmpv6_echo_reply_message(&reply);
    let reply_packet = RawIpv6Packet {
        src: reply.src,
        dst: reply.dst,
        next_header: protocol,
        payload: reply_payload,
    };
    let packet_type = reply_packet.payload.first().copied();
    deliver_raw_ipv6_packet_to_table(
        payload,
        protocol,
        reply.dst,
        packet_type,
        reply_packet,
        guard,
    );
    StepOutcome::Done(bytes.len())
}

fn learn_configured_icmpv6_neighbor(
    payload: &SocketPayload,
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
) {
    let netns = payload.net_namespace();
    let Some(link) = netns.link_snapshot().into_iter().find(|link| {
        link.is_up
            && !link.is_loopback
            && link.ipv6_addr == Some(src_addr)
            && ipv6_prefix_matches(src_addr, dst_addr, link.ipv6_prefix_len.unwrap_or(128))
    }) else {
        return;
    };
    let Some(peer_mac) = configured_ipv6_peer_mac(dst_addr) else {
        return;
    };
    let _ = netns.install_static_ndisc_by_ifindex(
        NetAdminAuthority::for_test_or_bootstrap(),
        link.ifindex,
        dst_addr,
        peer_mac,
    );
}

fn deliver_raw_ipv6_packet_to_table(
    payload: &SocketPayload,
    protocol: ProtocolNumber,
    dst_addr: Ipv6Address,
    packet_type: Option<u8>,
    packet: RawIpv6Packet,
    guard: &Guard<'_>,
) {
    for target in payload.socket_table().snapshot_raw_icmp(guard) {
        if target.kind != SocketKind::RawIcmp {
            continue;
        }
        let Some(target_payload) = target.acquire_operational() else {
            continue;
        };
        if target_payload.family() != AddressFamily::Inet6 {
            continue;
        }
        if target_payload.raw_icmp_protocol() != Some(protocol) {
            continue;
        }
        let accepts_destination = target_payload
            .raw_icmp_bound_local6()
            .is_none_or(|local| local.is_unspecified() || local == dst_addr);
        if !accepts_destination {
            continue;
        }
        if protocol == ProtocolNumber(58)
            && !icmp6_filter_accepts(
                target_payload.raw_icmp6_filter().unwrap_or([0; 8]),
                packet_type,
            )
        {
            continue;
        }
        if target_payload.record_raw_ipv6_packet(packet.clone()) {
            target.readiness.fire_recv(RecvWireSet::HAS_DATA);
        }
    }
}

fn icmp6_filter_accepts(filter: [u32; 8], packet_type: Option<u8>) -> bool {
    let Some(packet_type) = packet_type else {
        return true;
    };
    let bit = packet_type as usize;
    let word = bit / 32;
    let shift = bit % 32;
    filter
        .get(word)
        .is_none_or(|word| (word & (1u32 << shift)) == 0)
}

fn preferred_ipv6_source_for(
    net_namespace: &PayloadCap<NetNamespacePayload>,
    dst: Ipv6Address,
) -> Option<Ipv6Address> {
    let mut fallback = None;
    for link in net_namespace.link_snapshot() {
        if !link.is_up {
            continue;
        }
        let Some(addr) = link.ipv6_addr else {
            continue;
        };
        if addr.is_unspecified() {
            continue;
        }
        if fallback.is_none() && addr != Ipv6Address::LOOPBACK {
            fallback = Some(addr);
        }
        if ipv6_prefix_matches(addr, dst, link.ipv6_prefix_len.unwrap_or(128)) {
            return Some(addr);
        }
    }
    fallback
}

fn ipv6_addr_is_configured(addr: Ipv6Address) -> bool {
    net_namespace_payloads_snapshot()
        .into_iter()
        .any(|namespace| {
            namespace
                .link_snapshot()
                .into_iter()
                .any(|link| link.is_up && link.ipv6_addr == Some(addr))
        })
}

fn ipv4_addr_is_configured(addr: Ipv4Address) -> bool {
    net_namespace_payloads_snapshot()
        .into_iter()
        .any(|namespace| {
            namespace
                .link_snapshot()
                .into_iter()
                .any(|link| link.is_up && link.ipv4_addr == Some(addr))
        })
}

const IPV4_ALL_HOSTS_GROUP: Ipv4Address = Ipv4Address::new([224, 0, 0, 1]);

/// The all-hosts group (224.0.0.1) is implicitly joined by every multicast-
/// capable interface, so the local host always answers an ICMP echo to it
/// (we keep broadcast/multicast echo replies enabled, i.e. the
/// `icmp_echo_ignore_broadcasts` sysctl reads 0).
fn ipv4_multicast_group_is_joined_locally(addr: Ipv4Address) -> bool {
    addr == IPV4_ALL_HOSTS_GROUP
}

fn first_configured_ipv4_source(
    net_namespace: &PayloadCap<NetNamespacePayload>,
) -> Option<Ipv4Address> {
    net_namespace
        .link_snapshot()
        .into_iter()
        .find(|link| link.is_up && !link.is_loopback && link.ipv4_addr.is_some())
        .and_then(|link| link.ipv4_addr)
}

fn preferred_ipv4_source_for(
    net_namespace: &PayloadCap<NetNamespacePayload>,
    dst: Ipv4Address,
) -> Option<Ipv4Address> {
    let route = net_namespace.best_ipv4_route(dst)?;
    route.preferred_src.or_else(|| {
        net_namespace
            .link_snapshot()
            .into_iter()
            .find(|link| {
                link.name == route.oif_name
                    && link.is_up
                    && !link.is_loopback
                    && link.ipv4_addr.is_some()
            })
            .and_then(|link| link.ipv4_addr)
    })
}

fn configured_ipv6_peer_mac(addr: Ipv6Address) -> Option<crate::net::EthernetAddress> {
    for namespace in net_namespace_payloads_snapshot() {
        for link in namespace.link_snapshot() {
            if link.is_up && link.ipv6_addr == Some(addr) {
                if let Some(mac) = link.mac {
                    return Some(mac);
                }
            }
        }
    }
    None
}

fn ipv6_prefix_matches(lhs: Ipv6Address, rhs: Ipv6Address, prefix_len: u8) -> bool {
    let prefix_len = prefix_len.min(128);
    let whole_bytes = usize::from(prefix_len / 8);
    let remaining_bits = prefix_len % 8;
    let lhs = lhs.octets();
    let rhs = rhs.octets();
    if lhs[..whole_bytes] != rhs[..whole_bytes] {
        return false;
    }
    if remaining_bits == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - remaining_bits);
    (lhs[whole_bytes] & mask) == (rhs[whole_bytes] & mask)
}

fn send_unix_stream_bytes(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    bytes: &[u8],
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let table = payload.socket_table();
    let Some(peer) = table.lookup_unix_stream_peer(socket.raw(), guard) else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    if peer_payload.shutdown_rd() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    let Some(became_readable) = peer_payload.record_unix_stream_bytes(bytes.to_vec()) else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}

fn send_sctp_stream_bytes(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    bytes: &[u8],
    stream: u16,
    ppid: u32,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Connected { local, remote }) => (local, remote),
        SocketProtocol::Sctp(TcpState::Closed) => return StepOutcome::Err(Errno::ENOTCONN),
        SocketProtocol::Sctp(_) => return StepOutcome::Err(Errno::EPIPE),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    let table = payload.socket_table();
    let Some(peer) = table.lookup_sctp_connection(ConnectionKey::new(remote, local), guard) else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return StepOutcome::Err(Errno::EPIPE);
    };
    if !matches!(
        peer_payload.protocol_snapshot(),
        SocketProtocol::Sctp(TcpState::Connected {
            local: peer_local,
            remote: peer_remote,
        }) if peer_local == remote && peer_remote == local
    ) {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if peer_payload.shutdown_rd() {
        // The peer shut down its read side: the message is accepted and silently
        // discarded (the peer's recv returns EOF), not an EPIPE error.
        return StepOutcome::Done(bytes.len());
    }
    let Some(became_readable) =
        peer_payload.record_sctp_message(bytes.to_vec(), false, stream, ppid, None)
    else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}

/// Send one SCTP message carrying its sctp_sndrcvinfo (stream/ppid), used by
/// sendmsg with an SCTP_SNDRCV control message. Mirrors the send preamble of
/// `step_send_kernel_bytes` but threads the ancillary stream/ppid to the peer.
pub fn step_send_sctp_message(
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
    stream: u16,
    ppid: u32,
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if let Some(errno) = send_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    send_sctp_stream_bytes(socket, &payload, bytes, stream, ppid, guard)
}

/// 1-to-many (SEQPACKET) send: deliver one message to the peer socket bound or
/// listening at `dst`, creating the association on first contact (with COMM_UP
/// to both subscribed ends). The message is delivered to the peer's own receive
/// queue (no accept), tagged with this socket's endpoint as the source so the
/// peer's recvmsg fills msg_name.
pub fn step_send_sctp_seqpacket(
    socket: &Cap<SocketIdentity>,
    dst: IpEndpoint,
    bytes: &[u8],
    stream: u16,
    ppid: u32,
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if let Some(errno) = send_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    let local = match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Bound { local })
        | SocketProtocol::Sctp(TcpState::Listening { local, .. })
        | SocketProtocol::Sctp(TcpState::Connecting { local, .. })
        | SocketProtocol::Sctp(TcpState::Connected { local, .. }) => local,
        _ => return StepOutcome::Err(Errno::EADDRNOTAVAIL),
    };
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    if !dst.is_loopback() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    let table = payload.socket_table();
    let Some(peer) = table
        .lookup_sctp_listener_dual_stack_endpoint(dst, guard)
        .or_else(|| table.lookup_sctp_bound(dst, guard))
    else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };

    // On first contact, establish the association on both ends and surface
    // COMM_UP to whichever side subscribed to association events.
    let is_new = payload
        .sctp_ensure_assoc(dst)
        .map_or(false, |(_, is_new)| is_new);
    if is_new {
        super::step_connect::enqueue_sctp_comm_up(socket);
        let _ = peer_payload.sctp_ensure_assoc(local);
        super::step_connect::enqueue_sctp_comm_up(&peer);
    }

    let Some(became_readable) =
        peer_payload.record_sctp_message(bytes.to_vec(), false, stream, ppid, Some(local))
    else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}
