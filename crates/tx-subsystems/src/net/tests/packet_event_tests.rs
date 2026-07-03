use super::*;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    loopback_iface().clear_for_test_or_bootstrap();
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

/// P2-S7 (§6-2-A): IPv6 UDP frames pass the demux — same event shape as
/// v4, endpoints carried as v6. Frame built by hand (UDP checksum 0 is
/// tolerated by the byte-level demux, which defers verification to the
/// segment/datagram consumers).
#[test]
fn smoltcp_demux_extracts_ipv6_udp_event() {
    let transport = udp_transport(53_001, 8081, &[5, 6, 7]);
    let frame = RxFrame::new(ethernet_ipv6_frame(17, &transport));

    let src_ip = crate::net::structure::Ipv6Address::new([
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ]);
    let dst_ip = crate::net::structure::Ipv6Address::new([
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
    ]);
    assert_eq!(
        demux_rx_frame_with_smoltcp(&frame),
        PacketDispatch::Udp(UdpPacketEvent::new(
            IpEndpoint::new_v6(src_ip, 53_001),
            IpEndpoint::new_v6(dst_ip, 8081),
            std::vec![5, 6, 7],
        ))
    );
}

/// P2-S7 (§6-2-A): IPv6 TCP frames pass the demux with flags + v6
/// endpoints (the checksum-verified segment attach happens for wire
/// frames with real checksums; a zero-checksum hand frame yields
/// segment=None, same contract as malformed-checksum v4).
#[test]
fn smoltcp_demux_extracts_ipv6_tcp_event_flags() {
    let transport = tcp_transport(49_001, 8443, 0x12, &[0xaa]);
    let frame = RxFrame::new(ethernet_ipv6_frame(6, &transport));

    match demux_rx_frame_with_smoltcp(&frame) {
        PacketDispatch::Tcp(event) => {
            assert_eq!(event.src.port, 49_001);
            assert_eq!(event.dst.port, 8443);
            assert_eq!(
                event.src.family,
                crate::net::structure::AddressFamily::Inet6
            );
            assert!(event.flags.syn);
            assert!(event.flags.ack);
            assert!(!event.flags.rst);
            assert_eq!(event.payload, std::vec![0xaa]);
        }
        other => panic!("expected v6 tcp dispatch, got {other:?}"),
    }
}

/// P3-S2 (D13) decisive test: `OpenFile::step_read`/`step_write` on a
/// socket-backed file delegate to the socket `FileOps` impl —
/// `write(fd)` ≡ `send(...,0)`, `read(fd)` ≡ `recv(...,0)` — instead of
/// the former `EINVAL` that forced socket I/O through syscall-layer
/// special cases. Full loopback ping: client writes via the FILE op,
/// bytes travel the lo queue, the accepted child reads via the FILE op.
#[test]
fn open_file_read_write_delegate_to_socket_file_ops() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();

    let listener_open = match crate::net::execution::step_socket_open_file(2, 1, 6, &guard) {
        StepOutcome::Done(output) => output,
        _ => panic!("listener open_file failed"),
    };
    let client_open = match crate::net::execution::step_socket_open_file(2, 1, 6, &guard) {
        StepOutcome::Done(output) => output,
        _ => panic!("client open_file failed"),
    };
    let listener = listener_open.identity;
    let client = client_open.identity;
    let local = endpoint(40_460);
    let remote_port = 40_461;

    assert_eq!(
        step_bind(&listener, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    assert_eq!(
        step_bind(&client, inet(remote_port), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(local.port), &guard),
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
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("accept should return queued child"),
    };

    // write(fd) on the client's OpenFile — the P3-S2 delegation path.
    assert_eq!(
        client_open.file.step_write(b"ping", &guard),
        StepOutcome::Done(4),
        "socket-backed OpenFile write must delegate to FileOps (was EINVAL)"
    );
    // Move the bytes across the loopback queue.
    assert!(matches!(
        step_process_loopback_tcp(&client, 4096, loopback_iface(), &guard),
        StepOutcome::Done(_)
    ));

    // read(fd) on a temporary OpenFile wrapping the accepted child.
    let accepted_file = crate::net::execution::socket_open_file_from_identity(
        accepted.clone(),
        crate::net::facade::SocketHandleFlags {
            cloexec: false,
            nonblock: false,
        },
    )
    .expect("accepted open file")
    .file;
    let mut buf = [0u8; 8];
    match accepted_file.step_read(&mut buf, &guard) {
        StepOutcome::Done(4) => assert_eq!(&buf[..4], b"ping"),
        other => panic!("socket-backed OpenFile read must return the payload: {other:?}"),
    }
}
