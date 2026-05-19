use super::structure::{
    registry, AcceptWireSet, AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address, KernelSockAddr,
    PollMask, ProtocolNumber, RawIcmpState, RecvWireSet, SendRecvFlags, SendWireSet, SockAddrIn,
    SockShutdownCmd, SocketIdentity, SocketKind, SocketOptionSet, SocketProtocol, SocketType,
    TcpState, UdpInner, ValidSocketType,
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
    step_process_loopback_tcp, step_process_loopback_udp_on_iface, step_process_network_events,
    step_process_network_events_at, step_process_network_events_in_namespace_at,
    step_process_network_tick, step_process_network_tick_loopback, step_recv,
    step_recv_kernel_bytes, step_send, step_send_kernel_bytes, step_send_to_kernel_bytes,
    step_shutdown, step_socket_create, step_tcp_backlog_cleanup, step_tcp_close_staging,
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
    build_icmpv4_echo_request_message, decide_ipv4_route, loopback_iface, parse_icmpv4_payload,
    ArpSnapshotState, EtherIface, EtherPacketSource, EtherPacketTxSink, Icmpv4EchoPacket,
    Icmpv4Event, IfaceCommon, Ipv4RouteDecision, LoopbackIface, PollContext, RawTcpSocket,
    RawUdpSocket, SmoltcpAdapter, SmoltcpAdapterConfig, SmoltcpPacketSource, SmoltcpPacketTxSink,
    UdpTxDatagram, ARP_REQUEST_RETRY_LIMIT,
};
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::{
    netfilter_stats_snapshot, require_net_admin, reset_netfilter_for_test, NetAdminAuthority,
    NetNamespaceLinkInfo,
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
mod delegate_loopback_tests;
mod delegate_supervisor_tests;
mod ether_iface_arp_tests;
mod icmp_tests;
mod loopback_pending_tests;
mod loopback_tests;
mod netdevice_staging_tests;
mod projection_tests;
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
    frame
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

#[test]
fn ipv4_address_preserves_octets() {
    let addr = Ipv4Address::new([127, 0, 0, 1]);

    assert_eq!(addr.octets(), [127, 0, 0, 1]);
}

#[test]
fn initial_net_namespace_payload_owns_socket_table_state() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let socket = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("socket");
    let local = endpoint(41_901);

    SOCKET_TABLE
        .bind_tcp(local, socket.clone())
        .expect("bind in initial net namespace");

    let initial_payload = crate::net::initial_net_namespace_payload();
    let found = initial_payload
        .socket_table()
        .lookup_tcp_bound(local, &guard)
        .expect("socket table lookup");

    assert_eq!(found.raw(), socket.raw());
    assert_eq!(
        initial_payload.loopback_iface().local_ipv4(),
        Ipv4Address::LOOPBACK
    );
}

#[test]
fn isolated_net_namespaces_allow_same_tcp_endpoint_bind() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let ns_a = crate::net::create_isolated_net_namespace_for_test("n71-a")
        .expect("net namespace a")
        .payload_cap()
        .expect("net namespace a payload");
    let ns_b = crate::net::create_isolated_net_namespace_for_test("n71-b")
        .expect("net namespace b")
        .payload_cap()
        .expect("net namespace b payload");
    let sock_a = registry::create_socket_in_namespace(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
        ns_a.clone(),
    )
    .expect("socket a");
    let sock_b = registry::create_socket_in_namespace(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
        ns_b.clone(),
    )
    .expect("socket b");
    let local = endpoint(41_902);

    assert_eq!(
        step_bind(&sock_a, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&sock_b, inet(local.port), &guard),
        StepOutcome::Done(())
    );

    let found_a = ns_a
        .socket_table()
        .lookup_tcp_bound(local, &guard)
        .expect("socket a lookup");
    let found_b = ns_b
        .socket_table()
        .lookup_tcp_bound(local, &guard)
        .expect("socket b lookup");
    assert_eq!(found_a.raw(), sock_a.raw());
    assert_eq!(found_b.raw(), sock_b.raw());
    assert_ne!(found_a.raw(), found_b.raw());
}

#[test]
fn net_namespace_link_snapshot_reports_loopback_first() {
    init_zones();
    let payload = crate::net::initial_net_namespace_payload();
    let links = payload.link_snapshot();

    assert!(!links.is_empty());
    assert_eq!(links[0].ifindex, 1);
    assert_eq!(links[0].name, "lo");
    assert!(links[0].is_loopback);
    assert_eq!(links[0].ipv4_addr, Some(Ipv4Address::LOOPBACK));
}

#[test]
fn endpoint_orders_by_addr_and_port() {
    let first = IpEndpoint::new(Ipv4Address::new([10, 0, 0, 1]), 80);
    let second = IpEndpoint::new(Ipv4Address::new([10, 0, 0, 1]), 443);
    let third = IpEndpoint::new(Ipv4Address::new([10, 0, 0, 2]), 1);

    assert!(first < second);
    assert!(second < third);
}

#[test]
fn socket_type_validation_maps_to_kind() {
    let unix_dgram = ValidSocketType::validate(1, 2, 0).expect("unix dgram socket");
    let stream = ValidSocketType::validate(2, 1, 6).expect("tcp socket");
    let dgram = ValidSocketType::validate(2, 2, 17).expect("udp socket");
    let dgram_icmp = ValidSocketType::validate(2, 2, 1).expect("ping socket");
    let raw_icmp = ValidSocketType::validate(2, 3, 1).expect("raw icmp socket");
    let default_stream = ValidSocketType::validate(2, 1, 0).expect("default tcp socket");

    assert_eq!(unix_dgram.domain, AddressFamily::Unix);
    assert_eq!(
        SocketKind::from_valid_socket_type(unix_dgram),
        Ok(SocketKind::UnixDatagram)
    );
    assert_eq!(stream.sock_type, SocketType::Stream);
    assert_eq!(
        SocketKind::from_valid_socket_type(stream),
        Ok(SocketKind::Tcp)
    );
    assert_eq!(dgram.sock_type, SocketType::Dgram);
    assert_eq!(
        SocketKind::from_valid_socket_type(dgram),
        Ok(SocketKind::Udp)
    );
    assert_eq!(
        SocketKind::from_valid_socket_type(dgram_icmp),
        Ok(SocketKind::RawIcmp)
    );
    assert_eq!(raw_icmp.sock_type, SocketType::Raw);
    assert_eq!(
        SocketKind::from_valid_socket_type(raw_icmp),
        Ok(SocketKind::RawIcmp)
    );
    assert_eq!(
        SocketKind::from_valid_socket_type(default_stream),
        Ok(SocketKind::Tcp)
    );
    assert_eq!(
        ValidSocketType::validate(99, 1, 0),
        Err(Errno::EAFNOSUPPORT)
    );
    assert_eq!(
        ValidSocketType::validate(2, 999, 0),
        Err(Errno::ESOCKTNOSUPPORT)
    );
}

#[test]
fn raw_icmp_socket_identity_payload_split() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let valid = ValidSocketType::validate(2, 3, 1).expect("raw icmp");
    let socket = match step_socket_create(valid, &guard) {
        StepOutcome::Done(socket) => socket,
        other => panic!("unexpected raw icmp create outcome: {other:?}"),
    };

    assert_eq!(socket.kind, SocketKind::RawIcmp);
    let payload = socket.acquire_operational().expect("payload");
    assert!(payload.raw_icmp_socket().is_some());
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::RawIcmp(RawIcmpState::new(ProtocolNumber(1)))
    );
    assert!(SOCKET_TABLE
        .snapshot_raw_icmp(&guard)
        .iter()
        .any(|candidate| candidate.raw() == socket.raw()));
}

#[test]
fn raw_icmp_bind_records_local_addr_without_port() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let socket = registry::create_socket_for_test_or_bootstrap(
        SocketKind::RawIcmp,
        SocketOptionSet::for_kind(SocketKind::RawIcmp),
    )
    .expect("raw icmp");

    let local = KernelSockAddr::V4(SockAddrIn::new(0, Ipv4Address::LOOPBACK));
    assert_eq!(step_bind(&socket, local, &guard), StepOutcome::Done(()));
    assert_eq!(
        socket
            .acquire_operational()
            .expect("payload")
            .protocol_snapshot(),
        SocketProtocol::RawIcmp(RawIcmpState {
            bound_local: Some(Ipv4Address::LOOPBACK),
            protocol: ProtocolNumber(1),
        })
    );
}

#[test]
fn send_recv_flags_validate_mask() {
    let flags =
        SendRecvFlags::validate((SendRecvFlags::MSG_DONTWAIT | SendRecvFlags::MSG_PEEK).bits())
            .expect("known flags");

    assert!(flags.is_nonblocking());
    assert!(flags.contains(SendRecvFlags::MSG_PEEK));
    assert!(SendRecvFlags::empty().is_empty());
    assert_eq!(SendRecvFlags::validate(0x4000_0000), Err(Errno::EINVAL));
}

#[test]
fn socket_option_set_default_has_documented_limits() {
    let options = SocketOptionSet::default_tcp();

    assert!(options.socket.recv_buf_size > 0);
    assert!(options.socket.send_buf_size > 0);
    assert!(!options.socket.linger.enabled);
    assert_eq!(options.ip.ttl, 64);
    assert_eq!(options.tcp.maxseg, 536);
}

#[test]
fn readiness_wire_sets_preserve_bits() {
    let recv = RecvWireSet::HAS_DATA | RecvWireSet::BROKEN;
    let send = SendWireSet::SPACE | SendWireSet::BROKEN;
    let accept = AcceptWireSet::HAS_PENDING | AcceptWireSet::BROKEN;

    assert!(recv.contains(RecvWireSet::HAS_DATA));
    assert!(send.contains(SendWireSet::SPACE));
    assert!(accept.contains(AcceptWireSet::HAS_PENDING));
    assert_eq!(RecvWireSet::DECLARED_BITS, recv.bits());
}

#[test]
fn socket_identity_starts_with_live_payload() {
    init_zones();
    let identity = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("socket");

    assert!(identity.live_payload().is_some());
    assert!(identity.acquire_operational().is_some());
    assert!(identity.is_payload_live());
}

#[test]
fn socket_identity_can_drop_payload_without_dropping_identity() {
    init_zones();
    let identity = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("socket");

    assert!(identity.take_payload().is_some());
    assert!(identity.live_payload().is_none());
    assert_eq!(identity.kind, SocketKind::Udp);
}

#[test]
fn socket_payload_initial_protocol_matches_kind() {
    init_zones();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");

    let tcp_payload = tcp.acquire_operational().expect("tcp payload");
    let udp_payload = udp.acquire_operational().expect("udp payload");

    match tcp_payload.protocol_snapshot() {
        SocketProtocol::Tcp(state) => assert_eq!(state, TcpState::Init),
        SocketProtocol::UnixDatagram
        | SocketProtocol::Udp(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_) => panic!("tcp socket has wrong protocol"),
    }
    match udp_payload.protocol_snapshot() {
        SocketProtocol::Udp(inner) => assert_eq!(inner, UdpInner::Unbound),
        SocketProtocol::UnixDatagram
        | SocketProtocol::Tcp(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_) => panic!("udp socket has wrong protocol"),
    }
}

#[test]
fn raw_tcp_socket_owns_smoltcp_buffers_from_socket_options() {
    let mut options = SocketOptionSet::default_tcp();
    options.socket.recv_buf_size = 128;
    options.socket.send_buf_size = 64;
    options.socket.keep_alive = true;
    options.tcp.keepidle = 10;
    options.tcp.nodelay = true;

    let raw = RawTcpSocket::new(&options);

    assert_eq!(raw.recv_capacity(), 128);
    assert_eq!(raw.send_capacity(), 64);
    assert!(!raw.can_recv());
    assert!(!raw.may_recv());
    assert!(!raw.may_send());
}

#[test]
fn raw_udp_socket_owns_smoltcp_packet_buffers_from_socket_options() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.recv_buf_size = 4096;
    options.socket.send_buf_size = 2048;

    let raw = RawUdpSocket::new(&options);

    assert_eq!(raw.recv_capacity(), 4096);
    assert_eq!(raw.send_capacity(), 2048);
    assert_eq!(raw.recv_packet_capacity(), 3);
    assert_eq!(raw.send_packet_capacity(), 2);
    assert!(!raw.can_recv());
    assert!(raw.can_send());
}

#[test]
fn socket_payload_installs_matching_raw_socket_owner() {
    init_zones();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");

    let tcp_payload = tcp.acquire_operational().expect("tcp payload");
    let udp_payload = udp.acquire_operational().expect("udp payload");

    assert!(tcp_payload.raw_tcp_socket().is_some());
    assert!(tcp_payload.raw_udp_socket().is_none());
    assert!(udp_payload.raw_tcp_socket().is_none());
    assert!(udp_payload.raw_udp_socket().is_some());
}

#[test]
fn socket_payload_shutdown_flags_start_clear() {
    init_zones();
    let identity = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("socket");
    let payload = identity.acquire_operational().expect("payload");

    assert!(!payload.shutdown_rd());
    assert!(!payload.shutdown_wr());
}

#[test]
fn socket_readiness_has_independent_queues() {
    init_zones();
    let identity = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("socket");

    assert_eq!(identity.readiness.recv_wq.peek(), 0);
    assert_eq!(identity.readiness.send_wq.peek(), 0);
    assert_eq!(identity.readiness.accept_wq.peek(), 0);

    identity.readiness.fire_recv(RecvWireSet::HAS_DATA);

    assert_eq!(
        identity.readiness.recv_wq.peek(),
        RecvWireSet::HAS_DATA.bits()
    );
    assert_eq!(identity.readiness.send_wq.peek(), 0);
    assert_eq!(identity.readiness.accept_wq.peek(), 0);
}

#[test]
fn socket_wait_carriers_register_rawqueue_and_rawport() {
    init_zones();
    let identity = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("socket");

    assert!(crate::wait_source::lookup_wait_queue(identity.wait_carriers.recv).is_some());
    assert!(crate::wait_source::lookup_wait_queue(identity.wait_carriers.send).is_some());
    assert!(crate::wait_source::lookup_wait_queue(identity.wait_carriers.accept).is_some());
    assert!(crate::wait_source::lookup_wait_port(identity.wait_carriers.urgent).is_some());

    assert_eq!(
        socket_recv_wait_token(&identity).source_id(),
        identity.wait_carriers.recv
    );
    assert_eq!(
        socket_send_wait_token(&identity).source_id(),
        identity.wait_carriers.send
    );
    assert_eq!(
        socket_accept_wait_token(&identity).source_id(),
        identity.wait_carriers.accept
    );
    assert_eq!(
        socket_urgent_wait_token(&identity).source_id(),
        identity.wait_carriers.urgent
    );
}

#[test]
fn net_delegate_queue_registers_rawqueue_and_wakes_on_poll() {
    let bits =
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK;
    crate::net::delegate::net_delegate_clear(bits);
    let token = crate::net::delegate::net_delegate_wait_token();
    assert!(crate::wait_source::lookup_wait_queue(token.source_id()).is_some());
    assert_eq!(token.interest(), bits.bits());

    let mut future = crate::wait_source::wait_on_token(token).expect("delegate wait future");
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(Pin::new(&mut future).poll(&mut cx), Poll::Pending));
    crate::net::delegate::net_delegate_kick_poll();
    assert!(matches!(
        Pin::new(&mut future).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    ));
    crate::net::delegate::net_delegate_clear(bits);
}

#[test]
fn net_zones_register() {
    init_zones();
}

#[test]
fn checks_require_witnesses_preserve_guard_scoped_identity() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let tcp_for_connect = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("connect socket");
    let local = inet(40_001);

    let live = require_socket_payload_live(&tcp, &guard).expect("payload witness");
    assert_eq!(live.identity.raw(), tcp.raw());

    let read =
        require_socket_read_target(&tcp, SendRecvFlags::MSG_PEEK, &guard).expect("read witness");
    assert_eq!(read.identity.raw(), tcp.raw());
    assert!(read.flags.contains(SendRecvFlags::MSG_PEEK));

    let write =
        require_socket_write_target(&tcp, SendRecvFlags::empty(), &guard).expect("write witness");
    assert_eq!(write.identity.raw(), tcp.raw());
    assert!(write.flags.is_empty());

    let bind = require_socket_bind_target(&tcp, local, &guard).expect("bind witness");
    assert_eq!(bind.identity.raw(), tcp.raw());
    assert_eq!(bind.addr, local);
    assert_eq!(bind.local.port, 40_001);

    assert_eq!(step_bind(&tcp, local, &guard), StepOutcome::Done(()));
    let listen = require_socket_listen_target(&tcp, 8, &guard).expect("listen witness");
    assert_eq!(listen.identity.raw(), tcp.raw());
    assert_eq!(listen.backlog_limit, 8);
    assert_eq!(listen.local.port, 40_001);

    assert_eq!(step_listen(&tcp, 8, &guard), StepOutcome::Done(()));
    let accept = require_socket_accept_target(&tcp, &guard).expect("accept witness");
    assert_eq!(accept.identity.raw(), tcp.raw());

    let shutdown =
        require_socket_shutdown_target(&tcp, SockShutdownCmd::Both, &guard).expect("shutdown");
    assert_eq!(shutdown.identity.raw(), tcp.raw());
    assert_eq!(shutdown.how, SockShutdownCmd::Both);

    let connect = require_socket_connect_target(&tcp_for_connect, inet(40_002), &guard)
        .expect("connect witness");
    assert_eq!(connect.identity.raw(), tcp_for_connect.raw());
    assert_eq!(connect.remote.port, 40_002);

    let poll = require_socket_poll_target(&tcp, &guard).expect("poll witness");
    assert_eq!(poll.identity.raw(), tcp.raw());
}

#[test]
fn checks_reject_udp_listen_and_write_after_shutdown() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");

    assert_eq!(
        require_socket_listen_target(&udp, 4, &guard).map(|_| ()),
        Err(Errno::EOPNOTSUPP)
    );
    assert_eq!(
        step_shutdown(&udp, SockShutdownCmd::Send, &guard),
        StepOutcome::Done(super::ShutdownOutcome {
            recv_shutdown: false,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 0,
            delegate_kicked: false,
        })
    );
    assert_eq!(
        require_socket_write_target(&udp, SendRecvFlags::empty(), &guard).map(|_| ()),
        Err(Errno::EPIPE)
    );
}

#[test]
fn execution_socket_create_installs_matching_payload() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let valid = ValidSocketType::validate(2, 1, 6).expect("valid tcp");
    let socket = match step_socket_create(valid, &guard) {
        StepOutcome::Done(socket) => socket,
        other => panic!("unexpected create outcome: {other:?}"),
    };

    assert_eq!(socket.kind, SocketKind::Tcp);
    let payload = socket.acquire_operational().expect("payload");
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Init)
    );
}

#[test]
fn socket_create_facade_validates_raw_args_and_preserves_flags() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let type_with_flags =
        1 | super::SockFlags::SOCK_NONBLOCK.bits() | super::SockFlags::SOCK_CLOEXEC.bits();
    let output = match socket_create_facade(2, type_with_flags, 6, &guard) {
        StepOutcome::Done(output) => output,
        _ => panic!("unexpected create facade outcome"),
    };

    assert_eq!(output.handle.identity.kind, SocketKind::Tcp);
    assert!(output.handle.flags.nonblock);
    assert!(output.handle.flags.cloexec);
    assert!(matches!(
        socket_create_facade(99, 1, 0, &guard),
        StepOutcome::Err(Errno::EAFNOSUPPORT)
    ));
    assert!(matches!(
        socket_create_facade(2, 999, 0, &guard),
        StepOutcome::Err(Errno::ESOCKTNOSUPPORT)
    ));
}

#[test]
fn execution_bind_updates_protocol_and_socket_table() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let local = inet(40_011);

    assert_eq!(step_bind(&tcp, local, &guard), StepOutcome::Done(()));
    let payload = tcp.acquire_operational().expect("payload");
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Bound {
            local: local.as_ip_endpoint(),
        })
    );
    let registered = SOCKET_TABLE
        .lookup_tcp_bound(local.as_ip_endpoint(), &guard)
        .expect("tcp bound entry");
    assert_eq!(registered.raw(), tcp.raw());
}

#[test]
fn socket_facade_routes_bind_listen_and_poll_to_steps() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let output = match socket_create_facade(2, 1, 6, &guard) {
        StepOutcome::Done(output) => output,
        _ => panic!("socket create facade should succeed"),
    };
    let local = inet(40_016);

    assert_eq!(
        output.handle.bind_capability(local).bind(&guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        socket_listen_facade(output.handle.listen_capability(16), &guard),
        StepOutcome::Done(())
    );
    output
        .handle
        .identity
        .readiness
        .fire_accept(AcceptWireSet::HAS_PENDING);

    let mask = match socket_poll_ready_facade(output.handle.poll_capability(PollMask::IN), &guard) {
        StepOutcome::Done(mask) => mask,
        _ => panic!("socket poll facade should succeed"),
    };
    assert!(mask.contains(PollMask::IN));
    assert!(!mask.contains(PollMask::OUT));
}

#[test]
fn execution_bind_rejects_duplicate_local_endpoint() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let first = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("first tcp");
    let second = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("second tcp");
    let local = inet(40_012);

    assert_eq!(step_bind(&first, local, &guard), StepOutcome::Done(()));
    assert_eq!(
        step_bind(&second, local, &guard),
        StepOutcome::Err(Errno::EADDRINUSE)
    );
}

#[test]
fn execution_listen_promotes_tcp_bound_socket() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let local = inet(40_013);

    assert_eq!(step_bind(&tcp, local, &guard), StepOutcome::Done(()));
    assert_eq!(step_listen(&tcp, 4096, &guard), StepOutcome::Done(()));
    let payload = tcp.acquire_operational().expect("payload");
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Listening {
            local: local.as_ip_endpoint(),
            backlog_limit: super::execution::SOMAXCONN_STAGING,
        })
    );
    let registered = SOCKET_TABLE
        .lookup_tcp_listener(local.as_ip_endpoint(), &guard)
        .expect("tcp listener entry");
    assert_eq!(registered.raw(), tcp.raw());
}

#[test]
fn socket_table_lookup_tcp_connection_by_four_tuple() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let key = ConnectionKey::new(endpoint(40_120), endpoint(50_120));

    SOCKET_TABLE
        .insert_tcp_connection(key, tcp.clone())
        .expect("connection insert");
    let found = SOCKET_TABLE
        .lookup_tcp_connection(key, &guard)
        .expect("connection lookup");

    assert_eq!(found.raw(), tcp.raw());
}

#[test]
fn socket_table_listener_lookup_prefers_exact_over_wildcard() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let wildcard = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("wildcard listener");
    let exact = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("exact listener");
    let port = 40_121;

    assert_eq!(
        step_bind(&wildcard, any_inet(port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&wildcard, 8, &guard), StepOutcome::Done(()));
    assert_eq!(step_bind(&exact, inet(port), &guard), StepOutcome::Done(()));
    assert_eq!(step_listen(&exact, 8, &guard), StepOutcome::Done(()));

    let found = SOCKET_TABLE
        .lookup_tcp_listener_addr(Ipv4Address::LOOPBACK, port, &guard)
        .expect("exact listener lookup");
    assert_eq!(found.raw(), exact.raw());

    let wildcard_found = SOCKET_TABLE
        .lookup_tcp_listener_addr(Ipv4Address::new([10, 0, 0, 7]), port, &guard)
        .expect("wildcard listener lookup");
    assert_eq!(wildcard_found.raw(), wildcard.raw());
}

#[test]
fn execution_connect_blocks_tcp_and_completes_udp() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let remote = inet(40_014);

    let wait = expect_carrier_yield(step_connect(&tcp, remote, &guard));
    assert_ne!(wait.source_id(), tcp.raw() as u64);
    assert_eq!(wait.source_id(), tcp.wait_carriers.send);
    assert!(crate::wait_source::wait_on_token(wait).is_some());
    assert!(wait.interest() & SendWireSet::SPACE.bits() != 0);
    assert!(wait.interest() & SendWireSet::BROKEN.bits() != 0);
    assert_eq!(
        tcp.acquire_operational()
            .expect("tcp payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting {
            local: IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
            remote: remote.as_ip_endpoint(),
        })
    );

    assert_eq!(step_connect(&udp, remote, &guard), StepOutcome::Done(()));
    assert_eq!(
        udp.acquire_operational()
            .expect("udp payload")
            .protocol_snapshot(),
        SocketProtocol::Udp(UdpInner::Connected {
            local: IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
            remote: remote.as_ip_endpoint(),
        })
    );
}

#[test]
fn socket_nonblocking_driver_maps_blocked_connect_to_eagain() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let output = {
        let guard = tx_substrate::epoch::guard();
        match socket_create_facade(2, 1, 6, &guard) {
            StepOutcome::Done(output) => output,
            _ => panic!("socket create facade should succeed"),
        }
    };
    let remote = inet(40_017);
    let cap = output.handle.connect_capability(remote);

    assert_eq!(
        drive_socket_nonblocking(|guard| crate::net::facade::socket_connect_facade(cap, guard)),
        StepOutcome::Err(Errno::EAGAIN)
    );
}

#[test]
fn execution_shutdown_fires_broken_readiness_bits() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");

    assert_eq!(
        step_shutdown(&tcp, SockShutdownCmd::Both, &guard),
        StepOutcome::Done(super::ShutdownOutcome {
            recv_shutdown: true,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 0,
            delegate_kicked: false,
        })
    );
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.shutdown_rd());
    assert!(payload.shutdown_wr());
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0);
    assert!(tcp.readiness.send_wq.peek() & SendWireSet::BROKEN.bits() != 0);
}

#[test]
fn shutdown_fires_same_rawqueue_used_by_wait_token() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let token = socket_send_wait_token(&tcp);
    let mut future = crate::wait_source::wait_on_token(token).expect("send wq registered");
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(Pin::new(&mut future).poll(&mut cx), Poll::Pending));
    assert_eq!(
        step_shutdown(&tcp, SockShutdownCmd::Send, &guard),
        StepOutcome::Done(super::ShutdownOutcome {
            recv_shutdown: false,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 1,
            delegate_kicked: false,
        })
    );
    assert!(matches!(
        Pin::new(&mut future).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    ));
}

#[test]
fn udp_packet_event_sets_recv_readiness() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let local = endpoint(40_130);
    let remote = endpoint(50_130);

    assert_eq!(
        step_bind(
            &udp,
            KernelSockAddr::V4(SockAddrIn::new(local.port, local.addr)),
            &guard,
        ),
        StepOutcome::Done(())
    );
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Udp(
        UdpPacketEvent::with_payload_len(remote, local, 128),
    )]);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        udp.acquire_operational()
            .expect("payload")
            .io_snapshot()
            .recv_len,
        128
    );
    assert!(udp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
}

#[test]
fn smoltcp_demux_rejects_empty_frame_as_malformed() {
    let frame = RxFrame::new(std::vec::Vec::new());

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Malformed
    );
}

#[test]
fn smoltcp_demux_ignores_arp_as_unsupported() {
    let mut bytes = std::vec::Vec::new();
    bytes.extend_from_slice(&[0x02, 0, 0, 0, 0, 2]);
    bytes.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
    bytes.extend_from_slice(&[0x08, 0x06]);
    let frame = RxFrame::new(bytes);

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Unsupported
    );
}

#[test]
fn smoltcp_demux_extracts_ipv4_udp_event() {
    let transport = udp_transport(53_000, 8080, &[1, 2, 3, 4]);
    let frame = RxFrame::new(ethernet_ipv4_frame(17, &transport));

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Udp(UdpPacketEvent::new(
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 1]), 53_000),
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 2]), 8080),
            std::vec![1, 2, 3, 4],
        ))
    );
}

#[test]
fn smoltcp_demux_extracts_ipv4_tcp_event_flags() {
    let transport = tcp_transport(49_000, 443, 0x32, &[9, 8, 7]);
    let frame = RxFrame::new(ethernet_ipv4_frame(6, &transport));

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Tcp(TcpPacketEvent::new(
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 1]), 49_000),
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 2]), 443),
            TcpPacketFlags {
                syn: true,
                ack: true,
                rst: false,
            },
            std::vec![9, 8, 7],
            true
        ))
    );
}

#[test]
fn smoltcp_packet_source_reads_device_frame_and_dispatches() {
    let frame = RxFrame::new(ethernet_ipv4_frame(
        17,
        &udp_transport(40_140, 40_141, &[1]),
    ));
    let ops = std::boxed::Box::leak(std::boxed::Box::new(ScriptedNetDevice::new(std::vec![
        frame,
    ])));
    let device = std::boxed::Box::leak(std::boxed::Box::new(NetDeviceRegistration {
        devt: DevT::new(10, 0),
        name: "test-net0",
        ops,
    }));
    let adapter = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: ops.mac_addr(),
        local_ipv4: Ipv4Address::new([192, 0, 2, 2]),
        mtu: ops.mtu(),
    });
    let source = SmoltcpPacketSource {
        adapter: &adapter,
        device,
    };

    assert!(matches!(
        source.next_packet(),
        Some(PacketDispatch::Udp(UdpPacketEvent { payload, .. })) if payload == std::vec![1]
    ));
    assert_eq!(source.next_packet(), None);
}

#[test]
fn tcp_packet_event_sets_connection_readiness_and_urgent_port() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let local = endpoint(40_143);
    let remote = endpoint(50_143);
    let key = ConnectionKey::new(local, remote);
    SOCKET_TABLE
        .insert_tcp_connection(key, tcp.clone())
        .expect("connection insert");
    let mut urgent_future =
        crate::wait_source::wait_on_token(socket_urgent_wait_token(&tcp)).expect("urgent future");
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut urgent_future).poll(&mut cx),
        Poll::Pending
    ));
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: false,
            ack: true,
            rst: false,
        },
        std::vec![0u8; 64],
        true,
    ),)]);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(outcome.wakes_fired, 1);
    let io = tcp.acquire_operational().expect("payload").io_snapshot();
    assert_eq!(io.recv_len, 64);
    assert_eq!(
        io.send_space,
        SocketOptionSet::default_tcp().socket.send_buf_size
    );
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert_eq!(tcp.readiness.send_wq.peek() & SendWireSet::SPACE.bits(), 0);
    assert!(matches!(
        Pin::new(&mut urgent_future).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    ));
}

#[test]
fn tcp_syn_to_listener_sets_accept_readiness() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    let local = endpoint(40_132);
    let remote = endpoint(50_132);

    assert_eq!(
        step_bind(&listener, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: true,
            ack: false,
            rst: false,
        },
        std::vec::Vec::new(),
        false,
    ),)]);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        listener
            .acquire_operational()
            .expect("payload")
            .io_snapshot()
            .accept_pending,
        1
    );
    assert!(listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits() != 0);
}

#[test]
fn step_recv_consumes_available_bytes_and_clears_when_empty() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.record_recv_payload(endpoint(50_135), endpoint(40_135), std::vec![0u8; 128]));
    tcp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    assert_eq!(
        step_recv(&tcp, 64, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(64)
    );
    assert_eq!(payload.io_snapshot().recv_len, 64);
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);

    assert_eq!(
        step_recv(&tcp, 64, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(64)
    );
    assert_eq!(payload.io_snapshot().recv_len, 0);
    assert_eq!(
        tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0
    );
}

#[test]
fn step_recv_blocks_when_no_data() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    assert_eq!(step_bind(&udp, inet(40_134), &guard), StepOutcome::Done(()));

    let wait = expect_carrier_yield(step_recv(&udp, 32, SendRecvFlags::empty(), &guard));

    assert_eq!(wait.source_id(), udp.wait_carriers.recv);
    assert_eq!(
        wait.interest(),
        RecvWireSet::HAS_DATA.bits() | RecvWireSet::BROKEN.bits()
    );
}

#[test]
fn step_recv_peek_does_not_consume_or_clear() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.record_recv_payload(endpoint(50_136), endpoint(40_136), std::vec![0u8; 16]));
    tcp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    assert_eq!(
        step_recv(&tcp, 8, SendRecvFlags::MSG_PEEK, &guard),
        StepOutcome::Done(8)
    );
    assert_eq!(payload.io_snapshot().recv_len, 16);
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
}

#[test]
fn step_send_consumes_space_and_clears_when_full() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 64;
    let udp = registry::create_socket_for_test_or_bootstrap(SocketKind::Udp, options)
        .expect("udp socket");
    assert_eq!(step_bind(&udp, inet(40_136), &guard), StepOutcome::Done(()));
    let payload = udp.acquire_operational().expect("payload");
    udp.readiness.fire_send(SendWireSet::SPACE);

    assert_eq!(
        step_send(&udp, 32, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(32)
    );
    assert_eq!(payload.io_snapshot().send_space, 32);
    assert!(udp.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);

    assert_eq!(
        step_send(&udp, 32, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(32)
    );
    assert_eq!(payload.io_snapshot().send_space, 0);
    assert_eq!(udp.readiness.send_wq.peek() & SendWireSet::SPACE.bits(), 0);
}

#[test]
fn step_send_blocks_when_no_space() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 1;
    let udp = registry::create_socket_for_test_or_bootstrap(SocketKind::Udp, options)
        .expect("udp socket");
    assert_eq!(step_bind(&udp, inet(40_137), &guard), StepOutcome::Done(()));
    assert_eq!(
        step_send(&udp, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(1)
    );

    let wait = expect_carrier_yield(step_send(&udp, 32, SendRecvFlags::empty(), &guard));

    assert_eq!(wait.source_id(), udp.wait_carriers.send);
    assert_eq!(
        wait.interest(),
        SendWireSet::SPACE.bits() | SendWireSet::BROKEN.bits()
    );
}

#[test]
fn raw_udp_send_queue_preserves_datagram_atomicity() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 8;
    let udp = RawUdpSocket::new(&options);
    let dst = IpEndpoint::new(Ipv4Address::LOOPBACK, 40_138);

    assert_eq!(udp.enqueue_tx_bytes_to(dst, b"12345"), Some((5, false)));
    assert_eq!(udp.send_available(), 3);
    assert_eq!(udp.enqueue_tx_bytes_to(dst, b"abcd"), None);
    assert_eq!(udp.send_available(), 3);

    let drain = udp.pop_tx_datagram().expect("queued datagram");
    assert_eq!(drain.datagram.payload, b"12345");
    assert!(drain.became_available);
    assert_eq!(udp.enqueue_tx_bytes_to(dst, b"abcd"), Some((4, false)));
}

#[test]
fn raw_udp_recv_queue_drops_when_datagram_would_not_fit() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.recv_buf_size = 8;
    let udp = RawUdpSocket::new(&options);
    let src = IpEndpoint::new(Ipv4Address::LOOPBACK, 50_138);
    let dst = IpEndpoint::new(Ipv4Address::LOOPBACK, 40_138);

    assert!(udp.ingest_rx_datagram(src, dst, b"123456".to_vec()));
    assert!(!udp.ingest_rx_datagram(src, dst, b"abcd".to_vec()));
    assert_eq!(udp.recv_available(), 6);

    let mut out = [0u8; 8];
    let drain = udp
        .recv_datagram_bytes(&mut out, false)
        .expect("first datagram");
    assert_eq!(drain.bytes, 6);
    assert_eq!(&out[..drain.bytes], b"123456");
    assert!(drain.became_empty);
    assert!(udp.ingest_rx_datagram(src, dst, b"abcd".to_vec()));
}

#[test]
fn step_accept_returns_child_socket_and_clears_when_empty() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    let local = endpoint(40_138);
    let remote = endpoint(50_138);

    assert_eq!(
        step_bind(&listener, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: true,
            ack: false,
            rst: false,
        },
        std::vec::Vec::new(),
        false,
    ),)]);
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    let payload = listener.acquire_operational().expect("payload");
    assert_eq!(payload.accept_queue_len(), 1);
    assert_eq!(payload.io_snapshot().accept_pending, 1);
    assert!(listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits() != 0);

    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("accept should return queued child"),
    };
    assert_eq!(accepted.local, local);
    assert_eq!(accepted.peer, remote);
    assert_eq!(payload.accept_queue_len(), 0);
    assert_eq!(payload.io_snapshot().accept_pending, 0);
    assert_eq!(
        listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits(),
        0
    );
    assert!(matches!(
        accepted
            .child
            .acquire_operational()
            .expect("child payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local: child_local, remote: child_remote })
            if child_local == local && child_remote == remote
    ));
}

#[test]
fn step_accept_blocks_when_queue_empty() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");

    assert_eq!(
        step_bind(&listener, inet(40_139), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let wait = expect_carrier_yield(step_accept(&listener, &guard));

    assert_eq!(wait.source_id(), listener.wait_carriers.accept);
    assert_eq!(
        wait.interest(),
        AcceptWireSet::HAS_PENDING.bits() | AcceptWireSet::BROKEN.bits()
    );
}

#[test]
fn step_process_network_events_respects_budget() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let packets = (0..(NET_EVENT_BUDGET + 3))
        .map(|_| PacketDispatch::Unsupported)
        .collect();
    let source = ScriptedPacketSource::new(packets);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, NET_EVENT_BUDGET);
    assert_eq!(outcome.sockets_touched, 0);
}

#[test]
fn execution_poll_reports_socket_readiness() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let local = inet(40_015);

    assert_eq!(step_bind(&udp, local, &guard), StepOutcome::Done(()));
    udp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    let mask = match step_poll_ready(&udp, &guard) {
        StepOutcome::Done(mask) => mask,
        other => panic!("unexpected poll outcome: {other:?}"),
    };
    assert!(mask.contains(PollMask::IN));
    assert!(mask.contains(PollMask::OUT));

    assert!(udp.take_payload().is_some());
    let mask = match step_poll_ready(&udp, &guard) {
        StepOutcome::Done(mask) => mask,
        other => panic!("unexpected poll without payload outcome: {other:?}"),
    };
    assert!(mask.contains(PollMask::HUP));
    assert!(mask.contains(PollMask::ERR));
}

#[test]
fn execution_poll_udp_readiness_tracks_io_snapshot() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let local = endpoint(40_016);

    assert_eq!(
        step_bind(&udp, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    let payload = udp.acquire_operational().expect("udp payload");
    assert!(payload.record_recv_payload(endpoint(50_016), local, b"ready".to_vec()));
    assert_eq!(
        udp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0,
        "recording bytes alone must be enough for poll readiness"
    );

    let mask = match step_poll_ready(&udp, &guard) {
        StepOutcome::Done(mask) => mask,
        other => panic!("unexpected poll outcome: {other:?}"),
    };
    assert!(mask.contains(PollMask::IN));
    assert!(mask.contains(PollMask::OUT));
}
