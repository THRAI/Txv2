use tx_substrate::step::{ByteProgress, StepOutcome};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::checks::require::require_socket_read_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::{socket_recv_wait_token, yield_bytes_on_token, ByteStepOutcome};
use crate::net::structure::{
    RecvWireSet, SendRecvFlags, SocketIdentity, SocketKind, SocketPayload, SocketProtocol,
    SocketRecvBytesOutcome, SocketType, TcpState,
};

pub fn step_recv(
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
    let witness = match require_socket_read_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if let Some(errno) = recv_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_rd() || len == 0 {
        return StepOutcome::Done(0);
    }

    if witness.flags.contains(SendRecvFlags::MSG_PEEK) {
        return match payload.peek_recv_bytes(len) {
            Some(bytes) => StepOutcome::Done(bytes),
            None => {
                clear_recv_data_if_empty(socket, &payload);
                if recv_peer_closed(socket, &payload) {
                    StepOutcome::Done(0)
                } else {
                    yield_bytes_on_token(ByteProgress::EMPTY, socket_recv_wait_token(socket))
                }
            }
        };
    }

    let Some(consume) = payload.consume_recv_bytes(len) else {
        clear_recv_data_if_empty(socket, &payload);
        if recv_peer_closed(socket, &payload) {
            return StepOutcome::Done(0);
        }
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_recv_wait_token(socket));
    };

    if consume.became_empty {
        clear_recv_data_if_empty(socket, &payload);
    }
    kick_tcp_loopback_after_recv(&payload, consume.bytes);

    StepOutcome::Done(consume.bytes)
}

pub fn step_recv_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    out: &mut [u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<SocketRecvBytesOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_read_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if let Some(errno) = recv_flags_error(witness.flags) {
        return StepOutcome::Err(errno);
    }
    if payload.shutdown_rd() || out.is_empty() {
        return StepOutcome::Done(SocketRecvBytesOutcome::default());
    }

    let Some(outcome) = payload.consume_recv_bytes_into(out, witness.flags) else {
        clear_recv_data_if_empty(socket, &payload);
        // SCTP 1-to-1: with no data to drain, a recv on a socket whose
        // association is not established — never connected (listening/bound/init)
        // or locally shut down via SHUT_WR — returns ENOTCONN rather than
        // blocking, matching Linux SCTP recvmsg.
        if socket.kind == SocketKind::Sctp && sctp_recv_disconnected(&payload) {
            return StepOutcome::Err(Errno::ENOTCONN);
        }
        if recv_peer_closed(socket, &payload) {
            return StepOutcome::Done(SocketRecvBytesOutcome::default());
        }
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_recv_wait_token(socket));
    };

    if outcome.became_empty && !witness.flags.contains(SendRecvFlags::MSG_PEEK) {
        clear_recv_data_if_empty(socket, &payload);
    }
    if !witness.flags.contains(SendRecvFlags::MSG_PEEK) {
        kick_tcp_loopback_after_recv(&payload, outcome.bytes);
    }

    StepOutcome::Done(outcome)
}

/// Clear an edge hint using the standard clear-then-recheck pattern.
///
/// A producer may enqueue bytes after `recv` released the raw-socket lock but
/// before this CPU clears HAS_DATA.  Re-reading the authoritative queue after
/// the clear prevents that producer's wake from being erased.
fn clear_recv_data_if_empty(socket: &Cap<SocketIdentity>, payload: &SocketPayload) {
    socket.readiness.clear_recv(RecvWireSet::HAS_DATA);
    if payload.io_snapshot().recv_len != 0 {
        socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
}

fn kick_tcp_loopback_after_recv(payload: &SocketPayload, bytes: usize) {
    if bytes == 0 {
        return;
    }
    if matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { .. })
    ) {
        net_delegate_kick_poll();
    }
}

/// True when an SCTP recv with no buffered data should report ENOTCONN. This is
/// a 1-to-1 (TCP-style) concept only: the socket either never had an established
/// association (listening/bound/init/closed/connecting) or has locally shut down
/// the write side (SHUT_WR), tearing the association down. A 1-to-many
/// (SEQPACKET) socket can receive from any peer, so an empty recv blocks instead.
fn sctp_recv_disconnected(payload: &SocketPayload) -> bool {
    let seqpacket = payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket);
    match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Connected { .. }) => payload.shutdown_wr(),
        // 1-to-many: a listening socket (or one with associations) can still
        // receive, so it blocks; one that is neither listening nor associated
        // reports ENOTCONN.
        SocketProtocol::Sctp(TcpState::Listening { .. }) if seqpacket => false,
        SocketProtocol::Sctp(_) if seqpacket => payload.sctp_assoc_count() == 0,
        // 1-to-1 (Stream): anything other than Connected is ENOTCONN.
        SocketProtocol::Sctp(_) => true,
        _ => false,
    }
}

fn recv_peer_closed(socket: &Cap<SocketIdentity>, payload: &SocketPayload) -> bool {
    payload.tcp_recv_closed_by_peer()
        || socket.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0
}

fn recv_flags_error(flags: SendRecvFlags) -> Option<Errno> {
    if flags.contains(SendRecvFlags::MSG_OOB) {
        Some(Errno::EINVAL)
    } else if flags.contains(SendRecvFlags::MSG_ERRQUEUE) {
        Some(Errno::EAGAIN)
    } else {
        None
    }
}
