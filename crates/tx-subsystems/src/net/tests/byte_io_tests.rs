use super::*;

#[test]
fn raw_tcp_socket_ingests_and_drains_rx_bytes() {
    let mut options = SocketOptionSet::default_tcp();
    options.socket.recv_buf_size = 8;
    let raw = RawTcpSocket::new(&options);

    assert!(raw.ingest_rx_bytes(b"abcdef"));
    assert_eq!(raw.recv_available(), 6);
    assert_eq!(raw.recv_len(3, false), Some((3, false)));
    assert_eq!(raw.recv_available(), 3);
    assert_eq!(raw.recv_len(8, false), Some((3, true)));
    assert_eq!(raw.recv_available(), 0);
}

#[test]
fn raw_tcp_socket_peek_does_not_drain_rx_bytes() {
    let raw = RawTcpSocket::new(&SocketOptionSet::default_tcp());

    assert!(raw.ingest_rx_bytes(b"abcdef"));
    assert_eq!(raw.recv_len(3, true), Some((3, false)));
    assert_eq!(raw.recv_available(), 6);
}

#[test]
fn raw_udp_socket_preserves_datagram_boundary() {
    let raw = RawUdpSocket::new(&SocketOptionSet::default_udp());
    let src_a = endpoint(50_010);
    let src_b = endpoint(50_011);
    let dst = endpoint(40_010);

    assert!(raw.ingest_rx_datagram(src_a, dst, std::vec![1, 2, 3, 4, 5]));
    assert!(!raw.ingest_rx_datagram(src_b, dst, std::vec![6, 7]));
    assert_eq!(raw.recv_available(), 7);
    assert_eq!(raw.recv_len(3, false), Some((3, false)));
    assert_eq!(raw.recv_available(), 2);
    assert_eq!(raw.recv_len(8, false), Some((2, true)));
}

#[test]
fn tcp_packet_event_payload_bytes_are_consumed_by_step_recv() {
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
    let local = endpoint(40_131);
    let remote = endpoint(50_131);
    SOCKET_TABLE
        .insert_tcp_connection(ConnectionKey::new(local, remote), tcp.clone())
        .expect("connection insert");
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: false,
            ack: false,
            rst: false,
        },
        std::vec![1, 2, 3],
        false,
    ))]);

    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    let payload = tcp.acquire_operational().expect("payload");
    assert_eq!(payload.io_snapshot().recv_len, 3);

    assert_eq!(
        step_recv(&tcp, 2, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(2)
    );
    assert_eq!(payload.io_snapshot().recv_len, 1);
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert_eq!(
        step_recv(&tcp, 2, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(1)
    );
    assert_eq!(payload.io_snapshot().recv_len, 0);
    assert_eq!(
        tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0
    );
}

#[test]
fn step_send_kernel_bytes_records_tx_bytes_and_clears_when_full() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 4;
    let udp = registry::create_socket_for_test_or_bootstrap(SocketKind::Udp, options)
        .expect("udp socket");
    assert_eq!(step_bind(&udp, inet(40_139), &guard), StepOutcome::Done(()));
    udp.readiness.fire_send(SendWireSet::SPACE);

    assert_eq!(
        step_send_kernel_bytes(&udp, &[1, 2, 3, 4], SendRecvFlags::empty(), &guard),
        StepOutcome::Done(4)
    );
    let payload = udp.acquire_operational().expect("payload");
    assert_eq!(payload.io_snapshot().send_space, 0);
    assert_eq!(udp.readiness.send_wq.peek() & SendWireSet::SPACE.bits(), 0);
}
