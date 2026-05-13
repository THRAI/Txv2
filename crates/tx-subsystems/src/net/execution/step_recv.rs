use tx_substrate::step::{ByteProgress, StepOutcome};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::checks::require::require_socket_read_target;
use crate::net::execution::{socket_recv_wait_token, yield_bytes_on_token, ByteStepOutcome};
use crate::net::structure::{RecvWireSet, SendRecvFlags, SocketIdentity, SocketRecvBytesOutcome};

pub fn step_recv(
    socket: &Cap<SocketIdentity>,
    len: usize,
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let witness = match require_socket_read_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if payload.shutdown_rd() || len == 0 {
        return StepOutcome::Done(0);
    }

    if witness.flags.contains(SendRecvFlags::MSG_PEEK) {
        return match payload.peek_recv_bytes(len) {
            Some(bytes) => StepOutcome::Done(bytes),
            None => yield_bytes_on_token(ByteProgress::EMPTY, socket_recv_wait_token(socket)),
        };
    }

    let Some(consume) = payload.consume_recv_bytes(len) else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_recv_wait_token(socket));
    };

    if consume.became_empty {
        socket.readiness.clear_recv(RecvWireSet::HAS_DATA);
    }

    StepOutcome::Done(consume.bytes)
}

pub fn step_recv_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    out: &mut [u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<SocketRecvBytesOutcome> {
    let witness = match require_socket_read_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    if payload.shutdown_rd() || out.is_empty() {
        return StepOutcome::Done(SocketRecvBytesOutcome::default());
    }

    let Some(outcome) = payload.consume_recv_bytes_into(out, witness.flags) else {
        return yield_bytes_on_token(ByteProgress::EMPTY, socket_recv_wait_token(socket));
    };

    if outcome.became_empty && !witness.flags.contains(SendRecvFlags::MSG_PEEK) {
        socket.readiness.clear_recv(RecvWireSet::HAS_DATA);
    }

    StepOutcome::Done(outcome)
}
