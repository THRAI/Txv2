use super::*;
use std::boxed::Box;

const BRIDGE_MAC: EthernetAddress = EthernetAddress::new([0x02, 0, 0, 0, 0xaa, 0x01]);
const BRIDGE_NS_A_IP: Ipv4Address = Ipv4Address::new([172, 17, 0, 2]);
const BRIDGE_NS_B_IP: Ipv4Address = Ipv4Address::new([172, 17, 0, 3]);

#[test]
fn bridge_floods_broadcast_and_learns_source_mac() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    reset_netfilter_for_test();
    let bridge = new_test_bridge("docker0", 80);
    let pair_a = new_bridge_veth_pair("ct-a0", "veth-a0", 80);
    let pair_b = new_bridge_veth_pair("ct-b0", "veth-b0", 81);

    bridge
        .device
        .add_port_for_test_or_bootstrap(pair_a.right)
        .expect("bridge port a");
    bridge
        .device
        .add_port_for_test_or_bootstrap(pair_b.right)
        .expect("bridge port b");

    let src = pair_a.left.ops.mac_addr();
    let frame = ethernet_frame(EthernetAddress::BROADCAST, src, b"hello");
    assert_eq!(
        pair_a.left.ops.transmit(&frame, &guard),
        StepOutcome::Done(())
    );

    let outcome = bridge.device.poll_once(&guard);
    assert_eq!(outcome.frames_seen, 1);
    assert_eq!(outcome.learned, 1);
    assert_eq!(outcome.local_delivered, 1);
    assert_eq!(outcome.forwarded, 1);
    assert_eq!(outcome.flooded, 1);
    assert_eq!(bridge.device.learned_port_name(src), Some("veth-a0"));

    let received = pair_b.left.ops.receive().expect("peer b received flood");
    assert_eq!(received.as_bytes(), frame.as_slice());
    let local = bridge
        .registration
        .ops
        .receive()
        .expect("bridge local received broadcast");
    assert_eq!(local.as_bytes(), frame.as_slice());
    assert!(pair_a.left.ops.receive().is_none());

    let nf = netfilter_stats_snapshot();
    assert_eq!(nf.prerouting, 1);
    assert_eq!(nf.input, 1);
    assert_eq!(nf.forward, 1);
    assert_eq!(nf.postrouting, 1);
}

#[test]
fn bridge_uses_learned_unicast_instead_of_flooding() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    reset_netfilter_for_test();
    let bridge = new_test_bridge("docker1", 82);
    let pair_a = new_bridge_veth_pair("ct-a1", "veth-a1", 82);
    let pair_b = new_bridge_veth_pair("ct-b1", "veth-b1", 83);
    let pair_c = new_bridge_veth_pair("ct-c1", "veth-c1", 84);

    for port in [pair_a.right, pair_b.right, pair_c.right] {
        bridge
            .device
            .add_port_for_test_or_bootstrap(port)
            .expect("bridge port");
    }

    let mac_a = pair_a.left.ops.mac_addr();
    let mac_b = pair_b.left.ops.mac_addr();
    let learn_a = ethernet_frame(EthernetAddress::BROADCAST, mac_a, b"learn-a");
    assert_eq!(
        pair_a.left.ops.transmit(&learn_a, &guard),
        StepOutcome::Done(())
    );
    let first = bridge.device.poll_once(&guard);
    assert_eq!(first.forwarded, 2);
    assert_eq!(
        pair_b.left.ops.receive().expect("b flood").as_bytes(),
        learn_a.as_slice()
    );
    assert_eq!(
        pair_c.left.ops.receive().expect("c flood").as_bytes(),
        learn_a.as_slice()
    );

    let unicast = ethernet_frame(mac_a, mac_b, b"unicast");
    assert_eq!(
        pair_b.left.ops.transmit(&unicast, &guard),
        StepOutcome::Done(())
    );
    let second = bridge.device.poll_once(&guard);
    assert_eq!(second.frames_seen, 1);
    assert_eq!(second.forwarded, 1);
    assert_eq!(second.flooded, 0);
    assert_eq!(bridge.device.learned_port_name(mac_b), Some("veth-b1"));
    assert_eq!(
        pair_a
            .left
            .ops
            .receive()
            .expect("a learned unicast")
            .as_bytes(),
        unicast.as_slice()
    );
    assert!(pair_c.left.ops.receive().is_none());
}

#[test]
fn bridge_delivers_unicast_to_local_without_flooding() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    reset_netfilter_for_test();
    let bridge = new_test_bridge("docker-local0", 90);
    let pair = new_bridge_veth_pair("ct-local0", "veth-local0", 90);

    bridge
        .device
        .add_port_for_test_or_bootstrap(pair.right)
        .expect("bridge port");

    let src = pair.left.ops.mac_addr();
    let frame = ethernet_frame(BRIDGE_MAC, src, b"to-docker0");
    assert_eq!(
        pair.left.ops.transmit(&frame, &guard),
        StepOutcome::Done(())
    );

    let outcome = bridge.device.poll_once(&guard);
    assert_eq!(outcome.frames_seen, 1);
    assert_eq!(outcome.learned, 1);
    assert_eq!(outcome.local_delivered, 1);
    assert_eq!(outcome.forwarded, 0);
    assert_eq!(outcome.flooded, 0);
    assert_eq!(outcome.dropped, 0);
    assert_eq!(bridge.device.learned_port_name(src), Some("veth-local0"));

    let local = bridge
        .registration
        .ops
        .receive()
        .expect("bridge local received unicast");
    assert_eq!(local.as_bytes(), frame.as_slice());
    assert!(pair.left.ops.receive().is_none());

    let nf = netfilter_stats_snapshot();
    assert_eq!(nf.prerouting, 1);
    assert_eq!(nf.input, 1);
    assert_eq!(nf.forward, 0);
    assert_eq!(nf.postrouting, 0);
}

#[test]
fn bridge_local_transmit_uses_learned_port() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    reset_netfilter_for_test();
    let bridge = new_test_bridge("docker-local1", 91);
    let pair_a = new_bridge_veth_pair("ct-local1a", "veth-local1a", 91);
    let pair_b = new_bridge_veth_pair("ct-local1b", "veth-local1b", 92);

    for port in [pair_a.right, pair_b.right] {
        bridge
            .device
            .add_port_for_test_or_bootstrap(port)
            .expect("bridge port");
    }

    let mac_a = pair_a.left.ops.mac_addr();
    let learn = ethernet_frame(BRIDGE_MAC, mac_a, b"learn-a");
    assert_eq!(
        pair_a.left.ops.transmit(&learn, &guard),
        StepOutcome::Done(())
    );
    let learned = bridge.device.poll_once(&guard);
    assert_eq!(learned.local_delivered, 1);
    assert_eq!(bridge.device.learned_port_name(mac_a), Some("veth-local1a"));
    assert!(bridge.registration.ops.receive().is_some());

    reset_netfilter_for_test();
    let frame = ethernet_frame(mac_a, BRIDGE_MAC, b"from-docker0");
    assert_eq!(
        bridge.registration.ops.transmit(&frame, &guard),
        StepOutcome::Done(())
    );

    let received = pair_a
        .left
        .ops
        .receive()
        .expect("learned port received bridge-local tx");
    assert_eq!(received.as_bytes(), frame.as_slice());
    assert!(pair_b.left.ops.receive().is_none());

    let nf = netfilter_stats_snapshot();
    assert_eq!(nf.output, 1);
    assert_eq!(nf.postrouting, 1);
    assert_eq!(nf.forward, 0);
}

#[test]
fn bridge_carries_udp_between_two_veth_namespaces() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let ns_a_identity =
        crate::net::create_isolated_net_namespace_for_test("bridge-data-a").expect("namespace a");
    let ns_b_identity =
        crate::net::create_isolated_net_namespace_for_test("bridge-data-b").expect("namespace b");
    let ns_a = ns_a_identity.payload_cap().expect("namespace a payload");
    let ns_b = ns_b_identity.payload_cap().expect("namespace b payload");
    let bridge = new_test_bridge("docker-data0", 93);
    let pair_a = new_bridge_veth_pair("eth-data-a", "veth-data-a", 93);
    let pair_b = new_bridge_veth_pair("eth-data-b", "veth-data-b", 94);

    bridge
        .device
        .add_port_for_test_or_bootstrap(pair_a.right)
        .expect("bridge port a");
    bridge
        .device
        .add_port_for_test_or_bootstrap(pair_b.right)
        .expect("bridge port b");
    ns_a.attach_device_for_test_or_bootstrap(pair_a.left, Some(BRIDGE_NS_A_IP))
        .expect("attach namespace a veth");
    ns_b.attach_device_for_test_or_bootstrap(pair_b.left, Some(BRIDGE_NS_B_IP))
        .expect("attach namespace b veth");

    let guard = tx_substrate::epoch::guard();
    let server = registry::create_socket_in_namespace(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
        ns_b.clone(),
    )
    .expect("server socket");
    assert_eq!(
        step_bind(&server, bridge_inet_at(BRIDGE_NS_B_IP, 40_172), &guard),
        StepOutcome::Done(())
    );

    let client = registry::create_socket_in_namespace(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
        ns_a.clone(),
    )
    .expect("client socket");
    assert_eq!(
        step_bind(&client, bridge_inet_at(BRIDGE_NS_A_IP, 50_172), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, bridge_inet_at(BRIDGE_NS_B_IP, 40_172), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"via-bridge", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(10)
    );

    let adapter_a = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: pair_a.left.ops.mac_addr(),
        local_ipv4: BRIDGE_NS_A_IP,
        mtu: pair_a.left.ops.mtu(),
    });
    let sink_a = SmoltcpPacketTxSink {
        adapter: &adapter_a,
        device: pair_a.left,
    };
    let StepOutcome::Done(tx) = step_process_device_tx_pending_in_namespace_at(
        &sink_a,
        ns_a,
        smoltcp::time::Instant::ZERO,
        DeviceTxBudget {
            tcp_connecting: 0,
            tcp_connected: 0,
            udp_bound: 1,
            raw_icmp: 0,
        },
        &guard,
    ) else {
        panic!("device tx step should complete");
    };
    assert_eq!(tx.udp_packets, 1);

    let bridge_outcome = bridge.device.poll_once(&guard);
    assert_eq!(bridge_outcome.frames_seen, 1);
    assert_eq!(bridge_outcome.forwarded, 1);
    assert_eq!(bridge_outcome.flooded, 1);
    assert_eq!(pair_b.left_device.pending_rx(), 1);

    let adapter_b = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: pair_b.left.ops.mac_addr(),
        local_ipv4: BRIDGE_NS_B_IP,
        mtu: pair_b.left.ops.mtu(),
    });
    let source_b = SmoltcpPacketSource {
        adapter: &adapter_b,
        device: pair_b.left,
    };
    let StepOutcome::Done(rx) = step_process_network_events_in_namespace_at(
        &source_b,
        ns_b,
        smoltcp::time::Instant::ZERO,
        &guard,
    ) else {
        panic!("network rx step should complete");
    };
    assert_eq!(rx.packets_seen, 1);
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        10
    );
    assert_eq!(
        step_recv(&server, 10, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(10)
    );
}

#[test]
fn bridge_l3_iface_allows_container_ping_host_gateway() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    reset_netfilter_for_test();
    let bridge = new_test_bridge("docker-gw0", 95);
    let pair = new_bridge_veth_pair("eth-gw0", "veth-gw0", 95);

    bridge
        .device
        .add_port_for_test_or_bootstrap(pair.right)
        .expect("bridge port");

    let now = smoltcp::time::Instant::ZERO;
    let host_ip = Ipv4Address::new([172, 17, 0, 1]);
    let container_ip = Ipv4Address::new([172, 17, 0, 2]);
    let host_iface = new_bridge_ether_iface(
        bridge.registration,
        host_ip,
        bridge.registration.ops.mac_addr(),
        "docker-gw0",
    );
    let container_iface =
        new_bridge_ether_iface(pair.left, container_ip, pair.left.ops.mac_addr(), "eth-gw0");

    let echo = Icmpv4EchoPacket {
        src: container_ip,
        dst: host_ip,
        ident: 0x720b,
        seq_no: 1,
        payload: b"n72b".to_vec(),
    };
    let echo_packet = crate::net::protocol::build_icmpv4_echo_request(&echo);
    assert!(matches!(
        container_iface.dispatch_ip_at(echo_packet.as_bytes(), now, &guard),
        PacketTxResult::PendingResolution { next_hop } if next_hop == host_ip
    ));
    let arp = container_iface.flush_pending_arp_at(now, 8, &guard);
    assert_eq!(arp.sent, 1);

    let arp_to_host = bridge.device.poll_once(&guard);
    assert_eq!(arp_to_host.frames_seen, 1);
    assert_eq!(arp_to_host.local_delivered, 1);
    assert_eq!(arp_to_host.forwarded, 0);

    let host_source = EtherPacketSource { iface: host_iface };
    assert_eq!(
        host_source.next_packet_at(now, &guard),
        Some(PacketDispatch::Unsupported)
    );

    let arp_reply = pair
        .left
        .ops
        .receive()
        .expect("container received ARP reply");
    assert_eq!(
        container_iface.process_frame_at(arp_reply, now, Some(&guard)),
        PacketDispatch::Unsupported
    );
    assert!(container_iface.arp_entry(host_ip, now).is_some());

    assert!(matches!(
        container_iface.dispatch_ip_at(echo_packet.as_bytes(), now, &guard),
        PacketTxResult::Accepted { .. }
    ));
    let icmp_to_host = bridge.device.poll_once(&guard);
    assert_eq!(icmp_to_host.frames_seen, 1);
    assert_eq!(icmp_to_host.local_delivered, 1);
    assert_eq!(icmp_to_host.forwarded, 0);

    assert_eq!(
        host_source.next_packet_at(now, &guard),
        Some(PacketDispatch::Icmp(Icmpv4Event::EchoRequest(echo.clone())))
    );

    let echo_reply = pair
        .left
        .ops
        .receive()
        .expect("container received ICMP reply");
    assert_eq!(
        container_iface.process_frame_at(echo_reply, now, Some(&guard)),
        PacketDispatch::Icmp(Icmpv4Event::EchoReply(echo.reply_packet()))
    );
}

#[test]
fn namespace_runtime_drives_container_ping_host_gateway() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let guard = tx_substrate::epoch::guard();
    let now = smoltcp::time::Instant::ZERO;
    let host_ip = Ipv4Address::new([172, 17, 0, 1]);
    let container_ip = Ipv4Address::new([172, 17, 0, 2]);
    let host = crate::net::initial_net_namespace_payload();
    let container = crate::net::create_isolated_net_namespace_for_test("runtime-ping-container")
        .expect("container namespace")
        .payload_cap()
        .expect("container payload");
    let bridge = new_test_bridge("docker-runtime0", 96);
    let pair = new_bridge_veth_pair("eth-runtime0", "veth-runtime0", 96);

    bridge
        .device
        .add_port_for_test_or_bootstrap(pair.right)
        .expect("bridge port");
    host.attach_device_for_test_or_bootstrap(bridge.registration, None)
        .expect("attach docker0");
    host.attach_device_for_test_or_bootstrap(pair.right, None)
        .expect("attach host veth");
    container
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach container eth0");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let docker_ifindex = bridge_ifindex_for(&host.link_snapshot(), "docker-runtime0");
    host.set_device_ipv4_addr_by_ifindex(auth, docker_ifindex, Some(host_ip), Some(16))
        .expect("set docker0 addr");
    let eth_ifindex = bridge_ifindex_for(&container.link_snapshot(), "eth-runtime0");
    container
        .set_device_ipv4_addr_by_ifindex(auth, eth_ifindex, Some(container_ip), Some(16))
        .expect("set container eth addr");

    assert_eq!(host.ether_ifaces_snapshot().len(), 1);
    assert_eq!(container.ether_ifaces_snapshot().len(), 1);

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(2, 3, 1).expect("AF_INET SOCK_RAW ICMP"),
        container.clone(),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmp socket create failed: {other:?}"),
    };
    let echo = Icmpv4EchoPacket {
        src: container_ip,
        dst: host_ip,
        ident: 0x72c0,
        seq_no: 1,
        payload: b"runtime".to_vec(),
    };
    let message = crate::net::protocol::build_icmpv4_echo_request_message(&echo);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(host_ip, 0)),
            &message,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(message.len())
    );

    let mut total = crate::net::NetNamespaceRuntimeOutcome::default();
    let mut reply = [0u8; 64];
    let mut received = None;
    for _ in 0..8 {
        let outcome = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        total.merge(outcome);
        if let StepOutcome::Done(recv) =
            step_recv_kernel_bytes(&raw, &mut reply, SendRecvFlags::empty(), &guard)
        {
            received = Some(recv.bytes);
            break;
        }
    }

    assert_eq!(received, Some(message.len()), "runtime outcome: {total:?}");
    assert!(total.ifaces_seen >= 2);
    assert!(total.bridge_frames_seen >= 2);
    assert!(total.bridge_local_delivered >= 1);
    assert!(total.device_tx_packets >= 1);
    assert!(total.arp_sent >= 1);
}

#[test]
fn namespace_runtime_drives_container_ping_container_through_bridge() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let guard = tx_substrate::epoch::guard();
    let now = smoltcp::time::Instant::ZERO;
    let host = crate::net::initial_net_namespace_payload();
    let ns_a = crate::net::create_isolated_net_namespace_for_test("runtime-ping-a")
        .expect("namespace a")
        .payload_cap()
        .expect("namespace a payload");
    let ns_b = crate::net::create_isolated_net_namespace_for_test("runtime-ping-b")
        .expect("namespace b")
        .payload_cap()
        .expect("namespace b payload");
    let bridge = new_test_bridge("docker-runtime1", 97);
    let pair_a = new_bridge_veth_pair("eth-runtime1a", "veth-runtime1a", 97);
    let pair_b = new_bridge_veth_pair("eth-runtime1b", "veth-runtime1b", 98);

    bridge
        .device
        .add_port_for_test_or_bootstrap(pair_a.right)
        .expect("bridge port a");
    bridge
        .device
        .add_port_for_test_or_bootstrap(pair_b.right)
        .expect("bridge port b");
    host.attach_device_for_test_or_bootstrap(bridge.registration, None)
        .expect("attach docker0");
    host.attach_device_for_test_or_bootstrap(pair_a.right, None)
        .expect("attach host veth a");
    host.attach_device_for_test_or_bootstrap(pair_b.right, None)
        .expect("attach host veth b");
    ns_a.attach_device_for_test_or_bootstrap(pair_a.left, None)
        .expect("attach namespace a eth");
    ns_b.attach_device_for_test_or_bootstrap(pair_b.left, None)
        .expect("attach namespace b eth");

    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let eth_a_ifindex = bridge_ifindex_for(&ns_a.link_snapshot(), "eth-runtime1a");
    ns_a.set_device_ipv4_addr_by_ifindex(auth, eth_a_ifindex, Some(BRIDGE_NS_A_IP), Some(16))
        .expect("set namespace a addr");
    let eth_b_ifindex = bridge_ifindex_for(&ns_b.link_snapshot(), "eth-runtime1b");
    ns_b.set_device_ipv4_addr_by_ifindex(auth, eth_b_ifindex, Some(BRIDGE_NS_B_IP), Some(16))
        .expect("set namespace b addr");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(2, 3, 1).expect("AF_INET SOCK_RAW ICMP"),
        ns_a.clone(),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmp socket create failed: {other:?}"),
    };
    let echo = Icmpv4EchoPacket {
        src: BRIDGE_NS_A_IP,
        dst: BRIDGE_NS_B_IP,
        ident: 0x72c1,
        seq_no: 1,
        payload: b"peer-runtime".to_vec(),
    };
    let message = crate::net::protocol::build_icmpv4_echo_request_message(&echo);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(BRIDGE_NS_B_IP, 0)),
            &message,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(message.len())
    );

    let mut total = crate::net::NetNamespaceRuntimeOutcome::default();
    let mut reply = [0u8; 96];
    let mut received = None;
    for _ in 0..16 {
        let outcome = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        total.merge(outcome);
        if let StepOutcome::Done(recv) =
            step_recv_kernel_bytes(&raw, &mut reply, SendRecvFlags::empty(), &guard)
        {
            received = Some(recv.bytes);
            break;
        }
    }

    assert_eq!(
        received,
        Some(message.len()),
        "runtime outcome: {total:?}; a arp: {:?}; b arp: {:?}; learned a: {:?}; learned b: {:?}",
        ns_a.ether_ifaces_snapshot()[0].arp_snapshot(now),
        ns_b.ether_ifaces_snapshot()[0].arp_snapshot(now),
        bridge.device.learned_port_name(pair_a.left.ops.mac_addr()),
        bridge.device.learned_port_name(pair_b.left.ops.mac_addr()),
    );
    assert!(total.bridge_forwarded >= 2);
    assert!(total.device_tx_packets >= 1);
    assert!(total.arp_sent >= 1);
}

#[test]
fn namespace_runtime_forwards_container_external_ipv4_to_uplink_route() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let guard = tx_substrate::epoch::guard();
    let now = smoltcp::time::Instant::ZERO;
    let host = crate::net::create_isolated_net_namespace_for_test("runtime-forward-host")
        .expect("host namespace")
        .payload_cap()
        .expect("host payload");
    let container = crate::net::create_isolated_net_namespace_for_test("runtime-forward-container")
        .expect("container namespace")
        .payload_cap()
        .expect("container payload");
    let bridge = new_test_bridge("docker-forward0", 99);
    let container_pair = new_bridge_veth_pair("eth-forward0", "veth-forward0", 99);
    let uplink_pair = new_bridge_veth_pair("uplink-forward0", "gw-forward0", 100);

    bridge
        .device
        .add_port_for_test_or_bootstrap(container_pair.right)
        .expect("bridge port");
    host.attach_device_for_test_or_bootstrap(bridge.registration, None)
        .expect("attach docker0");
    host.attach_device_for_test_or_bootstrap(container_pair.right, None)
        .expect("attach host veth");
    host.attach_device_for_test_or_bootstrap(uplink_pair.left, None)
        .expect("attach uplink");
    container
        .attach_device_for_test_or_bootstrap(container_pair.left, None)
        .expect("attach container eth");

    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let docker_ip = Ipv4Address::new([172, 17, 0, 1]);
    let container_ip = Ipv4Address::new([172, 17, 0, 2]);
    let uplink_ip = Ipv4Address::new([10, 0, 2, 15]);
    let uplink_gw = Ipv4Address::new([10, 0, 2, 2]);
    let external_ip = Ipv4Address::new([8, 8, 8, 8]);

    let docker_ifindex = bridge_ifindex_for(&host.link_snapshot(), "docker-forward0");
    host.set_device_ipv4_addr_by_ifindex(auth, docker_ifindex, Some(docker_ip), Some(16))
        .expect("set docker0 addr");
    let uplink_ifindex = bridge_ifindex_for(&host.link_snapshot(), "uplink-forward0");
    host.set_device_ipv4_addr_by_ifindex(auth, uplink_ifindex, Some(uplink_ip), Some(24))
        .expect("set uplink addr");
    host.add_ipv4_route(
        auth,
        crate::net::NetNamespaceRouteConfig {
            dst: Ipv4Address::UNSPECIFIED,
            prefix_len: 0,
            gateway: Some(uplink_gw),
            oif_name: Some("uplink-forward0"),
            preferred_src: Some(uplink_ip),
            table: 254,
            protocol: 4,
            scope: 0,
            route_type: 1,
        },
    )
    .expect("host default route");
    host.set_ipv4_forwarding_for_test_or_bootstrap(true);
    host.ether_ifaces_snapshot()
        .into_iter()
        .find(|iface| iface.name == "uplink-forward0")
        .expect("uplink iface")
        .install_arp_for_test_or_bootstrap(
            uplink_gw,
            uplink_pair.right.ops.mac_addr(),
            smoltcp::time::Instant::from_secs(60),
        );

    let eth_ifindex = bridge_ifindex_for(&container.link_snapshot(), "eth-forward0");
    container
        .set_device_ipv4_addr_by_ifindex(auth, eth_ifindex, Some(container_ip), Some(16))
        .expect("set container addr");
    container
        .add_ipv4_route(
            auth,
            crate::net::NetNamespaceRouteConfig {
                dst: Ipv4Address::UNSPECIFIED,
                prefix_len: 0,
                gateway: Some(docker_ip),
                oif_name: Some("eth-forward0"),
                preferred_src: Some(container_ip),
                table: 254,
                protocol: 4,
                scope: 0,
                route_type: 1,
            },
        )
        .expect("container default route");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(2, 3, 1).expect("AF_INET SOCK_RAW ICMP"),
        container.clone(),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmp socket create failed: {other:?}"),
    };
    let echo = Icmpv4EchoPacket {
        src: Ipv4Address::UNSPECIFIED,
        dst: external_ip,
        ident: 0x72f0,
        seq_no: 1,
        payload: b"forward".to_vec(),
    };
    let message = crate::net::protocol::build_icmpv4_echo_request_message(&echo);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(external_ip, 0)),
            &message,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(message.len())
    );

    let mut total = crate::net::NetNamespaceRuntimeOutcome::default();
    let mut forwarded = None;
    for _ in 0..12 {
        let outcome = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        total.merge(outcome);
        if let Some(frame) = uplink_pair.right.ops.receive() {
            forwarded = Some(frame);
            break;
        }
    }

    let frame = forwarded.expect("external uplink should receive forwarded frame");
    let ethernet =
        smoltcp::wire::EthernetFrame::new_checked(frame.as_bytes()).expect("ethernet frame");
    assert_eq!(
        EthernetAddress::new(ethernet.dst_addr().0),
        uplink_pair.right.ops.mac_addr()
    );
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(ethernet.payload()).expect("ipv4 packet");
    assert_eq!(Ipv4Address::new(ipv4.dst_addr().octets()), external_ip);
    assert_eq!(Ipv4Address::new(ipv4.src_addr().octets()), container_ip);
    assert!(
        total.ipv4_forwarded >= 1,
        "runtime outcome should record forwarding: {total:?}"
    );
}

#[test]
fn namespace_runtime_masquerades_icmp_and_conntrack_dnat_reply() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    reset_netfilter_for_test();

    let guard = tx_substrate::epoch::guard();
    let now = smoltcp::time::Instant::ZERO;
    let host = crate::net::create_isolated_net_namespace_for_test("runtime-nat-host")
        .expect("host namespace")
        .payload_cap()
        .expect("host payload");
    let container = crate::net::create_isolated_net_namespace_for_test("runtime-nat-container")
        .expect("container namespace")
        .payload_cap()
        .expect("container payload");
    let bridge = new_test_bridge("docker-nat0", 101);
    let container_pair = new_bridge_veth_pair("eth-nat0", "veth-nat0", 101);
    let uplink_pair = new_bridge_veth_pair("uplink-nat0", "gw-nat0", 102);

    bridge
        .device
        .add_port_for_test_or_bootstrap(container_pair.right)
        .expect("bridge port");
    host.attach_device_for_test_or_bootstrap(bridge.registration, None)
        .expect("attach docker0");
    host.attach_device_for_test_or_bootstrap(container_pair.right, None)
        .expect("attach host veth");
    host.attach_device_for_test_or_bootstrap(uplink_pair.left, None)
        .expect("attach uplink");
    container
        .attach_device_for_test_or_bootstrap(container_pair.left, None)
        .expect("attach container eth");

    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let docker_ip = Ipv4Address::new([172, 17, 0, 1]);
    let container_ip = Ipv4Address::new([172, 17, 0, 2]);
    let uplink_ip = Ipv4Address::new([10, 0, 2, 15]);
    let uplink_gw = Ipv4Address::new([10, 0, 2, 2]);
    let external_ip = Ipv4Address::new([8, 8, 8, 8]);

    let docker_ifindex = bridge_ifindex_for(&host.link_snapshot(), "docker-nat0");
    host.set_device_ipv4_addr_by_ifindex(auth, docker_ifindex, Some(docker_ip), Some(16))
        .expect("set docker0 addr");
    let uplink_ifindex = bridge_ifindex_for(&host.link_snapshot(), "uplink-nat0");
    host.set_device_ipv4_addr_by_ifindex(auth, uplink_ifindex, Some(uplink_ip), Some(24))
        .expect("set uplink addr");
    host.add_ipv4_route(
        auth,
        crate::net::NetNamespaceRouteConfig {
            dst: Ipv4Address::UNSPECIFIED,
            prefix_len: 0,
            gateway: Some(uplink_gw),
            oif_name: Some("uplink-nat0"),
            preferred_src: Some(uplink_ip),
            table: 254,
            protocol: 4,
            scope: 0,
            route_type: 1,
        },
    )
    .expect("host default route");
    host.set_ipv4_forwarding_for_test_or_bootstrap(true);
    host.ether_ifaces_snapshot()
        .into_iter()
        .find(|iface| iface.name == "uplink-nat0")
        .expect("uplink iface")
        .install_arp_for_test_or_bootstrap(
            uplink_gw,
            uplink_pair.right.ops.mac_addr(),
            smoltcp::time::Instant::from_secs(60),
        );
    crate::net::netfilter::add_masquerade_rule_in_namespace_for_test_or_bootstrap(
        &host,
        NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "uplink-nat0",
    )
    .expect("masquerade rule");
    assert_eq!(
        crate::net::netfilter_rules_snapshot_for_namespace(&host).len(),
        1
    );

    let eth_ifindex = bridge_ifindex_for(&container.link_snapshot(), "eth-nat0");
    container
        .set_device_ipv4_addr_by_ifindex(auth, eth_ifindex, Some(container_ip), Some(16))
        .expect("set container addr");
    container
        .add_ipv4_route(
            auth,
            crate::net::NetNamespaceRouteConfig {
                dst: Ipv4Address::UNSPECIFIED,
                prefix_len: 0,
                gateway: Some(docker_ip),
                oif_name: Some("eth-nat0"),
                preferred_src: Some(container_ip),
                table: 254,
                protocol: 4,
                scope: 0,
                route_type: 1,
            },
        )
        .expect("container default route");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(2, 3, 1).expect("AF_INET SOCK_RAW ICMP"),
        container.clone(),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmp socket create failed: {other:?}"),
    };
    let ident = 0x72f1;
    let echo = Icmpv4EchoPacket {
        src: Ipv4Address::UNSPECIFIED,
        dst: external_ip,
        ident,
        seq_no: 1,
        payload: b"nat-forward".to_vec(),
    };
    let message = crate::net::protocol::build_icmpv4_echo_request_message(&echo);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(external_ip, 0)),
            &message,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(message.len())
    );

    let mut total = crate::net::NetNamespaceRuntimeOutcome::default();
    let mut outbound = None;
    for _ in 0..12 {
        let outcome = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        total.merge(outcome);
        if let Some(frame) = uplink_pair.right.ops.receive() {
            outbound = Some(frame);
            break;
        }
    }
    let outbound = outbound.expect("uplink should receive masqueraded frame");
    let ethernet =
        smoltcp::wire::EthernetFrame::new_checked(outbound.as_bytes()).expect("ethernet frame");
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(ethernet.payload()).expect("ipv4 packet");
    assert_eq!(Ipv4Address::new(ipv4.src_addr().octets()), uplink_ip);
    assert_eq!(Ipv4Address::new(ipv4.dst_addr().octets()), external_ip);
    assert_eq!(
        crate::net::netfilter_conntrack_snapshot_for_namespace(&host).len(),
        1
    );
    assert_eq!(
        crate::net::netfilter_conntrack_snapshot_for_namespace(&host)[0].original_src,
        container_ip
    );

    let reply = Icmpv4EchoPacket {
        src: external_ip,
        dst: uplink_ip,
        ident,
        seq_no: 1,
        payload: b"nat-forward".to_vec(),
    };
    let reply_packet = crate::net::protocol::build_icmpv4_echo_reply(&reply);
    let reply_frame = ethernet_frame(
        uplink_pair.left.ops.mac_addr(),
        uplink_pair.right.ops.mac_addr(),
        reply_packet.as_bytes(),
    );
    assert_eq!(
        uplink_pair.right.ops.transmit(&reply_frame, &guard),
        StepOutcome::Done(())
    );

    let mut reply_buf = [0u8; 96];
    let mut received = None;
    for _ in 0..12 {
        let outcome = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        total.merge(outcome);
        if let StepOutcome::Done(recv) =
            step_recv_kernel_bytes(&raw, &mut reply_buf, SendRecvFlags::empty(), &guard)
        {
            received = Some(recv.bytes);
            break;
        }
    }

    assert_eq!(received, Some(message.len()), "runtime outcome: {total:?}");
    assert!(total.ipv4_forwarded >= 2);
}

#[test]
fn namespace_runtime_masquerades_udp_and_conntrack_dnat_reply() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    reset_netfilter_for_test();

    let guard = tx_substrate::epoch::guard();
    let now = smoltcp::time::Instant::ZERO;
    let host = crate::net::create_isolated_net_namespace_for_test("runtime-udp-nat-host")
        .expect("host namespace")
        .payload_cap()
        .expect("host payload");
    let container = crate::net::create_isolated_net_namespace_for_test("runtime-udp-nat-container")
        .expect("container namespace")
        .payload_cap()
        .expect("container payload");
    let bridge = new_test_bridge("docker-udp-nat0", 103);
    let container_pair = new_bridge_veth_pair("eth-udp-nat0", "veth-udp-nat0", 103);
    let uplink_pair = new_bridge_veth_pair("uplink-udp-nat0", "gw-udp-nat0", 104);

    bridge
        .device
        .add_port_for_test_or_bootstrap(container_pair.right)
        .expect("bridge port");
    host.attach_device_for_test_or_bootstrap(bridge.registration, None)
        .expect("attach docker0");
    host.attach_device_for_test_or_bootstrap(container_pair.right, None)
        .expect("attach host veth");
    host.attach_device_for_test_or_bootstrap(uplink_pair.left, None)
        .expect("attach uplink");
    container
        .attach_device_for_test_or_bootstrap(container_pair.left, None)
        .expect("attach container eth");

    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let docker_ip = Ipv4Address::new([172, 17, 0, 1]);
    let container_ip = Ipv4Address::new([172, 17, 0, 2]);
    let uplink_ip = Ipv4Address::new([10, 0, 2, 15]);
    let uplink_gw = Ipv4Address::new([10, 0, 2, 2]);
    let external_ip = Ipv4Address::new([8, 8, 8, 8]);
    let local_port = 41_172;
    let remote_port = 53;

    let docker_ifindex = bridge_ifindex_for(&host.link_snapshot(), "docker-udp-nat0");
    host.set_device_ipv4_addr_by_ifindex(auth, docker_ifindex, Some(docker_ip), Some(16))
        .expect("set docker0 addr");
    let uplink_ifindex = bridge_ifindex_for(&host.link_snapshot(), "uplink-udp-nat0");
    host.set_device_ipv4_addr_by_ifindex(auth, uplink_ifindex, Some(uplink_ip), Some(24))
        .expect("set uplink addr");
    host.add_ipv4_route(
        auth,
        crate::net::NetNamespaceRouteConfig {
            dst: Ipv4Address::UNSPECIFIED,
            prefix_len: 0,
            gateway: Some(uplink_gw),
            oif_name: Some("uplink-udp-nat0"),
            preferred_src: Some(uplink_ip),
            table: 254,
            protocol: 4,
            scope: 0,
            route_type: 1,
        },
    )
    .expect("host default route");
    host.set_ipv4_forwarding_for_test_or_bootstrap(true);
    host.ether_ifaces_snapshot()
        .into_iter()
        .find(|iface| iface.name == "uplink-udp-nat0")
        .expect("uplink iface")
        .install_arp_for_test_or_bootstrap(
            uplink_gw,
            uplink_pair.right.ops.mac_addr(),
            smoltcp::time::Instant::from_secs(60),
        );
    crate::net::netfilter::add_masquerade_rule_in_namespace_for_test_or_bootstrap(
        &host,
        NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "uplink-udp-nat0",
    )
    .expect("masquerade rule");

    let eth_ifindex = bridge_ifindex_for(&container.link_snapshot(), "eth-udp-nat0");
    container
        .set_device_ipv4_addr_by_ifindex(auth, eth_ifindex, Some(container_ip), Some(16))
        .expect("set container addr");
    container
        .add_ipv4_route(
            auth,
            crate::net::NetNamespaceRouteConfig {
                dst: Ipv4Address::UNSPECIFIED,
                prefix_len: 0,
                gateway: Some(docker_ip),
                oif_name: Some("eth-udp-nat0"),
                preferred_src: Some(container_ip),
                table: 254,
                protocol: 4,
                scope: 0,
                route_type: 1,
            },
        )
        .expect("container default route");

    let udp = registry::create_socket_in_namespace(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
        container.clone(),
    )
    .expect("udp socket");
    assert_eq!(
        step_bind(&udp, bridge_inet_at(container_ip, local_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &udp,
            Some(IpEndpoint::new(external_ip, remote_port)),
            b"dns-query",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(9)
    );

    let mut outbound = None;
    for _ in 0..12 {
        let _ = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        if let Some(frame) = uplink_pair.right.ops.receive() {
            outbound = Some(frame);
            break;
        }
    }
    let outbound = outbound.expect("uplink should receive masqueraded UDP");
    let ethernet =
        smoltcp::wire::EthernetFrame::new_checked(outbound.as_bytes()).expect("ethernet frame");
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(ethernet.payload()).expect("ipv4 packet");
    assert_eq!(Ipv4Address::new(ipv4.src_addr().octets()), uplink_ip);
    let udp_packet = smoltcp::wire::UdpPacket::new_checked(ipv4.payload()).expect("udp packet");
    assert_eq!(udp_packet.src_port(), local_port);
    assert_eq!(udp_packet.dst_port(), remote_port);
    let entry = crate::net::netfilter_conntrack_snapshot_for_namespace(&host)
        .into_iter()
        .find(|entry| entry.protocol == NetfilterConntrackProtocol::Udp)
        .expect("udp conntrack entry");
    assert_eq!(entry.original_src, container_ip);
    assert_eq!(entry.original_src_port, local_port);
    assert_eq!(entry.external_dst_port, remote_port);

    let reply_packet = udp_ipv4_packet(
        IpEndpoint::new(external_ip, remote_port),
        IpEndpoint::new(uplink_ip, local_port),
        b"dns-reply",
    );
    let reply_frame = ethernet_frame(
        uplink_pair.left.ops.mac_addr(),
        uplink_pair.right.ops.mac_addr(),
        reply_packet.as_slice(),
    );
    assert_eq!(
        uplink_pair.right.ops.transmit(&reply_frame, &guard),
        StepOutcome::Done(())
    );

    let mut reply_buf = [0u8; 32];
    let mut received = None;
    for _ in 0..12 {
        let _ = crate::net::drive_all_net_namespace_runtimes_at(now, &guard);
        if let StepOutcome::Done(recv) =
            step_recv_kernel_bytes(&udp, &mut reply_buf, SendRecvFlags::empty(), &guard)
        {
            received = Some(recv.bytes);
            break;
        }
    }

    assert_eq!(received, Some(9));
    assert_eq!(&reply_buf[..9], b"dns-reply");
}

#[test]
fn netfilter_masquerades_tcp_tuple_and_rewrites_reply_checksum() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    add_masquerade_rule_for_test_or_bootstrap(
        NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "uplink-tcp-nat0",
    )
    .expect("masquerade rule");

    let container_ip = Ipv4Address::new([172, 17, 0, 2]);
    let uplink_ip = Ipv4Address::new([10, 0, 2, 15]);
    let external_ip = Ipv4Address::new([93, 184, 216, 34]);
    let syn = tcp_ipv4_packet(
        IpEndpoint::new(container_ip, 40_180),
        IpEndpoint::new(external_ip, 80),
        smoltcp::wire::TcpControl::Syn,
        None,
    );

    let masqueraded = apply_postrouting_nat_ipv4(
        NetfilterFrameContext {
            hook: NetfilterHook::Postrouting,
            bridge: None,
            ingress: Some("docker-tcp-nat0"),
            egress: Some("uplink-tcp-nat0"),
        },
        syn.as_slice(),
        uplink_ip,
    )
    .expect("tcp masquerade");
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(masqueraded.as_slice()).expect("masq ipv4");
    assert_eq!(Ipv4Address::new(ipv4.src_addr().octets()), uplink_ip);
    let tcp = smoltcp::wire::TcpPacket::new_checked(ipv4.payload()).expect("masq tcp");
    assert_eq!(tcp.src_port(), 40_180);
    assert_eq!(tcp.dst_port(), 80);
    assert!(tcp.verify_checksum(
        &smoltcp::wire::IpAddress::Ipv4(ipv4.src_addr()),
        &smoltcp::wire::IpAddress::Ipv4(ipv4.dst_addr()),
    ));

    let syn_ack = tcp_ipv4_packet(
        IpEndpoint::new(external_ip, 80),
        IpEndpoint::new(uplink_ip, 40_180),
        smoltcp::wire::TcpControl::Syn,
        Some(1),
    );
    let restored = apply_prerouting_nat_ipv4(
        NetfilterFrameContext {
            hook: NetfilterHook::Prerouting,
            bridge: None,
            ingress: Some("uplink-tcp-nat0"),
            egress: None,
        },
        syn_ack.as_slice(),
    )
    .expect("tcp reverse nat");
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(restored.as_slice()).expect("restored ipv4");
    assert_eq!(Ipv4Address::new(ipv4.dst_addr().octets()), container_ip);
    let tcp = smoltcp::wire::TcpPacket::new_checked(ipv4.payload()).expect("restored tcp");
    assert_eq!(tcp.dst_port(), 40_180);
    assert!(tcp.verify_checksum(
        &smoltcp::wire::IpAddress::Ipv4(ipv4.src_addr()),
        &smoltcp::wire::IpAddress::Ipv4(ipv4.dst_addr()),
    ));
}

#[test]
fn netfilter_dnat_published_tcp_port_and_rewrites_reply_checksum() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    let public_ip = Ipv4Address::new([10, 0, 2, 15]);
    let private_ip = Ipv4Address::new([172, 17, 0, 2]);
    let client_ip = Ipv4Address::new([198, 51, 100, 8]);
    add_dnat_rule_for_test_or_bootstrap(
        NetfilterConntrackProtocol::Tcp,
        public_ip,
        8080,
        private_ip,
        80,
    )
    .expect("dnat rule");

    let syn = tcp_ipv4_packet(
        IpEndpoint::new(client_ip, 40_000),
        IpEndpoint::new(public_ip, 8080),
        smoltcp::wire::TcpControl::Syn,
        None,
    );
    let forwarded = apply_prerouting_nat_ipv4(
        NetfilterFrameContext {
            hook: NetfilterHook::Prerouting,
            bridge: None,
            ingress: Some("uplink-dnat0"),
            egress: None,
        },
        syn.as_slice(),
    )
    .expect("published port dnat");
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(forwarded.as_slice()).expect("dnat ipv4");
    assert_eq!(Ipv4Address::new(ipv4.src_addr().octets()), client_ip);
    assert_eq!(Ipv4Address::new(ipv4.dst_addr().octets()), private_ip);
    let tcp = smoltcp::wire::TcpPacket::new_checked(ipv4.payload()).expect("dnat tcp");
    assert_eq!(tcp.src_port(), 40_000);
    assert_eq!(tcp.dst_port(), 80);
    assert!(tcp.verify_checksum(
        &smoltcp::wire::IpAddress::Ipv4(ipv4.src_addr()),
        &smoltcp::wire::IpAddress::Ipv4(ipv4.dst_addr()),
    ));

    let entry = netfilter_conntrack_snapshot()
        .into_iter()
        .find(|entry| entry.kind == NetfilterNatKind::Dnat)
        .expect("dnat conntrack");
    assert_eq!(entry.original_src, private_ip);
    assert_eq!(entry.original_src_port, 80);
    assert_eq!(entry.masquerade_src, public_ip);
    assert_eq!(entry.masquerade_src_port, 8080);
    assert_eq!(entry.external_dst, client_ip);
    assert_eq!(entry.external_dst_port, 40_000);

    let syn_ack = tcp_ipv4_packet(
        IpEndpoint::new(private_ip, 80),
        IpEndpoint::new(client_ip, 40_000),
        smoltcp::wire::TcpControl::Syn,
        Some(2),
    );
    let reply = apply_postrouting_nat_ipv4(
        NetfilterFrameContext {
            hook: NetfilterHook::Postrouting,
            bridge: None,
            ingress: Some("docker-dnat0"),
            egress: Some("uplink-dnat0"),
        },
        syn_ack.as_slice(),
        public_ip,
    )
    .expect("published port reply rewrite");
    let ipv4 = smoltcp::wire::Ipv4Packet::new_checked(reply.as_slice()).expect("reply ipv4");
    assert_eq!(Ipv4Address::new(ipv4.src_addr().octets()), public_ip);
    assert_eq!(Ipv4Address::new(ipv4.dst_addr().octets()), client_ip);
    let tcp = smoltcp::wire::TcpPacket::new_checked(ipv4.payload()).expect("reply tcp");
    assert_eq!(tcp.src_port(), 8080);
    assert_eq!(tcp.dst_port(), 40_000);
    assert!(tcp.verify_checksum(
        &smoltcp::wire::IpAddress::Ipv4(ipv4.src_addr()),
        &smoltcp::wire::IpAddress::Ipv4(ipv4.dst_addr()),
    ));
}

#[test]
fn bridge_add_port_requires_cap_net_admin_authority() {
    let bridge = new_test_bridge("docker2", 86);
    let pair = new_bridge_veth_pair("ct-a2", "veth-a2", 86);

    let denied = require_net_admin(unprivileged_cred());
    assert_eq!(denied, Err(Errno::EPERM));

    let authority = require_net_admin(crate::cred::Cred::root()).expect("root net admin");
    assert_eq!(bridge.device.add_port(authority, pair.right), Ok(()));
}

#[test]
fn namespace_snapshot_reports_bridge_and_port_membership() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let ns = crate::net::create_isolated_net_namespace_for_test("bridge-ns")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let bridge = new_test_bridge("docker3", 88);
    let pair_a = new_bridge_veth_pair("ct-a3", "veth-a3", 88);
    let pair_b = new_bridge_veth_pair("ct-b3", "veth-b3", 89);
    let authority = NetAdminAuthority::for_test_or_bootstrap();

    bridge
        .device
        .add_port(authority, pair_a.right)
        .expect("bridge port a");
    bridge
        .device
        .add_port(authority, pair_b.right)
        .expect("bridge port b");
    ns.attach_device(authority, bridge.registration, None)
        .expect("attach bridge");
    ns.attach_device(authority, pair_a.right, None)
        .expect("attach bridge port a");
    ns.attach_device(authority, pair_b.right, None)
        .expect("attach bridge port b");

    let snapshot = ns.network_snapshot();
    let bridge_info = snapshot
        .bridges
        .iter()
        .find(|bridge| bridge.name == "docker3")
        .expect("bridge info");
    assert_eq!(bridge_info.ports, std::vec!["veth-a3", "veth-b3"]);
    assert!(snapshot.links.iter().any(|link| {
        link.name == "docker3" && link.kind == NetDeviceKind::Bridge && link.master.is_none()
    }));
    assert!(snapshot.links.iter().any(|link| {
        link.name == "veth-a3" && link.kind == NetDeviceKind::Veth && link.master == Some("docker3")
    }));
}

fn new_test_bridge(name: &'static str, minor: u32) -> BridgeInstance {
    create_bridge_for_test_or_bootstrap(BridgeConfig {
        name,
        devt: DevT::new(92, minor),
        mac: BRIDGE_MAC,
        mtu: VETH_DEFAULT_MTU,
    })
}

fn new_bridge_veth_pair(left_name: &'static str, right_name: &'static str, minor: u32) -> VethPair {
    create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: left_name,
            devt: DevT::new(93, minor * 2),
            mac: EthernetAddress::new([0x02, 0, 0, 1, 0, minor as u8]),
        },
        right: VethEndpointConfig {
            name: right_name,
            devt: DevT::new(93, minor * 2 + 1),
            mac: EthernetAddress::new([0x02, 0, 0, 2, 0, minor as u8]),
        },
        mtu: VETH_DEFAULT_MTU,
    })
}

fn new_bridge_ether_iface(
    registration: &'static NetDeviceRegistration,
    local_ip: Ipv4Address,
    local_mac: EthernetAddress,
    name: &'static str,
) -> &'static EtherIface {
    Box::leak(Box::new(EtherIface::new(
        registration,
        IfaceCommon::new(
            local_ip,
            Ipv4Address::new([255, 255, 0, 0]),
            VETH_DEFAULT_MTU,
        ),
        local_mac,
        name,
    )))
}

fn ethernet_frame(dst: EthernetAddress, src: EthernetAddress, payload: &[u8]) -> std::vec::Vec<u8> {
    let mut frame = std::vec::Vec::new();
    frame.extend_from_slice(&dst.octets());
    frame.extend_from_slice(&src.octets());
    frame.extend_from_slice(&[0x08, 0x00]);
    frame.extend_from_slice(payload);
    frame
}

fn udp_ipv4_packet(src: IpEndpoint, dst: IpEndpoint, payload: &[u8]) -> std::vec::Vec<u8> {
    let udp_repr = smoltcp::wire::UdpRepr {
        src_port: src.port,
        dst_port: dst.port,
    };
    let udp_len = udp_repr.header_len() + payload.len();
    let ip_repr = smoltcp::wire::IpRepr::Ipv4(smoltcp::wire::Ipv4Repr {
        src_addr: smoltcp_ipv4(src.addr),
        dst_addr: smoltcp_ipv4(dst.addr),
        next_header: smoltcp::wire::IpProtocol::Udp,
        payload_len: udp_len,
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let mut bytes = std::vec![0u8; ip_header_len + udp_len];
    let checksum_caps = smoltcp::phy::ChecksumCapabilities::default();
    ip_repr.emit(&mut bytes[..ip_header_len], &checksum_caps);

    let src_addr = smoltcp::wire::IpAddress::Ipv4(smoltcp_ipv4(src.addr));
    let dst_addr = smoltcp::wire::IpAddress::Ipv4(smoltcp_ipv4(dst.addr));
    let mut udp_packet = smoltcp::wire::UdpPacket::new_unchecked(&mut bytes[ip_header_len..]);
    udp_repr.emit(
        &mut udp_packet,
        &src_addr,
        &dst_addr,
        payload.len(),
        |out| out.copy_from_slice(payload),
        &checksum_caps,
    );
    bytes
}

fn tcp_ipv4_packet(
    src: IpEndpoint,
    dst: IpEndpoint,
    control: smoltcp::wire::TcpControl,
    ack: Option<i32>,
) -> std::vec::Vec<u8> {
    let tcp_repr = smoltcp::wire::TcpRepr {
        src_port: src.port,
        dst_port: dst.port,
        control,
        seq_number: smoltcp::wire::TcpSeqNumber(1),
        ack_number: ack.map(smoltcp::wire::TcpSeqNumber),
        window_len: 4096,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let tcp_len = tcp_repr.buffer_len();
    let ip_repr = smoltcp::wire::IpRepr::Ipv4(smoltcp::wire::Ipv4Repr {
        src_addr: smoltcp_ipv4(src.addr),
        dst_addr: smoltcp_ipv4(dst.addr),
        next_header: smoltcp::wire::IpProtocol::Tcp,
        payload_len: tcp_len,
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let mut bytes = std::vec![0u8; ip_header_len + tcp_len];
    let checksum_caps = smoltcp::phy::ChecksumCapabilities::default();
    ip_repr.emit(&mut bytes[..ip_header_len], &checksum_caps);
    let mut tcp_packet = smoltcp::wire::TcpPacket::new_unchecked(&mut bytes[ip_header_len..]);
    tcp_repr.emit(
        &mut tcp_packet,
        &smoltcp::wire::IpAddress::Ipv4(smoltcp_ipv4(src.addr)),
        &smoltcp::wire::IpAddress::Ipv4(smoltcp_ipv4(dst.addr)),
        &checksum_caps,
    );
    bytes
}

fn smoltcp_ipv4(addr: Ipv4Address) -> smoltcp::wire::Ipv4Address {
    let [a, b, c, d] = addr.octets();
    smoltcp::wire::Ipv4Address::new(a, b, c, d)
}

fn bridge_inet_at(addr: Ipv4Address, port: u16) -> KernelSockAddr {
    KernelSockAddr::V4(SockAddrIn::new(port, addr))
}

fn bridge_ifindex_for(links: &[NetNamespaceLinkInfo], name: &str) -> u32 {
    links
        .iter()
        .find(|link| link.name == name)
        .map(|link| link.ifindex)
        .expect("link ifindex")
}

fn unprivileged_cred() -> crate::cred::Cred {
    crate::cred::Cred {
        uid: crate::cred::Uid(1000),
        euid: crate::cred::Uid(1000),
        suid: crate::cred::Uid(1000),
        gid: crate::cred::Gid(1000),
        egid: crate::cred::Gid(1000),
        sgid: crate::cred::Gid(1000),
        effective_caps: crate::cred::CapabilitySet::EMPTY,
        permitted_caps: crate::cred::CapabilitySet::EMPTY,
    }
}
