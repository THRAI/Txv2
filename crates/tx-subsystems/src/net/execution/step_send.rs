use tx_substrate::step::{ByteProgress, StepOutcome};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::checks::require::require_socket_write_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::{socket_send_wait_token, yield_bytes_on_token, ByteStepOutcome};
use crate::net::protocol::UDP_IPV4_MAX_PAYLOAD_BYTES;
use crate::net::structure::{
    IpEndpoint, RecvWireSet, SendRecvFlags, SendWireSet, SocketIdentity, SocketKind, SocketPayload,
    SocketProtocol, TcpState, UnixDatagramState, UnixSocketPath, UnixStreamState,
};

pub fn step_send(
    socket: &Cap<SocketIdentity>,
    len: usize,
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
    if let Some(errno) = udp_payload_len_error(socket.kind, payload.udp_corked_send_len() + len) {
        return StepOutcome::Err(errno);
    }
    if len == 0 {
        return StepOutcome::Done(0);
    }

    let Some(reserve) = payload.reserve_send_space(len) else {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    if !flags.contains(SendRecvFlags::MSG_MORE) {
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
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
    }
    if socket.kind == SocketKind::UnixDatagram {
        return send_unix_datagram_connected(socket, &payload, bytes, guard);
    }

    let Some(reserve) = payload.reserve_send_bytes_with_flags(bytes, flags) else {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    if !flags.contains(SendRecvFlags::MSG_MORE) {
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
    step_send_to_kernel_bytes_with_poll_kick(socket, dst, bytes, flags, guard, true)
}

pub fn step_send_to_unix_path_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    dst: UnixSocketPath,
    bytes: &[u8],
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
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }
    if socket.kind == SocketKind::UnixStream {
        return send_unix_stream_bytes(socket, &payload, bytes, guard);
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

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    if kick_poll && !flags.contains(SendRecvFlags::MSG_MORE) {
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
        SocketProtocol::UnixStream(UnixStreamState::Connected { .. }) => None,
        SocketProtocol::UnixStream(UnixStreamState::Closed) => Some(Errno::ENOTCONN),
        _ if socket.kind == SocketKind::Tcp || socket.kind == SocketKind::UnixStream => {
            Some(Errno::EPIPE)
        }
        _ => None,
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
        SocketProtocol::UnixDatagram(_) => return StepOutcome::Err(Errno::EDESTADDRREQ),
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    send_unix_datagram_to_path(socket, payload, peer, bytes, guard)
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
