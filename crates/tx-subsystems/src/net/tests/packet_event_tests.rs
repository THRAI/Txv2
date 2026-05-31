use super::*;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    lock
}

#[test]
fn udp_packet_event_sets_recv_readiness() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let local = endpoint(40_130);
    let remote = endpoint(50_130);

    assert_eq!(
        step_bind(
            &udp,
            KernelSockAddr::V4(SockAddrIn::new(local.port, local.addr)),
            &guard,
        ),
        StepOutcome::Done(())
    );
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Udp(
        UdpPacketEvent::with_payload_len(remote, local, 128),
    )]);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        udp.acquire_operational()
            .expect("payload")
            .io_snapshot()
            .recv_len,
        128
    );
    assert!(udp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
}

#[test]
fn smoltcp_demux_rejects_empty_frame_as_malformed() {
    let frame = RxFrame::new(std::vec::Vec::new());

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Malformed
    );
}

#[test]
fn smoltcp_demux_ignores_arp_as_unsupported() {
    let mut bytes = std::vec::Vec::new();
    bytes.extend_from_slice(&[0x02, 0, 0, 0, 0, 2]);
    bytes.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
    bytes.extend_from_slice(&[0x08, 0x06]);
    let frame = RxFrame::new(bytes);

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Unsupported
    );
}

#[test]
fn smoltcp_demux_extracts_ipv4_udp_event() {
    let transport = udp_transport(53_000, 8080, &[1, 2, 3, 4]);
    let frame = RxFrame::new(ethernet_ipv4_frame(17, &transport));

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Udp(UdpPacketEvent::new(
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 1]), 53_000),
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 2]), 8080),
            std::vec![1, 2, 3, 4],
        ))
    );
}

#[test]
fn smoltcp_demux_extracts_ipv4_tcp_event_flags() {
    let transport = tcp_transport(49_000, 443, 0x32, &[9, 8, 7]);
    let frame = RxFrame::new(ethernet_ipv4_frame(6, &transport));

    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Tcp(TcpPacketEvent::new(
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 1]), 49_000),
            IpEndpoint::new(Ipv4Address::new([192, 0, 2, 2]), 443),
            TcpPacketFlags {
                syn: true,
                ack: true,
                rst: false,
            },
            std::vec![9, 8, 7],
            true
        ))
    );
}

#[test]
fn smoltcp_packet_source_reads_device_frame_and_dispatches() {
    let frame = RxFrame::new(ethernet_ipv4_frame(
        17,
        &udp_transport(40_140, 40_141, &[1]),
    ));
    let ops = std::boxed::Box::leak(std::boxed::Box::new(ScriptedNetDevice::new(std::vec![
        frame,
    ])));
    let device = std::boxed::Box::leak(std::boxed::Box::new(NetDeviceRegistration {
        devt: DevT::new(10, 0),
        name: "test-net0",
        ops,
    }));
    let adapter = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: ops.mac_addr(),
        local_ipv4: Ipv4Address::new([192, 0, 2, 2]),
        mtu: ops.mtu(),
    });
    let source = SmoltcpPacketSource {
        adapter: &adapter,
        device,
    };

    assert!(matches!(
        source.next_packet(),
        Some(PacketDispatch::Udp(UdpPacketEvent { payload, .. })) if payload == std::vec![1]
    ));
    assert_eq!(source.next_packet(), None);
}

#[test]
fn tcp_packet_event_sets_connection_readiness_and_urgent_port() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp socket");
    let local = endpoint(40_143);
    let remote = endpoint(50_143);
    assert_eq!(
        step_bind(&listener, inet(remote.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    assert_eq!(
        step_bind(&tcp, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&tcp, inet(remote.port), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));
    assert!(matches!(
        step_tcp_loopback_handshake(&tcp, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));
    let mut urgent_future =
        crate::wait_source::wait_on_token(socket_urgent_wait_token(&tcp)).expect("urgent future");
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut urgent_future).poll(&mut cx),
        Poll::Pending
    ));
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: false,
            ack: true,
            rst: false,
        },
        std::vec![0u8; 64],
        true,
    ),)]);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(outcome.wakes_fired, 1);
    let io = tcp.acquire_operational().expect("payload").io_snapshot();
    assert_eq!(io.recv_len, 64);
    assert_eq!(
        io.send_space,
        SocketOptionSet::default_tcp().socket.send_buf_size
    );
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert!(tcp.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);
    assert!(matches!(
        Pin::new(&mut urgent_future).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    ));
}

#[test]
fn tcp_syn_to_listener_sets_accept_readiness() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    let local = endpoint(40_132);
    let remote = endpoint(50_132);

    assert_eq!(
        step_bind(&listener, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: true,
            ack: false,
            rst: false,
        },
        std::vec::Vec::new(),
        false,
    ),)]);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        listener
            .acquire_operational()
            .expect("payload")
            .io_snapshot()
            .accept_pending,
        1
    );
    assert!(listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits() != 0);
}
