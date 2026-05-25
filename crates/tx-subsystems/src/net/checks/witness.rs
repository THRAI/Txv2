use tx_substrate::zone::IdentRef;

use crate::net::structure::{
    IpEndpoint, KernelSockAddr, SendRecvFlags, SockShutdownCmd, SocketIdentity,
};

#[derive(Debug)]
pub struct SocketPayloadLiveWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
}

#[derive(Debug)]
pub struct SocketReadWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
    pub flags: SendRecvFlags,
}

#[derive(Debug)]
pub struct SocketWriteWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
    pub flags: SendRecvFlags,
}

#[derive(Debug)]
pub struct SocketBindWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
    pub addr: KernelSockAddr,
    pub local: IpEndpoint,
}

#[derive(Debug)]
pub struct SocketListenWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
    pub backlog_limit: usize,
    pub local: IpEndpoint,
}

#[derive(Debug)]
pub struct SocketConnectWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
    pub remote: IpEndpoint,
}

#[derive(Debug)]
pub struct SocketAcceptWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
}

#[derive(Debug)]
pub struct SocketShutdownWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
    pub how: SockShutdownCmd,
}

#[derive(Debug)]
pub struct SocketPollWitness<'g> {
    pub identity: IdentRef<'g, SocketIdentity>,
}
