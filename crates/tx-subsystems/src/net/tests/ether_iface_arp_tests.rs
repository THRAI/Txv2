use super::*;
use core::sync::atomic::Ordering;
use std::boxed::Box;
use std::collections::VecDeque;
use std::sync::Mutex;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress as SmoltcpEthernetAddress, EthernetFrame,
    EthernetProtocol, EthernetRepr, Icmpv6Packet, Icmpv6Repr, IpProtocol, IpRepr,
    Ipv4Address as SmoltcpIpv4Address, Ipv6Address as SmoltcpIpv6Address, Ipv6Packet, Ipv6Repr,
    NdiscNeighborFlags, NdiscRepr,
};

struct MockEtherDevice {
    rx: Mutex<VecDeque<RxFrame>>,
    tx: Mutex<std::vec::Vec<std::vec::Vec<u8>>>,
    mac: EthernetAddress,
    mtu: u16,
}

struct EtherDelegateDriver<'a> {
    now: smoltcp::time::Instant,
    source: EtherPacketSource<'a>,
    tx_sink: EtherPacketTxSink<'a>,
    iface: &'a EtherIface,
}

impl MockEtherDevice {
    fn new(mac: EthernetAddress) -> Self {
        Self {
            rx: Mutex::new(VecDeque::new()),
            tx: Mutex::new(std::vec::Vec::new()),
            mac,
            mtu: 1500,
        }
    }

    fn push_rx(&self, frame: RxFrame) {
        self.rx.lock().expect("mock ether rx").push_back(frame);
    }

    fn tx_frames(&self) -> std::vec::Vec<std::vec::Vec<u8>> {
        self.tx.lock().expect("mock ether tx").clone()
    }
}

impl NetDeviceOps for MockEtherDevice {
    fn receive(&self) -> Option<RxFrame> {
        self.rx.lock().expect("mock ether rx").pop_front()
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> crate::execution::StepOutcome<()> {
        self.tx.lock().expect("mock ether tx").push(frame.to_vec());
        StepOutcome::Done(())
    }

    fn mac_addr(&self) -> EthernetAddress {
        self.mac
    }

    fn mtu(&self) -> u16 {
        self.mtu
    }
}

impl NetDelegateDriver for EtherDelegateDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        self.now
    }

    fn packet_source(&self) -> &dyn PacketSource {
        &self.source
    }

    fn packet_tx_sink(&self) -> Option<&dyn PacketTxSink> {
        Some(&self.tx_sink)
    }

    fn ether_iface(&self) -> Option<&EtherIface> {
        Some(self.iface)
    }

    fn device_tx_budget(&self) -> DeviceTxBudget {
        DeviceTxBudget {
            tcp_connecting: 0,
            tcp_connected: 0,
            udp_bound: 256,
            raw_icmp: 256,
        }
    }
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    clear_delegate_queue();
    lock
}

#[test]
fn ether_iface_route_decision_uses_direct_gateway_and_unreachable_paths() {
    let local = Ipv4Address::new([10, 0, 0, 1]);
    let netmask = Ipv4Address::new([255, 255, 255, 0]);
    let gateway = Ipv4Address::new([10, 0, 0, 254]);
    let common = IfaceCommon::with_gateway(local, netmask, Some(gateway), 1500);

    assert_eq!(
        decide_ipv4_route(common, Ipv4Address::new([10, 0, 0, 2])),
        Ipv4RouteDecision::Direct {
            next_hop: Ipv4Address::new([10, 0, 0, 2])
        }
    );
    assert_eq!(
        decide_ipv4_route(common, Ipv4Address::new([192, 0, 2, 10])),
        Ipv4RouteDecision::Gateway { next_hop: gateway }
    );
    assert_eq!(
        decide_ipv4_route(common, Ipv4Address::BROADCAST),
        Ipv4RouteDecision::Broadcast {
            next_hop: Ipv4Address::BROADCAST
        }
    );
    assert_eq!(
        decide_ipv4_route(
            IfaceCommon::new(local, netmask, 1500),
            Ipv4Address::new([192, 0, 2, 10])
        ),
        Ipv4RouteDecision::Unreachable {
            dst: Ipv4Address::new([192, 0, 2, 10])
        }
    );
}

#[test]
fn ether_iface_arp_miss_sends_request_and_keeps_udp_datagram() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let device = leak_ether_device(local_mac, 41);
    let iface = leak_ether_iface(device.registration, local_ip, local_mac);
    let client = connected_udp_client_on(local_ip, 50_241, remote_ip, 40_241, b"hello");
    let send_space_after_send = client
        .acquire_operational()
        .expect("client payload")
        .io_snapshot()
        .send_space;

    let driver = ether_driver(iface);
    let guard = tx_substrate::epoch::guard();
    crate::net::delegate::net_delegate_kick_poll();
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert_eq!(outcome.device_tx.udp_attempted, 1);
    assert_eq!(outcome.device_tx.udp_resolution_pending, 1);
    assert_eq!(outcome.device_tx.udp_packets, 0);
    assert_eq!(outcome.arp_flush.sent, 1);
    assert_eq!(iface.pending_arp_len(), 1);
    assert_eq!(
        iface
            .pending_arp_entry(remote_ip)
            .expect("pending arp")
            .attempts,
        1
    );
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        send_space_after_send
    );

    let tx = device.ops.tx_frames();
    assert_eq!(tx.len(), 1);
    assert_arp_request(&tx[0], local_ip, local_mac, remote_ip);

    iface.install_arp_for_test_or_bootstrap(
        remote_ip,
        remote_mac,
        smoltcp::time::Instant::from_millis(1),
    );
    crate::net::delegate::net_delegate_kick_poll();
    let drain = net_delegate_step_once(&driver, &guard);
    assert!(drain.device_tx.udp_packets >= 1);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        client
            .acquire_operational()
            .expect("client payload")
            .raw_udp_socket()
            .expect("UDP socket")
            .send_capacity()
    );
    assert_eq!(iface.pending_arp_len(), 0);
}

#[test]
fn ether_iface_arp_reply_learns_cache_and_udp_retry_uses_peer_mac() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let device = leak_ether_device(local_mac, 42);
    let iface = leak_ether_iface(device.registration, local_ip, local_mac);
    let client = connected_udp_client_on(local_ip, 50_242, remote_ip, 40_242, b"hello");

    let driver = ether_driver(iface);
    let guard = tx_substrate::epoch::guard();
    crate::net::delegate::net_delegate_kick_poll();
    let first = net_delegate_step_once(&driver, &guard);
    assert!(first.device_tx.udp_resolution_pending >= 1);
    assert!(first.arp_flush.sent >= 1);

    device.ops.push_rx(RxFrame::new(arp_reply_frame(
        local_ip, local_mac, remote_ip, remote_mac,
    )));
    crate::net::delegate::net_delegate_kick_poll();
    let retry = net_delegate_step_once(&driver, &guard);

    assert!(retry.poll_seen);
    assert_eq!(retry.packets_seen, 1);
    assert_eq!(retry.device_tx.udp_attempted, 1);
    assert_eq!(retry.device_tx.udp_packets, 1);
    assert_eq!(retry.device_tx.udp_resolution_pending, 0);
    assert_eq!(
        iface
            .arp_entry(remote_ip, smoltcp::time::Instant::ZERO)
            .expect("learned arp")
            .mac,
        remote_mac
    );
    assert_eq!(iface.pending_arp_len(), 0);
    assert_eq!(iface.arp_snapshot(smoltcp::time::Instant::ZERO).len(), 1);
    assert_eq!(
        iface.arp_snapshot(smoltcp::time::Instant::ZERO)[0].state,
        ArpSnapshotState::Resolved
    );
    assert_eq!(iface.arp_stats.resolved.load(Ordering::Relaxed), 1);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        client
            .acquire_operational()
            .expect("client payload")
            .raw_udp_socket()
            .expect("UDP socket")
            .send_capacity()
    );

    let tx = device.ops.tx_frames();
    assert_udp_ethernet_frame(
        tx.last().expect("udp tx frame"),
        local_mac,
        remote_mac,
        IpEndpoint::new(local_ip, 50_242),
        IpEndpoint::new(remote_ip, 40_242),
        b"hello",
    );
}

#[test]
fn ether_iface_replies_to_icmp_echo_request_for_local_ip() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let device = leak_ether_device(local_mac, 48);
    let iface = leak_ether_iface(device.registration, local_ip, local_mac);
    iface.install_arp_for_test_or_bootstrap(
        remote_ip,
        remote_mac,
        smoltcp::time::Instant::from_secs(60),
    );

    let request = crate::net::protocol::Icmpv4EchoPacket {
        src: remote_ip,
        dst: local_ip,
        ident: 0x5050,
        seq_no: 1,
        payload: b"ether".to_vec(),
    };
    let frame = ipv4_ethernet_frame(
        local_mac,
        remote_mac,
        crate::net::protocol::build_icmpv4_echo_request(&request).as_bytes(),
    );

    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        iface.process_frame_at(
            RxFrame::new(frame),
            smoltcp::time::Instant::ZERO,
            Some(&guard),
        ),
        PacketDispatch::Icmp(crate::net::protocol::Icmpv4Event::EchoRequest(_))
    ));

    let tx = device.ops.tx_frames();
    assert_eq!(tx.len(), 1);
    assert!(matches!(
        demux_rx_frame_with_smoltcp(&RxFrame::new(tx[0].clone())),
        PacketDispatch::Icmp(crate::net::protocol::Icmpv4Event::EchoReply(reply))
            if reply == request.reply_packet()
    ));
}

#[test]
fn ether_iface_fragments_and_reassembles_large_icmp_echo() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let local_device = leak_ether_device(local_mac, 49);
    let remote_device = leak_ether_device(remote_mac, 50);
    let local_iface =
        leak_ether_iface_without_attach(local_device.registration, local_ip, local_mac);
    let remote_iface =
        leak_ether_iface_without_attach(remote_device.registration, remote_ip, remote_mac);
    let now = smoltcp::time::Instant::ZERO;
    let arp_expires = smoltcp::time::Instant::from_secs(60);
    let guard = tx_substrate::epoch::guard();

    local_iface.install_arp_for_test_or_bootstrap(local_ip, local_mac, arp_expires);
    local_iface.install_arp_for_test_or_bootstrap(remote_ip, remote_mac, arp_expires);
    remote_iface.install_arp_for_test_or_bootstrap(local_ip, local_mac, arp_expires);

    let echo = crate::net::protocol::Icmpv4EchoPacket {
        src: local_ip,
        dst: remote_ip,
        ident: 0x5151,
        seq_no: 7,
        payload: std::vec![0x5a; 2048],
    };
    let request = crate::net::protocol::build_icmpv4_echo_request(&echo);

    let tx_result = local_iface.dispatch_ip_at(request.as_bytes(), now, &guard);
    assert!(
        matches!(tx_result, PacketTxResult::Accepted { .. }),
        "unexpected dispatch result: {tx_result:?}"
    );
    let outbound = local_device.ops.tx_frames();
    assert_eq!(outbound.len(), 2);
    assert_ipv4_fragment(&outbound[0], true, 0);
    assert_ipv4_fragment(&outbound[1], false, 1480);

    assert_eq!(
        remote_iface.process_frame_at(RxFrame::new(outbound[0].clone()), now, Some(&guard)),
        PacketDispatch::Unsupported
    );
    assert_eq!(
        remote_iface.process_frame_at(RxFrame::new(outbound[1].clone()), now, Some(&guard)),
        PacketDispatch::Icmp(Icmpv4Event::EchoRequest(echo.clone()))
    );

    let replies = remote_device.ops.tx_frames();
    assert_eq!(replies.len(), 2);
    assert_ipv4_fragment(&replies[0], true, 0);
    assert_ipv4_fragment(&replies[1], false, 1480);
    assert_eq!(
        local_iface.process_frame_at(RxFrame::new(replies[0].clone()), now, Some(&guard)),
        PacketDispatch::Unsupported
    );
    assert_eq!(
        local_iface.process_frame_at(RxFrame::new(replies[1].clone()), now, Some(&guard)),
        PacketDispatch::Icmp(Icmpv4Event::EchoReply(echo.reply_packet()))
    );
}

#[test]
fn ether_iface_arp_request_is_rate_limited_by_pending_deadline() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let device = leak_ether_device(local_mac, 43);
    let iface = leak_ether_iface(device.registration, local_ip, local_mac);
    let guard = tx_substrate::epoch::guard();
    let packet = udp_ipv4_packet(local_ip, 50_243, remote_ip, 40_243, b"hello");

    assert!(matches!(
        iface.dispatch_ip_at(&packet, smoltcp::time::Instant::ZERO, &guard),
        PacketTxResult::PendingResolution { next_hop } if next_hop == remote_ip
    ));
    let first = iface.flush_pending_arp_at(smoltcp::time::Instant::ZERO, 8, &guard);
    assert_eq!(first.sent, 1);
    assert_eq!(device.ops.tx_frames().len(), 1);
    assert_eq!(
        iface
            .pending_arp_entry(remote_ip)
            .expect("pending arp")
            .attempts,
        1
    );

    assert!(matches!(
        iface.dispatch_ip_at(
            &packet,
            smoltcp::time::Instant::from_millis(100),
            &guard
        ),
        PacketTxResult::PendingResolution { next_hop } if next_hop == remote_ip
    ));
    let early = iface.flush_pending_arp_at(smoltcp::time::Instant::from_millis(100), 8, &guard);
    assert_eq!(early.sent, 0);
    assert_eq!(device.ops.tx_frames().len(), 1);
    assert_eq!(
        iface
            .pending_arp_entry(remote_ip)
            .expect("pending arp")
            .attempts,
        1
    );

    assert!(matches!(
        iface.dispatch_ip_at(
            &packet,
            smoltcp::time::Instant::from_millis(1_000),
            &guard
        ),
        PacketTxResult::PendingResolution { next_hop } if next_hop == remote_ip
    ));
    let retry = iface.flush_pending_arp_at(smoltcp::time::Instant::from_millis(1_000), 8, &guard);
    assert_eq!(retry.sent, 1);
    assert_eq!(device.ops.tx_frames().len(), 2);
    assert_eq!(
        iface
            .pending_arp_entry(remote_ip)
            .expect("pending arp")
            .attempts,
        2
    );
    assert_eq!(iface.arp_stats.requests_tx.load(Ordering::Relaxed), 2);
}

#[test]
fn ether_iface_arp_retry_limit_marks_pending_entry_failed() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let device = leak_ether_device(local_mac, 44);
    let iface = leak_ether_iface(device.registration, local_ip, local_mac);
    let guard = tx_substrate::epoch::guard();
    let packet = udp_ipv4_packet(local_ip, 50_244, remote_ip, 40_244, b"hello");

    assert!(matches!(
        iface.dispatch_ip_at(&packet, smoltcp::time::Instant::ZERO, &guard),
        PacketTxResult::PendingResolution { next_hop } if next_hop == remote_ip
    ));
    for millis in [0, 1_000, 2_000] {
        let outcome =
            iface.flush_pending_arp_at(smoltcp::time::Instant::from_millis(millis), 8, &guard);
        assert_eq!(outcome.sent, 1);
    }

    let failed = iface.flush_pending_arp_at(smoltcp::time::Instant::from_millis(3_000), 8, &guard);
    assert_eq!(failed.failed, 1);
    assert_eq!(
        device.ops.tx_frames().len(),
        usize::from(ARP_REQUEST_RETRY_LIMIT)
    );
    assert_eq!(
        iface
            .pending_arp_entry(remote_ip)
            .expect("failed pending arp")
            .last_error,
        Some(Errno::EADDRNOTAVAIL)
    );
    assert_eq!(
        iface.arp_stats.retry_limit_exceeded.load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        iface.arp_snapshot(smoltcp::time::Instant::from_millis(3_000))[0].state,
        ArpSnapshotState::Failed
    );

    assert_eq!(
        iface.dispatch_ip_at(&packet, smoltcp::time::Instant::from_millis(4_000), &guard),
        PacketTxResult::Failed {
            errno: Errno::EADDRNOTAVAIL
        }
    );
}

#[test]
fn ether_iface_snapshots_project_arp_and_netdev_state() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let device = leak_ether_device(local_mac, 45);
    let iface = leak_ether_iface(device.registration, local_ip, local_mac);
    let guard = tx_substrate::epoch::guard();
    let packet = udp_ipv4_packet(local_ip, 50_245, remote_ip, 40_245, b"hello");

    assert!(matches!(
        iface.dispatch_ip_at(&packet, smoltcp::time::Instant::ZERO, &guard),
        PacketTxResult::PendingResolution { next_hop } if next_hop == remote_ip
    ));
    let _ = iface.flush_pending_arp_at(smoltcp::time::Instant::ZERO, 8, &guard);

    let arp = iface.arp_snapshot(smoltcp::time::Instant::ZERO);
    assert_eq!(arp.len(), 1);
    assert_eq!(arp[0].iface_name, "eth-test");
    assert_eq!(arp[0].ip, remote_ip);
    assert_eq!(arp[0].mac, None);
    assert_eq!(arp[0].state, ArpSnapshotState::Pending);
    assert_eq!(arp[0].attempts, 1);

    let stats = iface.net_stats_snapshot();
    assert_eq!(stats.iface_name, "eth-test");
    assert_eq!(stats.tx_packets, 1);
    assert!(stats.tx_bytes > 0);
}

fn udp_ipv4_packet(
    local_ip: Ipv4Address,
    local_port: u16,
    remote_ip: Ipv4Address,
    remote_port: u16,
    payload: &[u8],
) -> std::vec::Vec<u8> {
    UdpTxDatagram {
        dst: IpEndpoint::new(remote_ip, remote_port),
        payload: payload.to_vec(),
    }
    .emit_ipv4_packet(IpEndpoint::new(local_ip, local_port))
    .expect("udp ipv4 packet")
    .as_bytes()
    .to_vec()
}

fn ether_driver(iface: &EtherIface) -> EtherDelegateDriver<'_> {
    ether_driver_at(iface, smoltcp::time::Instant::ZERO)
}

fn ether_driver_at(iface: &EtherIface, now: smoltcp::time::Instant) -> EtherDelegateDriver<'_> {
    EtherDelegateDriver {
        now,
        source: EtherPacketSource { iface },
        tx_sink: EtherPacketTxSink { iface },
        iface,
    }
}

struct LeakedEtherDevice {
    ops: &'static MockEtherDevice,
    registration: &'static NetDeviceRegistration,
}

fn leak_ether_device(mac: EthernetAddress, minor: u32) -> LeakedEtherDevice {
    let ops = Box::leak(Box::new(MockEtherDevice::new(mac)));
    let registration = Box::leak(Box::new(NetDeviceRegistration {
        devt: DevT::new(91, minor),
        name: "ether-test",
        ops,
    }));
    LeakedEtherDevice { ops, registration }
}

fn leak_ether_iface(
    registration: &'static NetDeviceRegistration,
    local_ip: Ipv4Address,
    local_mac: EthernetAddress,
) -> &'static EtherIface {
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(local_ip))
        .expect("attach ether iface to initial net namespace");
    Box::leak(Box::new(EtherIface::new(
        registration,
        IfaceCommon::new(local_ip, Ipv4Address::new([255, 255, 255, 0]), 1500),
        local_mac,
        "eth-test",
    )))
}

fn leak_ether_iface_without_attach(
    registration: &'static NetDeviceRegistration,
    local_ip: Ipv4Address,
    local_mac: EthernetAddress,
) -> &'static EtherIface {
    Box::leak(Box::new(EtherIface::new(
        registration,
        IfaceCommon::new(local_ip, Ipv4Address::new([255, 255, 255, 0]), 1500),
        local_mac,
        "eth-test",
    )))
}

fn connected_udp_client_on(
    local_ip: Ipv4Address,
    local_port: u16,
    remote_ip: Ipv4Address,
    remote_port: u16,
    bytes: &[u8],
) -> tx_substrate::zone::Cap<SocketIdentity> {
    let guard = tx_substrate::epoch::guard();
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client");
    assert_eq!(
        step_bind(&client, sockaddr(local_ip, local_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, sockaddr(remote_ip, remote_port), &guard),
        StepOutcome::Done(())
    );
    clear_delegate_queue();
    assert_eq!(
        step_send_kernel_bytes(&client, bytes, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(bytes.len())
    );
    client
}

fn sockaddr(addr: Ipv4Address, port: u16) -> KernelSockAddr {
    KernelSockAddr::V4(SockAddrIn::new(port, addr))
}

fn assert_arp_request(
    frame: &[u8],
    local_ip: Ipv4Address,
    local_mac: EthernetAddress,
    target_ip: Ipv4Address,
) {
    let ethernet = EthernetFrame::new_checked(frame).expect("ethernet frame");
    assert_eq!(ethernet.ethertype(), EthernetProtocol::Arp);
    assert_eq!(ethernet.src_addr(), to_smoltcp_ether(local_mac));
    assert_eq!(ethernet.dst_addr(), SmoltcpEthernetAddress::BROADCAST);

    let arp = ArpPacket::new_checked(ethernet.payload()).expect("arp packet");
    match ArpRepr::parse(&arp).expect("arp repr") {
        ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        } => {
            assert_eq!(operation, ArpOperation::Request);
            assert_eq!(source_hardware_addr, to_smoltcp_ether(local_mac));
            assert_eq!(source_protocol_addr, to_smoltcp_ipv4(local_ip));
            assert_eq!(target_hardware_addr, SmoltcpEthernetAddress::BROADCAST);
            assert_eq!(target_protocol_addr, to_smoltcp_ipv4(target_ip));
        }
        _ => panic!("unexpected arp repr"),
    }
}

fn assert_udp_ethernet_frame(
    frame: &[u8],
    local_mac: EthernetAddress,
    remote_mac: EthernetAddress,
    src: IpEndpoint,
    dst: IpEndpoint,
    payload: &[u8],
) {
    let ethernet = EthernetFrame::new_checked(frame).expect("ethernet frame");
    assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv4);
    assert_eq!(ethernet.src_addr(), to_smoltcp_ether(local_mac));
    assert_eq!(ethernet.dst_addr(), to_smoltcp_ether(remote_mac));
    assert!(matches!(
        demux_rx_frame_with_smoltcp(&RxFrame::new(frame.to_vec())),
        PacketDispatch::Udp(event)
            if event.src == src && event.dst == dst && event.payload == payload
    ));
}

fn arp_reply_frame(
    local_ip: Ipv4Address,
    local_mac: EthernetAddress,
    remote_ip: Ipv4Address,
    remote_mac: EthernetAddress,
) -> std::vec::Vec<u8> {
    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Reply,
        source_hardware_addr: to_smoltcp_ether(remote_mac),
        source_protocol_addr: to_smoltcp_ipv4(remote_ip),
        target_hardware_addr: to_smoltcp_ether(local_mac),
        target_protocol_addr: to_smoltcp_ipv4(local_ip),
    };
    let ether = EthernetRepr {
        src_addr: to_smoltcp_ether(remote_mac),
        dst_addr: to_smoltcp_ether(local_mac),
        ethertype: EthernetProtocol::Arp,
    };
    let mut frame = std::vec![0u8; ether.buffer_len() + repr.buffer_len()];
    let mut ethernet = EthernetFrame::new_unchecked(frame.as_mut_slice());
    ether.emit(&mut ethernet);
    repr.emit(&mut ArpPacket::new_unchecked(ethernet.payload_mut()));
    frame
}

fn ipv4_ethernet_frame(
    local_mac: EthernetAddress,
    remote_mac: EthernetAddress,
    ipv4_packet: &[u8],
) -> std::vec::Vec<u8> {
    let ether = EthernetRepr {
        src_addr: to_smoltcp_ether(remote_mac),
        dst_addr: to_smoltcp_ether(local_mac),
        ethertype: EthernetProtocol::Ipv4,
    };
    let mut frame = std::vec![0u8; ether.buffer_len() + ipv4_packet.len()];
    let mut ethernet = EthernetFrame::new_unchecked(frame.as_mut_slice());
    ether.emit(&mut ethernet);
    ethernet.payload_mut().copy_from_slice(ipv4_packet);
    frame
}

fn assert_ipv4_fragment(frame: &[u8], more_fragments: bool, offset: usize) {
    let ethernet = EthernetFrame::new_checked(frame).expect("ethernet frame");
    assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv4);
    let ipv4 = ethernet.payload();
    let flags_fragment = u16::from_be_bytes([ipv4[6], ipv4[7]]);
    assert_eq!(flags_fragment & 0x2000 != 0, more_fragments);
    assert_eq!(usize::from(flags_fragment & 0x1fff) * 8, offset);
    assert!(usize::from(u16::from_be_bytes([ipv4[2], ipv4[3]])) <= 1500);
}

fn to_smoltcp_ether(addr: EthernetAddress) -> SmoltcpEthernetAddress {
    SmoltcpEthernetAddress(addr.octets())
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> SmoltcpIpv4Address {
    let [a, b, c, d] = addr.octets();
    SmoltcpIpv4Address::new(a, b, c, d)
}

fn clear_delegate_queue() {
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
}

// ===== IPv6 V2: dynamic NDP tests (mirror of the ARP neighbour tests) =====

fn to_smoltcp_ipv6(addr: Ipv6Address) -> SmoltcpIpv6Address {
    SmoltcpIpv6Address::from(addr.octets())
}

fn solicited_node_v6(target: Ipv6Address) -> Ipv6Address {
    let t = target.octets();
    Ipv6Address::new([
        0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xff, t[13], t[14], t[15],
    ])
}

fn solicited_node_mac(target: Ipv6Address) -> EthernetAddress {
    let t = target.octets();
    EthernetAddress::new([0x33, 0x33, 0xff, t[13], t[14], t[15]])
}

fn leak_ether_iface_v6(
    registration: &'static NetDeviceRegistration,
    local_ip: Ipv4Address,
    local_v6: Ipv6Address,
    local_mac: EthernetAddress,
) -> &'static EtherIface {
    Box::leak(Box::new(EtherIface::new(
        registration,
        IfaceCommon::new(local_ip, Ipv4Address::new([255, 255, 255, 0]), 1500)
            .with_ipv6(Some(local_v6), Some(64)),
        local_mac,
        "eth6-test",
    )))
}

/// Emit an ICMPv6 NDP message inside an IPv6/Ethernet frame (independent of the
/// production builder, so the RX path is verified against a separate encoder).
fn ndisc_eth_frame(
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src_ip: Ipv6Address,
    dst_ip: Ipv6Address,
    icmp_repr: Icmpv6Repr,
) -> std::vec::Vec<u8> {
    let checksum = ChecksumCapabilities::default();
    let mut icmp_bytes = std::vec![0u8; icmp_repr.buffer_len()];
    let mut icmp_packet = Icmpv6Packet::new_unchecked(&mut icmp_bytes);
    icmp_repr.emit(
        &to_smoltcp_ipv6(src_ip),
        &to_smoltcp_ipv6(dst_ip),
        &mut icmp_packet,
        &checksum,
    );
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr: to_smoltcp_ipv6(src_ip),
        dst_addr: to_smoltcp_ipv6(dst_ip),
        next_header: IpProtocol::Icmpv6,
        payload_len: icmp_bytes.len(),
        hop_limit: 255,
    });
    let ip_header_len = ip_repr.header_len();
    let mut ip_bytes = std::vec![0u8; ip_header_len + icmp_bytes.len()];
    ip_repr.emit(&mut ip_bytes[..ip_header_len], &checksum);
    ip_bytes[ip_header_len..].copy_from_slice(&icmp_bytes);
    let ether = EthernetRepr {
        src_addr: to_smoltcp_ether(src_mac),
        dst_addr: to_smoltcp_ether(dst_mac),
        ethertype: EthernetProtocol::Ipv6,
    };
    let mut frame = std::vec![0u8; ether.buffer_len() + ip_bytes.len()];
    let mut ethernet = EthernetFrame::new_unchecked(frame.as_mut_slice());
    ether.emit(&mut ethernet);
    ethernet.payload_mut().copy_from_slice(&ip_bytes);
    frame
}

fn ipv6_echo_packet(src: Ipv6Address, dst: Ipv6Address) -> std::vec::Vec<u8> {
    let icmp_repr = Icmpv6Repr::EchoRequest {
        ident: 0x11,
        seq_no: 1,
        data: b"nd",
    };
    let checksum = ChecksumCapabilities::default();
    let mut icmp_bytes = std::vec![0u8; icmp_repr.buffer_len()];
    let mut icmp_packet = Icmpv6Packet::new_unchecked(&mut icmp_bytes);
    icmp_repr.emit(
        &to_smoltcp_ipv6(src),
        &to_smoltcp_ipv6(dst),
        &mut icmp_packet,
        &checksum,
    );
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr: to_smoltcp_ipv6(src),
        dst_addr: to_smoltcp_ipv6(dst),
        next_header: IpProtocol::Icmpv6,
        payload_len: icmp_bytes.len(),
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let mut ip_bytes = std::vec![0u8; ip_header_len + icmp_bytes.len()];
    ip_repr.emit(&mut ip_bytes[..ip_header_len], &checksum);
    ip_bytes[ip_header_len..].copy_from_slice(&icmp_bytes);
    ip_bytes
}

fn ndisc_advert_target(frame: &[u8]) -> SmoltcpIpv6Address {
    let ethernet = EthernetFrame::new_checked(frame).expect("ethernet frame");
    assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv6);
    let ipv6 = Ipv6Packet::new_checked(ethernet.payload()).expect("ipv6 packet");
    let icmp = Icmpv6Packet::new_checked(ipv6.payload()).expect("icmpv6 packet");
    match Icmpv6Repr::parse(
        &ipv6.src_addr(),
        &ipv6.dst_addr(),
        &icmp,
        &ChecksumCapabilities::default(),
    )
    .expect("icmpv6 repr")
    {
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert { target_addr, .. }) => target_addr,
        _ => panic!("expected neighbor advertisement"),
    }
}

fn ndisc_solicit_target(frame: &[u8]) -> SmoltcpIpv6Address {
    let ethernet = EthernetFrame::new_checked(frame).expect("ethernet frame");
    assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv6);
    let ipv6 = Ipv6Packet::new_checked(ethernet.payload()).expect("ipv6 packet");
    let icmp = Icmpv6Packet::new_checked(ipv6.payload()).expect("icmpv6 packet");
    match Icmpv6Repr::parse(
        &ipv6.src_addr(),
        &ipv6.dst_addr(),
        &icmp,
        &ChecksumCapabilities::default(),
    )
    .expect("icmpv6 repr")
    {
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit { target_addr, .. }) => target_addr,
        _ => panic!("expected neighbor solicitation"),
    }
}

#[test]
fn ether_iface_ndisc_advert_learns_neighbor() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let local_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let remote_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let device = leak_ether_device(local_mac, 60);
    let iface = leak_ether_iface_v6(device.registration, local_ip, local_v6, local_mac);

    let na = ndisc_eth_frame(
        remote_mac,
        local_mac,
        remote_v6,
        local_v6,
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            flags: NdiscNeighborFlags::SOLICITED | NdiscNeighborFlags::OVERRIDE,
            target_addr: to_smoltcp_ipv6(remote_v6),
            lladdr: Some(to_smoltcp_ether(remote_mac).into()),
        }),
    );
    let guard = tx_substrate::epoch::guard();
    iface.process_frame_at(RxFrame::new(na), smoltcp::time::Instant::ZERO, Some(&guard));

    assert_eq!(
        iface
            .ndisc_entry(remote_v6, smoltcp::time::Instant::ZERO)
            .expect("learned ndisc")
            .mac,
        remote_mac
    );
}

#[test]
fn ether_iface_ndisc_solicit_for_local_learns_and_replies() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let local_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let remote_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let device = leak_ether_device(local_mac, 61);
    let iface = leak_ether_iface_v6(device.registration, local_ip, local_v6, local_mac);

    let ns = ndisc_eth_frame(
        remote_mac,
        solicited_node_mac(local_v6),
        remote_v6,
        solicited_node_v6(local_v6),
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
            target_addr: to_smoltcp_ipv6(local_v6),
            lladdr: Some(to_smoltcp_ether(remote_mac).into()),
        }),
    );
    let guard = tx_substrate::epoch::guard();
    iface.process_frame_at(RxFrame::new(ns), smoltcp::time::Instant::ZERO, Some(&guard));

    // Learned the solicitor's mapping.
    assert_eq!(
        iface
            .ndisc_entry(remote_v6, smoltcp::time::Instant::ZERO)
            .expect("learned solicitor")
            .mac,
        remote_mac
    );

    // Answered with a solicited NA targeting our address, unicast to the peer.
    let tx = device.ops.tx_frames();
    assert_eq!(tx.len(), 1);
    let ethernet = EthernetFrame::new_checked(&tx[0]).expect("ethernet frame");
    assert_eq!(ethernet.src_addr(), to_smoltcp_ether(local_mac));
    assert_eq!(ethernet.dst_addr(), to_smoltcp_ether(remote_mac));
    assert_eq!(ndisc_advert_target(&tx[0]), to_smoltcp_ipv6(local_v6));
}

#[test]
fn ether_iface_ndisc_miss_sends_solicit_and_advert_resolves() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let local_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let remote_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 2]);
    let device = leak_ether_device(local_mac, 62);
    let iface = leak_ether_iface_v6(device.registration, local_ip, local_v6, local_mac);
    let guard = tx_substrate::epoch::guard();

    // On-link v6 send to an unresolved neighbour → pending + NS queued.
    let pending = iface.dispatch_ip_at(
        &ipv6_echo_packet(local_v6, remote_v6),
        smoltcp::time::Instant::ZERO,
        &guard,
    );
    assert!(matches!(pending, PacketTxResult::PendingResolution { .. }));
    assert_eq!(iface.pending_ndisc_len(), 1);

    // Flush drives one neighbour solicitation to the solicited-node group.
    let out = iface.flush_pending_ndisc_at(smoltcp::time::Instant::ZERO, 8, &guard);
    assert_eq!(out.sent, 1);
    let tx = device.ops.tx_frames();
    assert_eq!(tx.len(), 1);
    let ethernet = EthernetFrame::new_checked(&tx[0]).expect("ethernet frame");
    assert_eq!(
        ethernet.dst_addr(),
        to_smoltcp_ether(solicited_node_mac(remote_v6))
    );
    assert_eq!(ndisc_solicit_target(&tx[0]), to_smoltcp_ipv6(remote_v6));

    // The neighbour's advertisement resolves the pending entry.
    let na = ndisc_eth_frame(
        remote_mac,
        local_mac,
        remote_v6,
        local_v6,
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            flags: NdiscNeighborFlags::SOLICITED | NdiscNeighborFlags::OVERRIDE,
            target_addr: to_smoltcp_ipv6(remote_v6),
            lladdr: Some(to_smoltcp_ether(remote_mac).into()),
        }),
    );
    iface.process_frame_at(RxFrame::new(na), smoltcp::time::Instant::ZERO, Some(&guard));
    assert_eq!(
        iface
            .ndisc_entry(remote_v6, smoltcp::time::Instant::ZERO)
            .expect("resolved neighbour")
            .mac,
        remote_mac
    );
    assert_eq!(iface.pending_ndisc_len(), 0);
}

#[test]
fn ether_iface_ndisc_solicit_retry_limit_marks_failed() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let local_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let remote_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let device = leak_ether_device(local_mac, 63);
    let iface = leak_ether_iface_v6(device.registration, local_ip, local_v6, local_mac);
    let guard = tx_substrate::epoch::guard();

    let _ = iface.dispatch_ip_at(
        &ipv6_echo_packet(local_v6, remote_v6),
        smoltcp::time::Instant::ZERO,
        &guard,
    );

    // One solicitation per retry-delay window, up to the retry limit.
    for ms in [0i64, 1_000, 2_000] {
        let out = iface.flush_pending_ndisc_at(smoltcp::time::Instant::from_millis(ms), 8, &guard);
        assert_eq!(out.sent, 1);
    }
    // Past the limit the entry is marked failed rather than re-probed.
    let failed =
        iface.flush_pending_ndisc_at(smoltcp::time::Instant::from_millis(3_000), 8, &guard);
    assert_eq!(failed.sent, 0);
    assert_eq!(failed.failed, 1);
    assert!(iface
        .pending_ndisc_entry(remote_v6)
        .expect("pending entry")
        .last_error
        .is_some());
}

// ===== IPv6 V3b: off-link routing via the v6 gateway =====

fn leak_ether_iface_v6_gw(
    registration: &'static NetDeviceRegistration,
    local_ip: Ipv4Address,
    local_v6: Ipv6Address,
    gateway: Ipv6Address,
    local_mac: EthernetAddress,
) -> &'static EtherIface {
    Box::leak(Box::new(EtherIface::new(
        registration,
        IfaceCommon::new(local_ip, Ipv4Address::new([255, 255, 255, 0]), 1500)
            .with_ipv6(Some(local_v6), Some(64))
            .with_ipv6_gateway(Some(gateway)),
        local_mac,
        "eth6gw-test",
    )))
}

#[test]
fn decide_ipv6_route_uses_direct_gateway_and_unreachable_paths() {
    let local_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let gateway = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff]);
    let on_link = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    let off_link = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mcast = Ipv6Address::new([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

    let common = IfaceCommon::new(
        Ipv4Address::new([10, 0, 0, 1]),
        Ipv4Address::new([255, 255, 255, 0]),
        1500,
    )
    .with_ipv6(Some(local_v6), Some(64))
    .with_ipv6_gateway(Some(gateway));

    assert_eq!(
        decide_ipv6_route(common, mcast),
        Ipv6RouteDecision::Multicast { next_hop: mcast }
    );
    assert_eq!(
        decide_ipv6_route(common, on_link),
        Ipv6RouteDecision::Direct { next_hop: on_link }
    );
    // Off-link with a configured gateway → route via the gateway next-hop.
    assert_eq!(
        decide_ipv6_route(common, off_link),
        Ipv6RouteDecision::Gateway { next_hop: gateway }
    );
    // Off-link without a gateway → unreachable.
    assert_eq!(
        decide_ipv6_route(common.with_ipv6_gateway(None), off_link),
        Ipv6RouteDecision::Unreachable { dst: off_link }
    );
}

#[test]
fn ether_iface_v6_offlink_solicits_gateway_not_dst() {
    let _lock = setup();

    let local_ip = Ipv4Address::new([10, 0, 0, 1]);
    let local_v6 = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let gateway = Ipv6Address::new([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff]);
    let off_link = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0, 0, 1]);
    let device = leak_ether_device(local_mac, 64);
    let iface = leak_ether_iface_v6_gw(device.registration, local_ip, local_v6, gateway, local_mac);
    let guard = tx_substrate::epoch::guard();

    // Off-link v6 send → decide_ipv6_route returns Gateway → resolve_ndisc(gateway)
    // → the neighbour solicitation targets the GATEWAY, not the off-link dst.
    let pending = iface.dispatch_ip_at(
        &ipv6_echo_packet(local_v6, off_link),
        smoltcp::time::Instant::ZERO,
        &guard,
    );
    assert!(matches!(pending, PacketTxResult::PendingResolution { .. }));
    assert!(
        iface.pending_ndisc_entry(gateway).is_some(),
        "off-link route must solicit the gateway"
    );
    assert!(
        iface.pending_ndisc_entry(off_link).is_none(),
        "off-link dst must not be solicited directly"
    );
}
