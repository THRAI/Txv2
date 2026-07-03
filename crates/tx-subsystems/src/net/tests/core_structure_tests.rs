use super::*;

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
    let unix_stream = ValidSocketType::validate(1, 1, 0).expect("unix stream socket");
    let unix_seqpacket = ValidSocketType::validate(1, 5, 0).expect("unix seqpacket socket");
    let stream = ValidSocketType::validate(2, 1, 6).expect("tcp socket");
    let dgram = ValidSocketType::validate(2, 2, 17).expect("udp socket");
    let dgram_udplite = ValidSocketType::validate(2, 2, 136).expect("udplite socket");
    let dgram_icmp = ValidSocketType::validate(2, 2, 1).expect("ping socket");
    let raw_icmp = ValidSocketType::validate(2, 3, 1).expect("raw icmp socket");
    let xfrm = ValidSocketType::validate(16, 3, 6).expect("netlink xfrm socket");
    let nft = ValidSocketType::validate(16, 3, 12).expect("netlink netfilter socket");
    let packet = ValidSocketType::validate(17, 3, 0x0300).expect("packet socket");
    let default_stream = ValidSocketType::validate(2, 1, 0).expect("default tcp socket");

    assert_eq!(unix_dgram.domain, AddressFamily::Unix);
    assert_eq!(
        SocketKind::from_valid_socket_type(unix_dgram),
        Ok(SocketKind::UnixDatagram)
    );
    assert_eq!(
        SocketKind::from_valid_socket_type(unix_stream),
        Ok(SocketKind::UnixStream)
    );
    assert_eq!(unix_seqpacket.sock_type, SocketType::SeqPacket);
    assert_eq!(
        SocketKind::from_valid_socket_type(unix_seqpacket),
        Ok(SocketKind::UnixStream)
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
        SocketKind::from_valid_socket_type(dgram_udplite),
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
        SocketKind::from_valid_socket_type(nft),
        Ok(SocketKind::NetlinkNetfilter)
    );
    assert_eq!(
        SocketKind::from_valid_socket_type(xfrm),
        Ok(SocketKind::NetlinkXfrm)
    );
    assert_eq!(packet.domain, AddressFamily::Packet);
    assert_eq!(
        SocketKind::from_valid_socket_type(packet),
        Ok(SocketKind::Packet)
    );
    assert_eq!(
        SocketKind::from_valid_socket_type(default_stream),
        Ok(SocketKind::Tcp)
    );
    assert_eq!(
        ValidSocketType::validate(99, 1, 0),
        Err(Errno::EAFNOSUPPORT)
    );
    assert_eq!(ValidSocketType::validate(2, 999, 0), Err(Errno::EINVAL));
    let udp_stream = ValidSocketType::validate(2, 1, 17).expect("valid raw socket tuple");
    assert_eq!(
        SocketKind::from_valid_socket_type(udp_stream),
        Err(Errno::EPROTONOSUPPORT)
    );
    let raw_default = ValidSocketType::validate(2, 3, 0).expect("valid raw socket tuple");
    assert_eq!(
        SocketKind::from_valid_socket_type(raw_default),
        Err(Errno::EPROTONOSUPPORT)
    );
    let inet_seqpacket = ValidSocketType::validate(2, 5, 0).expect("inet seqpacket tuple");
    assert_eq!(
        SocketKind::from_valid_socket_type(inet_seqpacket),
        Err(Errno::EPROTONOSUPPORT)
    );
    let unix_options =
        SocketOptionSet::for_valid_socket_type(unix_seqpacket, SocketKind::UnixStream);
    assert_eq!(unix_options.socket.sock_type, SocketType::SeqPacket);
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
            bound_local6: None,
            protocol: ProtocolNumber(1),
            icmp6_filter: [0; 8],
        })
    );
}

#[test]
fn raw_icmpv6_bind_accepts_configured_nonloopback_ipv6() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("raw-icmpv6-bind")
        .expect("net namespace")
        .payload_cap()
        .expect("net namespace payload");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "eth-icmp6-bind",
            devt: DevT::new(96, 1),
            mac: EthernetAddress::new([0x02, 0, 0, 0x96, 0, 1]),
        },
        right: VethEndpointConfig {
            name: "veth-icmp6-bind",
            devt: DevT::new(96, 2),
            mac: EthernetAddress::new([0x02, 0, 0, 0x96, 0, 2]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    ns.attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach eth-icmp6-bind");
    let ifindex = ns
        .link_snapshot()
        .iter()
        .find(|link| link.name == "eth-icmp6-bind")
        .expect("eth-icmp6-bind link")
        .ifindex;
    let local = Ipv6Address::new([0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2]);
    ns.set_device_ipv6_addr_by_ifindex(auth, ifindex, Some(local), Some(64))
        .expect("set iface ipv6");
    let socket = registry::create_socket_in_namespace_with_family(
        SocketKind::RawIcmp,
        AddressFamily::Inet6,
        SocketOptionSet::for_kind(SocketKind::RawIcmp),
        ns,
    )
    .expect("raw icmpv6 socket");

    assert_eq!(
        step_bind(
            &socket,
            KernelSockAddr::V6(SockAddrIn6::new(0, local)),
            &guard
        ),
        StepOutcome::Done(())
    );
    assert_eq!(
        socket
            .acquire_operational()
            .expect("payload")
            .protocol_snapshot(),
        SocketProtocol::RawIcmp(RawIcmpState {
            bound_local: None,
            bound_local6: Some(local),
            protocol: ProtocolNumber(1),
            icmp6_filter: [0; 8],
        })
    );
}

#[test]
fn raw_icmp_wildcard_bind_accepts_ipv4_replies_to_local_addr() {
    let local = Ipv4Address::new([10, 0, 0, 2]);
    let other = Ipv4Address::new([10, 0, 0, 3]);

    assert!(RawIcmpState::new(ProtocolNumber(1)).accepts_ipv4_reply_to(local));

    let wildcard = RawIcmpState {
        bound_local: Some(Ipv4Address::UNSPECIFIED),
        bound_local6: None,
        protocol: ProtocolNumber(1),
        icmp6_filter: [0; 8],
    };
    assert!(wildcard.accepts_ipv4_reply_to(local));
    assert!(wildcard.accepts_ipv4_reply_to(other));

    let bound = RawIcmpState {
        bound_local: Some(local),
        bound_local6: None,
        protocol: ProtocolNumber(1),
        icmp6_filter: [0; 8],
    };
    assert!(bound.accepts_ipv4_reply_to(local));
    assert!(!bound.accepts_ipv4_reply_to(other));
}

#[test]
fn send_recv_flags_validate_mask() {
    let flags = SendRecvFlags::validate(
        (SendRecvFlags::MSG_DONTWAIT
            | SendRecvFlags::MSG_PEEK
            | SendRecvFlags::MSG_CONFIRM
            | SendRecvFlags::MSG_ERRQUEUE)
            .bits(),
    )
    .expect("known flags");

    assert!(flags.is_nonblocking());
    assert!(flags.contains(SendRecvFlags::MSG_PEEK));
    assert!(flags.contains(SendRecvFlags::MSG_CONFIRM));
    assert!(flags.contains(SendRecvFlags::MSG_ERRQUEUE));
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
    assert_eq!(options.ip.ipv4_multicast_if, Ipv4Address::UNSPECIFIED);
    assert_eq!(options.tcp.maxseg, 0);
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
        SocketProtocol::UnixDatagram(_)
        | SocketProtocol::UnixStream(_)
        | SocketProtocol::Udp(_)
        | SocketProtocol::Sctp(_)
        | SocketProtocol::Rds(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => panic!("tcp socket has wrong protocol"),
    }
    match udp_payload.protocol_snapshot() {
        SocketProtocol::Udp(inner) => assert_eq!(inner, UdpInner::Unbound),
        SocketProtocol::UnixDatagram(_)
        | SocketProtocol::UnixStream(_)
        | SocketProtocol::Tcp(_)
        | SocketProtocol::Sctp(_)
        | SocketProtocol::Rds(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => panic!("udp socket has wrong protocol"),
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
fn raw_udp_socket_preserves_option_capacity_with_compact_backing_buffers() {
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
fn raw_udp_socket_close_releases_corked_and_queued_payloads() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 8192;

    let raw = RawUdpSocket::new(&options);
    let dst = IpEndpoint::new(Ipv4Address::LOOPBACK, 12345);
    // P2-S6: the smoltcp ring is the queue; `send` needs a bound socket.
    assert!(raw.bind_endpoint(IpEndpoint::new(Ipv4Address::LOOPBACK, 40_242)));

    assert_eq!(
        raw.enqueue_tx_datagram_with_more(dst, alloc::vec![0xAA; 4000], true),
        Some((4000, false))
    );
    assert_eq!(
        raw.enqueue_tx_datagram_with_more(dst, alloc::vec![0xBB; 1], false),
        Some((1, false))
    );
    assert_eq!(raw.send_available(), 8192 - 4001);

    raw.close();

    assert_eq!(raw.send_available(), 8192);
    assert!(raw.pop_tx_datagram().is_none());
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

/// P3-S1 (R4a) decisive test: socket readiness carriers must be visible
/// to the SUBSTRATE wait-source registry (the one epoll's
/// `await_wait_source` looks up) and `fire_*` must wake a substrate
/// subscriber. Before S1, `lookup_source` returned None for socket
/// carriers, so `epoll_wait` on a pure-socket set returned 0 immediately
/// instead of blocking (registry mismatch, audit R4a).
#[test]
fn socket_readiness_carriers_visible_to_substrate_registry() {
    init_zones();
    let socket = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");

    use crate::adapter::step_engine::wake;

    for (name, id) in [
        ("recv", socket.wait_carriers.recv),
        ("send", socket.wait_carriers.send),
        ("accept", socket.wait_carriers.accept),
    ] {
        assert!(
            wake::lookup_source(tx_substrate::step::WaitSourceId::new(id)).is_some(),
            "socket {name} carrier must resolve in the substrate registry (R4a)"
        );
    }

    let source = wake::lookup_source(tx_substrate::step::WaitSourceId::new(
        socket.wait_carriers.recv,
    ))
    .expect("recv carrier");
    let mailbox = alloc::sync::Arc::new(wake::mailbox::TaskMailbox::new());
    let generation = mailbox.next_generation();
    let _sub = source.register(
        alloc::sync::Arc::downgrade(&mailbox),
        generation,
        tx_substrate::step::InterestMask::new(RecvWireSet::HAS_DATA.bits()),
    );
    assert!(mailbox.poll().is_none(), "no event before fire");
    socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
    assert!(
        mailbox.poll().is_some(),
        "fire_recv must notify the substrate mirror (epoll wake path)"
    );
}
