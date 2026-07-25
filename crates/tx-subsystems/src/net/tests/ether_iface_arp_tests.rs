use super::*;
use core::sync::atomic::Ordering;
use std::boxed::Box;
use std::collections::VecDeque;
use std::sync::Mutex;

use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress as SmoltcpEthernetAddress, EthernetFrame,
    EthernetProtocol, EthernetRepr, Ipv4Address as SmoltcpIpv4Address,
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
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));
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
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));
    let drain = net_delegate_step_once(&driver, &guard);
    assert!(drain.device_tx.udp_packets >= 1);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        SocketOptionSet::default_udp().socket.send_buf_size
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
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));
    let first = net_delegate_step_once(&driver, &guard);
    assert!(first.device_tx.udp_resolution_pending >= 1);
    assert!(first.arp_flush.sent >= 1);

    device.ops.push_rx(RxFrame::new(arp_reply_frame(
        local_ip, local_mac, remote_ip, remote_mac,
    )));
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));
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
        SocketOptionSet::default_udp().socket.send_buf_size
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
