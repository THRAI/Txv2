use tx_substrate::step::{ByteProgress, StepOutcome};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::checks::require::require_socket_write_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::{socket_send_wait_token, yield_bytes_on_token, ByteStepOutcome};
use crate::net::protocol::{RawIpv6Packet, UDP_IPV4_MAX_PAYLOAD_BYTES};
use crate::net::structure::{
    AddressFamily, ConnectionKey, IpEndpoint, Ipv6Address, ProtocolNumber, RdsState, RecvWireSet,
    SendRecvFlags, SendWireSet, SocketIdentity, SocketKind, SocketPayload, SocketProtocol,
    TcpState, UnixDatagramState, UnixSocketPath, UnixStreamState,
};

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
    if socket.kind == SocketKind::Sctp {
        return send_sctp_stream_bytes(socket, &payload, bytes, guard);
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
    if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet6 {
        return send_raw_ipv6_loopback(socket, &payload, dst, bytes, guard);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::Sctp {
        return send_sctp_stream_bytes(socket, &payload, bytes, guard);
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
    let Some(peer) = payload
        .socket_table()
        .lookup_tcp_connection(ConnectionKey::new(remote, local), guard)
    else {
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

fn send_raw_ipv6_loopback(
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
    if !destination.is_loopback() && !destination.is_unspecified() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
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
        .unwrap_or(Ipv6Address::LOOPBACK);
    let packet = RawIpv6Packet {
        src: src_addr,
        dst: dst_addr,
        next_header: protocol,
        payload: bytes.to_vec(),
    };

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
                bytes.first().copied(),
            )
        {
            continue;
        }
        if target_payload.record_raw_ipv6_packet(packet.clone()) {
            target.readiness.fire_recv(RecvWireSet::HAS_DATA);
        }
    }

    StepOutcome::Done(bytes.len())
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
        return StepOutcome::Err(Errno::EPIPE);
    }
    let Some(became_readable) = peer_payload.record_sctp_stream_bytes(bytes.to_vec()) else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };
    if became_readable {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    StepOutcome::Done(bytes.len())
}
