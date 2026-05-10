use tx_substrate::zone::Cap;

use crate::net::structure::{
    KernelSockAddr, PollMask, SendRecvFlags, SockFlags, SockShutdownCmd, SocketIdentity,
};

#[derive(Clone)]
pub struct SocketHandle {
    pub identity: Cap<SocketIdentity>,
    pub flags: SocketHandleFlags,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketHandleFlags {
    pub nonblock: bool,
    pub cloexec: bool,
}

#[derive(Clone)]
pub struct SocketCreateOutput {
    pub handle: SocketHandle,
}

#[derive(Clone)]
pub struct SocketBindCapability {
    pub handle: SocketHandle,
    pub addr: KernelSockAddr,
}

#[derive(Clone)]
pub struct SocketListenCapability {
    pub handle: SocketHandle,
    pub backlog: usize,
}

#[derive(Clone)]
pub struct SocketConnectCapability {
    pub handle: SocketHandle,
    pub remote: KernelSockAddr,
}

#[derive(Clone)]
pub struct SocketRecvCapability {
    pub handle: SocketHandle,
    pub len: usize,
    pub flags: SendRecvFlags,
}

#[derive(Clone)]
pub struct SocketSendCapability {
    pub handle: SocketHandle,
    pub len: usize,
    pub flags: SendRecvFlags,
}

#[derive(Clone)]
pub struct SocketAcceptCapability {
    pub handle: SocketHandle,
}

#[derive(Clone)]
pub struct SocketShutdownCapability {
    pub handle: SocketHandle,
    pub how: SockShutdownCmd,
}

#[derive(Clone)]
pub struct SocketPollCapability {
    pub handle: SocketHandle,
    pub interest: PollMask,
}

impl SocketHandleFlags {
    pub const fn empty() -> Self {
        Self {
            nonblock: false,
            cloexec: false,
        }
    }

    pub const fn from_sock_flags(flags: SockFlags) -> Self {
        Self {
            nonblock: flags.contains(SockFlags::SOCK_NONBLOCK),
            cloexec: flags.contains(SockFlags::SOCK_CLOEXEC),
        }
    }
}

impl SocketHandle {
    pub fn new(identity: Cap<SocketIdentity>, flags: SocketHandleFlags) -> Self {
        Self { identity, flags }
    }

    pub fn bind_capability(&self, addr: KernelSockAddr) -> SocketBindCapability {
        SocketBindCapability {
            handle: self.clone(),
            addr,
        }
    }

    pub fn listen_capability(&self, backlog: usize) -> SocketListenCapability {
        SocketListenCapability {
            handle: self.clone(),
            backlog,
        }
    }

    pub fn connect_capability(&self, remote: KernelSockAddr) -> SocketConnectCapability {
        SocketConnectCapability {
            handle: self.clone(),
            remote,
        }
    }

    pub fn recv_capability(&self, len: usize, flags: SendRecvFlags) -> SocketRecvCapability {
        SocketRecvCapability {
            handle: self.clone(),
            len,
            flags,
        }
    }

    pub fn send_capability(&self, len: usize, flags: SendRecvFlags) -> SocketSendCapability {
        SocketSendCapability {
            handle: self.clone(),
            len,
            flags,
        }
    }

    pub fn accept_capability(&self) -> SocketAcceptCapability {
        SocketAcceptCapability {
            handle: self.clone(),
        }
    }

    pub fn shutdown_capability(&self, how: SockShutdownCmd) -> SocketShutdownCapability {
        SocketShutdownCapability {
            handle: self.clone(),
            how,
        }
    }

    pub fn poll_capability(&self, interest: PollMask) -> SocketPollCapability {
        SocketPollCapability {
            handle: self.clone(),
            interest,
        }
    }
}
