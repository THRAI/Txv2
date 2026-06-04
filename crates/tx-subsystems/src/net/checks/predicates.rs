use crate::execution::Errno;
use crate::net::structure::{
    AddressFamily, IpEndpoint, KernelSockAddr, RdsState, SendRecvFlags, SockAddrIn, SockAddrIn6,
    SockShutdownCmd, SocketIdentity, SocketKind, SocketPayload, SocketProtocol, TcpState, UdpInner,
    UnixDatagramState, UnixStreamState,
};

pub(crate) fn endpoint_from_sockaddr(addr: KernelSockAddr) -> Result<IpEndpoint, Errno> {
    match addr {
        KernelSockAddr::V4(sockaddr) if sockaddr.family == SockAddrIn::AF_INET => {
            Ok(IpEndpoint::new(sockaddr.addr, sockaddr.port))
        }
        KernelSockAddr::V6(sockaddr) if sockaddr.family == SockAddrIn6::AF_INET6 => {
            Ok(IpEndpoint::new_v6(sockaddr.addr, sockaddr.port))
        }
        KernelSockAddr::V4(_) => Err(Errno::EAFNOSUPPORT),
        KernelSockAddr::V6(_) => Err(Errno::EAFNOSUPPORT),
        KernelSockAddr::Unix(_) => Err(Errno::EAFNOSUPPORT),
        KernelSockAddr::Packet(_) => Err(Errno::EAFNOSUPPORT),
        KernelSockAddr::Unspec => Err(Errno::EAFNOSUPPORT),
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
    Ok(IpEndpoint::from_ip(endpoint.ip_addr(), 0))
}

fn require_local_bind_addr(
    payload: &SocketPayload,
    endpoint: IpEndpoint,
) -> Result<IpEndpoint, Errno> {
    match endpoint.family {
        AddressFamily::Inet => {
            if endpoint.addr == crate::net::structure::Ipv4Address::UNSPECIFIED
                || payload.net_namespace().owns_ipv4_addr(endpoint.addr)
            {
                Ok(endpoint)
            } else {
                Err(Errno::EADDRNOTAVAIL)
            }
        }
        AddressFamily::Inet6 => {
            if endpoint.addr6 == crate::net::structure::Ipv6Address::UNSPECIFIED
                || payload.net_namespace().owns_ipv6_addr(endpoint.addr6)
            {
                Ok(endpoint)
            } else {
                Err(Errno::EADDRNOTAVAIL)
            }
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn require_socket_family(
    payload: &SocketPayload,
    endpoint: IpEndpoint,
) -> Result<IpEndpoint, Errno> {
    if payload.family() == endpoint.family {
        Ok(endpoint)
    } else {
        Err(Errno::EAFNOSUPPORT)
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
                let endpoint = require_socket_family(payload, endpoint)?;
                require_local_bind_addr(payload, endpoint)
            }
            (SocketKind::Sctp, SocketProtocol::Sctp(TcpState::Init)) => {
                let endpoint = require_bind_endpoint(addr)?;
                let endpoint = require_socket_family(payload, endpoint)?;
                require_local_bind_addr(payload, endpoint)
            }
            (SocketKind::Udp, SocketProtocol::Udp(UdpInner::Unbound)) => {
                let endpoint = require_bind_endpoint(addr)?;
                let endpoint = require_socket_family(payload, endpoint)?;
                require_local_bind_addr(payload, endpoint)
            }
            (SocketKind::RdsSeqPacket, SocketProtocol::Rds(RdsState::Unbound)) => {
                let endpoint = require_bind_endpoint(addr)?;
                if endpoint.family != AddressFamily::Inet {
                    return Err(Errno::EAFNOSUPPORT);
                }
                require_local_bind_addr(payload, endpoint)
            }
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(state)) => {
                let endpoint = raw_bind_endpoint(addr)?;
                let endpoint = require_socket_family(payload, endpoint)?;
                let endpoint = require_local_bind_addr(payload, endpoint)?;
                match endpoint.family {
                    AddressFamily::Inet => {
                        if state.bound_local.is_none_or(|local| local == endpoint.addr) {
                            Ok(endpoint)
                        } else {
                            Err(Errno::EINVAL)
                        }
                    }
                    AddressFamily::Inet6 => {
                        if state
                            .bound_local6
                            .is_none_or(|local| local == endpoint.addr6)
                        {
                            Ok(endpoint)
                        } else {
                            Err(Errno::EINVAL)
                        }
                    }
                    _ => Err(Errno::EAFNOSUPPORT),
                }
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
            (SocketKind::Sctp, SocketProtocol::Sctp(TcpState::Bound { local })) => Ok(local),
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
                | SocketProtocol::UnixDatagram(UnixDatagramState::Connected { .. })
                | SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { .. }),
            ) => Err(Errno::EOPNOTSUPP),
            (SocketKind::Udp, SocketProtocol::Udp(_)) => Err(Errno::EOPNOTSUPP),
            (SocketKind::RdsSeqPacket, SocketProtocol::Rds(_)) => Err(Errno::EOPNOTSUPP),
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
                    | UnixDatagramState::Connected { .. }
                    | UnixDatagramState::ConnectedPair { .. },
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
            ) => require_socket_family(payload, endpoint_from_sockaddr(addr)?),
            (
                SocketKind::Sctp,
                SocketProtocol::Sctp(
                    TcpState::Init | TcpState::Bound { .. } | TcpState::Connecting { .. },
                ),
            ) => require_socket_family(payload, endpoint_from_sockaddr(addr)?),
            (SocketKind::Tcp, SocketProtocol::Tcp(TcpState::Connected { .. })) => {
                Err(Errno::EISCONN)
            }
            (SocketKind::Sctp, SocketProtocol::Sctp(TcpState::Connected { .. })) => {
                Err(Errno::EISCONN)
            }
            // 1-to-1 (TCP-style) SCTP: connect() on a listening socket is
            // rejected with EISCONN, like connect() on an established one.
            (SocketKind::Sctp, SocketProtocol::Sctp(TcpState::Listening { .. })) => {
                Err(Errno::EISCONN)
            }
            (
                SocketKind::Udp,
                SocketProtocol::Udp(
                    UdpInner::Unbound | UdpInner::Bound { .. } | UdpInner::Connected { .. },
                ),
            ) => require_socket_family(payload, endpoint_from_sockaddr(addr)?),
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(_)) => {
                require_socket_family(payload, endpoint_from_sockaddr(addr)?)
            }
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
            (SocketKind::Sctp, SocketProtocol::Sctp(TcpState::Listening { .. })) => Ok(()),
            (
                SocketKind::UnixStream,
                SocketProtocol::UnixStream(UnixStreamState::Listening { .. }),
            ) => Ok(()),
            (
                SocketKind::UnixDatagram,
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound)
                | SocketProtocol::UnixDatagram(UnixDatagramState::Bound { .. })
                | SocketProtocol::UnixDatagram(UnixDatagramState::Connected { .. })
                | SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { .. }),
            ) => Err(Errno::EOPNOTSUPP),
            (SocketKind::Udp, SocketProtocol::Udp(_)) => Err(Errno::EOPNOTSUPP),
            (SocketKind::RdsSeqPacket, SocketProtocol::Rds(_)) => Err(Errno::EOPNOTSUPP),
            (SocketKind::RawIcmp, SocketProtocol::RawIcmp(_)) => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::EINVAL),
        }
    })
}

pub(crate) fn socket_can_shutdown(
    socket: &SocketIdentity,
    _how: SockShutdownCmd,
) -> Result<(), Errno> {
    socket.with_payload_for_check(|payload| {
        let Some(payload) = payload else {
            return Err(Errno::ENOTCONN);
        };
        // SCTP 1-to-1: shutdown() on a socket with no established association
        // returns ENOTCONN.
        if socket.kind == SocketKind::Sctp
            && !matches!(
                payload.protocol_snapshot(),
                SocketProtocol::Sctp(TcpState::Connected { .. })
            )
        {
            return Err(Errno::ENOTCONN);
        }
        Ok(())
    })
}

pub(crate) fn socket_can_poll(socket: &SocketIdentity) -> Result<(), Errno> {
    let _ = socket;
    Ok(())
}
