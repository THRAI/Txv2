use core::ops::{BitAnd, BitOr, BitOrAssign};
use core::time::Duration;

use crate::execution::Errno;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressFamily {
    Unix,
    Inet,
    Netlink,
    Packet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketType {
    Stream,
    Dgram,
    Raw,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SockFlags {
    bits: i32,
}

impl SockFlags {
    pub const SOCK_NONBLOCK: Self = Self { bits: 0x800 };
    pub const SOCK_CLOEXEC: Self = Self { bits: 0x80000 };

    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    pub const fn all() -> Self {
        Self {
            bits: Self::SOCK_NONBLOCK.bits | Self::SOCK_CLOEXEC.bits,
        }
    }

    pub const fn from_bits_truncate(bits: i32) -> Self {
        Self {
            bits: bits & Self::all().bits,
        }
    }

    pub const fn bits(self) -> i32 {
        self.bits
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.bits & other.bits) == other.bits
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidSocketType {
    pub domain: AddressFamily,
    pub sock_type: SocketType,
    pub flags: SockFlags,
    pub protocol: u16,
}

impl ValidSocketType {
    pub fn validate(domain: i32, type_: i32, protocol: i32) -> Result<Self, Errno> {
        let flags = SockFlags::from_bits_truncate(type_);
        let raw_type = type_ & !SockFlags::all().bits();

        let domain = match domain {
            1 => AddressFamily::Unix,
            2 => AddressFamily::Inet,
            16 => AddressFamily::Netlink,
            17 => AddressFamily::Packet,
            _ => return Err(Errno::EAFNOSUPPORT),
        };

        let sock_type = match raw_type {
            1 => SocketType::Stream,
            2 => SocketType::Dgram,
            3 => SocketType::Raw,
            _ => return Err(Errno::EINVAL),
        };

        let protocol = u16::try_from(protocol).map_err(|_| Errno::EINVAL)?;
        Ok(Self {
            domain,
            sock_type,
            flags,
            protocol,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketKind {
    UnixDatagram,
    UnixStream,
    Tcp,
    Udp,
    RawIcmp,
    NetlinkRoute,
    NetlinkNetfilter,
    Packet,
}

impl SocketKind {
    pub fn from_valid_socket_type(valid: ValidSocketType) -> Result<Self, Errno> {
        match (valid.domain, valid.sock_type, valid.protocol) {
            (AddressFamily::Unix, SocketType::Dgram, 0) => Ok(Self::UnixDatagram),
            (AddressFamily::Unix, SocketType::Stream, 0) => Ok(Self::UnixStream),
            (AddressFamily::Inet, SocketType::Stream, 0 | 6) => Ok(Self::Tcp),
            (AddressFamily::Inet, SocketType::Dgram, 0 | 17) => Ok(Self::Udp),
            (AddressFamily::Inet, SocketType::Dgram, 1)
            | (AddressFamily::Inet, SocketType::Raw, 1) => Ok(Self::RawIcmp),
            (AddressFamily::Inet, _, _) => Err(Errno::EPROTONOSUPPORT),
            (AddressFamily::Netlink, SocketType::Raw | SocketType::Dgram, 0) => {
                Ok(Self::NetlinkRoute)
            }
            (AddressFamily::Netlink, SocketType::Raw | SocketType::Dgram, 12) => {
                Ok(Self::NetlinkNetfilter)
            }
            (AddressFamily::Packet, SocketType::Raw | SocketType::Dgram, _) => Ok(Self::Packet),
            _ => Err(Errno::EOPNOTSUPP),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolNumber(pub u16);

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Ipv4Address {
    octets: [u8; 4],
}

impl Ipv4Address {
    pub const UNSPECIFIED: Self = Self {
        octets: [0, 0, 0, 0],
    };
    pub const LOOPBACK: Self = Self {
        octets: [127, 0, 0, 1],
    };
    pub const BROADCAST: Self = Self {
        octets: [255, 255, 255, 255],
    };

    pub const fn new(octets: [u8; 4]) -> Self {
        Self { octets }
    }

    pub const fn octets(self) -> [u8; 4] {
        self.octets
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IpEndpoint {
    pub addr: Ipv4Address,
    pub port: u16,
}

impl IpEndpoint {
    pub const fn new(addr: Ipv4Address, port: u16) -> Self {
        Self { addr, port }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelSockAddr {
    V4(SockAddrIn),
    Packet(SockAddrLl),
}

impl KernelSockAddr {
    pub const fn as_ip_endpoint(self) -> IpEndpoint {
        match self {
            Self::V4(sockaddr) => IpEndpoint::new(sockaddr.addr, sockaddr.port),
            Self::Packet(_) => IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SockAddrIn {
    pub family: u16,
    pub port: u16,
    pub addr: Ipv4Address,
}

impl SockAddrIn {
    pub const AF_INET: u16 = 2;

    pub const fn new(port: u16, addr: Ipv4Address) -> Self {
        Self {
            family: Self::AF_INET,
            port,
            addr,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SockAddrLl {
    pub family: u16,
    pub protocol: u16,
    pub ifindex: i32,
}

impl SockAddrLl {
    pub const AF_PACKET: u16 = 17;

    pub const fn new(protocol: u16, ifindex: i32) -> Self {
        Self {
            family: Self::AF_PACKET,
            protocol,
            ifindex,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SockShutdownCmd {
    Recv,
    Send,
    Both,
}

impl SockShutdownCmd {
    pub fn validate(how: i32) -> Result<Self, Errno> {
        match how {
            0 => Ok(Self::Recv),
            1 => Ok(Self::Send),
            2 => Ok(Self::Both),
            _ => Err(Errno::EINVAL),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SendRecvFlags {
    bits: i32,
}

impl SendRecvFlags {
    pub const MSG_OOB: Self = Self { bits: 0x01 };
    pub const MSG_DONTWAIT: Self = Self { bits: 0x40 };
    pub const MSG_PEEK: Self = Self { bits: 0x02 };
    pub const MSG_ERRQUEUE: Self = Self { bits: 0x2000 };
    pub const MSG_WAITALL: Self = Self { bits: 0x100 };
    pub const MSG_NOSIGNAL: Self = Self { bits: 0x4000 };
    pub const MSG_TRUNC: Self = Self { bits: 0x20 };
    pub const MSG_MORE: Self = Self { bits: 0x8000 };

    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    pub const fn all() -> Self {
        Self {
            bits: Self::MSG_OOB.bits
                | Self::MSG_DONTWAIT.bits
                | Self::MSG_PEEK.bits
                | Self::MSG_ERRQUEUE.bits
                | Self::MSG_WAITALL.bits
                | Self::MSG_NOSIGNAL.bits
                | Self::MSG_TRUNC.bits
                | Self::MSG_MORE.bits,
        }
    }

    pub fn validate(bits: i32) -> Result<Self, Errno> {
        let all = Self::all().bits;
        if bits & !all != 0 {
            return Err(Errno::EINVAL);
        }
        Ok(Self { bits })
    }

    pub const fn bits(self) -> i32 {
        self.bits
    }

    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.bits & other.bits) == other.bits
    }

    pub const fn is_nonblocking(self) -> bool {
        self.contains(Self::MSG_DONTWAIT)
    }
}

impl BitOr for SendRecvFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self {
            bits: self.bits | rhs.bits,
        }
    }
}

impl BitOrAssign for SendRecvFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.bits |= rhs.bits;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LingerOption {
    pub enabled: bool,
    pub timeout_secs: u32,
}

impl LingerOption {
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            timeout_secs: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocketOptionSet {
    pub socket: SocketLevelOptions,
    pub ip: IpLevelOptions,
    pub tcp: TcpLevelOptions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocketLevelOptions {
    pub reuse_addr: bool,
    pub reuse_port: bool,
    pub dont_route: bool,
    pub keep_alive: bool,
    pub broadcast: bool,
    pub linger: LingerOption,
    pub recv_buf_size: usize,
    pub send_buf_size: usize,
    pub recv_timeout: Option<Duration>,
    pub send_timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpLevelOptions {
    pub tos: u8,
    pub ttl: u8,
    pub multicast_ttl: u8,
    pub recv_err: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpLevelOptions {
    pub nodelay: bool,
    pub maxseg: u16,
    pub keepidle: u32,
    pub keepintvl: u32,
    pub keepcnt: u32,
    pub cork: bool,
    pub window_clamp: u32,
}

impl SocketOptionSet {
    pub const fn default_tcp() -> Self {
        Self {
            socket: SocketLevelOptions {
                reuse_addr: false,
                reuse_port: false,
                dont_route: false,
                keep_alive: false,
                broadcast: false,
                linger: LingerOption::disabled(),
                recv_buf_size: 262_144,
                send_buf_size: 65_536,
                recv_timeout: None,
                send_timeout: None,
            },
            ip: IpLevelOptions {
                tos: 0,
                ttl: 64,
                multicast_ttl: 1,
                recv_err: false,
            },
            tcp: TcpLevelOptions {
                nodelay: false,
                maxseg: 536,
                keepidle: 7200,
                keepintvl: 75,
                keepcnt: 9,
                cork: false,
                window_clamp: 0,
            },
        }
    }

    pub const fn default_udp() -> Self {
        Self {
            socket: SocketLevelOptions {
                reuse_addr: false,
                reuse_port: false,
                dont_route: false,
                keep_alive: false,
                broadcast: false,
                linger: LingerOption::disabled(),
                recv_buf_size: 87_380,
                send_buf_size: 16_384,
                recv_timeout: None,
                send_timeout: None,
            },
            ip: IpLevelOptions {
                tos: 0,
                ttl: 64,
                multicast_ttl: 1,
                recv_err: false,
            },
            tcp: TcpLevelOptions {
                nodelay: false,
                maxseg: 536,
                keepidle: 7200,
                keepintvl: 75,
                keepcnt: 9,
                cork: false,
                window_clamp: 0,
            },
        }
    }

    pub const fn for_kind(kind: SocketKind) -> Self {
        match kind {
            SocketKind::Tcp => Self::default_tcp(),
            SocketKind::UnixDatagram
            | SocketKind::UnixStream
            | SocketKind::Udp
            | SocketKind::RawIcmp
            | SocketKind::NetlinkRoute
            | SocketKind::NetlinkNetfilter
            | SocketKind::Packet => Self::default_udp(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PollMask(u32);

impl PollMask {
    pub const IN: Self = Self(0x0001);
    pub const PRI: Self = Self(0x0002);
    pub const OUT: Self = Self(0x0004);
    pub const ERR: Self = Self(0x0008);
    pub const HUP: Self = Self(0x0010);
    pub const RDHUP: Self = Self(0x2000);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }
}

impl BitOr for PollMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PollMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for PollMask {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TcpState {
    Init,
    Bound {
        local: IpEndpoint,
    },
    Listening {
        local: IpEndpoint,
        backlog_limit: usize,
    },
    Connecting {
        local: IpEndpoint,
        remote: IpEndpoint,
    },
    Connected {
        local: IpEndpoint,
        remote: IpEndpoint,
    },
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UdpInner {
    Unbound,
    Bound {
        local: IpEndpoint,
    },
    Connected {
        local: IpEndpoint,
        remote: IpEndpoint,
    },
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIcmpState {
    pub bound_local: Option<Ipv4Address>,
    pub protocol: ProtocolNumber,
}

impl RawIcmpState {
    pub const fn new(protocol: ProtocolNumber) -> Self {
        Self {
            bound_local: None,
            protocol,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketSocketState {
    pub protocol: u16,
    pub ifindex: Option<i32>,
}

impl PacketSocketState {
    pub const fn new(protocol: u16) -> Self {
        Self {
            protocol,
            ifindex: None,
        }
    }

    pub const fn new_from_network_order(protocol: u16) -> Self {
        Self::new(u16::from_be(protocol))
    }

    pub const fn sockaddr(self) -> SockAddrLl {
        SockAddrLl::new(
            self.protocol,
            match self.ifindex {
                Some(ifindex) => ifindex,
                None => 0,
            },
        )
    }
}
