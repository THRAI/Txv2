use tx_substrate::step::{ByteProgress, StepOutcome};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::checks::require::require_socket_write_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::{socket_send_wait_token, yield_bytes_on_token, ByteStepOutcome};
use crate::net::structure::{IpEndpoint, SendRecvFlags, SendWireSet, SocketIdentity};

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
    let _flags = witness.flags;

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
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

    net_delegate_kick_poll();
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
    let _flags = witness.flags;

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }

    let Some(reserve) = payload.reserve_send_bytes(bytes) else {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_send_wait_token(socket));
    };

    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }

    net_delegate_kick_poll();
    StepOutcome::Done(reserve.bytes)
}

pub fn step_send_to_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());
    let _flags = witness.flags;

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if bytes.is_empty() {
        return StepOutcome::Done(0);
    }

    let reserve = match payload.reserve_send_bytes_to(dst, bytes) {
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

    net_delegate_kick_poll();
    StepOutcome::Done(reserve.bytes)
}
