//! Socket structure types owned by the network subsystem.

mod identity;
mod multicast;
mod payload;
mod readiness;
pub(crate) mod registry;
pub(crate) mod table;
mod types;

pub use identity::{SocketIdentity, SocketWaitCarriers};
pub use multicast::Ipv4MulticastGroup;
pub use payload::{
    SocketAcceptEntry, SocketAcceptQueue, SocketIoConsume, SocketIoState,
    SocketOperationalEvidence, SocketPayload, SocketProtocol, SocketRecvBytesOutcome,
    SocketSendReserve, Takeable, TcpBacklog, TcpBacklogEntry, TcpBacklogRetransmitOutcome,
    UnixDatagramState, UnixPeerCred, UnixStreamState, TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS,
    TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING, TCP_BACKLOG_TIMEOUT_STAGING_MILLIS,
};
pub use readiness::{AcceptWireSet, RecvWireSet, SendWireSet, SocketReadiness, UrgentEvent};
pub use table::{
    ConnectionKey, InitialSocketTableProxy, ListenerKey, LocalEndpointKey, RawIcmpSocketKey,
    SocketTable, UnixStreamPeerKey,
};
pub use types::{
    AddressFamily, IpEndpoint, IpLevelOptions, Ipv4Address, KernelSockAddr, LingerOption,
    PacketSocketState, PollMask, ProtocolNumber, RawIcmpState, SendRecvFlags, SockAddrIn,
    SockAddrLl, SockFlags, SockShutdownCmd, SocketKind, SocketLevelOptions, SocketOptionSet,
    SocketType, TcpLevelOptions, TcpState, UdpInner, UnixSocketPath, ValidSocketType,
    UNIX_SOCKET_PATH_MAX,
};
