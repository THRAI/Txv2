use super::*;

#[test]
fn step_recv_consumes_available_bytes_and_clears_when_empty() {
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
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.record_recv_payload(endpoint(50_135), endpoint(40_135), std::vec![0u8; 128]));
    tcp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    assert_eq!(
        step_recv(&tcp, 64, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(64)
    );
    assert_eq!(payload.io_snapshot().recv_len, 64);
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);

    assert_eq!(
        step_recv(&tcp, 64, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(64)
    );
    assert_eq!(payload.io_snapshot().recv_len, 0);
    assert_eq!(
        tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0
    );
}

#[test]
fn step_recv_blocks_when_no_data() {
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
    assert_eq!(step_bind(&udp, inet(40_134), &guard), StepOutcome::Done(()));

    let wait = expect_carrier_yield(step_recv(&udp, 32, SendRecvFlags::empty(), &guard));

    assert_eq!(wait.source_id(), udp.wait_carriers.recv);
    assert_eq!(
        wait.interest(),
        RecvWireSet::HAS_DATA.bits() | RecvWireSet::BROKEN.bits()
    );
}

#[test]
fn step_recv_broken_without_buffered_data_returns_eof() {
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
    tcp.readiness.fire_recv(RecvWireSet::BROKEN);

    assert_eq!(
        step_recv(&tcp, 32, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
}

#[test]
fn step_recv_peek_does_not_consume_or_clear() {
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
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.record_recv_payload(endpoint(50_136), endpoint(40_136), std::vec![0u8; 16]));
    tcp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    assert_eq!(
        step_recv(&tcp, 8, SendRecvFlags::MSG_PEEK, &guard),
        StepOutcome::Done(8)
    );
    assert_eq!(payload.io_snapshot().recv_len, 16);
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
}

#[test]
fn step_recv_errqueue_without_error_returns_eagain() {
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
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.record_recv_payload(endpoint(50_137), endpoint(40_137), std::vec![0u8; 16]));
    tcp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    assert_eq!(
        step_recv(&tcp, 8, SendRecvFlags::MSG_ERRQUEUE, &guard),
        StepOutcome::Err(Errno::EAGAIN)
    );
    assert_eq!(payload.io_snapshot().recv_len, 16);
}

#[test]
fn step_send_consumes_space_and_clears_when_full() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 64;
    let udp = registry::create_socket_for_test_or_bootstrap(SocketKind::Udp, options)
        .expect("udp socket");
    assert_eq!(step_bind(&udp, inet(40_136), &guard), StepOutcome::Done(()));
    let payload = udp.acquire_operational().expect("payload");
    udp.readiness.fire_send(SendWireSet::SPACE);

    assert_eq!(
        step_send(&udp, 32, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(32)
    );
    assert_eq!(payload.io_snapshot().send_space, 32);
    assert!(udp.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);

    assert_eq!(
        step_send(&udp, 32, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(32)
    );
    assert_eq!(payload.io_snapshot().send_space, 0);
    assert_eq!(udp.readiness.send_wq.peek() & SendWireSet::SPACE.bits(), 0);
}

#[test]
fn step_send_blocks_when_no_space() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 1;
    let udp = registry::create_socket_for_test_or_bootstrap(SocketKind::Udp, options)
        .expect("udp socket");
    assert_eq!(step_bind(&udp, inet(40_137), &guard), StepOutcome::Done(()));
    assert_eq!(
        step_send(&udp, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(1)
    );

    let wait = expect_carrier_yield(step_send(&udp, 32, SendRecvFlags::empty(), &guard));

    assert_eq!(wait.source_id(), udp.wait_carriers.send);
    assert_eq!(
        wait.interest(),
        SendWireSet::SPACE.bits() | SendWireSet::BROKEN.bits()
    );
}

#[test]
fn step_send_oob_is_not_supported() {
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
    assert_eq!(step_bind(&udp, inet(40_200), &guard), StepOutcome::Done(()));

    assert_eq!(
        step_send(&udp, 1, SendRecvFlags::MSG_OOB, &guard),
        StepOutcome::Err(Errno::EOPNOTSUPP)
    );
}

#[test]
fn step_send_udp_datagram_too_large_returns_emsgsize() {
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
    assert_eq!(step_bind(&udp, inet(40_201), &guard), StepOutcome::Done(()));

    assert_eq!(
        step_send_kernel_bytes(&udp, &std::vec![0; 65_508], SendRecvFlags::empty(), &guard),
        StepOutcome::Err(Errno::EMSGSIZE)
    );
}

#[test]
fn step_send_udp_loopback_oob_is_not_supported() {
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
    assert_eq!(step_bind(&udp, inet(40_206), &guard), StepOutcome::Done(()));

    assert_eq!(
        step_send_udp_loopback_kernel_bytes(
            &udp,
            Some(endpoint(50_206)),
            b"x",
            SendRecvFlags::MSG_OOB,
            &guard
        ),
        StepOutcome::Err(Errno::EOPNOTSUPP)
    );
}

#[test]
fn step_sendto_connected_tcp_ignores_destination_argument() {
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
        step_bind(&listener, inet(40_202), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("client");
    assert_eq!(
        step_bind(&client, inet(50_202), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(40_202), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));
    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));

    assert_eq!(
        step_send_to_kernel_bytes(
            &client,
            Some(IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0)),
            b"x",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(1)
    );
}

#[test]
fn raw_udp_send_queue_preserves_datagram_atomicity() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 8;
    let udp = RawUdpSocket::new(&options);
    let dst = IpEndpoint::new(Ipv4Address::LOOPBACK, 40_138);
    // P2-S6: the smoltcp ring is the queue; `send` needs a bound socket.
    assert!(udp.bind_endpoint(IpEndpoint::new(Ipv4Address::LOOPBACK, 40_240)));

    assert_eq!(udp.enqueue_tx_bytes_to(dst, b"12345"), Some((5, false)));
    assert_eq!(udp.send_available(), 3);
    assert_eq!(udp.enqueue_tx_bytes_to(dst, b"abcd"), None);
    assert_eq!(udp.send_available(), 3);

    let drain = udp.pop_tx_datagram().expect("queued datagram");
    assert_eq!(drain.datagram.payload, b"12345");
    assert!(drain.became_available);
    assert_eq!(udp.enqueue_tx_bytes_to(dst, b"abcd"), Some((4, false)));
}

#[test]
fn raw_udp_msg_more_corks_until_uncork_send() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.send_buf_size = 8;
    let udp = RawUdpSocket::new(&options);
    let dst = IpEndpoint::new(Ipv4Address::LOOPBACK, 40_139);
    // P2-S6: the smoltcp ring is the queue; `send` needs a bound socket.
    assert!(udp.bind_endpoint(IpEndpoint::new(Ipv4Address::LOOPBACK, 40_241)));

    assert_eq!(
        udp.enqueue_tx_bytes_to_with_more(dst, b"12345", true),
        Some((5, false))
    );
    assert_eq!(udp.send_available(), 3);
    assert!(udp.pop_tx_datagram().is_none());

    assert_eq!(
        udp.enqueue_tx_bytes_to_with_more(dst, b"67", false),
        Some((2, false))
    );
    let drain = udp.pop_tx_datagram().expect("uncorked datagram");
    assert_eq!(drain.datagram.dst, dst);
    assert_eq!(drain.datagram.payload, b"1234567");
    assert!(drain.became_available);
    assert_eq!(udp.send_available(), 8);
}

#[test]
fn raw_udp_recv_queue_drops_when_datagram_would_not_fit() {
    let mut options = SocketOptionSet::default_udp();
    options.socket.recv_buf_size = 8;
    let udp = RawUdpSocket::new(&options);
    let src = IpEndpoint::new(Ipv4Address::LOOPBACK, 50_138);
    let dst = IpEndpoint::new(Ipv4Address::LOOPBACK, 40_138);
    // P2-S6: inbound datagrams pass smoltcp `accepts`; bind the dst first.
    assert!(udp.bind_endpoint(dst));

    assert!(udp.ingest_rx_datagram(src, dst, b"123456".to_vec()));
    assert!(!udp.ingest_rx_datagram(src, dst, b"abcd".to_vec()));
    assert_eq!(udp.recv_available(), 6);

    let mut out = [0u8; 8];
    let drain = udp
        .recv_datagram_bytes(&mut out, false)
        .expect("first datagram");
    assert_eq!(drain.bytes, 6);
    assert_eq!(&out[..drain.bytes], b"123456");
    assert!(drain.became_empty);
    assert!(udp.ingest_rx_datagram(src, dst, b"abcd".to_vec()));
}
