use super::*;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    loopback_iface().clear_for_test_or_bootstrap();
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    lock
}

#[test]
fn smoltcp_tcp_segment_emits_and_parses_ipv4_packet() {
    let _lock = setup();
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_165, 50_165);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));
    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );

    let segment = client
        .acquire_operational()
        .expect("client payload")
        .raw_tcp_socket()
        .expect("client raw tcp")
        .dispatch_segment()
        .expect("tcp segment");
    let packet = segment.emit_ipv4_packet().expect("ipv4 packet");
    let parsed = crate::net::protocol::SmoltcpTcpSegment::parse_ipv4_packet(&packet)
        .expect("parsed tcp segment");

    assert!(!packet.is_empty());
    assert_eq!(parsed.src_endpoint(), segment.src_endpoint());
    assert_eq!(parsed.dst_endpoint(), segment.dst_endpoint());
    assert_eq!(parsed.tcp.control, segment.tcp.control);
    assert_eq!(parsed.payload, segment.payload);
}

#[test]
fn loopback_iface_dispatch_ip_requeues_packet_for_ingress() {
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let packet = LoopbackIpPacket::new(std::vec![1, 2, 3, 4]);

    assert!(iface.dispatch_ip(packet.clone()));
    assert_eq!(iface.pending_packets(), 1);
    assert_eq!(iface.pop_ingress(), Some(packet));
    assert_eq!(iface.pending_packets(), 0);
}

#[test]
fn loopback_iface_singleton_has_loopback_common_fields() {
    let iface = loopback_iface();
    iface.clear_for_test_or_bootstrap();

    assert_eq!(iface.local_ipv4(), Ipv4Address::LOOPBACK);
    assert_eq!(iface.netmask(), Ipv4Address::new([255, 0, 0, 0]));
    assert_eq!(iface.mtu(), 65_535);
    assert_eq!(iface.pending_packets(), 0);
}

#[test]
fn udp_loopback_connected_send_reaches_bound_receiver() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, inet(40_186), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_186), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet(40_186), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );

    let transfer = match step_process_loopback_udp_on_iface(&client, 8, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected udp loopback outcome"),
    };

    assert_eq!(transfer.tx_packets, 1);
    assert_eq!(transfer.packets_seen, 1);
    assert_eq!(transfer.bytes_moved, 5);
    assert!(transfer.peer_wake_fired);
    assert_eq!(iface.pending_packets(), 0);
    assert!(server.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
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
    assert_eq!(
        server.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0
    );
}

#[test]
fn udp_loopback_sendto_reaches_wildcard_bound_receiver() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, any_inet(40_196), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_196), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &client,
            Some(endpoint(40_196)),
            b"x",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(1)
    );

    let transfer = match step_process_loopback_udp_on_iface(&client, 8, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected udp loopback outcome"),
    };

    assert_eq!(transfer.tx_packets, 1);
    assert_eq!(transfer.packets_seen, 1);
    assert_eq!(transfer.bytes_moved, 1);
    assert!(transfer.peer_wake_fired);
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        1
    );
}

#[test]
fn udp_loopback_wildcard_server_reply_reaches_connected_client() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, any_inet(40_197), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_197), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet(40_197), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"ping", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(4)
    );

    let client_to_server = match step_process_loopback_udp_on_iface(&client, 8, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected client udp loopback outcome"),
    };
    assert_eq!(client_to_server.bytes_moved, 4);

    let mut request = [0u8; 8];
    let request_recv =
        match step_recv_kernel_bytes(&server, &mut request, SendRecvFlags::empty(), &guard) {
            StepOutcome::Done(outcome) => outcome,
            _ => panic!("unexpected server recv outcome"),
        };
    assert_eq!(request_recv.bytes, 4);
    assert_eq!(&request[..4], b"ping");
    assert_eq!(request_recv.source, Some(endpoint(50_197)));
    assert_eq!(
        request_recv.destination,
        Some(IpEndpoint::new(Ipv4Address::LOOPBACK, 40_197))
    );

    assert_eq!(
        step_send_to_kernel_bytes(
            &server,
            request_recv.source,
            b"pong",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(4)
    );

    let server_to_client = match step_process_loopback_udp_on_iface(&server, 8, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected server udp loopback outcome"),
    };
    assert_eq!(server_to_client.bytes_moved, 4);

    let mut response = [0u8; 8];
    assert_eq!(
        step_recv_kernel_bytes(&client, &mut response, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: 4,
            source: Some(endpoint(40_197)),
            unix_source: None,
            packet_source: None,
            destination: Some(endpoint(50_197)),
            truncated: false,
            became_empty: true,
            eor: false,
        })
    );
    assert_eq!(&response[..4], b"pong");
}

#[test]
fn udp_loopback_netperf_rr_ephemeral_collision_shape() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, any_inet(49_152), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(49_152), &guard),
        StepOutcome::Err(Errno::EADDRINUSE)
    );
    assert_eq!(
        step_bind(&client, inet(49_153), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet(49_152), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_udp_loopback_kernel_bytes(
            &client,
            None,
            b"hello",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(5)
    );

    let mut request = [0u8; 8];
    let request_recv =
        match step_recv_kernel_bytes(&server, &mut request, SendRecvFlags::empty(), &guard) {
            StepOutcome::Done(outcome) => outcome,
            _ => panic!("unexpected server recv outcome"),
        };
    assert_eq!(request_recv.bytes, 5);
    assert_eq!(request_recv.source, Some(endpoint(49_153)));
    assert_eq!(
        request_recv.destination,
        Some(IpEndpoint::new(Ipv4Address::LOOPBACK, 49_152))
    );
    assert_eq!(&request[..5], b"hello");
}

#[test]
fn udp_loopback_inline_send_can_defer_delegate_poll_kick() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, any_inet(40_206), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_206), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_to_kernel_bytes_with_poll_kick(
            &client,
            Some(endpoint(40_206)),
            b"x",
            SendRecvFlags::empty(),
            &guard,
            false,
        ),
        StepOutcome::Done(1)
    );
    assert_eq!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits(),
        0
    );

    let transfer = match step_process_loopback_udp_on_iface(&client, 8, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected udp loopback outcome"),
    };
    assert_eq!(transfer.bytes_moved, 1);
    assert!(transfer.peer_wake_fired);
}

#[test]
fn udp_loopback_direct_send_kernel_bytes_reaches_receiver() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, any_inet(40_207), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_207), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_udp_loopback_kernel_bytes(
            &client,
            Some(endpoint(40_207)),
            b"x",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(1)
    );
    assert_eq!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits(),
        0
    );
    assert!(server.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        1
    );
}

#[test]
fn udp_loopback_msg_more_defers_until_uncork_send() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server udp");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client udp");

    assert_eq!(
        step_bind(&server, any_inet(40_208), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_208), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_udp_loopback_kernel_bytes(
            &client,
            Some(endpoint(40_208)),
            b"hello",
            SendRecvFlags::MSG_MORE,
            &guard,
        ),
        StepOutcome::Done(5)
    );
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        0
    );

    assert_eq!(
        step_send_udp_loopback_kernel_bytes(
            &client,
            Some(endpoint(40_208)),
            b"!",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(1)
    );
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        6
    );
    let mut out = [0u8; 8];
    assert_eq!(
        step_recv_kernel_bytes(&server, &mut out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: 6,
            source: Some(endpoint(50_208)),
            unix_source: None,
            packet_source: None,
            destination: Some(IpEndpoint::new(Ipv4Address::LOOPBACK, 40_208)),
            truncated: false,
            became_empty: true,
            eor: false,
        })
    );
    assert_eq!(&out[..6], b"hello!");
}
