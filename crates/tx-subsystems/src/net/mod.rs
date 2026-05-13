//! Network subsystem staging slice.
//!
//! N1/N2 define the core socket value vocabulary and the
//! `SocketIdentity`/`SocketPayload` split.  N3/N4 add guard-scoped checks
//! and net-only execution steps.  Syscall wiring, VFS socket backing,
//! protocol I/O, and reactor delegate tasks are later slices.

pub mod checks;
pub mod delegate;
pub mod device;
pub mod execution;
pub mod facade;
pub mod packet;
pub mod project;
pub mod protocol;
pub mod structure;

pub use device::{
    net_device_by_devt, net_device_by_name, net_device_snapshot, register_net_devices,
    EthernetAddress, NetDeviceOps, NetDeviceRegistration, VirtioNetConfig, VirtioNetDevice,
    VirtioNetFeatureSet, VirtioNetIrqEvent, VirtioNetIrqOutcome, VirtioNetQueueConfig,
    VirtioNetRxInjectOutcome, VirtioNetStats, VirtioNetStatsSnapshot, VirtioNetTxCompleteOutcome,
    VIRTIO_NET0_DEVICE, VIRTIO_NET0_REGISTRATION, VIRTIO_NET_DEFAULT_MTU, VIRTIO_NET_STAGING_MAJOR,
};
pub use execution::{
    socket_accept_wait_token, socket_open_file_from_identity, socket_recv_wait_token,
    socket_send_wait_token, socket_urgent_wait_token, step_accept, step_bind, step_connect,
    step_flush_pending_arp, step_listen, step_poll_ready, step_process_device_tx_pending,
    step_process_device_tx_pending_at, step_process_network_events, step_process_network_events_at,
    step_process_network_tick, step_process_network_tick_loopback, step_recv_kernel_bytes,
    step_send_kernel_bytes, step_send_to_kernel_bytes, step_shutdown, step_socket_close,
    step_socket_create, step_socket_open_file, step_tcp_loopback_handshake,
    step_tcp_loopback_transfer, ArpFlushOutcome, DeviceTxBudget, DeviceTxOutcome,
    LoopbackTcpConnectOutcome, LoopbackTcpTransferOutcome, NetworkBacklogTickOutcome,
    NetworkStepOutcome, ShutdownOutcome, SocketCloseOutcome, SocketOpenFileOutput,
    ARP_FLUSH_BUDGET_DEFAULT, DEVICE_TX_BUDGET_DEFAULT, NET_BACKLOG_SCAN_BUDGET, NET_EVENT_BUDGET,
};
pub use facade::{
    drive_socket_connect_waiting, drive_socket_nonblocking, socket_bind_facade,
    socket_connect_facade, socket_create_facade, socket_listen_facade, socket_poll_ready_facade,
    socket_shutdown_facade, SocketBindCapability, SocketBindOps, SocketConnectCapability,
    SocketConnectOps, SocketCreateOutput, SocketFacadeDriveMode, SocketHandle, SocketHandleFlags,
    SocketListenCapability, SocketListenOps, SocketPollCapability, SocketPollOps,
    SocketShutdownCapability, SocketShutdownOps,
};
pub use packet::{
    demux_rx_frame_with_smoltcp, NetworkPublish, PacketDispatch, PacketSource, PacketTxReadiness,
    PacketTxResult, PacketTxSink, RxFrame, TcpPacketEvent, TcpPacketFlags, UdpPacketEvent,
};
pub use project::{proc_net_arp_snapshot_text, proc_net_dev_snapshot_text};
pub use protocol::{
    ArpEntry, EtherIface, EtherPacketSource, EtherPacketTxSink, Icmpv4EchoPacket, Icmpv4Event,
    NetStats, RawIcmpSocket, RawTcpSocket, RawUdpSocket, SmoltcpAdapter, SmoltcpAdapterConfig,
    SmoltcpPacketSource, SmoltcpPacketTxSink,
};
pub use structure::{
    AcceptWireSet, AddressFamily, ConnectionKey, IpEndpoint, IpLevelOptions, Ipv4Address,
    KernelSockAddr, LingerOption, ListenerKey, LocalEndpointKey, PollMask, ProtocolNumber,
    RawIcmpSocketKey, RawIcmpState, RecvWireSet, SendRecvFlags, SendWireSet, SockAddrIn, SockFlags,
    SockShutdownCmd, SocketIdentity, SocketIoState, SocketKind, SocketLevelOptions,
    SocketOperationalEvidence, SocketOptionSet, SocketPayload, SocketProtocol, SocketReadiness,
    SocketRecvBytesOutcome, SocketTable, SocketType, SocketWaitCarriers, Takeable, TcpLevelOptions,
    TcpState, UdpInner, UrgentEvent, ValidSocketType,
};

#[cfg(test)]
mod tests;
