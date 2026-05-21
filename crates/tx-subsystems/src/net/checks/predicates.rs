use crate::execution::Errno;
use crate::net::structure::{
    IpEndpoint, KernelSockAddr, SendRecvFlags, SockAddrIn, SockShutdownCmd, SocketIdentity,
    SocketKind, SocketPayload, SocketProtocol, TcpState, UdpInner, UnixDatagramState,
    UnixStreamState,
};

pub(crate) fn endpoint_from_sockaddr(addr: KernelSockAddr) -> Result<IpEndpoint, Errno> {
    match addr {
        KernelSockAddr::V4(sockaddr) if sockaddr.family == SockAddrIn::AF_INET => {
            Ok(IpEndpoint::new(sockaddr.addr, sockaddr.port))
        }
        KernelSockAddr::V4(_) => Err(Errno::EAFNOSUPPORT),
        KernelSockAddr::Unix(_) => Err(Errno::EAFNOSUPPORT),
        KernelSockAddr::Packet(_) => Err(Errno::EAFNOSUPPORT),
    }
}

pub(crate) fn require_bind_endpoint(addr: KernelSockAddr) -> Result<IpEndpoint, Errno> {
    let endpoint = endpoint_from_sockaddr(addr)?;
    if endpoint.port == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(endpoint)
}

pub(crate) fn raw_bind_endpoint(addr: KernelSockAddr) -> Result<IpEndpoint, Errno> {
    let endpoint = endpoint_from_sockaddr(addr)?;
    Ok(IpEndpoint::new(endpoint.addr, 0))
}

fn require_local_bind_addr(
    payload: &SocketPayload,
    endpoint: IpEndpoint,
) -> Result<IpEndpoint, Errno> {
    if endpoint.addr == crate::net::structure::Ipv4Address::UNSPECIFIED
        || payload.net_namespace().owns_ipv4_addr(endpoint.addr)
    {
        Ok(endpoint)
    } else {
        Err(Errno::EADDRNOTAVAIL)
    }
}

pub(crate) fn socket_payload_present(socket: &SocketIdentity) -> Result<(), Errno> {
    if socket.is_payload_live() {
        Ok(())
    } else {
        Err(Errno::ENOTCONN)
    }
}

pub(crate) fn socket_can_read(socket: &SocketIdentity, _flags: SendRecvFlags) -> Result<(), Errno> {
    socket_payload_present(socket)
}

pub(crate) fn socket_can_write(
    socket: &SocketIdentity,
    _flags: SendRecvFlags,
) -> Result<(), Errno> {
    socket.with_payload_for_check(|payload| match payload {
        Some(payload) if payload.shutdown_wr() => Err(Errno::EPIPE),
        Some(_) => Ok(()),
        None => Err(Errno::ENOTCONN),
    })
}

pub(crate) fn socket_can_bind(
    socket: &SocketIdentity,
    addr: KernelSockAddr,
) -> Result<IpEndpoint, Errno> {
    socket.with_payload_for_check(|payload| {
        let Some(payload) = payload else {
            return Err(Errno::ENOTCONN);
        };
        match (socket.kind, payload.protocol_snapshot()) {
            (
                SocketKind::UnixDatagram,
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound),
            )
            | (SocketKind::UnixStream, SocketProtocol::UnixStream(UnixStreamState::Init)) => {
                if matches!(addr, KernelSockAddr::Unix(_)) {
                    Ok(IpEndpoint::new(
                        crate::net::structure::Ipv4Address::UNSPECIFIED,
                        0,
                    ))
                } else {
                    Err(Errno::EAFNOSUPPORT)
                }
            }
            (SocketKind::Tcp, SocketProtocol::Tcp(TcpState::Init)) => {
                let endpoint = require_bind_endpoint(addr)?;
                require_local_bind_addr(payload, endpoint)
            }
            (SocketKind::Udp, SocketProtocol::Udp(UdpInner::Unbound)) => {
                let endpoint = require_bind_endpoint(addr)?;
                require_local_bind_addr(payload, endpoint)
            }
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(state))
                if state.bound_local.is_none() =>
            {
                let endpoint = raw_bind_endpoint(addr)?;
                require_local_bind_addr(payload, endpoint)
            }
            _ => Err(Errno::EINVAL),
        }
    })
}

pub(crate) fn socket_can_listen(socket: &SocketIdentity) -> Result<IpEndpoint, Errno> {
    socket.with_payload_for_check(|payload| {
        let Some(payload) = payload else {
            return Err(Errno::ENOTCONN);
        };
        match (socket.kind, payload.protocol_snapshot()) {
            (SocketKind::Tcp, SocketProtocol::Tcp(TcpState::Bound { local })) => Ok(local),
            (SocketKind::UnixStream, SocketProtocol::UnixStream(UnixStreamState::Bound { .. })) => {
                Ok(IpEndpoint::new(
                    crate::net::structure::Ipv4Address::UNSPECIFIED,
                    0,
                ))
            }
            (
                SocketKind::UnixDatagram,
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound)
                | SocketProtocol::UnixDatagram(UnixDatagramState::Bound { .. })
                | SocketProtocol::UnixDatagram(UnixDatagramState::Connected { .. }),
            ) => Err(Errno::EOPNOTSUPP),
            (SocketKind::Udp, SocketProtocol::Udp(_)) => Err(Errno::EOPNOTSUPP),
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(_)) => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::EINVAL),
        }
    })
}

pub(crate) fn socket_can_connect(
    socket: &SocketIdentity,
    addr: KernelSockAddr,
) -> Result<IpEndpoint, Errno> {
    socket.with_payload_for_check(|payload| {
        let Some(payload) = payload else {
            return Err(Errno::ENOTCONN);
        };
        match (socket.kind, payload.protocol_snapshot()) {
            (
                SocketKind::UnixDatagram,
                SocketProtocol::UnixDatagram(
                    UnixDatagramState::Unbound
                    | UnixDatagramState::Bound { .. }
                    | UnixDatagramState::Connected { .. },
                ),
            )
            | (
                SocketKind::UnixStream,
                SocketProtocol::UnixStream(UnixStreamState::Init | UnixStreamState::Bound { .. }),
            ) => {
                if matches!(addr, KernelSockAddr::Unix(_)) {
                    Ok(IpEndpoint::new(
                        crate::net::structure::Ipv4Address::UNSPECIFIED,
                        0,
                    ))
                } else {
                    Err(Errno::EAFNOSUPPORT)
                }
            }
            (
                SocketKind::UnixStream,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. }),
            ) => Err(Errno::EISCONN),
            (
                SocketKind::UnixStream,
                SocketProtocol::UnixStream(UnixStreamState::Listening { .. }),
            ) => Err(Errno::EINVAL),
            (
                SocketKind::Tcp,
                SocketProtocol::Tcp(
                    TcpState::Init | TcpState::Bound { .. } | TcpState::Connecting { .. },
                ),
            ) => Ok(endpoint_from_sockaddr(addr)?),
            (SocketKind::Tcp, SocketProtocol::Tcp(TcpState::Connected { .. })) => {
                Err(Errno::EISCONN)
            }
            (
                SocketKind::Udp,
                SocketProtocol::Udp(
                    UdpInner::Unbound | UdpInner::Bound { .. } | UdpInner::Connected { .. },
                ),
            ) => Ok(endpoint_from_sockaddr(addr)?),
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(_)) => Ok(endpoint_from_sockaddr(addr)?),
            _ => Err(Errno::EINVAL),
        }
    })
}

pub(crate) fn socket_can_accept(socket: &SocketIdentity) -> Result<(), Errno> {
    socket.with_payload_for_check(|payload| {
        let Some(payload) = payload else {
            return Err(Errno::ENOTCONN);
        };
        match (socket.kind, payload.protocol_snapshot()) {
            (SocketKind::Tcp, SocketProtocol::Tcp(TcpState::Listening { .. })) => Ok(()),
            (
                SocketKind::UnixStream,
                SocketProtocol::UnixStream(UnixStreamState::Listening { .. }),
            ) => Ok(()),
            (
                SocketKind::UnixDatagram,
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound)
                | SocketProtocol::UnixDatagram(UnixDatagramState::Bound { .. })
                | SocketProtocol::UnixDatagram(UnixDatagramState::Connected { .. }),
            ) => Err(Errno::EOPNOTSUPP),
            (SocketKind::Udp, SocketProtocol::Udp(_)) => Err(Errno::EOPNOTSUPP),
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(_)) => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::EINVAL),
        }
    })
}

pub(crate) fn socket_can_shutdown(
    socket: &SocketIdentity,
    _how: SockShutdownCmd,
) -> Result<(), Errno> {
    socket_payload_present(socket)
}

pub(crate) fn socket_can_poll(socket: &SocketIdentity) -> Result<(), Errno> {
    let _ = socket;
    Ok(())
}
