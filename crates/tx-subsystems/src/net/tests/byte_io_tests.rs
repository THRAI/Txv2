use super::*;

// P1-S1 note: the old `raw_tcp_socket_ingests_and_drains_rx_bytes` /
// `raw_tcp_socket_peek_does_not_drain_rx_bytes` tests asserted the staging
// `rx_buffer`'s bounded-ingest contract. That buffer is gone — TCP recv now
// reads the smoltcp rx ring directly; end-to-end recv coverage lives in
// `loopback_tests` (segment path) and the empty-ring contract below.

#[test]
fn raw_tcp_socket_recv_reports_empty_smoltcp_ring() {
    let raw = RawTcpSocket::new(&SocketOptionSet::default_tcp());

    assert_eq!(raw.recv_available(), 0);
    assert_eq!(raw.recv_len(3, false), None);
    assert_eq!(raw.recv_len(0, false), Some((0, false)));
    let mut out = [0u8; 4];
    assert_eq!(raw.recv_bytes(&mut out, false), None);
    assert_eq!(raw.recv_bytes(&mut [], false), Some((0, false)));
}

#[test]
fn raw_udp_socket_preserves_datagram_boundary() {
    let raw = RawUdpSocket::new(&SocketOptionSet::default_udp());
    let src_a = endpoint(50_010);
    let src_b = endpoint(50_011);
    let dst = endpoint(40_010);
    // P2-S6: inbound datagrams pass smoltcp `accepts`; bind the dst first.
    assert!(raw.bind_endpoint(dst));

    assert!(raw.ingest_rx_datagram(src_a, dst, std::vec![1, 2, 3, 4, 5]));
    assert!(!raw.ingest_rx_datagram(src_b, dst, std::vec![6, 7]));
    assert_eq!(raw.recv_available(), 7);
    assert_eq!(raw.recv_len(3, false), Some((3, false)));
    assert_eq!(raw.recv_available(), 2);
    assert_eq!(raw.recv_len(8, false), Some((2, true)));
}

#[test]
fn raw_udp_socket_preserves_zero_length_datagram_presence() {
    let raw = RawUdpSocket::new(&SocketOptionSet::default_udp());
    let src = endpoint(50_012);
    let dst = endpoint(40_012);
    assert!(raw.bind_endpoint(dst));

    assert!(
        raw.ingest_rx_datagram(src, dst, std::vec![]),
        "an empty UDP payload is still one readable datagram"
    );
    assert_eq!(
        raw.recv_available(),
        0,
        "byte availability must not invent payload for an empty datagram"
    );
    assert!(
        raw.can_recv(),
        "datagram presence must remain distinct from payload byte count"
    );

    let mut out = [];
    let drain = raw
        .recv_datagram_bytes(&mut out, false)
        .expect("zero-length datagram must be consumable by a zero-length receive");
    assert_eq!(drain.bytes, 0);
    assert_eq!(drain.source, src);
    assert_eq!(drain.destination, dst);
    assert!(!drain.truncated);
    assert!(drain.became_empty);
    assert!(!raw.can_recv());

    assert!(raw.ingest_rx_datagram(src, dst, std::vec![1]));
    let drain = raw
        .recv_datagram_bytes(&mut out, false)
        .expect("a zero-length receive must consume one queued datagram");
    assert_eq!(drain.bytes, 0);
    assert!(drain.truncated);
    assert!(drain.became_empty);
    assert!(!raw.can_recv());
}

#[test]
fn tcp_packet_event_without_segment_is_dropped() {
    // P1-S1: established-connection RX only accepts events carrying a full
    // parsed segment (fed to smoltcp `process_segment`). A bare-byte event —
    // the shape the old rx bypass consumed — no longer reaches user-visible
    // data.
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
    assert_eq!(payload.io_snapshot().recv_len, 0);
    assert_eq!(
        tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0
    );
    assert!(matches!(
        step_recv(&tcp, 2, SendRecvFlags::empty(), &guard),
        StepOutcome::Yield { .. }
    ));
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
    // A plain send() needs a destination: connect so the datagram is
    // addressable and actually consumes the send buffer. (An unconnected
    // send() with no msg_name now correctly fails EDESTADDRREQ and buffers
    // nothing, matching Linux and the sendto path.)
    assert_eq!(
        step_connect(&udp, inet(40_140), &guard),
        StepOutcome::Done(())
    );
    udp.readiness.fire_send(SendWireSet::SPACE);

    assert_eq!(
        step_send_kernel_bytes(&udp, &[1, 2, 3, 4], SendRecvFlags::empty(), &guard),
        StepOutcome::Done(4)
    );
    let payload = udp.acquire_operational().expect("payload");
    assert_eq!(payload.io_snapshot().send_space, 0);
    assert_eq!(udp.readiness.send_wq.peek() & SendWireSet::SPACE.bits(), 0);
}

#[test]
fn step_send_kernel_bytes_unconnected_udp_fails_edestaddrreq() {
    // Regression: a plain send() with no msg_name on an unconnected UDP socket
    // must fail EDESTADDRREQ (like Linux and the sendto path) rather than
    // silently drop the datagram while reporting success — the latter also left
    // the send-buffer accounting inconsistent (send_space never dropped).
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
    assert_eq!(step_bind(&udp, inet(40_141), &guard), StepOutcome::Done(()));

    assert_eq!(
        step_send_kernel_bytes(&udp, &[1, 2, 3, 4], SendRecvFlags::empty(), &guard),
        StepOutcome::Err(Errno::EDESTADDRREQ)
    );
    // Nothing was staged, so the send buffer is untouched.
    let payload = udp.acquire_operational().expect("payload");
    assert!(payload.io_snapshot().send_space > 0);
}
