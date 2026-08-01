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
use crate::net::structure::table::SocketTable;
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
    if len == 0 && socket.kind != SocketKind::Udp {
        return StepOutcome::Done(0);
    }

    let Some(reserve) = payload.reserve_send_space(len) else {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };

    if reserve.bytes == 0 && !(socket.kind == SocketKind::Udp && len == 0) {
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
    if let Some(errno) = stream_send_state_error(socket, &payload) {
        return StepOutcome::Err(errno);
    }
    if let Some(errno) =
        udp_payload_len_error(socket.kind, payload.udp_corked_send_len() + bytes.len())
    {
        return StepOutcome::Err(errno);
    }
    if let Some(errno) = tcp_connected_peer_error(&payload, guard) {
        return StepOutcome::Err(errno);
    }
    if bytes.is_empty() && socket.kind != SocketKind::Udp {
        return StepOutcome::Done(0);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::Sctp {
        return send_sctp_stream_bytes(socket, &payload, bytes, 0, 0, guard);
    }
    if socket.kind == SocketKind::UnixDatagram {
        return send_unix_datagram_connected(socket, &payload, bytes, guard);
    }

    let reserve = match payload.reserve_send_bytes_with_flags(bytes, flags) {
        Ok(Some(reserve)) => reserve,
        Ok(None) => {
            socket.readiness.clear_send(SendWireSet::SPACE);
            return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
        }
        Err(errno) => return StepOutcome::Err(errno),
    };

    if reserve.bytes == 0 && !(socket.kind == SocketKind::Udp && bytes.is_empty()) {
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
    if bytes.is_empty() && socket.kind != SocketKind::Udp {
        return StepOutcome::Done(0);
    }
    if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet {
        if let Some(destination) = dst {
            if destination.family != AddressFamily::Inet {
                return StepOutcome::Err(Errno::EAFNOSUPPORT);
            }
            if !destination.is_loopback() && !destination.is_unspecified() {
                // Configured/local-multicast peers keep the synthesized
                // loopback-style reply (netns/LTP flows). A real external
                // destination falls through to the generic reserve below —
                // icmp tx queue → device-TX lane → wire (replies come back
                // through the demux → process_icmp_event → raw socket).
                if ipv4_multicast_group_is_joined_locally(destination.addr)
                    || ipv4_addr_is_configured(destination.addr)
                {
                    return send_configured_icmpv4_echo(&payload, destination.addr, bytes, guard);
                }
            }
        }
    }
    if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet6 {
        return send_raw_ipv6(socket, &payload, dst, bytes, guard);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
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

    if reserve.bytes == 0 && !(socket.kind == SocketKind::Udp && bytes.is_empty()) {
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
        // No in-kernel peer: this is an external (real-device) TCP connection
        // whose peer lives on the wire, not a loopback/intra-kernel pair. The
        // connection is writable as long as its smoltcp socket can still
        // send; only a socket that can no longer send is a genuine EPIPE.
        return match payload.raw_tcp_socket() {
            Some(raw) if raw.may_send() => None,
            _ => Some(Errno::EPIPE),
        };
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
    learn_configured_icmpv4_neighbor(payload, src_addr, dst_addr);
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
    socket: &Cap<SocketIdentity>,
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
        if ipv6_addr_is_configured(dst_addr) {
            return send_configured_icmpv6_echo(payload, protocol, src_addr, dst_addr, bytes, guard);
        }
        // Real external v6 destination: queue for the device-TX lane (mirror
        // of the v4 external echo flow). Replies come back through the wire
        // demux (PacketDispatch::Icmp6) into this raw socket.
        return send_external_icmpv6_echo(socket, payload, protocol, src_addr, dst_addr, bytes);
    }

    let packet = RawIpv6Packet {
        src: src_addr,
        dst: dst_addr,
        next_header: protocol,
        payload: bytes.to_vec(),
    };

    deliver_raw_ipv6_packet_to_table(
        payload.socket_table(),
        protocol,
        dst_addr,
        bytes.first().copied(),
        packet,
        guard,
    );
    StepOutcome::Done(bytes.len())
}

/// External (non-configured) v6 echo: parse the request and queue it for the
/// device-TX lane, mirroring the v4 external flow through the generic reserve.
fn send_external_icmpv6_echo(
    socket: &Cap<SocketIdentity>,
    payload: &SocketPayload,
    protocol: ProtocolNumber,
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    bytes: &[u8],
) -> ByteStepOutcome<usize> {
    if protocol != ProtocolNumber(58) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    let request = match parse_icmpv6_payload_unchecked(src_addr, dst_addr, bytes) {
        Icmpv6Event::EchoRequest(request) => request,
        Icmpv6Event::Malformed => return StepOutcome::Err(Errno::EINVAL),
        Icmpv6Event::EchoReply(_) | Icmpv6Event::Unsupported => {
            return StepOutcome::Err(Errno::EOPNOTSUPP)
        }
    };
    if payload.enqueue_icmp6_tx_echo(request).is_none() {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    }
    net_delegate_kick_poll();
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
        payload.socket_table(),
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

pub(crate) fn deliver_raw_ipv6_packet_to_table(
    table: &SocketTable,
    protocol: ProtocolNumber,
    dst_addr: Ipv6Address,
    packet_type: Option<u8>,
    packet: RawIpv6Packet,
    guard: &Guard<'_>,
) {
    for target in table.snapshot_raw_icmp(guard) {
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

/// Source address for an outgoing raw ICMPv6 packet.
///
/// V5-3: consult the FIB first (same answer the TCP/UDP paths now get), but
/// unlike them keep the old on-link/first-address heuristic as a fallback.
/// `ping6` to a destination with no route should still put a packet on the
/// wire from a real local address — returning None here would make
/// `send_raw_ipv6` fall back to `::1`, which is strictly worse than a
/// best-guess source.
fn preferred_ipv6_source_for(
    net_namespace: &PayloadCap<NetNamespacePayload>,
    dst: Ipv6Address,
) -> Option<Ipv6Address> {
    if let Some(routed) = net_namespace.preferred_ipv6_source(dst) {
        return Some(routed);
    }
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
        .any(|namespace| namespace.ipv6_addr_is_local_up(addr))
}

fn ipv4_addr_is_configured(addr: Ipv4Address) -> bool {
    net_namespace_payloads_snapshot()
        .into_iter()
        .any(|namespace| namespace.ipv4_addr_is_local_up(addr))
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

/// Auto-create an ARP/neighbor entry for the pinged IPv4 dst (mirrors
/// `learn_configured_icmpv6_neighbor`). LTP `ipneigh01` pings a peer then expects
/// `ip neigh show` to list it; our synthetic echo never does real ARP, so install
/// the peer's configured MAC on the sending link. The entry surfaces via
/// `/proc/net/tx_neigh`.
fn learn_configured_icmpv4_neighbor(
    payload: &SocketPayload,
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
) {
    let netns = payload.net_namespace();
    let Some(link) = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.is_up && !link.is_loopback && link.ipv4_addr == Some(src_addr))
    else {
        return;
    };
    let Some(peer_mac) = configured_ipv4_peer_mac(dst_addr) else {
        return;
    };
    let _ = netns.install_static_neighbor_by_ifindex(
        NetAdminAuthority::for_test_or_bootstrap(),
        link.ifindex,
        dst_addr,
        peer_mac,
    );
}

fn configured_ipv4_peer_mac(addr: Ipv4Address) -> Option<crate::net::EthernetAddress> {
    for namespace in net_namespace_payloads_snapshot() {
        for link in namespace.link_snapshot() {
            if link.is_up && link.ipv4_addr == Some(addr) {
                if let Some(mac) = link.mac {
                    return Some(mac);
                }
            }
        }
    }
    None
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
        // No 1-to-1 connection peer: after sctp_peeloff the peer is a 1-to-many
        // (SEQPACKET) socket bound at `remote`. Deliver tagged with our local
        // endpoint as the source so the client attributes it to its association.
        if let Some(peer) = table.lookup_sctp_bound(remote, guard) {
            if let Some(peer_payload) = peer.acquire_operational() {
                if peer_payload.shutdown_rd() {
                    return StepOutcome::Done(bytes.len());
                }
                if let Some(became_readable) = peer_payload.record_sctp_message(
                    bytes.to_vec(),
                    false,
                    stream,
                    ppid,
                    Some(local),
                ) {
                    if became_readable {
                        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
                    }
                    return StepOutcome::Done(bytes.len());
                }
                return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
            }
        }
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
    dst: Option<IpEndpoint>,
    assoc_id: u32,
    bytes: &[u8],
    stream: u16,
    ppid: u32,
    ttl: u32,
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
    // Resolve the destination. An explicit msg_name wins; otherwise route by the
    // sctp_sndrcvinfo association id, which must name an existing association.
    // With neither (no msg_name, no/unknown assoc id) there is nothing to send
    // on — Linux SCTP reports EPIPE.
    let dst = match dst {
        Some(dst) => dst,
        None => match payload
            .sctp_peers()
            .into_iter()
            .find(|assoc| assoc.assoc_id == assoc_id)
        {
            Some(assoc) if assoc_id != 0 => assoc.peer,
            _ => return StepOutcome::Err(Errno::EPIPE),
        },
    };
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    if !dst.is_loopback() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    // PR-SCTP timed reliability: a message sent with a non-zero TTL is abandoned
    // if it cannot be delivered before it expires. We have no rwnd back-pressure or
    // timers on the loopback path, but the lksctp ttl tests always fill the rwnd
    // then sleep past the TTL, so a ttl>0 message is treated as abandoned: it is
    // NOT delivered to the peer, and the sender (if subscribed to the send-failure
    // event) receives one SCTP_SEND_FAILED notification per fragment — sliced at
    // the association fragmentation point (SCTP_MAXSEG) — carrying the dropped data.
    if ttl > 0 {
        if payload.with_options(|o| o.sctp.events_subscribe[3] != 0) {
            let maxseg = payload.with_options(|o| o.sctp.maxseg);
            let frag = if maxseg > 0 { maxseg as usize } else { 1452 };
            let send_assoc_id = payload.sctp_assoc_id_for_peer(dst).unwrap_or(assoc_id);
            let mut offset = 0;
            let mut fired = false;
            while offset < bytes.len() {
                let end = core::cmp::min(offset + frag, bytes.len());
                let last = end == bytes.len();
                let failed = crate::net::execution::sctp_send_failed_bytes(
                    &bytes[offset..end],
                    stream,
                    ppid,
                    send_assoc_id,
                    last,
                );
                if payload
                    .record_sctp_message(failed, true, 0, 0, None)
                    .is_some()
                {
                    fired = true;
                }
                offset = end;
            }
            if fired {
                socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
            }
        }
        return StepOutcome::Done(bytes.len());
    }

    // The source endpoint the peer observes is the local address on the route to
    // dst. When this socket is bound to the wildcard address (INADDR_ANY / ::),
    // that resolves to the destination's (loopback) address rather than the
    // literal 0.0.0.0 / :: — keep the local port.
    let source = if local.is_unspecified() {
        IpEndpoint::from_ip(dst.ip_addr(), local.port)
    } else {
        local
    };

    let table = payload.socket_table();
    // A peeled-off association lives on its own 1-to-1 socket registered under
    // (dst, source); deliver there instead of the 1-to-many listener if present.
    if let Some(peeled) = table.lookup_sctp_connection(ConnectionKey::new(dst, source), guard) {
        if let Some(peeled_payload) = peeled.acquire_operational() {
            if peeled_payload.shutdown_rd() {
                return StepOutcome::Done(bytes.len());
            }
            let Some(became_readable) = peeled_payload.record_sctp_message(
                bytes.to_vec(),
                false,
                stream,
                ppid,
                Some(source),
            ) else {
                return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
            };
            if became_readable {
                peeled.readiness.fire_recv(RecvWireSet::HAS_DATA);
            }
            return StepOutcome::Done(bytes.len());
        }
    }
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
    // COMM_UP to whichever side subscribed to association events. The COMM_UP
    // carries the remote endpoint as msg_name and the association id.
    let (my_assoc_id, is_new) = payload.sctp_ensure_assoc(dst).unwrap_or((0, false));
    if is_new {
        super::step_connect::enqueue_sctp_comm_up(socket, Some(dst), my_assoc_id);
        let peer_assoc_id = peer_payload
            .sctp_ensure_assoc(source)
            .map_or(0, |(id, _)| id);
        super::step_connect::enqueue_sctp_comm_up(&peer, Some(source), peer_assoc_id);
    }

    let Some(became_readable) =
        peer_payload.record_sctp_message(bytes.to_vec(), false, stream, ppid, Some(source))
    else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }

    // SCTP_AUTOCLOSE: a 1-to-many association closes after `autoclose` idle
    // seconds. With no reactor timer on the loopback path we close eagerly once
    // the (first) message is delivered — deliver SHUTDOWN_COMP to both ends after
    // the data and drop the association. Only test_autoclose enables this option.
    if payload.with_options(|o| o.sctp.autoclose) > 0 {
        if payload.with_options(|o| o.sctp.event_assoc_change()) {
            let streams = payload.with_options(|o| o.sctp.initmsg_num_ostreams);
            let bytes = crate::net::execution::sctp_assoc_change_bytes(3, streams, my_assoc_id);
            if payload
                .record_sctp_message(bytes, true, 0, 0, Some(dst))
                .is_some()
            {
                socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
            }
        }
        let peer_assoc_id = peer_payload.sctp_assoc_id_for_peer(source).unwrap_or(0);
        if peer_payload.with_options(|o| o.sctp.event_assoc_change()) {
            let streams = peer_payload.with_options(|o| o.sctp.initmsg_num_ostreams);
            let bytes = crate::net::execution::sctp_assoc_change_bytes(3, streams, peer_assoc_id);
            if peer_payload
                .record_sctp_message(bytes, true, 0, 0, Some(source))
                .is_some()
            {
                peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
            }
        }
        payload.sctp_remove_assoc(my_assoc_id);
        peer_payload.sctp_remove_assoc(peer_assoc_id);
    }
    StepOutcome::Done(bytes.len())
}
