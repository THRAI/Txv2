//! Network subsystem staging slice.
//!
//! N1/N2 define the core socket value vocabulary and the
//! `SocketIdentity`/`SocketPayload` split.  N3/N4 add guard-scoped checks
//! and net-only execution steps.  Syscall wiring, VFS socket backing,
//! protocol I/O, and reactor delegate tasks are later slices.

pub mod admin;
pub mod checks;
pub mod delegate;
pub mod device;
pub mod execution;
pub mod facade;
pub mod namespace;
pub mod netfilter;
pub mod nfnetlink;
pub mod packet;
pub mod project;
pub mod protocol;
pub mod rtnetlink;
pub mod structure;

pub use admin::{require_net_admin, require_net_raw, NetAdminAuthority, NetRawAuthority};
pub use device::{
    create_bridge_for_test_or_bootstrap, create_veth_pair_for_test_or_bootstrap,
    net_device_by_devt, net_device_by_name, net_device_snapshot, register_net_devices,
    BridgeConfig, BridgeDevice, BridgeForwardOutcome, BridgeInstance, BridgePortSnapshot,
    BridgeSnapshot, EthernetAddress, NetDeviceKind, NetDeviceOps, NetDeviceRegistration,
    VethDevice, VethEndpointConfig, VethPair, VethPairConfig, VethStatsSnapshot, VirtioNetConfig,
    VirtioNetDevice, VirtioNetFeatureSet, VirtioNetIrqEvent, VirtioNetIrqOutcome,
    VirtioNetQueueConfig, VirtioNetRxInjectOutcome, VirtioNetStats, VirtioNetStatsSnapshot,
    VirtioNetTxCompleteOutcome, BRIDGE_FORWARD_BUDGET_DEFAULT, VETH_DEFAULT_MTU,
    VIRTIO_NET0_DEVICE, VIRTIO_NET0_REGISTRATION, VIRTIO_NET_DEFAULT_MTU, VIRTIO_NET_STAGING_MAJOR,
};
pub use execution::{
    socket_accept_wait_token, socket_open_file_from_identity, socket_recv_wait_token,
    socket_send_wait_token, socket_urgent_wait_token, step_accept, step_bind, step_connect,
    step_flush_pending_arp, step_listen, step_poll_ready, step_poll_wait_token,
    step_process_device_tx_pending, step_process_device_tx_pending_at,
    step_process_device_tx_pending_in_namespace_at, step_process_loopback_pending_in_namespace,
    step_process_loopback_udp, step_process_network_events, step_process_network_events_at,
    step_process_network_events_in_namespace_at, step_process_network_tick,
    step_process_network_tick_in_namespace, step_process_network_tick_loopback,
    step_process_network_tick_loopback_in_namespace, step_recv_kernel_bytes,
    step_send_kernel_bytes, step_send_to_kernel_bytes, step_send_to_kernel_bytes_with_poll_kick,
    step_send_to_unix_path_kernel_bytes, step_send_udp_loopback_kernel_bytes, step_shutdown,
    step_socket_close, step_socket_create, step_socket_create_in_namespace, step_socket_open_file,
    step_socket_open_file_in_namespace, step_tcp_loopback_handshake, step_tcp_loopback_transfer,
    ArpFlushOutcome, DeviceTxBudget, DeviceTxOutcome, LoopbackTcpConnectOutcome,
    LoopbackTcpTransferOutcome, NetworkBacklogTickOutcome, NetworkStepOutcome, ShutdownOutcome,
    SocketCloseOutcome, SocketOpenFileOutput, ARP_FLUSH_BUDGET_DEFAULT, DEVICE_TX_BUDGET_DEFAULT,
    NET_BACKLOG_SCAN_BUDGET, NET_EVENT_BUDGET,
};
pub use facade::{
    drive_socket_connect_waiting, drive_socket_nonblocking, socket_bind_facade,
    socket_connect_facade, socket_create_facade, socket_create_facade_in_namespace,
    socket_listen_facade, socket_poll_ready_facade, socket_shutdown_facade, SocketBindCapability,
    SocketBindOps, SocketConnectCapability, SocketConnectOps, SocketCreateOutput,
    SocketFacadeDriveMode, SocketHandle, SocketHandleFlags, SocketListenCapability,
    SocketListenOps, SocketPollCapability, SocketPollOps, SocketShutdownCapability,
    SocketShutdownOps,
};
pub use namespace::{
    create_isolated_net_namespace, drive_all_net_namespace_runtimes_at,
    drive_net_namespace_runtime_at, initial_loopback_iface, initial_net_namespace,
    initial_net_namespace_payload, net_namespace_open_file_from_payload,
    net_namespace_payload_from_file, NetNamespaceBridgeInfo, NetNamespaceForwardOutcome,
    NetNamespaceIdentity, NetNamespaceLinkInfo, NetNamespacePayload, NetNamespaceRouteConfig,
    NetNamespaceRouteDecision, NetNamespaceRouteInfo, NetNamespaceRouteKind,
    NetNamespaceRouteSelector, NetNamespaceRuntimeOutcome, NetNamespaceSnapshot,
};
#[cfg(any(test, feature = "test-support"))]
pub use namespace::{create_isolated_net_namespace_for_test, reset_initial_net_namespace_for_test};
#[cfg(any(test, feature = "test-support"))]
pub use netfilter::reset_netfilter_for_test;
pub use netfilter::{
    add_dnat_rule_for_test_or_bootstrap, add_masquerade_rule_for_test_or_bootstrap,
    add_netfilter_rule_for_test_or_bootstrap,
    add_netfilter_rule_in_namespace_for_test_or_bootstrap, apply_netfilter_control_command,
    apply_postrouting_nat_ipv4, apply_postrouting_nat_ipv4_in_namespace, apply_prerouting_nat_ipv4,
    apply_prerouting_nat_ipv4_in_namespace, cleanup_netfilter_device_state_for_test_or_bootstrap,
    cleanup_netfilter_device_state_in_namespace_for_test_or_bootstrap,
    flush_netfilter_rules_and_conntrack_for_test_or_bootstrap, netfilter_conntrack_snapshot,
    netfilter_conntrack_snapshot_for_namespace, netfilter_rule_snapshots,
    netfilter_rule_snapshots_for_namespace, netfilter_rules_snapshot,
    netfilter_rules_snapshot_for_namespace, netfilter_stats_snapshot,
    remove_netfilter_rule_for_test_or_bootstrap, run_frame_hook, run_frame_hook_in_namespace,
    NetfilterConntrackProtocol, NetfilterConntrackSnapshot, NetfilterFrameContext, NetfilterHook,
    NetfilterIpv4Cidr, NetfilterNatKind, NetfilterRule, NetfilterRuleCounters,
    NetfilterRuleSnapshot, NetfilterState, NetfilterStatsSnapshot, NetfilterTable, NetfilterTarget,
    NetfilterVerdict,
};
pub use nfnetlink::{
    netlink_netfilter_recv, netlink_netfilter_send, nfnetlink_handle_request,
    nfnetlink_handle_request_in_namespace, nfnetlink_handle_request_in_namespace_with_cred,
    nfnetlink_handle_request_with_cred, NetlinkNetfilterState, RawNetlinkNetfilterSocket,
    NETLINK_NETFILTER, NFNL_MSG_BATCH_BEGIN, NFNL_MSG_BATCH_END, NFNL_SUBSYS_NFTABLES,
    NFPROTO_IPV4, NFT_MSG_DELCHAIN, NFT_MSG_DELRULE, NFT_MSG_DELTABLE, NFT_MSG_GETCHAIN,
    NFT_MSG_GETGEN, NFT_MSG_GETRULE, NFT_MSG_GETTABLE, NFT_MSG_NEWCHAIN, NFT_MSG_NEWGEN,
    NFT_MSG_NEWRULE, NFT_MSG_NEWTABLE,
};
pub use packet::{
    demux_rx_frame_with_smoltcp, NetworkPublish, PacketDispatch, PacketSource, PacketTxReadiness,
    PacketTxResult, PacketTxSink, RxFrame, TcpPacketEvent, TcpPacketFlags, UdpPacketEvent,
};
pub use project::{
    proc_net_arp_snapshot_text, proc_net_arp_snapshot_zero_text, proc_net_dev_snapshot_text,
    proc_net_dev_snapshot_text_for_namespace, proc_net_netfilter_rules_text,
    proc_net_netfilter_rules_text_for_namespace, proc_net_nf_conntrack_text,
    proc_net_nf_conntrack_text_for_namespace, proc_net_route_snapshot_text,
};
pub use protocol::{
    ArpEntry, EtherIface, EtherPacketSource, EtherPacketTxSink, Icmpv4EchoPacket, Icmpv4Event,
    NetStats, RawIcmpSocket, RawTcpSocket, RawUdpSocket, SmoltcpAdapter, SmoltcpAdapterConfig,
    SmoltcpPacketSource, SmoltcpPacketTxSink,
};
pub use rtnetlink::{
    netlink_route_recv, netlink_route_send, netlink_route_send_with_netns_resolver,
    netlink_route_send_with_netns_resolvers, rtnetlink_handle_request,
    rtnetlink_handle_request_with_netns_resolver, rtnetlink_handle_request_with_netns_resolvers,
    NetlinkRouteState, RawNetlinkRouteSocket, AF_NETLINK, NETLINK_ROUTE, NLMSG_DONE, NLMSG_ERROR,
    NLM_F_ACK, NLM_F_DUMP, NLM_F_MULTI, NLM_F_REQUEST, RTM_DELLINK, RTM_DELROUTE, RTM_GETADDR,
    RTM_GETLINK, RTM_GETNEIGH, RTM_GETROUTE, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWNEIGH, RTM_NEWROUTE,
    RTM_SETLINK,
};
pub use structure::{
    AcceptWireSet, AddressFamily, ConnectionKey, InitialSocketTableProxy, IpEndpoint,
    IpLevelOptions, Ipv4Address, KernelSockAddr, LingerOption, ListenerKey, LocalEndpointKey,
    PacketSocketState, PollMask, ProtocolNumber, RawIcmpSocketKey, RawIcmpState, RecvWireSet,
    SendRecvFlags, SendWireSet, SockAddrIn, SockAddrLl, SockFlags, SockShutdownCmd, SocketIdentity,
    SocketIoState, SocketKind, SocketLevelOptions, SocketOperationalEvidence, SocketOptionSet,
    SocketPayload, SocketProtocol, SocketReadiness, SocketRecvBytesOutcome, SocketTable,
    SocketType, SocketWaitCarriers, Takeable, TcpLevelOptions, TcpState, UdpInner,
    UnixDatagramState, UnixSocketPath, UnixStreamState, UrgentEvent, ValidSocketType,
    UNIX_SOCKET_PATH_MAX,
};

pub(crate) fn register_zones() -> Result<(), tx_substrate::zone::ZoneError> {
    namespace::register_zones()?;
    structure::registry::register_zones()?;
    Ok(())
}

#[cfg(test)]
mod tests;
