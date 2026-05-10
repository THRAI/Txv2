use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
use crate::net::structure::{KernelSockAddr, SendRecvFlags, SockShutdownCmd, SocketIdentity};

use super::predicates::{
    socket_can_accept, socket_can_bind, socket_can_connect, socket_can_listen, socket_can_poll,
    socket_can_read, socket_can_shutdown, socket_can_write, socket_payload_present,
};
use super::witness::{
    SocketAcceptWitness, SocketBindWitness, SocketConnectWitness, SocketListenWitness,
    SocketPayloadLiveWitness, SocketPollWitness, SocketReadWitness, SocketShutdownWitness,
    SocketWriteWitness,
};

pub fn require_socket_payload_live<'g>(
    socket: &Cap<SocketIdentity>,
    guard: &'g Guard<'_>,
) -> Result<SocketPayloadLiveWitness<'g>, Errno> {
    socket_payload_present(socket)?;
    Ok(SocketPayloadLiveWitness {
        identity: socket.ident_ref(guard),
    })
}

pub fn require_socket_read_target<'g>(
    socket: &Cap<SocketIdentity>,
    flags: SendRecvFlags,
    guard: &'g Guard<'_>,
) -> Result<SocketReadWitness<'g>, Errno> {
    socket_can_read(socket, flags)?;
    Ok(SocketReadWitness {
        identity: socket.ident_ref(guard),
        flags,
    })
}

pub fn require_socket_write_target<'g>(
    socket: &Cap<SocketIdentity>,
    flags: SendRecvFlags,
    guard: &'g Guard<'_>,
) -> Result<SocketWriteWitness<'g>, Errno> {
    socket_can_write(socket, flags)?;
    Ok(SocketWriteWitness {
        identity: socket.ident_ref(guard),
        flags,
    })
}

pub fn require_socket_bind_target<'g>(
    socket: &Cap<SocketIdentity>,
    addr: KernelSockAddr,
    guard: &'g Guard<'_>,
) -> Result<SocketBindWitness<'g>, Errno> {
    let local = socket_can_bind(socket, addr)?;
    Ok(SocketBindWitness {
        identity: socket.ident_ref(guard),
        addr,
        local,
    })
}

pub fn require_socket_listen_target<'g>(
    socket: &Cap<SocketIdentity>,
    backlog_limit: usize,
    guard: &'g Guard<'_>,
) -> Result<SocketListenWitness<'g>, Errno> {
    let local = socket_can_listen(socket)?;
    Ok(SocketListenWitness {
        identity: socket.ident_ref(guard),
        backlog_limit,
        local,
    })
}

pub fn require_socket_connect_target<'g>(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
    guard: &'g Guard<'_>,
) -> Result<SocketConnectWitness<'g>, Errno> {
    let remote = socket_can_connect(socket, remote)?;
    Ok(SocketConnectWitness {
        identity: socket.ident_ref(guard),
        remote,
    })
}

pub fn require_socket_accept_target<'g>(
    socket: &Cap<SocketIdentity>,
    guard: &'g Guard<'_>,
) -> Result<SocketAcceptWitness<'g>, Errno> {
    socket_can_accept(socket)?;
    Ok(SocketAcceptWitness {
        identity: socket.ident_ref(guard),
    })
}

pub fn require_socket_shutdown_target<'g>(
    socket: &Cap<SocketIdentity>,
    how: SockShutdownCmd,
    guard: &'g Guard<'_>,
) -> Result<SocketShutdownWitness<'g>, Errno> {
    socket_can_shutdown(socket, how)?;
    Ok(SocketShutdownWitness {
        identity: socket.ident_ref(guard),
        how,
    })
}

pub fn require_socket_poll_target<'g>(
    socket: &Cap<SocketIdentity>,
    guard: &'g Guard<'_>,
) -> Result<SocketPollWitness<'g>, Errno> {
    socket_can_poll(socket)?;
    Ok(SocketPollWitness {
        identity: socket.ident_ref(guard),
    })
}
