use super::*;
use tx_substrate::zone::Cap;

#[test]
fn loopback_pending_step_drives_tcp_connecting_handshake() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    let (client, listener, local, remote) = prepare_loopback_connecting(40_191, 50_191);
    let guard = tx_substrate::epoch::guard();

    let outcome = match step_process_loopback_pending(
        smoltcp::time::Instant::ZERO,
        loopback_iface(),
        small_budget(),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback pending outcome"),
    };

    assert_eq!(outcome.tcp_connect_attempted, 1);
    assert_eq!(outcome.tcp_connected, 1);
    assert_eq!(outcome.tcp_connect_failed, 0);
    assert!(outcome.tx_packets >= 3);
    assert!(outcome.packets_seen >= 3);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(ConnectionKey::new(local, remote), &guard)
        .is_some());
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote })
    );
    assert_eq!(
        listener
            .acquire_operational()
            .expect("listener payload")
            .accept_queue_len(),
        1
    );
}

#[test]
fn loopback_pending_step_drives_tcp_connected_transfer() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    let (client, listener, _local, _remote) = prepare_loopback_connecting(40_192, 50_192);
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            small_budget(),
            &guard,
        ),
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

    let outcome = match step_process_loopback_pending(
        smoltcp::time::Instant::ZERO,
        loopback_iface(),
        small_budget(),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback pending outcome"),
    };

    assert!(outcome.tcp_transfer_attempted >= 1);
    assert_eq!(outcome.tcp_bytes_moved, 5);
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert_eq!(
        step_recv(&accepted, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
}

#[test]
fn loopback_pending_step_drives_udp_connected_datagram() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    let guard = tx_substrate::epoch::guard();
    let server = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("server");
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client");
    assert_eq!(
        step_bind(&server, inet(40_193), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(50_193), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet(40_193), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );

    let outcome = match step_process_loopback_pending(
        smoltcp::time::Instant::ZERO,
        loopback_iface(),
        small_budget(),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback pending outcome"),
    };

    assert!(outcome.udp_transfer_attempted >= 1);
    assert_eq!(outcome.udp_bytes_moved, 5);
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

#[test]
fn loopback_pending_step_drives_udp_bound_sendto_datagram() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    let guard = tx_substrate::epoch::guard();
    let socket = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("socket");
    assert_eq!(
        step_bind(&socket, inet(40_194), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &socket,
            Some(endpoint(40_194)),
            b"hello",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(5)
    );

    let outcome = match step_process_loopback_pending(
        smoltcp::time::Instant::ZERO,
        loopback_iface(),
        small_budget(),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback pending outcome"),
    };

    assert_eq!(outcome.udp_transfer_attempted, 1);
    assert_eq!(outcome.udp_transfer_failed, 0);
    assert_eq!(outcome.udp_bytes_moved, 5);
    assert_eq!(
        socket
            .acquire_operational()
            .expect("socket payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert_eq!(
        step_recv(&socket, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
}

#[test]
fn loopback_pending_step_drives_raw_icmp_echo() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    let guard = tx_substrate::epoch::guard();
    let valid = ValidSocketType::validate(2, 2, 1).expect("ping socket");
    let socket = match step_socket_create(valid, &guard) {
        StepOutcome::Done(socket) => socket,
        other => panic!("unexpected socket create outcome: {other:?}"),
    };
    assert_eq!(step_bind(&socket, inet(0), &guard), StepOutcome::Done(()));

    let request = Icmpv4EchoPacket {
        src: Ipv4Address::LOOPBACK,
        dst: Ipv4Address::LOOPBACK,
        ident: 0x46,
        seq_no: 1,
        payload: b"ping".to_vec(),
    };
    let request_bytes = build_icmpv4_echo_request_message(&request);
    assert_eq!(
        step_send_to_kernel_bytes(
            &socket,
            Some(IpEndpoint::new(Ipv4Address::LOOPBACK, 0)),
            &request_bytes,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(request_bytes.len())
    );

    let outcome = match step_process_loopback_pending(
        smoltcp::time::Instant::ZERO,
        loopback_iface(),
        small_budget(),
        &guard,
    ) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected loopback pending outcome"),
    };

    assert!(outcome.icmp_transfer_attempted >= 1);
    assert_eq!(outcome.icmp_transfer_failed, 0);
    assert_eq!(outcome.icmp_bytes_moved, request_bytes.len());
    assert_eq!(
        socket
            .acquire_operational()
            .expect("raw icmp payload")
            .io_snapshot()
            .recv_len,
        request_bytes.len()
    );

    let mut out = std::vec![0u8; 64];
    let recv = match step_recv_kernel_bytes(&socket, &mut out, SendRecvFlags::empty(), &guard) {
        StepOutcome::Done(recv) => recv,
        _ => panic!("unexpected recv outcome"),
    };
    assert_eq!(recv.bytes, request_bytes.len());
    assert_eq!(recv.source, Some(IpEndpoint::new(Ipv4Address::LOOPBACK, 0)));
    assert_eq!(
        parse_icmpv4_payload(
            Ipv4Address::LOOPBACK,
            Ipv4Address::LOOPBACK,
            &out[..recv.bytes]
        ),
        Icmpv4Event::EchoReply(request.reply_packet())
    );
}

fn prepare_loopback_connecting(
    server_port: u16,
    client_port: u16,
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
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("client");
    assert_eq!(
        step_bind(&listener, inet(server_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
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

fn small_budget() -> LoopbackPollBudget {
    LoopbackPollBudget {
        tcp_connecting: 8,
        tcp_connected: 8,
        udp_bound: 8,
        raw_icmp: 8,
        packet_budget: 8,
        tcp_transfer_bytes: 64,
    }
}
