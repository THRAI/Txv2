//! Socket syscall/fd-facing facade over net execution steps.

mod capability;
mod driver;
mod ops;

pub use capability::{
    SocketAcceptCapability, SocketBindCapability, SocketConnectCapability, SocketCreateOutput,
    SocketHandle, SocketHandleFlags, SocketListenCapability, SocketPollCapability,
    SocketRecvCapability, SocketSendCapability, SocketShutdownCapability,
};
pub use driver::{drive_socket_connect_waiting, drive_socket_nonblocking, SocketFacadeDriveMode};
pub use ops::{
    SocketAcceptOps, SocketBindOps, SocketConnectOps, SocketListenOps, SocketPollOps,
    SocketRecvOps, SocketSendOps, SocketShutdownOps,
};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::execution::{
    step_accept, step_bind, step_connect, step_listen, step_poll_ready, step_recv, step_send,
    step_shutdown, step_socket_create, ByteStepOutcome, ShutdownOutcome, SocketAcceptOutcome,
};
use crate::net::structure::{PollMask, ValidSocketType};

pub fn socket_create_facade(
    domain: i32,
    type_: i32,
    protocol: i32,
    guard: &Guard<'_>,
) -> StepOutcome<SocketCreateOutput> {
    let valid = match ValidSocketType::validate(domain, type_, protocol) {
        Ok(valid) => valid,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let flags = SocketHandleFlags::from_sock_flags(valid.flags);

    match step_socket_create(valid, guard) {
        StepOutcome::Done(identity) => StepOutcome::Done(SocketCreateOutput {
            handle: SocketHandle::new(identity, flags),
        }),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => StepOutcome::Err(Errno::EIO),
        StepOutcome::Err(errno) => StepOutcome::Err(errno),
    }
}

pub fn socket_bind_facade(cap: SocketBindCapability, guard: &Guard<'_>) -> StepOutcome<()> {
    step_bind(&cap.handle.identity, cap.addr, guard)
}

pub fn socket_listen_facade(cap: SocketListenCapability, guard: &Guard<'_>) -> StepOutcome<()> {
    step_listen(&cap.handle.identity, cap.backlog, guard)
}

pub fn socket_connect_facade(cap: SocketConnectCapability, guard: &Guard<'_>) -> StepOutcome<()> {
    step_connect(&cap.handle.identity, cap.remote, guard)
}

pub fn socket_recv_facade(cap: SocketRecvCapability, guard: &Guard<'_>) -> ByteStepOutcome<usize> {
    step_recv(&cap.handle.identity, cap.len, cap.flags, guard)
}

pub fn socket_send_facade(cap: SocketSendCapability, guard: &Guard<'_>) -> ByteStepOutcome<usize> {
    step_send(&cap.handle.identity, cap.len, cap.flags, guard)
}

pub fn socket_accept_facade(
    cap: SocketAcceptCapability,
    guard: &Guard<'_>,
) -> StepOutcome<SocketAcceptOutcome> {
    step_accept(&cap.handle.identity, guard)
}

pub fn socket_shutdown_facade(
    cap: SocketShutdownCapability,
    guard: &Guard<'_>,
) -> StepOutcome<ShutdownOutcome> {
    step_shutdown(&cap.handle.identity, cap.how, guard)
}

pub fn socket_poll_ready_facade(
    cap: SocketPollCapability,
    guard: &Guard<'_>,
) -> StepOutcome<PollMask> {
    match step_poll_ready(&cap.handle.identity, guard) {
        StepOutcome::Done(mask) => StepOutcome::Done(mask & cap.interest),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => StepOutcome::Err(Errno::EIO),
        StepOutcome::Err(errno) => StepOutcome::Err(errno),
    }
}
