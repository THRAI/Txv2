use super::*;

use crate::net::protocol::{
    build_icmpv4_echo_reply, build_icmpv4_echo_request, build_icmpv4_echo_request_message,
    build_icmpv6_echo_request_message, icmpv4_echo_message_len,
    parse_icmpv4_echo_payload_unchecked, parse_icmpv4_from_ipv4_bytes,
    parse_icmpv4_loopback_packet, parse_icmpv4_payload, parse_icmpv6_payload_unchecked,
    parse_raw_icmpv4_echo_payload_unchecked, Icmpv4EchoPacket, Icmpv4Event, Icmpv6EchoPacket,
    Icmpv6Event, RawIcmpSocket,
};

#[test]
fn icmpv4_parse_echo_request_and_build_reply() {
    let request = Icmpv4EchoPacket {
        src: Ipv4Address::new([127, 0, 0, 2]),
        dst: Ipv4Address::LOOPBACK,
        ident: 0x1234,
        seq_no: 7,
        payload: b"ping".to_vec(),
    };

    let packet = build_icmpv4_echo_request(&request);
    assert_eq!(
        parse_icmpv4_loopback_packet(&packet),
        Icmpv4Event::EchoRequest(request.clone())
    );

    let reply = request.reply_packet();
    let packet = build_icmpv4_echo_reply(&reply);
    assert_eq!(
        parse_icmpv4_loopback_packet(&packet),
        Icmpv4Event::EchoReply(reply)
    );
}

#[test]
fn icmpv4_unchecked_echo_parser_accepts_kernel_checksum_payload() {
    let mut payload = std::vec![0u8; 16];
    payload[0] = 8;
    payload[4..6].copy_from_slice(&0x5151u16.to_be_bytes());
    payload[6..8].copy_from_slice(&7u16.to_be_bytes());
    payload[8..].fill(0xaa);

    assert_eq!(
        parse_icmpv4_echo_payload_unchecked(
            Ipv4Address::new([10, 0, 0, 2]),
            Ipv4Address::new([10, 0, 0, 1]),
            &payload,
        ),
        Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src: Ipv4Address::new([10, 0, 0, 2]),
            dst: Ipv4Address::new([10, 0, 0, 1]),
            ident: 0x5151,
            seq_no: 7,
            payload: std::vec![0xaa; 8],
        })
    );
}

#[test]
fn icmpv4_parser_accepts_busybox_pattern_echo_payload() {
    let src = Ipv4Address::new([10, 0, 0, 2]);
    let dst = Ipv4Address::new([10, 0, 0, 1]);
    let mut payload = std::vec![0xaa; 16];
    payload[0] = 8;
    payload[1] = 0;
    payload[2..4].copy_from_slice(&0u16.to_be_bytes());
    payload[4..6].copy_from_slice(&0x5151u16.to_be_bytes());
    payload[6..8].copy_from_slice(&0u16.to_be_bytes());
    payload[8..12].copy_from_slice(&0x1234_5678u32.to_ne_bytes());
    let checksum = internet_checksum(&payload);
    payload[2..4].copy_from_slice(&checksum.to_be_bytes());

    assert_eq!(
        parse_icmpv4_payload(src, dst, &payload),
        Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src,
            dst,
            ident: 0x5151,
            seq_no: 0,
            payload: payload[8..].to_vec(),
        })
    );
}

#[test]
fn raw_icmp_send_accepts_busybox_pattern_echo_code() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();
    let socket = match step_socket_create(
        ValidSocketType::validate(2, 3, 1).expect("raw icmp socket"),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("unexpected socket create outcome: {other:?}"),
    };
    let src = Ipv4Address::LOOPBACK;
    let dst = Ipv4Address::new([10, 0, 0, 1]);
    let mut payload = std::vec![0xaa; 16];
    payload[0] = 8;
    payload[2..4].copy_from_slice(&0u16.to_be_bytes());
    payload[4..6].copy_from_slice(&0x5151u16.to_be_bytes());
    payload[6..8].copy_from_slice(&0u16.to_be_bytes());
    payload[8..12].copy_from_slice(&0x1234_5678u32.to_ne_bytes());
    let checksum = internet_checksum(&payload);
    payload[2..4].copy_from_slice(&checksum.to_be_bytes());

    assert_eq!(
        parse_icmpv4_payload(src, dst, &payload),
        Icmpv4Event::Malformed
    );
    assert_eq!(
        parse_raw_icmpv4_echo_payload_unchecked(src, dst, &payload),
        Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src,
            dst,
            ident: 0x5151,
            seq_no: 0,
            payload: payload[8..].to_vec(),
        })
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &socket,
            Some(IpEndpoint::new(dst, 0)),
            &payload,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(payload.len())
    );
}

#[test]
fn raw_icmp_ipv4_recv_returns_ip_header_for_raw_socket() {
    let socket = RawIcmpSocket::new(&SocketOptionSet::for_kind(SocketKind::RawIcmp));
    let reply = Icmpv4EchoPacket {
        src: Ipv4Address::new([10, 0, 0, 1]),
        dst: Ipv4Address::new([10, 0, 0, 2]),
        ident: 0x5151,
        seq_no: 1,
        payload: b"trace".to_vec(),
    };
    let expected_len = 20 + icmpv4_echo_message_len(&reply);

    assert!(socket.ingest_rx_echo_reply(reply.clone()));
    assert_eq!(
        socket.recv_len(usize::MAX, true),
        Some((expected_len, false))
    );

    let mut out = std::vec![0u8; expected_len];
    let drain = socket.recv_bytes(&mut out, false).expect("raw reply");
    assert_eq!(drain.bytes, expected_len);
    assert_eq!(out[0] >> 4, 4);
    assert_eq!(out[0] & 0x0f, 5);
    assert_eq!(out[8], 64);
    assert_eq!(
        parse_icmpv4_from_ipv4_bytes(&out[..drain.bytes]),
        Icmpv4Event::EchoReply(reply)
    );
}

#[test]
fn raw_icmpv4_send_to_configured_peer_route_returns_echo_reply() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();
    let local_ip = Ipv4Address::new([10, 0, 0, 2]);
    let remote_ip = Ipv4Address::new([10, 23, 1, 1]);
    let local_ns = crate::net::create_isolated_net_namespace_for_test("icmpv4-route-local")
        .expect("local namespace")
        .payload_cap()
        .expect("local payload");
    let remote_ns = crate::net::create_isolated_net_namespace_for_test("icmpv4-route-remote")
        .expect("remote namespace")
        .payload_cap()
        .expect("remote payload");
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "icmpv4-route-local0",
            devt: DevT::new(91, 184),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 4, 1]),
        },
        right: VethEndpointConfig {
            name: "icmpv4-route-remote0",
            devt: DevT::new(91, 185),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 4, 2]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    local_ns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach local veth");
    remote_ns
        .attach_device_for_test_or_bootstrap(pair.right, None)
        .expect("attach remote veth");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    local_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(local_ip), Some(24))
        .expect("set local ipv4");
    remote_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(remote_ip), Some(24))
        .expect("set remote ipv4");
    local_ns
        .add_ipv4_route(
            auth,
            crate::net::NetNamespaceRouteConfig {
                dst: Ipv4Address::new([10, 23, 1, 0]),
                prefix_len: 24,
                gateway: None,
                oif_name: Some("icmpv4-route-local0"),
                preferred_src: None,
                table: 254,
                protocol: 4,
                scope: 253,
                route_type: 1,
            },
        )
        .expect("add route to remote alias network");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(2, 3, 1).expect("AF_INET SOCK_RAW ICMP"),
        local_ns,
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmp socket create failed: {other:?}"),
    };
    let request = Icmpv4EchoPacket {
        src: local_ip,
        dst: remote_ip,
        ident: 0x5151,
        seq_no: 7,
        payload: b"icmpv4-route".to_vec(),
    };
    let request_bytes = build_icmpv4_echo_request_message(&request);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(remote_ip, 0)),
            &request_bytes,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(request_bytes.len())
    );

    let mut out = [0u8; 128];
    let recv = match step_recv_kernel_bytes(&raw, &mut out, SendRecvFlags::empty(), &guard) {
        StepOutcome::Done(recv) => recv,
        other => panic!("expected icmpv4 echo reply, got {other:?}"),
    };
    assert_eq!(
        parse_icmpv4_from_ipv4_bytes(&out[..recv.bytes]),
        Icmpv4Event::EchoReply(request.reply_packet())
    );
}

/// LTP `net_stress.interface/if4-addr-change` shape: the local address
/// churns N times (busybox `ifconfig` → SIOCSIFADDR), then the final
/// connectivity ping must still round-trip from the *new* address.
#[test]
fn raw_icmpv4_echo_still_replies_after_local_addr_change_churn() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();
    let local_ip = Ipv4Address::new([10, 0, 0, 2]);
    let remote_ip = Ipv4Address::new([10, 0, 0, 1]);
    let local_ns = crate::net::create_isolated_net_namespace_for_test("icmpv4-churn-local")
        .expect("local namespace")
        .payload_cap()
        .expect("local payload");
    let remote_ns = crate::net::create_isolated_net_namespace_for_test("icmpv4-churn-remote")
        .expect("remote namespace")
        .payload_cap()
        .expect("remote payload");
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "icmpv4-churn-local0",
            devt: DevT::new(91, 186),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 4, 3]),
        },
        right: VethEndpointConfig {
            name: "icmpv4-churn-remote0",
            devt: DevT::new(91, 187),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 4, 4]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    local_ns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach local veth");
    remote_ns
        .attach_device_for_test_or_bootstrap(pair.right, None)
        .expect("attach remote veth");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    local_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(local_ip), Some(24))
        .expect("set local ipv4");
    remote_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(remote_ip), Some(24))
        .expect("set remote ipv4");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(2, 3, 1).expect("AF_INET SOCK_RAW ICMP"),
        local_ns.clone(),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmp socket create failed: {other:?}"),
    };

    // Baseline: echo round-trips with the initial address.
    let request = Icmpv4EchoPacket {
        src: local_ip,
        dst: remote_ip,
        ident: 0x6161,
        seq_no: 1,
        payload: b"pre-churn".to_vec(),
    };
    let request_bytes = build_icmpv4_echo_request_message(&request);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(remote_ip, 0)),
            &request_bytes,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(request_bytes.len())
    );
    let mut out = [0u8; 128];
    let recv = match step_recv_kernel_bytes(&raw, &mut out, SendRecvFlags::empty(), &guard) {
        StepOutcome::Done(recv) => recv,
        other => panic!("expected pre-churn echo reply, got {other:?}"),
    };
    assert_eq!(
        parse_icmpv4_from_ipv4_bytes(&out[..recv.bytes]),
        Icmpv4Event::EchoReply(request.reply_packet())
    );

    // Churn the local address like if4-addr-change's 10 ifconfig loops
    // (10.0.0.2 → 10.0.0.3 → … → 10.0.0.11).
    let mut churned = local_ip;
    for host in 2..=11u8 {
        churned = Ipv4Address::new([10, 0, 0, host]);
        local_ns
            .set_device_ipv4_addr_by_ifindex(auth, 2, Some(churned), Some(24))
            .expect("churn local ipv4");
    }

    // Final connectivity check from the new address.
    let request = Icmpv4EchoPacket {
        src: churned,
        dst: remote_ip,
        ident: 0x6161,
        seq_no: 2,
        payload: b"post-churn".to_vec(),
    };
    let request_bytes = build_icmpv4_echo_request_message(&request);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new(remote_ip, 0)),
            &request_bytes,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(request_bytes.len())
    );
    let mut out = [0u8; 128];
    let recv = match step_recv_kernel_bytes(&raw, &mut out, SendRecvFlags::empty(), &guard) {
        StepOutcome::Done(recv) => recv,
        other => panic!("expected post-churn echo reply, got {other:?}"),
    };
    assert_eq!(
        parse_icmpv4_from_ipv4_bytes(&out[..recv.bytes]),
        Icmpv4Event::EchoReply(request.reply_packet())
    );
}

/// LTP `net_stress.interface/if-addr-addlarge_ifconfig` shape: 40 labeled
/// add → del-by-label rounds (the witnessed guest failure was round 32's
/// `ifconfig eth0:1:32 down` leaving the address behind).
#[test]
fn secondary_ipv4_label_add_del_loop_40_rounds() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let primary = Ipv4Address::new([10, 0, 0, 2]);
    let ns = crate::net::create_isolated_net_namespace_for_test("ipv4-label-loop")
        .expect("ns")
        .payload_cap()
        .expect("payload");
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "ipv4-label-loop0",
            devt: DevT::new(91, 200),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 7, 0]),
        },
        right: VethEndpointConfig {
            name: "ipv4-label-loop1",
            devt: DevT::new(91, 201),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 7, 1]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    ns.attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    ns.set_device_ipv4_addr_by_ifindex(auth, 2, Some(primary), Some(24))
        .expect("primary");
    for y in 1..=40u8 {
        let addr = Ipv4Address::new([10, 23, 1, y]);
        let label = alloc::format!("ipv4-label-loop0:1:{y}");
        ns.add_device_ipv4_addr_by_ifindex(auth, 2, addr, 16, Some(&label))
            .unwrap_or_else(|e| panic!("add y={y}: {e:?}"));
        assert!(ns.ipv4_addr_is_local_up(addr), "y={y} not local after add");
        assert_eq!(
            ns.ipv4_extra_by_label(2, &label),
            Some((addr, 16)),
            "y={y} label lookup"
        );
        let removed = ns
            .del_device_ipv4_addr_by_label(auth, 2, &label)
            .unwrap_or_else(|e| panic!("del y={y}: {e:?}"));
        assert!(removed, "y={y} label del returned false");
        assert!(
            ns.ipv4_extra_snapshot().is_empty(),
            "y={y} extras not empty after del"
        );
    }
    let link = ns
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == 2)
        .expect("link");
    assert_eq!(link.ipv4_addr, Some(primary));
}

/// LTP `net_stress.interface/if-addr-adddel` shape: a secondary address (with
/// an `eth0:1`-style label) is added next to the primary, must be visible and
/// locally owned, and its removal must leave the primary configured.
#[test]
fn secondary_ipv4_addr_add_del_keeps_primary() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let primary = Ipv4Address::new([10, 0, 0, 2]);
    let secondary = Ipv4Address::new([172, 16, 1, 57]);
    let local_ns = crate::net::create_isolated_net_namespace_for_test("ipv4-secondary-local")
        .expect("local namespace")
        .payload_cap()
        .expect("local payload");
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "ipv4-secondary0",
            devt: DevT::new(91, 190),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 5, 0]),
        },
        right: VethEndpointConfig {
            name: "ipv4-secondary1",
            devt: DevT::new(91, 191),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 5, 1]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    local_ns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach local veth");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    local_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(primary), Some(24))
        .expect("set primary ipv4");

    // Add: primary untouched, secondary owned + dumped + label-addressable.
    local_ns
        .add_device_ipv4_addr_by_ifindex(auth, 2, secondary, 24, Some("ipv4-secondary0:1"))
        .expect("add secondary ipv4");
    let link = local_ns
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == 2)
        .expect("local link");
    assert_eq!(link.ipv4_addr, Some(primary));
    assert!(local_ns.ipv4_addr_is_local_up(secondary));
    assert!(local_ns.ipv4_addr_is_local_up(primary));
    let extras = local_ns.ipv4_extra_snapshot();
    assert_eq!(extras.len(), 1);
    assert_eq!(extras[0].ifindex, 2);
    assert_eq!(extras[0].addr, secondary);
    assert_eq!(extras[0].prefix_len, 24);
    assert_eq!(extras[0].label.as_deref(), Some("ipv4-secondary0:1"));
    assert_eq!(
        local_ns.ipv4_extra_by_label(2, "ipv4-secondary0:1"),
        Some((secondary, 24))
    );

    // Delete by address: secondary gone, primary still configured.
    assert_eq!(
        local_ns.del_device_ipv4_addr_by_ifindex(auth, 2, secondary),
        Ok(true)
    );
    assert!(local_ns.ipv4_extra_snapshot().is_empty());
    assert!(!local_ns.ipv4_addr_is_local_up(secondary));
    let link = local_ns
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == 2)
        .expect("local link after del");
    assert_eq!(link.ipv4_addr, Some(primary));

    // Delete by label (`ifconfig eth0:1 down` shape).
    local_ns
        .add_device_ipv4_addr_by_ifindex(auth, 2, secondary, 16, Some("ipv4-secondary0:1"))
        .expect("re-add secondary ipv4");
    assert_eq!(
        local_ns.del_device_ipv4_addr_by_label(auth, 2, "ipv4-secondary0:1"),
        Ok(true)
    );
    assert!(local_ns.ipv4_extra_snapshot().is_empty());
    assert_eq!(
        local_ns.del_device_ipv4_addr_by_label(auth, 2, "ipv4-secondary0:1"),
        Ok(false)
    );
}

#[test]
fn raw_icmpv6_send_to_configured_peer_addr_returns_echo_reply() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();
    let local_ip = Ipv6Address::new([0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2]);
    let remote_ip = Ipv6Address::new([0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
    let local_ns = crate::net::create_isolated_net_namespace_for_test("icmpv6-local")
        .expect("local namespace")
        .payload_cap()
        .expect("local payload");
    let remote_ns = crate::net::create_isolated_net_namespace_for_test("icmpv6-remote")
        .expect("remote namespace")
        .payload_cap()
        .expect("remote payload");
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "icmpv6-local0",
            devt: DevT::new(91, 180),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 6, 0]),
        },
        right: VethEndpointConfig {
            name: "icmpv6-remote0",
            devt: DevT::new(91, 181),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 6, 1]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    local_ns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach local veth");
    remote_ns
        .attach_device_for_test_or_bootstrap(pair.right, None)
        .expect("attach remote veth");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    local_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(Ipv4Address::new([10, 0, 0, 2])), Some(24))
        .expect("set local ipv4");
    remote_ns
        .set_device_ipv4_addr_by_ifindex(auth, 2, Some(Ipv4Address::new([10, 0, 0, 1])), Some(24))
        .expect("set remote ipv4");
    local_ns
        .set_device_ipv6_addr_by_ifindex(auth, 2, Some(local_ip), Some(64))
        .expect("set local ipv6");
    remote_ns
        .set_device_ipv6_addr_by_ifindex(auth, 2, Some(remote_ip), Some(64))
        .expect("set remote ipv6");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(10, 3, 58).expect("AF_INET6 SOCK_RAW ICMPV6"),
        local_ns.clone(),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmpv6 socket create failed: {other:?}"),
    };
    let raw_payload = raw.acquire_operational().expect("raw payload");
    let mut filter = [u32::MAX; 8];
    filter[129 / 32] &= !(1u32 << (129 % 32));
    raw_payload
        .set_raw_icmp6_filter(filter)
        .expect("set echo-reply-only filter");
    let request = Icmpv6EchoPacket {
        src: local_ip,
        dst: remote_ip,
        ident: 0x6060,
        seq_no: 7,
        payload: b"icmpv6-peer".to_vec(),
    };
    let request_bytes = build_icmpv6_echo_request_message(&request);
    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new_v6(remote_ip, 0)),
            &request_bytes,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(request_bytes.len())
    );
    let neigh = crate::net::proc_net_neigh_snapshot_text_for_namespace(&local_ns);
    assert!(
        neigh.contains("fd00:1:1:1::1 dev icmpv6-local0 lladdr 02:00:00:00:06:01 REACHABLE"),
        "raw ICMPv6 echo should learn NDISC neighbor: {neigh}"
    );

    let mut out = [0u8; 128];
    let recv = match step_recv_kernel_bytes(&raw, &mut out, SendRecvFlags::empty(), &guard) {
        StepOutcome::Done(recv) => recv,
        other => panic!("expected icmpv6 echo reply, got {other:?}"),
    };
    assert_eq!(
        parse_icmpv6_payload_unchecked(local_ip, remote_ip, &request_bytes),
        Icmpv6Event::EchoRequest(request.clone())
    );
    assert_eq!(
        parse_icmpv6_payload_unchecked(remote_ip, local_ip, &out[..recv.bytes]),
        Icmpv6Event::EchoReply(request.reply_packet())
    );
}

#[test]
fn icmpv6_unchecked_parser_accepts_busybox_pattern_echo_code() {
    let src = Ipv6Address::new([0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2]);
    let dst = Ipv6Address::new([0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut payload = std::vec![0xaau8; 16];
    payload[0] = 128;
    payload[4..6].copy_from_slice(&0x6060u16.to_be_bytes());
    payload[6..8].copy_from_slice(&7u16.to_be_bytes());

    assert_eq!(
        parse_icmpv6_payload_unchecked(src, dst, &payload),
        Icmpv6Event::EchoRequest(Icmpv6EchoPacket {
            src,
            dst,
            ident: 0x6060,
            seq_no: 7,
            payload: std::vec![0xaa; 8],
        })
    );
}

#[test]
fn raw_icmpv6_unknown_peer_addr_stays_unsupported() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();
    let local_ip = Ipv6Address::new([0xfd, 0, 0, 2, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2]);
    let unknown_ip = Ipv6Address::new([0xfd, 0, 0, 2, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 99]);
    let local_ns = crate::net::create_isolated_net_namespace_for_test("icmpv6-unknown-local")
        .expect("local namespace")
        .payload_cap()
        .expect("local payload");
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "icmpv6-unknown0",
            devt: DevT::new(91, 182),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 6, 2]),
        },
        right: VethEndpointConfig {
            name: "icmpv6-unused0",
            devt: DevT::new(91, 183),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 6, 3]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    local_ns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach local veth");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    local_ns
        .set_device_ipv6_addr_by_ifindex(auth, 2, Some(local_ip), Some(64))
        .expect("set local ipv6");

    let raw = match crate::net::step_socket_create_in_namespace(
        ValidSocketType::validate(10, 3, 58).expect("AF_INET6 SOCK_RAW ICMPV6"),
        local_ns,
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("raw icmpv6 socket create failed: {other:?}"),
    };
    let request = Icmpv6EchoPacket {
        src: local_ip,
        dst: unknown_ip,
        ident: 0x6061,
        seq_no: 1,
        payload: b"unknown".to_vec(),
    };
    let request_bytes = build_icmpv6_echo_request_message(&request);

    assert_eq!(
        step_send_to_kernel_bytes(
            &raw,
            Some(IpEndpoint::new_v6(unknown_ip, 0)),
            &request_bytes,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Err(Errno::EOPNOTSUPP)
    );
}

#[test]
fn loopback_iface_echo_request_becomes_echo_reply() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let iface = loopback_iface();
    iface.clear_for_test_or_bootstrap();

    let request = Icmpv4EchoPacket {
        src: Ipv4Address::new([127, 0, 0, 2]),
        dst: Ipv4Address::LOOPBACK,
        ident: 0x4321,
        seq_no: 9,
        payload: b"loop".to_vec(),
    };
    assert!(iface.dispatch_ip(build_icmpv4_echo_request(&request)));
    assert_eq!(iface.pending_packets(), 1);

    assert_eq!(
        iface.process_icmpv4_echo_once(),
        Some(Icmpv4Event::EchoRequest(request.clone()))
    );
    assert_eq!(iface.pending_packets(), 1);

    let reply = iface.pop_ingress().expect("echo reply");
    assert_eq!(
        parse_icmpv4_loopback_packet(&reply),
        Icmpv4Event::EchoReply(request.reply_packet())
    );
}

fn internet_checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]]) as u32
        } else {
            (chunk[0] as u32) << 8
        };
        sum += word;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
