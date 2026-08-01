use super::*;

#[test]
fn tcp_loopback_pollcontext_moves_data_through_packet_queue() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(41_166, 51_166, 5);
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
        prepare_loopback_connect_with_client_send_buf(41_168, 51_168, 5);
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
fn tcp_loopback_ipv4_client_reaches_inet6_wildcard_listener() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let guard = tx_substrate::epoch::guard();
    let server_port = 41_180;
    let client_port = 51_180;

    let listener = registry::create_socket_in_namespace_with_family(
        SocketKind::Tcp,
        AddressFamily::Inet6,
        SocketOptionSet::default_tcp(),
        crate::net::namespace::initial_net_namespace_payload(),
    )
    .expect("inet6 listener");
    assert_eq!(
        step_bind(&listener, any_inet6(server_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("inet client");
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

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("unexpected accept outcome"),
    };

    assert_eq!(accepted.local, endpoint(server_port));
    assert_eq!(accepted.peer, endpoint(client_port));
    assert_eq!(
        accepted
            .child
            .acquire_operational()
            .expect("accepted child payload")
            .family(),
        AddressFamily::Inet6
    );
}

#[test]
fn tcp_loopback_ipv6_client_reaches_inet6_loopback_listener() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let guard = tx_substrate::epoch::guard();
    let server_port = 41_181;
    let client_port = 51_181;

    let listener = registry::create_socket_in_namespace_with_family(
        SocketKind::Tcp,
        AddressFamily::Inet6,
        SocketOptionSet::default_tcp(),
        crate::net::namespace::initial_net_namespace_payload(),
    )
    .expect("inet6 listener");
    assert_eq!(
        step_bind(
            &listener,
            KernelSockAddr::V6(SockAddrIn6::new(server_port, Ipv6Address::LOOPBACK)),
            &guard
        ),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let client = registry::create_socket_in_namespace_with_family(
        SocketKind::Tcp,
        AddressFamily::Inet6,
        SocketOptionSet::default_tcp(),
        crate::net::namespace::initial_net_namespace_payload(),
    )
    .expect("inet6 client");
    assert_eq!(
        step_bind(
            &client,
            KernelSockAddr::V6(SockAddrIn6::new(client_port, Ipv6Address::LOOPBACK)),
            &guard
        ),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(
            &client,
            KernelSockAddr::V6(SockAddrIn6::new(server_port, Ipv6Address::LOOPBACK)),
            &guard
        ),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("unexpected accept outcome"),
    };

    assert_eq!(
        accepted.local,
        IpEndpoint::new_v6(Ipv6Address::LOOPBACK, server_port)
    );
    assert_eq!(
        accepted.peer,
        IpEndpoint::new_v6(Ipv6Address::LOOPBACK, client_port)
    );
}

#[test]
fn tcp_loopback_transfer_moves_large_write_within_one_budgeted_step() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(41_169, 51_169, 64 * 1024);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let bytes = alloc::vec![0x5a; 32 * 1024];
    assert_eq!(
        step_send_kernel_bytes(&client, &bytes, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(bytes.len())
    );

    let transfer = match step_tcp_loopback_transfer(&client, bytes.len(), &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected transfer outcome"),
    };

    assert_eq!(transfer.bytes_moved, bytes.len());
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted child payload")
            .io_snapshot()
            .recv_len,
        bytes.len()
    );
}

#[test]
fn tcp_loopback_accepted_recv_without_payload_waits() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_170, 51_170);
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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_171, 51_171);
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
    let (client, listener, local, remote) = prepare_loopback_connect(41_269, 51_269);
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
    let (client, listener, local, remote) = prepare_loopback_connect(41_270, 51_270);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_172, 51_172);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let start = smoltcp::time::Instant::from_millis(7);
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_173, 51_173);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_174, 51_174);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_175, 51_175);
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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_176, 51_176);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_177, 51_177);
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
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, local, remote) = prepare_loopback_connect(41_178, 51_178);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
    let (client, listener, _local, _remote) = prepare_loopback_connect(41_179, 51_179);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    start_raw_tcp_connect_for_active_attempt(&client_payload);

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
