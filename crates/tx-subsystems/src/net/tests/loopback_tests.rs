use super::*;
use crate::net::step_socket_close;
use crate::net::structure::SocketIdentity;
use tx_substrate::zone::Cap;

struct LoopbackDelegateDriver<'a> {
    now: smoltcp::time::Instant,
    source: &'a ScriptedPacketSource,
    iface: Option<&'a LoopbackIface>,
}

impl NetDelegateDriver for LoopbackDelegateDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        self.now
    }

    fn packet_source(&self) -> &dyn PacketSource {
        self.source
    }

    fn loopback_iface(&self) -> Option<&LoopbackIface> {
        self.iface
    }
}

#[test]
fn smoltcp_tcp_segment_emits_and_parses_ipv4_packet() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
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
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
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
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
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
fn udp_loopback_inline_send_can_defer_delegate_poll_kick() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
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
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
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
fn tcp_loopback_pollcontext_moves_data_through_packet_queue() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(40_166, 50_166, 5);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };
    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );

    let transfer = match step_process_loopback_tcp(&client, 64, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected pollcontext transfer outcome"),
    };

    assert_eq!(transfer.bytes_moved, 5);
    assert_eq!(iface.pending_packets(), 0);
    assert!(accepted.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert!(client.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted child payload")
            .io_snapshot()
            .recv_len,
        5
    );
}

#[test]
fn tcp_loopback_default_steps_use_persistent_loopback_iface() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(40_168, 50_168, 5);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };
    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );

    let transfer = match step_tcp_loopback_transfer(&client, 64, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected transfer outcome"),
    };

    assert_eq!(transfer.bytes_moved, 5);
    assert_eq!(loopback_iface().pending_packets(), 0);
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted child payload")
            .io_snapshot()
            .recv_len,
        5
    );
}

#[test]
fn tcp_loopback_accepted_recv_without_payload_waits() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_170, 50_170);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let mut out = [0u8; 37];
    assert!(matches!(
        step_recv_kernel_bytes(&accepted, &mut out, SendRecvFlags::empty(), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));
}

#[test]
fn tcp_loopback_iperf_like_control_exchange_moves_both_directions() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_171, 50_171);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let mut initial = [0u8; 37];
    assert!(matches!(
        step_recv_kernel_bytes(&accepted, &mut initial, SendRecvFlags::empty(), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    assert_tcp_payload_round_trip(
        "client parameters",
        &client,
        &accepted,
        b"client parameters",
        &guard,
    );
    assert_tcp_payload_round_trip(
        "server parameters",
        &accepted,
        &client,
        b"server parameters",
        &guard,
    );
    assert_tcp_payload_round_trip("client marker", &client, &accepted, b"!", &guard);
    assert_tcp_payload_round_trip("client length", &client, &accepted, &[1, 0, 0, 0], &guard);
    assert_tcp_payload_round_trip("server length", &accepted, &client, &[1, 0, 0, 0], &guard);
    assert_tcp_payload_round_trip("server payload", &accepted, &client, &[0x7b; 123], &guard);
    assert_tcp_payload_round_trip("client payload", &client, &accepted, &[0x5a; 123], &guard);
}

#[test]
fn tcp_loopback_handshake_creates_child_from_listener_first_syn() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, local, remote) = prepare_loopback_connect(40_169, 50_169);
    let guard = tx_substrate::epoch::guard();

    assert_eq!(
        listener
            .acquire_operational()
            .expect("listener payload")
            .io_snapshot()
            .accept_pending,
        0
    );

    let outcome = match step_tcp_loopback_handshake(&client, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback handshake outcome"),
    };
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("unexpected accept outcome"),
    };

    assert_eq!(accepted.child.raw(), outcome.child.raw());
    assert_eq!(accepted.local, remote);
    assert_eq!(accepted.peer, local);
    assert_eq!(outcome.handshake.tx_packets, 3);
    assert_eq!(outcome.handshake.packets_seen, 3);
    assert_eq!(loopback_iface().pending_packets(), 0);
    assert_eq!(
        accepted
            .child
            .acquire_operational()
            .expect("accepted child payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: remote,
            remote: local,
        })
    );
    assert_eq!(
        accepted
            .child
            .acquire_operational()
            .expect("accepted child payload")
            .raw_tcp_socket()
            .expect("accepted child raw tcp")
            .protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
}

#[test]
fn tcp_loopback_first_syn_uses_listener_connecting_backlog() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_170, 50_170);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    let client_raw = client_payload.raw_tcp_socket().expect("client raw tcp");
    client_raw
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    assert_eq!(syn.created_children.len(), 1);
    let child = syn.created_children[0].clone();

    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(listener_payload.accept_queue_len(), 0);
    assert_eq!(
        child
            .acquire_operational()
            .expect("child payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting {
            local: remote,
            remote: local,
        })
    );

    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    let syn_ack = ctx.poll_ingress_to_socket(&iface, &client, &guard, 1);
    assert!(syn_ack.created_children.is_empty());
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(listener_payload.accept_queue_len(), 0);

    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let ack = ctx.poll_ingress(&iface, &guard, 1);
    for publish in ack.publishes {
        publish.publish();
    }

    assert_eq!(listener_payload.connecting_backlog_len(), 0);
    assert_eq!(listener_payload.accept_queue_len(), 1);
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("unexpected accept outcome"),
    };
    assert_eq!(accepted.child.raw(), child.raw());
    assert_eq!(accepted.local, remote);
    assert_eq!(accepted.peer, local);
}

#[test]
fn tcp_backlog_next_deadline_tracks_connecting_child() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_172, 50_172);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let start = smoltcp::time::Instant::from_millis(7);
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(start);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);

    assert_eq!(syn.created_children.len(), 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(
        listener_payload.tcp_backlog_next_deadline(),
        Some(
            start + smoltcp::time::Duration::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS as u64)
        )
    );
}

#[test]
fn tcp_backlog_cleanup_removes_expired_half_open_child() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_173, 50_173);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    assert_eq!(syn.created_children.len(), 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(listener_payload.accept_queue_len(), 0);

    let cleanup = match step_tcp_backlog_cleanup(
        &listener,
        smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected backlog cleanup outcome"),
    };

    assert_eq!(cleanup.scanned, 1);
    assert_eq!(cleanup.expired, 1);
    assert_eq!(cleanup.failed, 0);
    assert_eq!(cleanup.remaining_connecting, 0);
    assert_eq!(listener_payload.connecting_backlog_len(), 0);
    assert_eq!(listener_payload.accept_queue_len(), 0);
    assert_eq!(listener_payload.tcp_backlog_next_deadline(), None);
}

#[test]
fn tcp_backlog_cleanup_removes_failed_half_open_child() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_174, 50_174);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    child
        .acquire_operational()
        .expect("child payload")
        .raw_tcp_socket()
        .expect("child raw tcp")
        .abort();

    let cleanup = match step_tcp_backlog_cleanup(&listener, smoltcp::time::Instant::ZERO, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected backlog cleanup outcome"),
    };

    assert_eq!(cleanup.scanned, 1);
    assert_eq!(cleanup.expired, 0);
    assert_eq!(cleanup.failed, 1);
    assert_eq!(cleanup.remaining_connecting, 0);
    assert_eq!(listener_payload.connecting_backlog_len(), 0);
    assert_eq!(listener_payload.accept_queue_len(), 0);
}

#[test]
fn tcp_backlog_cleanup_does_not_clear_connected_accept_queue() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_175, 50_175);
    let guard = tx_substrate::epoch::guard();
    let listener_payload = listener.acquire_operational().expect("listener payload");

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert_eq!(listener_payload.connecting_backlog_len(), 0);
    assert_eq!(listener_payload.accept_queue_len(), 1);

    let cleanup = match step_tcp_backlog_cleanup(
        &listener,
        smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected backlog cleanup outcome"),
    };

    assert_eq!(cleanup.scanned, 0);
    assert_eq!(cleanup.expired, 0);
    assert_eq!(cleanup.failed, 0);
    assert_eq!(cleanup.remaining_connecting, 0);
    assert_eq!(listener_payload.accept_queue_len(), 1);
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));
}

#[test]
fn network_tick_cleans_expired_tcp_backlog_without_direct_listener_cleanup() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_176, 50_176);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    assert_eq!(syn.created_children.len(), 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);

    let tick = match step_process_network_tick(
        smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected network tick outcome"),
    };

    assert!(tick.listeners_seen >= 1);
    assert!(tick.listeners_touched >= 1);
    assert!(tick.half_open_scanned >= 1);
    assert!(tick.half_open_expired >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 0);
    assert_eq!(listener_payload.accept_queue_len(), 0);
}

#[test]
fn network_tick_reports_next_tcp_backlog_deadline() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_177, 50_177);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let start = smoltcp::time::Instant::from_millis(-20_000);
    let expected_deadline =
        start + smoltcp::time::Duration::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS as u64);
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(start);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    assert_eq!(syn.created_children.len(), 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let events = match step_process_network_events_at(&source, smoltcp::time::Instant::ZERO, &guard)
    {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected network events outcome"),
    };

    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(
        listener_payload.tcp_backlog_next_deadline(),
        Some(expected_deadline)
    );
    assert_eq!(events.backlog.next_deadline, Some(expected_deadline));
    assert_eq!(events.backlog.half_open_expired, 0);
}

#[test]
fn network_tick_loopback_retransmits_syn_ack_before_cleanup_limit() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_178, 50_178);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());
    assert_eq!(iface.pending_packets(), 0);

    let now = smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1);
    let tick = match step_process_network_tick_loopback(now, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected network tick outcome"),
    };

    assert!(tick.half_open_retransmitted >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(
        listener_payload.tcp_backlog_next_deadline(),
        Some(
            now + smoltcp::time::Duration::from_millis(
                TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS as u64
            )
        )
    );
    let packet = iface.pop_ingress().expect("retransmitted syn-ack packet");
    let segment = crate::net::protocol::SmoltcpTcpSegment::parse_ipv4_packet(&packet)
        .expect("parsed retransmitted syn-ack");
    assert_eq!(segment.src_endpoint(), Some(remote));
    assert_eq!(segment.dst_endpoint(), Some(local));
    assert_eq!(segment.tcp.control, smoltcp::wire::TcpControl::Syn);
    assert!(segment.tcp.ack_number.is_some());
}

#[test]
fn network_tick_loopback_expires_backlog_after_retransmit_limit() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_179, 50_179);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());

    let first = smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1);
    assert!(matches!(
        step_process_network_tick_loopback(first, &iface, &guard),
        StepOutcome::Done(outcome) if outcome.half_open_retransmitted >= 1
    ));
    assert!(iface.pop_ingress().is_some());
    assert_eq!(listener_payload.connecting_backlog_len(), 1);

    let second =
        first + smoltcp::time::Duration::from_millis(TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS as u64);
    assert!(matches!(
        step_process_network_tick_loopback(second, &iface, &guard),
        StepOutcome::Done(outcome) if outcome.half_open_retransmitted >= 1
    ));
    assert!(iface.pop_ingress().is_some());
    assert_eq!(listener_payload.connecting_backlog_len(), 1);

    let third =
        second + smoltcp::time::Duration::from_millis(TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS as u64);
    let expired = match step_process_network_tick_loopback(third, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected network tick outcome"),
    };

    assert!(expired.half_open_expired >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 0);
}

#[test]
fn net_delegate_step_once_processes_poll_packet_source() {
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
    let local = endpoint(40_180);
    let remote = endpoint(50_180);
    assert_eq!(
        step_bind(
            &udp,
            KernelSockAddr::V4(SockAddrIn::new(local.port, local.addr)),
            &guard,
        ),
        StepOutcome::Done(())
    );
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Udp(
        UdpPacketEvent::with_payload_len(remote, local, 64),
    )]);
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: None,
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    crate::net::delegate::net_delegate_kick_poll();

    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert!(!outcome.tick_seen);
    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        udp.acquire_operational()
            .expect("udp payload")
            .io_snapshot()
            .recv_len,
        64
    );
}

#[test]
fn net_delegate_step_once_processes_tick_backlog_retransmit() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_181, 50_181);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1),
        source: &source,
        iface: Some(&iface),
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    crate::net::delegate::net_delegate_kick_tick();

    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(!outcome.poll_seen);
    assert!(outcome.tick_seen);
    assert!(outcome.backlog_retransmitted >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(iface.pending_packets(), 1);
}

#[test]
fn net_delegate_timer_adapter_converts_smoltcp_deadline_to_reactor_ns() {
    let base = smoltcp::time::Instant::from_millis(10);
    let deadline = smoltcp::time::Instant::from_millis(15);

    assert_eq!(
        smoltcp_instant_to_reactor_deadline_ns(base, deadline, 1_000),
        Some(5_001_000)
    );
    assert_eq!(
        smoltcp_instant_to_reactor_deadline_ns(deadline, base, 1_000),
        Some(1_000)
    );
}

#[test]
fn net_delegate_reactor_timer_adapter_fires_tick_and_drives_retransmit() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_182, 50_182);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());
    let next_deadline = listener_payload
        .tcp_backlog_next_deadline()
        .expect("backlog deadline");
    let deadline_ns =
        smoltcp_instant_to_reactor_deadline_ns(smoltcp::time::Instant::ZERO, next_deadline, 0)
            .expect("reactor deadline");

    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mut reactor = Reactor::new();
    let timer_channel = reactor.channel();
    reactor.submit(async move {
        assert_eq!(
            net_delegate_wait_tick_deadline(timer_channel, deadline_ns).await,
            WaitOutcome::TimedOut
        );
    });
    let mut programmed = None;
    let armed = reactor.run_until_idle_with_clock(|| 0, |deadline| programmed = deadline);
    assert_eq!(armed.next_deadline_ns(), Some(deadline_ns));
    assert_eq!(programmed, Some(deadline_ns));
    assert_eq!(crate::net::delegate::net_delegate_queue().peek(), 0);

    let fired = reactor.run_until_idle_with_clock(|| deadline_ns, |_| {});
    assert_eq!(fired.timer_wakes(), 1);
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::TICK.bits()
            != 0
    );

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = LoopbackDelegateDriver {
        now: next_deadline,
        source: &source,
        iface: Some(&iface),
    };
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.tick_seen);
    assert!(outcome.backlog_retransmitted >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(iface.pending_packets(), 1);
}

#[test]
fn net_delegate_task_loop_waits_for_poll_and_processes_bounded_steps() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let guard = tx_substrate::epoch::guard();
    let local = endpoint(40_183);
    let first_remote = endpoint(50_183);
    assert_eq!(
        step_bind(
            &udp,
            KernelSockAddr::V4(SockAddrIn::new(local.port, local.addr)),
            &guard,
        ),
        StepOutcome::Done(())
    );
    drop(guard);

    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Udp(
        UdpPacketEvent::with_payload_len(first_remote, local, 64),
    )]);
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: None,
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mut task = std::boxed::Box::pin(net_delegate_task_loop(
        &driver,
        NetDelegateTaskConfig::run_steps(1),
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
    crate::net::delegate::net_delegate_kick_poll();
    let report = match task.as_mut().poll(&mut cx) {
        Poll::Ready(report) => report,
        Poll::Pending => panic!("delegate task should finish after one ready step"),
    };

    assert_eq!(report.ready_steps, 1);
    assert_eq!(report.waits_ready, 1);
    assert_eq!(report.waits_failed, 0);
    assert!(report.runtime.poll_seen);
    assert_eq!(report.runtime.packets_seen, 1);
    assert_eq!(
        udp.acquire_operational()
            .expect("udp payload")
            .io_snapshot()
            .recv_len,
        64
    );
}

#[test]
fn net_delegate_task_loop_reports_deadline_refresh_from_tick() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_184, 50_185);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());
    drop(guard);

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let now = smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1);
    let driver = LoopbackDelegateDriver {
        now,
        source: &source,
        iface: Some(&iface),
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mut observed_deadline = None;
    let mut task = std::boxed::Box::pin(net_delegate_task_loop_with_deadline_hook(
        &driver,
        NetDelegateTaskConfig::run_steps(1),
        |deadline| observed_deadline = deadline,
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
    crate::net::delegate::net_delegate_kick_tick();
    let report = match task.as_mut().poll(&mut cx) {
        Poll::Ready(report) => report,
        Poll::Pending => panic!("delegate task should finish after one tick"),
    };

    let expected_deadline =
        now + smoltcp::time::Duration::from_millis(TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS as u64);
    drop(task);
    assert!(report.runtime.tick_seen);
    assert!(report.runtime.backlog_retransmitted >= 1);
    assert!(report.last_deadline.is_some());
    assert_eq!(observed_deadline, report.last_deadline);
    assert_eq!(
        listener_payload.tcp_backlog_next_deadline(),
        Some(expected_deadline)
    );
}

#[test]
fn tcp_loopback_cleanup_withdraws_connection_table_entries() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_171, 50_171);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let client_key = ConnectionKey::new(local, remote);
    let server_key = ConnectionKey::new(remote, local);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_some());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_some());

    let cleanup = match step_tcp_connection_cleanup(&client, &guard) {
        StepOutcome::Done(cleanup) => cleanup,
        _ => panic!("unexpected cleanup outcome"),
    };

    assert!(cleanup.was_connected);
    assert!(cleanup.local_withdrawn);
    assert!(cleanup.peer_withdrawn);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_none());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_none());
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Closed)
    );
    assert_eq!(
        step_tcp_connection_cleanup(&client, &guard),
        StepOutcome::Done(super::super::execution::TcpConnectionCleanupOutcome::default())
    );
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted child payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: remote,
            remote: local,
        })
    );
}

#[test]
fn tcp_close_staging_withdraws_connection_and_publishes_broken() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_187, 50_187);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));
    let client_key = ConnectionKey::new(local, remote);
    let server_key = ConnectionKey::new(remote, local);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_some());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_some());

    let close = match step_tcp_close_staging(&client, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected tcp close staging outcome"),
    };

    assert!(close.cleanup.was_connected);
    assert!(close.cleanup.local_withdrawn);
    assert!(close.cleanup.peer_withdrawn);
    assert!(close.recv_shutdown);
    assert!(close.send_shutdown);
    assert!(close.recv_broken_published);
    assert!(close.send_broken_published);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_none());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_none());
    assert!(client.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0);
    assert!(client.readiness.send_wq.peek() & SendWireSet::BROKEN.bits() != 0);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Closed)
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"x", SendRecvFlags::empty(), &guard),
        StepOutcome::Err(Errno::EPIPE)
    );
    assert_eq!(
        step_recv(&client, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
}

#[test]
fn tcp_socket_close_marks_connected_peer_broken() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_188, 50_188);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("expected accepted child"),
    };

    assert!(matches!(
        step_socket_close(&accepted, &guard),
        StepOutcome::Done(_)
    ));

    assert!(client.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0);
    assert!(client.readiness.send_wq.peek() & SendWireSet::BROKEN.bits() != 0);
    assert_eq!(
        step_send_kernel_bytes(&client, b"x", SendRecvFlags::empty(), &guard),
        StepOutcome::Err(Errno::EPIPE)
    );
    assert_eq!(
        step_recv(&client, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
}

#[test]
fn tcp_socket_close_flushes_queued_bytes_to_peer_before_eof() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_189, 50_189);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("expected accepted child"),
    };
    assert_eq!(
        step_send_kernel_bytes(&client, b"0", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(1)
    );

    let close = match step_socket_close(&client, &guard) {
        StepOutcome::Done(close) => close,
        _ => panic!("unexpected socket close outcome"),
    };
    assert_eq!(close.tcp_flushed_bytes, 1);

    let mut out = [0u8; 1];
    assert_eq!(
        step_recv_kernel_bytes(&accepted, &mut out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(crate::net::structure::SocketRecvBytesOutcome {
            bytes: 1,
            source: None,
            destination: None,
            truncated: false,
            became_empty: true,
        })
    );
    assert_eq!(&out, b"0");
    assert_eq!(
        step_recv(&accepted, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
    assert_eq!(
        step_send_kernel_bytes(&accepted, b"x", SendRecvFlags::empty(), &guard),
        StepOutcome::Err(Errno::EPIPE)
    );
}

#[test]
fn tcp_loopback_handshake_connects_bound_client_to_listener() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_160, 50_160);
    let guard = tx_substrate::epoch::guard();

    let outcome = match step_tcp_loopback_handshake(&client, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback handshake outcome"),
    };

    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: endpoint(50_160),
            remote: endpoint(40_160),
        })
    );
    assert_eq!(
        listener
            .acquire_operational()
            .expect("listener payload")
            .io_snapshot()
            .accept_pending,
        1
    );
    assert!(listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits() != 0);
    assert!(client.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);
    assert!(outcome.child.acquire_operational().is_some());
    assert_eq!(outcome.handshake.tx_packets, 3);
    assert_eq!(outcome.handshake.packets_seen, 3);
    assert_eq!(outcome.handshake.sockets_touched, 6);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .raw_tcp_socket()
            .expect("client raw tcp")
            .protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
    assert_eq!(
        outcome
            .child
            .acquire_operational()
            .expect("child payload")
            .raw_tcp_socket()
            .expect("child raw tcp")
            .protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
    assert!(
        client
            .acquire_operational()
            .expect("client payload")
            .raw_tcp_socket()
            .expect("client raw tcp")
            .protocol_runtime_state()
            .has_connected
    );
}

#[test]
fn tcp_loopback_handshake_selects_loopback_for_wildcard_bound_client() {
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
        step_bind(&listener, inet(40_168), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("client");
    assert_eq!(
        step_bind(&client, any_inet(50_168), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(40_168), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting {
            local: endpoint(50_168),
            remote: endpoint(40_168),
        })
    );

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert_eq!(
        listener
            .acquire_operational()
            .expect("listener payload")
            .io_snapshot()
            .accept_pending,
        1
    );
}

#[test]
fn tcp_loopback_handshake_drains_loopback_packet_queue() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, _listener, _local, _remote) = prepare_loopback_connect(40_167, 50_167);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));

    let outcome = match step_tcp_loopback_handshake_on_iface(&client, &iface, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback handshake outcome"),
    };

    assert_eq!(iface.pending_packets(), 0);
    assert_eq!(outcome.handshake.tx_packets, 3);
    assert_eq!(outcome.handshake.packets_seen, 3);
    assert_eq!(outcome.handshake.sockets_touched, 6);
    assert_eq!(
        outcome
            .child
            .acquire_operational()
            .expect("child payload")
            .raw_tcp_socket()
            .expect("child raw tcp")
            .protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
}

#[test]
fn tcp_loopback_accept_returns_connected_child() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_161, 50_161);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("unexpected accept outcome"),
    };

    assert_eq!(accepted.local, remote);
    assert_eq!(accepted.peer, local);
    assert_eq!(
        accepted
            .child
            .acquire_operational()
            .expect("accepted child payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: remote,
            remote: local,
        })
    );
}

#[test]
fn tcp_loopback_send_transfer_recv_moves_payload_bytes() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(40_162, 50_162, 5);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        0
    );

    let transfer = match step_tcp_loopback_transfer(&client, 64, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected transfer outcome"),
    };

    assert_eq!(transfer.bytes_moved, 5);
    assert!(transfer.source_wake_fired);
    assert!(transfer.peer_wake_fired);
    assert!(client.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);
    assert!(accepted.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted child payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted child payload")
            .raw_tcp_socket()
            .expect("accepted child raw tcp")
            .protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
    assert_eq!(
        step_recv(&accepted, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
    assert_eq!(
        accepted.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0
    );
}

#[test]
fn tcp_loopback_transfer_without_tx_data_is_noop() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_163, 50_163);
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
        step_tcp_loopback_transfer(&client, 64, &guard),
        StepOutcome::Done(super::super::execution::LoopbackTcpTransferOutcome::default())
    );
}

#[test]
fn tcp_loopback_handshake_requires_bound_client_for_now() {
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
        step_bind(&listener, inet(40_164), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("client");
    assert!(matches!(
        step_connect(&client, inet(40_164), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Err(Errno::EADDRNOTAVAIL)
    ));
}

fn assert_tcp_payload_round_trip(
    label: &str,
    source: &Cap<SocketIdentity>,
    destination: &Cap<SocketIdentity>,
    bytes: &[u8],
    guard: &tx_subsystems::execution::Guard<'_>,
) {
    assert_eq!(
        step_send_kernel_bytes(source, bytes, SendRecvFlags::empty(), guard),
        StepOutcome::Done(bytes.len())
    );
    let transfer = match step_tcp_loopback_transfer(source, bytes.len(), guard) {
        StepOutcome::Done(transfer) => transfer,
        _ => panic!("unexpected tcp loopback transfer outcome"),
    };
    assert_eq!(transfer.bytes_moved, bytes.len(), "{label}");

    let mut out = alloc::vec![0u8; bytes.len()];
    assert_eq!(
        step_recv_kernel_bytes(destination, &mut out, SendRecvFlags::empty(), guard),
        StepOutcome::Done(crate::net::structure::SocketRecvBytesOutcome {
            bytes: bytes.len(),
            source: None,
            destination: None,
            truncated: false,
            became_empty: true,
        })
    );
    assert_eq!(out, bytes);
}

fn prepare_loopback_connect(
    server_port: u16,
    client_port: u16,
) -> (
    Cap<SocketIdentity>,
    Cap<SocketIdentity>,
    IpEndpoint,
    IpEndpoint,
) {
    prepare_loopback_connect_with_client_send_buf(server_port, client_port, 16_384)
}

fn prepare_loopback_connect_with_client_send_buf(
    server_port: u16,
    client_port: u16,
    client_send_buf: usize,
) -> (
    Cap<SocketIdentity>,
    Cap<SocketIdentity>,
    IpEndpoint,
    IpEndpoint,
) {
    let guard = tx_substrate::epoch::guard();

    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    assert_eq!(
        step_bind(&listener, inet(server_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let mut options = SocketOptionSet::default_tcp();
    options.socket.send_buf_size = client_send_buf;
    let client =
        registry::create_socket_for_test_or_bootstrap(SocketKind::Tcp, options).expect("client");
    assert_eq!(
        step_bind(&client, inet(client_port), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(server_port), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    (
        client,
        listener,
        endpoint(client_port),
        endpoint(server_port),
    )
}
