//! Socket structure types owned by the network subsystem.

mod identity;
mod payload;
mod readiness;
pub(crate) mod registry;
pub(crate) mod table;
mod types;

pub use identity::{SocketIdentity, SocketWaitCarriers};
pub use payload::{
    SocketAcceptEntry, SocketAcceptQueue, SocketIoConsume, SocketIoState,
    SocketOperationalEvidence, SocketPayload, SocketProtocol, SocketRecvBytesOutcome,
    SocketSendReserve, Takeable, TcpBacklog, TcpBacklogEntry, TcpBacklogRetransmitOutcome,
    TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS, TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING,
    TCP_BACKLOG_TIMEOUT_STAGING_MILLIS,
};
pub use readiness::{AcceptWireSet, RecvWireSet, SendWireSet, SocketReadiness, UrgentEvent};
pub use table::{
    ConnectionKey, InitialSocketTableProxy, ListenerKey, LocalEndpointKey, RawIcmpSocketKey,
    SocketTable,
};
pub use types::{
    AddressFamily, IpEndpoint, IpLevelOptions, Ipv4Address, KernelSockAddr, LingerOption,
    PacketSocketState, PollMask, ProtocolNumber, RawIcmpState, SendRecvFlags, SockAddrIn,
    SockAddrLl, SockFlags, SockShutdownCmd, SocketKind, SocketLevelOptions, SocketOptionSet,
    SocketType, TcpLevelOptions, TcpState, UdpInner, ValidSocketType,
};
