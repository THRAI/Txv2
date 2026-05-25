use super::*;

const NS_A_IP: Ipv4Address = Ipv4Address::new([172, 18, 0, 2]);
const NS_B_IP: Ipv4Address = Ipv4Address::new([172, 18, 0, 3]);

#[test]
fn veth_pair_transmit_delivers_to_peer_rx_queue() {
    let guard = tx_substrate::epoch::guard();
    let pair = new_test_veth_pair("vetha0", "vethb0", 70);
    let frame = ethernet_ipv4_frame(17, &udp_transport(50_100, 40_100, b"hello"));

    assert_eq!(
        pair.left.ops.transmit(&frame, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(pair.left_device.stats_snapshot().tx_packets, 1);
    assert_eq!(pair.right_device.stats_snapshot().rx_pending, 1);
    assert!(pair.left.ops.receive().is_none());

    let received = pair.right.ops.receive().expect("peer rx frame");
    assert_eq!(received.as_bytes(), frame.as_slice());
    assert_eq!(pair.right_device.stats_snapshot().rx_pending, 0);
}

#[test]
fn veth_pair_can_bridge_udp_between_isolated_namespace_socket_tables() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let ns_a_identity =
        crate::net::create_isolated_net_namespace_for_test("veth-ns-a").expect("namespace a");
    let ns_b_identity =
        crate::net::create_isolated_net_namespace_for_test("veth-ns-b").expect("namespace b");
    let ns_a = ns_a_identity.payload_cap().expect("namespace a payload");
    let ns_b = ns_b_identity.payload_cap().expect("namespace b payload");
    let pair = new_test_veth_pair("vetha1", "vethb1", 71);

    ns_a.attach_device_for_test_or_bootstrap(pair.left, Some(NS_A_IP))
        .expect("attach namespace a veth");
    ns_b.attach_device_for_test_or_bootstrap(pair.right, Some(NS_B_IP))
        .expect("attach namespace b veth");
    assert_link(&ns_a.link_snapshot(), "vetha1", NS_A_IP);
    assert_link(&ns_b.link_snapshot(), "vethb1", NS_B_IP);

    let guard = tx_substrate::epoch::guard();
    let server = registry::create_socket_in_namespace(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
        ns_b.clone(),
    )
    .expect("server socket");
    assert_eq!(
        step_bind(&server, inet_at(NS_B_IP, 40_101), &guard),
        StepOutcome::Done(())
    );

    let client = registry::create_socket_in_namespace(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
        ns_a.clone(),
    )
    .expect("client socket");
    assert_eq!(
        step_bind(&client, inet_at(NS_A_IP, 50_101), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet_at(NS_B_IP, 40_101), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );

    let adapter_a = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: pair.left.ops.mac_addr(),
        local_ipv4: NS_A_IP,
        mtu: pair.left.ops.mtu(),
    });
    let sink_a = SmoltcpPacketTxSink {
        adapter: &adapter_a,
        device: pair.left,
    };
    assert_eq!(pair.right_device.pending_rx(), 0);
    let StepOutcome::Done(tx) = step_process_device_tx_pending_in_namespace_at(
        &sink_a,
        ns_a.clone(),
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
    assert_eq!(pair.right_device.pending_rx(), 1);

    let adapter_b = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: pair.right.ops.mac_addr(),
        local_ipv4: NS_B_IP,
        mtu: pair.right.ops.mtu(),
    });
    let source_b = SmoltcpPacketSource {
        adapter: &adapter_b,
        device: pair.right,
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
        5
    );
    assert_eq!(
        step_recv(&server, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
}

fn new_test_veth_pair(left_name: &'static str, right_name: &'static str, minor: u32) -> VethPair {
    create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: left_name,
            devt: DevT::new(91, minor * 2),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 0, minor as u8]),
        },
        right: VethEndpointConfig {
            name: right_name,
            devt: DevT::new(91, minor * 2 + 1),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 1, minor as u8]),
        },
        mtu: VETH_DEFAULT_MTU,
    })
}

fn inet_at(addr: Ipv4Address, port: u16) -> KernelSockAddr {
    KernelSockAddr::V4(SockAddrIn::new(port, addr))
}

fn assert_link(links: &[NetNamespaceLinkInfo], name: &str, addr: Ipv4Address) {
    assert!(links.iter().any(|link| {
        link.name == name
            && link.ipv4_addr == Some(addr)
            && !link.is_loopback
            && link.is_up
            && link.mtu == VETH_DEFAULT_MTU
    }));
}
