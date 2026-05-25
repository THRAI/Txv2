use crate::execution::{Guard, StepOutcome};
use crate::net::execution::{ByteStepOutcome, ShutdownOutcome, SocketAcceptOutcome};
use crate::net::facade::{
    socket_accept_facade, socket_bind_facade, socket_connect_facade, socket_listen_facade,
    socket_poll_ready_facade, socket_recv_facade, socket_send_facade, socket_shutdown_facade,
    SocketAcceptCapability, SocketBindCapability, SocketConnectCapability, SocketListenCapability,
    SocketPollCapability, SocketRecvCapability, SocketSendCapability, SocketShutdownCapability,
};
use crate::net::structure::PollMask;

pub trait SocketBindOps {
    fn bind(self, guard: &Guard<'_>) -> StepOutcome<()>;
}

pub trait SocketListenOps {
    fn listen(self, guard: &Guard<'_>) -> StepOutcome<()>;
}

pub trait SocketConnectOps {
    fn connect(self, guard: &Guard<'_>) -> StepOutcome<()>;
}

pub trait SocketRecvOps {
    fn recv(self, guard: &Guard<'_>) -> ByteStepOutcome<usize>;
}

pub trait SocketSendOps {
    fn send(self, guard: &Guard<'_>) -> ByteStepOutcome<usize>;
}

pub trait SocketAcceptOps {
    fn accept(self, guard: &Guard<'_>) -> StepOutcome<SocketAcceptOutcome>;
}

pub trait SocketShutdownOps {
    fn shutdown(self, guard: &Guard<'_>) -> StepOutcome<ShutdownOutcome>;
}

pub trait SocketPollOps {
    fn poll_ready(self, guard: &Guard<'_>) -> StepOutcome<PollMask>;
}

impl SocketBindOps for SocketBindCapability {
    fn bind(self, guard: &Guard<'_>) -> StepOutcome<()> {
        socket_bind_facade(self, guard)
    }
}

impl SocketListenOps for SocketListenCapability {
    fn listen(self, guard: &Guard<'_>) -> StepOutcome<()> {
        socket_listen_facade(self, guard)
    }
}

impl SocketConnectOps for SocketConnectCapability {
    fn connect(self, guard: &Guard<'_>) -> StepOutcome<()> {
        socket_connect_facade(self, guard)
    }
}

impl SocketRecvOps for SocketRecvCapability {
    fn recv(self, guard: &Guard<'_>) -> ByteStepOutcome<usize> {
        socket_recv_facade(self, guard)
    }
}

impl SocketSendOps for SocketSendCapability {
    fn send(self, guard: &Guard<'_>) -> ByteStepOutcome<usize> {
        socket_send_facade(self, guard)
    }
}

impl SocketAcceptOps for SocketAcceptCapability {
    fn accept(self, guard: &Guard<'_>) -> StepOutcome<SocketAcceptOutcome> {
        socket_accept_facade(self, guard)
    }
}

impl SocketShutdownOps for SocketShutdownCapability {
    fn shutdown(self, guard: &Guard<'_>) -> StepOutcome<ShutdownOutcome> {
        socket_shutdown_facade(self, guard)
    }
}

impl SocketPollOps for SocketPollCapability {
    fn poll_ready(self, guard: &Guard<'_>) -> StepOutcome<PollMask> {
        socket_poll_ready_facade(self, guard)
    }
}
