use super::structure::{
    registry, AcceptWireSet, AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address,
    Ipv4MulticastGroup, Ipv6Address, KernelSockAddr, PollMask, ProtocolNumber, RawIcmpState,
    RecvWireSet, SendRecvFlags, SendWireSet, SockAddrIn, SockAddrIn6, SockShutdownCmd,
    SocketIdentity, SocketKind, SocketOptionSet, SocketProtocol, SocketType, TcpState,
    TcpTlsUlpState, UdpInner, ValidSocketType,
};
use crate::execution::{Errno, WaitToken};
use crate::net::checks::require::{
    require_socket_accept_target, require_socket_bind_target, require_socket_connect_target,
    require_socket_listen_target, require_socket_payload_live, require_socket_poll_target,
    require_socket_read_target, require_socket_shutdown_target, require_socket_write_target,
};
use crate::net::delegate::{
    net_delegate_step_once, net_delegate_task_loop, net_delegate_task_loop_with_deadline_hook,
    net_delegate_wait_supervised_deadline, net_delegate_wait_tick_deadline,
    smoltcp_instant_to_reactor_deadline_ns, NetDelegateDriver, NetDelegateSupervisor,
    NetDelegateTaskConfig,
};
use crate::net::device::{
    create_bridge_for_test_or_bootstrap, create_veth_pair_for_test_or_bootstrap, BridgeConfig,
    BridgeInstance, EthernetAddress, NetDeviceKind, NetDeviceOps, NetDeviceRegistration,
    VethEndpointConfig, VethPair, VethPairConfig, VirtioNetConfig, VirtioNetDevice,
    VirtioNetFeatureSet, VirtioNetIrqEvent, VirtioNetQueueConfig, VETH_DEFAULT_MTU,
    VIRTIO_NET_DEFAULT_MTU, VIRTIO_NET_STAGING_MAJOR,
};
use crate::net::execution::{
    socket_accept_wait_token, socket_recv_wait_token, socket_send_wait_token,
    socket_urgent_wait_token, step_accept, step_bind, step_connect, step_listen, step_poll_ready,
    step_process_device_tx_pending_in_namespace_at, step_process_loopback_pending,
    step_process_loopback_pending_zero, step_process_loopback_tcp,
    step_process_loopback_udp_on_iface, step_process_network_events,
    step_process_network_events_at, step_process_network_events_in_namespace_at,
    step_process_network_tick, step_process_network_tick_loopback, step_recv,
    step_recv_kernel_bytes, step_send, step_send_kernel_bytes, step_send_to_kernel_bytes,
    step_send_to_kernel_bytes_with_poll_kick, step_send_udp_loopback_kernel_bytes, step_shutdown,
    step_socket_close, step_socket_create, step_tcp_backlog_cleanup, step_tcp_close_staging,
    step_tcp_connection_cleanup, step_tcp_loopback_handshake, step_tcp_loopback_handshake_on_iface,
    step_tcp_loopback_transfer, DeviceTxBudget, LoopbackPollBudget, NET_EVENT_BUDGET,
    TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS, TCP_BACKLOG_TIMEOUT_STAGING_MILLIS,
};
use crate::net::facade::{
    drive_socket_nonblocking, socket_create_facade, socket_listen_facade, socket_poll_ready_facade,
    SocketBindOps,
};
use crate::net::packet::{
    demux_rx_frame_with_smoltcp, LoopbackIpPacket, PacketDispatch, PacketSource, PacketTxReadiness,
    PacketTxResult, PacketTxSink, RxFrame, TcpPacketEvent, TcpPacketFlags, UdpPacketEvent,
};
use crate::net::protocol::{
    build_icmpv4_echo_request_message, decide_ipv4_route, decide_ipv6_route, loopback_iface,
    ArpSnapshotState, EtherIface, EtherPacketSource, EtherPacketTxSink, Icmpv4EchoPacket,
    Icmpv4Event, IfaceCommon, Ipv4RouteDecision, Ipv6RouteDecision, LoopbackIface, PollContext,
    RawTcpSocket, RawUdpSocket, SmoltcpAdapter, SmoltcpAdapterConfig, SmoltcpPacketSource,
    SmoltcpPacketTxSink, UdpTxDatagram, ARP_REQUEST_RETRY_LIMIT, TCP_CORK_AUTO_FLUSH_BYTES,
};
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::{
    add_dnat_rule_for_test_or_bootstrap, add_masquerade_rule_for_test_or_bootstrap,
    apply_postrouting_nat_ipv4, apply_prerouting_nat_ipv4, netfilter_conntrack_snapshot,
    netfilter_rules_snapshot, netfilter_stats_snapshot, require_net_admin,
    reset_netfilter_for_test, NetAdminAuthority, NetNamespaceLinkInfo, NetfilterConntrackProtocol,
    NetfilterFrameContext, NetfilterHook, NetfilterIpv4Cidr, NetfilterNatKind,
};
use crate::{device::DevT, execution::Guard};
use core::future::Future;
use core::pin::Pin;
use core::ptr::null;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use tx_reactor::{wait::WaitOutcome, Reactor};
use tx_substrate::step::{NoProgress, StepOutcome, YieldShape};

mod bridge_tests;
mod byte_io_tests;
mod clock_tests;
mod delegate_loopback_tests;
mod delegate_supervisor_tests;
mod ether_iface_arp_tests;
mod external_connect_tests;
mod icmp_tests;
mod loopback_pending_tests;
mod loopback_tests;
mod netdevice_staging_tests;
mod nfnetlink_tests;
mod projection_tests;
mod rds_sctp_ltp_tests;
mod rtnetlink_tests;
mod smoltcp_fork_tests;
mod table_snapshot_tests;
mod tcp_graceful_shutdown_tests;
mod veth_tests;
mod virtio_net_device_tests;

struct ScriptedPacketSource {
    packets: std::sync::Mutex<std::vec::Vec<PacketDispatch>>,
}

impl ScriptedPacketSource {
    fn new(packets: std::vec::Vec<PacketDispatch>) -> Self {
        Self {
            packets: std::sync::Mutex::new(packets),
        }
    }
}

impl PacketSource for ScriptedPacketSource {
    fn next_packet(&self) -> Option<PacketDispatch> {
        let mut packets = self.packets.lock().expect("packet source lock");
        if packets.is_empty() {
            None
        } else {
            Some(packets.remove(0))
        }
    }
}

const NOOP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(null(), &NOOP_WAKER_VTABLE),
    |_| {},
    |_| {},
    |_| {},
);

fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(null(), &NOOP_WAKER_VTABLE)) }
}

fn init_zones() {
    tx_substrate::testing::init_host_for_test_once();
    let _ = crate::zones::register_all();
}

fn inet(port: u16) -> KernelSockAddr {
    KernelSockAddr::V4(SockAddrIn::new(port, Ipv4Address::LOOPBACK))
}

fn any_inet(port: u16) -> KernelSockAddr {
    KernelSockAddr::V4(SockAddrIn::new(port, Ipv4Address::UNSPECIFIED))
}

fn any_inet6(port: u16) -> KernelSockAddr {
    KernelSockAddr::V6(SockAddrIn6::new(port, Ipv6Address::UNSPECIFIED))
}

fn inet_addr(port: u16, addr: Ipv4Address) -> KernelSockAddr {
    KernelSockAddr::V4(SockAddrIn::new(port, addr))
}

fn endpoint(port: u16) -> IpEndpoint {
    IpEndpoint::new(Ipv4Address::LOOPBACK, port)
}

fn expect_carrier_yield<T, P>(outcome: StepOutcome<T, P>) -> WaitToken {
    match outcome {
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, interests },
            ..
        } => WaitToken::new(source.raw(), interests.raw()),
        _ => panic!("expected OnWaitSource yield"),
    }
}

fn ethernet_ipv4_frame(protocol: u8, transport: &[u8]) -> std::vec::Vec<u8> {
    let mut frame = std::vec::Vec::new();
    frame.extend_from_slice(&[0x02, 0, 0, 0, 0, 2]);
    frame.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
    frame.extend_from_slice(&[0x08, 0x00]);
    frame.push(0x45);
    frame.push(0);
    let total_len = u16::try_from(20 + transport.len()).expect("test frame length");
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&[0, 0]);
    frame.push(64);
    frame.push(protocol);
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&[192, 0, 2, 1]);
    frame.extend_from_slice(&[192, 0, 2, 2]);
    frame.extend_from_slice(transport);
    fill_frame_checksums(&mut frame);
    frame
}

fn ethernet_ipv6_frame(protocol: u8, transport: &[u8]) -> std::vec::Vec<u8> {
    let mut frame = std::vec::Vec::new();
    frame.extend_from_slice(&[0x02, 0, 0, 0, 0, 2]);
    frame.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
    frame.extend_from_slice(&[0x86, 0xdd]);
    frame.extend_from_slice(&[0x60, 0, 0, 0]); // version 6, tc/flow 0
    let payload_len = u16::try_from(transport.len()).expect("test frame length");
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.push(protocol); // next header
    frame.push(64); // hop limit
                    // 2001:db8::1 -> 2001:db8::2
    frame.extend_from_slice(&[
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ]);
    frame.extend_from_slice(&[
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
    ]);
    frame.extend_from_slice(transport);
    fill_frame_checksums(&mut frame);
    frame
}

/// Patch the IPv4 header checksum and the L4 (TCP/UDP) checksum of a
/// hand-assembled ethernet frame so it looks like real wire traffic. The demux
/// (R3a) verifies these, so test frames must carry valid checksums; corruption
/// tests deliberately damage a byte afterwards.
fn fill_frame_checksums(frame: &mut [u8]) {
    use smoltcp::wire::{IpAddress, Ipv4Packet, Ipv6Packet};

    // Ethernet header is 14 bytes; the IP packet follows.
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        0x0800 => {
            let mut ip = Ipv4Packet::new_unchecked(&mut frame[14..]);
            ip.fill_checksum();
            let src = IpAddress::Ipv4(ip.src_addr());
            let dst = IpAddress::Ipv4(ip.dst_addr());
            let protocol = ip.next_header();
            fill_l4_checksum(protocol, ip.payload_mut(), &src, &dst);
        }
        0x86dd => {
            let mut ip = Ipv6Packet::new_unchecked(&mut frame[14..]);
            let src = IpAddress::Ipv6(ip.src_addr());
            let dst = IpAddress::Ipv6(ip.dst_addr());
            let protocol = ip.next_header();
            fill_l4_checksum(protocol, ip.payload_mut(), &src, &dst);
        }
        _ => {}
    }
}

fn fill_l4_checksum(
    protocol: smoltcp::wire::IpProtocol,
    l4: &mut [u8],
    src: &smoltcp::wire::IpAddress,
    dst: &smoltcp::wire::IpAddress,
) {
    use smoltcp::wire::{IpProtocol, TcpPacket, UdpPacket};
    match protocol {
        IpProtocol::Tcp => TcpPacket::new_unchecked(l4).fill_checksum(src, dst),
        IpProtocol::Udp => UdpPacket::new_unchecked(l4).fill_checksum(src, dst),
        _ => {}
    }
}

fn udp_transport(src_port: u16, dst_port: u16, payload: &[u8]) -> std::vec::Vec<u8> {
    let mut transport = std::vec::Vec::new();
    transport.extend_from_slice(&src_port.to_be_bytes());
    transport.extend_from_slice(&dst_port.to_be_bytes());
    let len = u16::try_from(8 + payload.len()).expect("test udp length");
    transport.extend_from_slice(&len.to_be_bytes());
    transport.extend_from_slice(&[0, 0]);
    transport.extend_from_slice(payload);
    transport
}

fn tcp_transport(src_port: u16, dst_port: u16, flags: u8, payload: &[u8]) -> std::vec::Vec<u8> {
    let mut transport = std::vec::Vec::new();
    transport.extend_from_slice(&src_port.to_be_bytes());
    transport.extend_from_slice(&dst_port.to_be_bytes());
    transport.extend_from_slice(&1u32.to_be_bytes());
    transport.extend_from_slice(&2u32.to_be_bytes());
    transport.push(0x50);
    transport.push(flags);
    transport.extend_from_slice(&4096u16.to_be_bytes());
    transport.extend_from_slice(&[0, 0]);
    transport.extend_from_slice(&[0, 0]);
    transport.extend_from_slice(payload);
    transport
}

struct ScriptedNetDevice {
    frames: std::sync::Mutex<std::vec::Vec<RxFrame>>,
}

impl ScriptedNetDevice {
    fn new(frames: std::vec::Vec<RxFrame>) -> Self {
        Self {
            frames: std::sync::Mutex::new(frames),
        }
    }
}

impl NetDeviceOps for ScriptedNetDevice {
    fn receive(&self) -> Option<RxFrame> {
        let mut frames = self.frames.lock().expect("net device frame lock");
        if frames.is_empty() {
            None
        } else {
            Some(frames.remove(0))
        }
    }

    fn transmit(&self, _frame: &[u8], _guard: &Guard<'_>) -> crate::execution::StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn mac_addr(&self) -> EthernetAddress {
        EthernetAddress::new([0x02, 0, 0, 0, 0, 1])
    }

    fn mtu(&self) -> u16 {
        1500
    }
}

mod accept_poll_tests;
mod checks_bind_tests;
mod core_structure_tests;
mod io_step_tests;
mod packet_event_tests;
