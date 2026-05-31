use core::ops::{BitAnd, BitOr, BitOrAssign};
use core::time::Duration;

use crate::execution::Errno;

pub const UNIX_SOCKET_PATH_MAX: usize = 108;
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AddressFamily {
    Unix,
    Inet,
    Inet6,
    Netlink,
    Packet,
    Rds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketType {
    Stream,
    Dgram,
    Raw,
    SeqPacket,
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
            10 => AddressFamily::Inet6,
            16 => AddressFamily::Netlink,
            17 => AddressFamily::Packet,
            21 => AddressFamily::Rds,
            _ => return Err(Errno::EAFNOSUPPORT),
        };

        let sock_type = match raw_type {
            1 => SocketType::Stream,
            2 => SocketType::Dgram,
            3 => SocketType::Raw,
            5 => SocketType::SeqPacket,
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
    Sctp,
    RdsSeqPacket,
    RawIcmp,
    NetlinkRoute,
    NetlinkNetfilter,
    Packet,
}

impl SocketKind {
    pub fn from_valid_socket_type(valid: ValidSocketType) -> Result<Self, Errno> {
        match (valid.domain, valid.sock_type, valid.protocol) {
            (AddressFamily::Unix, SocketType::Dgram, 0) => Ok(Self::UnixDatagram),
            (AddressFamily::Unix, SocketType::Stream | SocketType::SeqPacket, 0) => {
                Ok(Self::UnixStream)
            }
            (AddressFamily::Inet, SocketType::Stream, 0 | 6) => Ok(Self::Tcp),
            (AddressFamily::Inet, SocketType::Stream, 132) => Ok(Self::Sctp),
            (AddressFamily::Inet, SocketType::Dgram, 0 | 17 | 136) => Ok(Self::Udp),
            (AddressFamily::Inet, SocketType::Dgram, 1)
            | (AddressFamily::Inet, SocketType::Raw, 1) => Ok(Self::RawIcmp),
            (AddressFamily::Inet, _, _) => Err(Errno::EPROTONOSUPPORT),
            (AddressFamily::Inet6, SocketType::Stream, 0 | 6) => Ok(Self::Tcp),
            (AddressFamily::Inet6, SocketType::Stream, 132) => Ok(Self::Sctp),
            (AddressFamily::Inet6, SocketType::Dgram, 0 | 17 | 136) => Ok(Self::Udp),
            (AddressFamily::Inet6, SocketType::Raw, 58 | 159) => Ok(Self::RawIcmp),
            (AddressFamily::Inet6, _, _) => Err(Errno::EPROTONOSUPPORT),
            (AddressFamily::Netlink, SocketType::Raw | SocketType::Dgram, 0) => {
                Ok(Self::NetlinkRoute)
            }
            (AddressFamily::Netlink, SocketType::Raw | SocketType::Dgram, 12) => {
                Ok(Self::NetlinkNetfilter)
            }
            (AddressFamily::Packet, SocketType::Raw | SocketType::Dgram, _) => Ok(Self::Packet),
            (AddressFamily::Rds, SocketType::SeqPacket, 0) => Ok(Self::RdsSeqPacket),
            (AddressFamily::Rds, _, _) => Err(Errno::EPROTONOSUPPORT),
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

    pub const fn is_multicast(self) -> bool {
        (self.octets[0] & 0xf0) == 0xe0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Ipv6Address {
    octets: [u8; 16],
}

impl Ipv6Address {
    pub const UNSPECIFIED: Self = Self { octets: [0; 16] };
    pub const LOOPBACK: Self = Self {
        octets: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    };

    pub const fn new(octets: [u8; 16]) -> Self {
        Self { octets }
    }

    pub const fn octets(self) -> [u8; 16] {
        self.octets
    }

    pub fn is_unspecified(self) -> bool {
        let mut idx = 0;
        while idx < self.octets.len() {
            if self.octets[idx] != 0 {
                return false;
            }
            idx += 1;
        }
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum IpAddress {
    V4(Ipv4Address),
    V6(Ipv6Address),
}

impl IpAddress {
    pub const fn family(self) -> AddressFamily {
        match self {
            Self::V4(_) => AddressFamily::Inet,
            Self::V6(_) => AddressFamily::Inet6,
        }
    }

    pub fn is_unspecified(self) -> bool {
        match self {
            Self::V4(addr) => {
                addr.octets()[0] == 0
                    && addr.octets()[1] == 0
                    && addr.octets()[2] == 0
                    && addr.octets()[3] == 0
            }
            Self::V6(addr) => addr.is_unspecified(),
        }
    }

    pub fn is_loopback(self) -> bool {
        match self {
            Self::V4(addr) => {
                let octets = addr.octets();
                octets[0] == 127 && octets[1] == 0 && octets[2] == 0 && octets[3] == 1
            }
            Self::V6(addr) => addr == Ipv6Address::LOOPBACK,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IpEndpoint {
    pub family: AddressFamily,
    pub addr: Ipv4Address,
    pub addr6: Ipv6Address,
    pub port: u16,
}

impl IpEndpoint {
    pub const fn new(addr: Ipv4Address, port: u16) -> Self {
        Self {
            family: AddressFamily::Inet,
            addr,
            addr6: Ipv6Address::UNSPECIFIED,
            port,
        }
    }

    pub const fn new_v6(addr: Ipv6Address, port: u16) -> Self {
        Self {
            family: AddressFamily::Inet6,
            addr: Ipv4Address::UNSPECIFIED,
            addr6: addr,
            port,
        }
    }

    pub const fn from_ip(addr: IpAddress, port: u16) -> Self {
        match addr {
            IpAddress::V4(addr) => Self::new(addr, port),
            IpAddress::V6(addr) => Self::new_v6(addr, port),
        }
    }

    pub const fn ip_addr(self) -> IpAddress {
        match self.family {
            AddressFamily::Inet => IpAddress::V4(self.addr),
            AddressFamily::Inet6 => IpAddress::V6(self.addr6),
            AddressFamily::Unix
            | AddressFamily::Netlink
            | AddressFamily::Packet
            | AddressFamily::Rds => IpAddress::V4(self.addr),
        }
    }

    pub const fn unspecified_for_family(family: AddressFamily, port: u16) -> Self {
        match family {
            AddressFamily::Inet6 => Self::new_v6(Ipv6Address::UNSPECIFIED, port),
            _ => Self::new(Ipv4Address::UNSPECIFIED, port),
        }
    }

    pub const fn loopback_for_family(family: AddressFamily, port: u16) -> Self {
        match family {
            AddressFamily::Inet6 => Self::new_v6(Ipv6Address::LOOPBACK, port),
            _ => Self::new(Ipv4Address::LOOPBACK, port),
        }
    }

    pub fn is_unspecified(self) -> bool {
        self.ip_addr().is_unspecified()
    }

    pub fn is_loopback(self) -> bool {
        self.ip_addr().is_loopback()
    }

    pub fn same_family(self, other: Self) -> bool {
        self.family == other.family
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelSockAddr {
    Unix(UnixSocketPath),
    V4(SockAddrIn),
    V6(SockAddrIn6),
    Packet(SockAddrLl),
    Unspec,
}

impl KernelSockAddr {
    pub const fn as_ip_endpoint(self) -> IpEndpoint {
        match self {
            Self::Unix(_) => IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
            Self::V4(sockaddr) => IpEndpoint::new(sockaddr.addr, sockaddr.port),
            Self::V6(sockaddr) => IpEndpoint::new_v6(sockaddr.addr, sockaddr.port),
            Self::Packet(_) => IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
            Self::Unspec => IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UnixSocketPath {
    len: u8,
    bytes: [u8; UNIX_SOCKET_PATH_MAX],
}

impl UnixSocketPath {
    pub fn new(path: &[u8]) -> Result<Self, Errno> {
        if path.is_empty() || path.len() > UNIX_SOCKET_PATH_MAX {
            return Err(Errno::EINVAL);
        }
        let mut bytes = [0u8; UNIX_SOCKET_PATH_MAX];
        bytes[..path.len()].copy_from_slice(path);
        Ok(Self {
            len: path.len() as u8,
            bytes,
        })
    }

    pub const fn len(self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn is_abstract(&self) -> bool {
        self.as_bytes().first().is_some_and(|byte| *byte == 0)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len()]
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
pub struct SockAddrIn6 {
    pub family: u16,
    pub port: u16,
    pub flowinfo: u32,
    pub addr: Ipv6Address,
    pub scope_id: u32,
}

impl SockAddrIn6 {
    pub const AF_INET6: u16 = 10;

    pub const fn new(port: u16, addr: Ipv6Address) -> Self {
        Self {
            family: Self::AF_INET6,
            port,
            flowinfo: 0,
            addr,
            scope_id: 0,
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
    pub sock_type: SocketType,
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
    pub hdr_incl: bool,
    pub ipv6_v6only: bool,
    pub ipv6_checksum: i32,
    pub ipv6_recv_pktinfo: bool,
    pub ipv6_recv_hoplimit: bool,
    pub ipv6_recv_rthdr: bool,
    pub ipv6_recv_hopopts: bool,
    pub ipv6_recv_dstopts: bool,
    pub ipv6_recv_tclass: bool,
    pub ipv6_2292_pktinfo: bool,
    pub ipv6_2292_hoplimit: bool,
    pub ipv6_2292_rthdr: bool,
    pub ipv6_2292_hopopts: bool,
    pub ipv6_2292_dstopts: bool,
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
    pub tls_ulp: Option<TcpTlsUlpState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpTlsUlpState {
    pub tx_configured: bool,
}

impl TcpTlsUlpState {
    pub const fn attached() -> Self {
        Self {
            tx_configured: false,
        }
    }

    pub const fn with_tx_config() -> Self {
        Self {
            tx_configured: true,
        }
    }
}

impl SocketOptionSet {
    pub const fn default_tcp() -> Self {
        Self {
            socket: SocketLevelOptions {
                sock_type: SocketType::Stream,
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
                hdr_incl: false,
                ipv6_v6only: false,
                ipv6_checksum: -1,
                ipv6_recv_pktinfo: false,
                ipv6_recv_hoplimit: false,
                ipv6_recv_rthdr: false,
                ipv6_recv_hopopts: false,
                ipv6_recv_dstopts: false,
                ipv6_recv_tclass: false,
                ipv6_2292_pktinfo: false,
                ipv6_2292_hoplimit: false,
                ipv6_2292_rthdr: false,
                ipv6_2292_hopopts: false,
                ipv6_2292_dstopts: false,
            },
            tcp: TcpLevelOptions {
                nodelay: false,
                maxseg: 0,
                keepidle: 7200,
                keepintvl: 75,
                keepcnt: 9,
                cork: false,
                window_clamp: 0,
                tls_ulp: None,
            },
        }
    }

    pub const fn default_udp() -> Self {
        Self {
            socket: SocketLevelOptions {
                sock_type: SocketType::Dgram,
                reuse_addr: false,
                reuse_port: false,
                dont_route: false,
                keep_alive: false,
                broadcast: false,
                linger: LingerOption::disabled(),
                recv_buf_size: 262_144,
                send_buf_size: 262_144,
                recv_timeout: None,
                send_timeout: None,
            },
            ip: IpLevelOptions {
                tos: 0,
                ttl: 64,
                multicast_ttl: 1,
                recv_err: false,
                hdr_incl: false,
                ipv6_v6only: false,
                ipv6_checksum: -1,
                ipv6_recv_pktinfo: false,
                ipv6_recv_hoplimit: false,
                ipv6_recv_rthdr: false,
                ipv6_recv_hopopts: false,
                ipv6_recv_dstopts: false,
                ipv6_recv_tclass: false,
                ipv6_2292_pktinfo: false,
                ipv6_2292_hoplimit: false,
                ipv6_2292_rthdr: false,
                ipv6_2292_hopopts: false,
                ipv6_2292_dstopts: false,
            },
            tcp: TcpLevelOptions {
                nodelay: false,
                maxseg: 0,
                keepidle: 7200,
                keepintvl: 75,
                keepcnt: 9,
                cork: false,
                window_clamp: 0,
                tls_ulp: None,
            },
        }
    }

    pub const fn for_kind(kind: SocketKind) -> Self {
        let mut options = match kind {
            SocketKind::Tcp | SocketKind::Sctp | SocketKind::UnixStream => Self::default_tcp(),
            SocketKind::UnixDatagram
            | SocketKind::Udp
            | SocketKind::RdsSeqPacket
            | SocketKind::RawIcmp
            | SocketKind::NetlinkRoute
            | SocketKind::NetlinkNetfilter
            | SocketKind::Packet => Self::default_udp(),
        };
        options.socket.sock_type = match kind {
            SocketKind::Tcp | SocketKind::Sctp | SocketKind::UnixStream => SocketType::Stream,
            SocketKind::UnixDatagram | SocketKind::Udp => SocketType::Dgram,
            SocketKind::RdsSeqPacket => SocketType::SeqPacket,
            SocketKind::RawIcmp
            | SocketKind::NetlinkRoute
            | SocketKind::NetlinkNetfilter
            | SocketKind::Packet => SocketType::Raw,
        };
        options
    }

    pub const fn for_valid_socket_type(valid: ValidSocketType, kind: SocketKind) -> Self {
        let mut options = Self::for_kind(kind);
        options.socket.sock_type = valid.sock_type;
        options
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
pub enum RdsState {
    Unbound,
    Bound { local: IpEndpoint },
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIcmpState {
    pub bound_local: Option<Ipv4Address>,
    pub bound_local6: Option<Ipv6Address>,
    pub protocol: ProtocolNumber,
    pub icmp6_filter: [u32; 8],
}

impl RawIcmpState {
    pub const fn new(protocol: ProtocolNumber) -> Self {
        Self {
            bound_local: None,
            bound_local6: None,
            protocol,
            icmp6_filter: [0; 8],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketSocketState {
    pub protocol: u16,
    pub ifindex: Option<i32>,
    pub packet_version: i32,
    pub packet_reserve: u32,
    pub packet_vnet_hdr: bool,
    pub packet_rx_ring_block_size: Option<u32>,
}

impl PacketSocketState {
    pub const fn new(protocol: u16) -> Self {
        Self {
            protocol,
            ifindex: None,
            packet_version: 0,
            packet_reserve: 0,
            packet_vnet_hdr: false,
            packet_rx_ring_block_size: None,
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
