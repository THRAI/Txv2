use super::*;

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
    assert!(matches!(
        step_poll_ready(&client, &guard),
        StepOutcome::Done(mask)
            if mask.contains(PollMask::ERR) && mask.contains(PollMask::OUT)
    ));
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
            unix_source: None,
            packet_source: None,
            truncated: false,
            became_empty: true,
            eor: false,
            sctp_notification: false,
            sctp_stream: 0,
            sctp_ppid: 0,
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
fn tcp_loopback_listener_accepts_after_clients_close_without_draining() {
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
        step_bind(&listener, inet(40_203), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    for client_port in [50_203, 50_204, 50_205] {
        let client = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Tcp,
            SocketOptionSet::default_tcp(),
        )
        .expect("client");
        assert_eq!(
            step_bind(&client, inet(client_port), &guard),
            StepOutcome::Done(())
        );
        assert!(matches!(
            step_connect(&client, inet(40_203), &guard),
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
            _ => panic!("expected accepted child"),
        };
        assert_eq!(
            step_send_kernel_bytes(&accepted, b"hoser\n", SendRecvFlags::empty(), &guard),
            StepOutcome::Done(6)
        );
        let transfer = match step_tcp_loopback_transfer(&accepted, 6, &guard) {
            StepOutcome::Done(transfer) => transfer,
            _ => panic!("unexpected transfer outcome"),
        };
        assert_eq!(transfer.bytes_moved, 6);
        assert!(matches!(
            step_poll_ready(&accepted, &guard),
            StepOutcome::Done(mask) if !mask.intersects(PollMask::IN | PollMask::RDHUP)
        ));
        assert!(matches!(
            step_poll_ready(&client, &guard),
            StepOutcome::Done(mask) if mask.intersects(PollMask::IN)
        ));

        assert!(matches!(
            step_socket_close(&client, &guard),
            StepOutcome::Done(_)
        ));
        assert!(matches!(
            step_poll_ready(&accepted, &guard),
            StepOutcome::Done(mask) if mask.intersects(PollMask::IN | PollMask::RDHUP)
        ));
        assert_eq!(
            step_recv(&accepted, 1024, SendRecvFlags::empty(), &guard),
            StepOutcome::Done(0)
        );
        assert!(matches!(
            step_socket_close(&accepted, &guard),
            StepOutcome::Done(_)
        ));
    }
}

#[test]
fn tcp_listener_poll_ready_uses_accept_queue_level() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_204, 50_206);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let payload = listener.acquire_operational().expect("listener payload");
    assert_eq!(payload.io_snapshot().accept_pending, 1);
    listener.readiness.clear_accept(AcceptWireSet::HAS_PENDING);

    assert!(matches!(
        step_poll_ready(&listener, &guard),
        StepOutcome::Done(mask) if mask.intersects(PollMask::IN)
    ));
}

#[test]
fn tcp_loopback_handshake_does_not_mark_client_readable_without_data() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, _listener, _local, _remote) = prepare_loopback_connect(40_205, 50_207);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_poll_ready(&client, &guard),
        StepOutcome::Done(mask) if !mask.intersects(PollMask::IN | PollMask::RDHUP)
    ));
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
    // P1-S3: `has_connected` 闩锁已删——"已连接"的单一真相就是 smoltcp 状态。
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .raw_tcp_socket()
            .expect("client raw tcp")
            .protocol_state(),
        smoltcp::socket::tcp::State::Established
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
fn tcp_recv_kicks_loopback_after_freeing_peer_window() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let guard = tx_substrate::epoch::guard();
    let mut listener_options = SocketOptionSet::default_tcp();
    listener_options.socket.recv_buf_size = 5;
    let listener = registry::create_socket_for_test_or_bootstrap(SocketKind::Tcp, listener_options)
        .expect("listener");
    assert_eq!(
        step_bind(&listener, inet(40_194), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let mut client_options = SocketOptionSet::default_tcp();
    client_options.socket.send_buf_size = 10;
    let client = registry::create_socket_for_test_or_bootstrap(SocketKind::Tcp, client_options)
        .expect("client");
    assert_eq!(
        step_bind(&client, inet(50_194), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(40_194), &guard),
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
        _ => panic!("unexpected accept outcome"),
    };

    assert_eq!(
        step_send_kernel_bytes(&client, b"abcdefghij", SendRecvFlags::empty(), &guard),
        StepOutcome::Done(10)
    );
    let first = match step_tcp_loopback_transfer(&client, 10, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected transfer outcome"),
    };
    assert_eq!(first.bytes_moved, 5);

    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mut out = [0u8; 5];
    assert_eq!(
        step_recv_kernel_bytes(&accepted, &mut out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: 5,
            source: None,
            unix_source: None,
            packet_source: None,
            destination: None,
            truncated: false,
            became_empty: true,
            eor: false,
            sctp_notification: false,
            sctp_stream: 0,
            sctp_ppid: 0,
        })
    );
    assert_eq!(&out, b"abcde");

    let source = ScriptedPacketSource::new(std::vec![]);
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: Some(loopback_iface()),
    };
    let outcome = net_delegate_step_once(&driver, &guard);
    assert!(outcome.poll_seen);
    assert_eq!(outcome.loopback.tcp_bytes_moved, 5);

    let mut tail = [0u8; 5];
    assert_eq!(
        step_recv_kernel_bytes(&accepted, &mut tail, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: 5,
            source: None,
            unix_source: None,
            packet_source: None,
            destination: None,
            truncated: false,
            became_empty: true,
            eor: false,
            sctp_notification: false,
            sctp_stream: 0,
            sctp_ppid: 0,
        })
    );
    assert_eq!(&tail, b"fghij");
}

#[test]
fn tcp_pollout_ignores_stale_send_space_wake_when_full() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(40_193, 50_193, 5);
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
    client.readiness.fire_send(SendWireSet::SPACE);

    assert!(matches!(
        step_poll_ready(&client, &guard),
        StepOutcome::Done(mask) if !mask.intersects(PollMask::OUT)
    ));
}

#[test]
fn tcp_msg_more_auto_flushes_full_segment_for_stream_progress() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) = prepare_loopback_connect_with_client_send_buf(
        40_190,
        50_190,
        TCP_CORK_AUTO_FLUSH_BYTES * 2,
    );
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
        step_send_kernel_bytes(&client, b"hello", SendRecvFlags::MSG_MORE, &guard),
        StepOutcome::Done(5)
    );
    assert_eq!(
        step_tcp_loopback_transfer(&client, TCP_CORK_AUTO_FLUSH_BYTES, &guard),
        StepOutcome::Done(crate::net::execution::LoopbackTcpTransferOutcome::default())
    );

    let tail = alloc::vec![0x5a; TCP_CORK_AUTO_FLUSH_BYTES - 5];
    assert_eq!(
        step_send_kernel_bytes(&client, &tail, SendRecvFlags::MSG_MORE, &guard),
        StepOutcome::Done(tail.len())
    );

    let transfer = match step_tcp_loopback_transfer(&client, TCP_CORK_AUTO_FLUSH_BYTES, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected transfer outcome"),
    };
    assert_eq!(transfer.bytes_moved, TCP_CORK_AUTO_FLUSH_BYTES);
    assert!(transfer.peer_wake_fired);

    let mut out = alloc::vec![0; TCP_CORK_AUTO_FLUSH_BYTES];
    assert_eq!(
        step_recv_kernel_bytes(&accepted, &mut out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: TCP_CORK_AUTO_FLUSH_BYTES,
            source: None,
            unix_source: None,
            packet_source: None,
            destination: None,
            truncated: false,
            became_empty: true,
            eor: false,
            sctp_notification: false,
            sctp_stream: 0,
            sctp_ppid: 0,
        })
    );
    assert_eq!(&out[..5], b"hello");
    assert!(out[5..].iter().all(|byte| *byte == 0x5a));
}

#[test]
fn tcp_loopback_pending_moves_multiple_msg_more_streams() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    assert_eq!(
        step_bind(&listener, inet(40_191), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let mut pairs = alloc::vec::Vec::new();
    for index in 0..5 {
        let mut options = SocketOptionSet::default_tcp();
        options.socket.send_buf_size = TCP_CORK_AUTO_FLUSH_BYTES * 2;
        let client = registry::create_socket_for_test_or_bootstrap(SocketKind::Tcp, options)
            .expect("client");
        assert_eq!(
            step_bind(&client, inet(50_191 + index), &guard),
            StepOutcome::Done(())
        );
        assert!(matches!(
            step_connect(&client, inet(40_191), &guard),
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
            _ => panic!("unexpected accept outcome"),
        };
        pairs.push((client, accepted));
    }

    let bytes = alloc::vec![0x33; TCP_CORK_AUTO_FLUSH_BYTES];
    for (client, _) in &pairs {
        assert_eq!(
            step_send_kernel_bytes(client, &bytes, SendRecvFlags::MSG_MORE, &guard),
            StepOutcome::Done(bytes.len())
        );
    }

    let pending = match step_process_loopback_pending_zero(
        loopback_iface(),
        LoopbackPollBudget::default(),
        &guard,
    ) {
        StepOutcome::Done(pending) => pending,
        _ => panic!("unexpected loopback pending outcome"),
    };
    assert_eq!(
        pending.tcp_bytes_moved,
        TCP_CORK_AUTO_FLUSH_BYTES * pairs.len()
    );

    for (_, accepted) in &pairs {
        let mut out = alloc::vec![0; TCP_CORK_AUTO_FLUSH_BYTES];
        assert_eq!(
            step_recv_kernel_bytes(accepted, &mut out, SendRecvFlags::empty(), &guard),
            StepOutcome::Done(SocketRecvBytesOutcome {
                bytes: TCP_CORK_AUTO_FLUSH_BYTES,
                source: None,
                unix_source: None,
                packet_source: None,
                destination: None,
                truncated: false,
                became_empty: true,
                eor: false,
                sctp_notification: false,
                sctp_stream: 0,
                sctp_ppid: 0,
            })
        );
        assert!(out.iter().all(|byte| *byte == 0x33));
    }
}

#[test]
fn tcp_close_preserves_peer_receive_bytes_until_eof() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(40_166, 50_166, 64);
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
    assert!(matches!(
        step_socket_close(&client, &guard),
        StepOutcome::Done(close) if close.tcp_flushed_bytes == 5
    ));

    let mut out = [0u8; 5];
    assert_eq!(
        step_recv_kernel_bytes(&accepted, &mut out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: 5,
            source: None,
            unix_source: None,
            packet_source: None,
            destination: None,
            truncated: false,
            became_empty: true,
            eor: false,
            sctp_notification: false,
            sctp_stream: 0,
            sctp_ppid: 0,
        })
    );
    assert_eq!(&out, b"hello");
    assert_eq!(
        step_recv(&accepted, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
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
        StepOutcome::Done(crate::net::execution::LoopbackTcpTransferOutcome::default())
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

#[test]
fn tcp_loopback_lost_data_segment_is_retransmitted_after_rto() {
    // P1 灵魂测试：数据只在 smoltcp 段级单通路上流动（P1）+ 时钟活着（P0）
    // ⇒ 人为丢弃一个数据段后，越过 RTO 重新驱动，对端仍能收齐字节。
    // 旧直拷世界不存在"段"可丢，此测试同时锁死 P0 与 P1 的成果。
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::clock::net_set_now_ns(0);
    let (client, _listener, _local, _remote) = prepare_loopback_connect(40_299, 50_299);
    let guard = tx_substrate::epoch::guard();

    let outcome = match step_tcp_loopback_handshake(&client, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("loopback handshake should succeed"),
    };
    let child = outcome.child;

    let iface = crate::net::namespace::initial_loopback_iface();
    while iface.pop_ingress().is_some() {} // 清残包，保证丢的是我们的段

    // 客户端把数据写进 smoltcp tx ring，egress 出数据段——然后丢弃它
    let client_payload = client.acquire_operational().expect("client payload");
    let client_raw = client_payload.raw_tcp_socket().expect("client raw");
    assert!(client_raw.enqueue_tx_bytes(b"hello").is_some());

    let mut ctx = PollContext::new_with_table(
        smoltcp::time::Instant::ZERO,
        client_payload.socket_table(),
    );
    assert!(ctx.poll_egress_one(&client, iface, &guard).is_some());
    assert!(
        iface.pop_ingress().is_some(),
        "数据段应已入 iface 队列——人为丢弃，模拟丢包"
    );

    // 对端没有数据；时间没走，立刻重试 egress 不该重传
    let child_payload = child.acquire_operational().expect("child payload");
    let child_raw = child_payload.raw_tcp_socket().expect("child raw");
    assert_eq!(child_raw.recv_available(), 0);
    assert!(
        ctx.poll_egress_one(&client, iface, &guard).is_none(),
        "RTO 未到期不该重传"
    );

    // 拨钟越过初始 RTO(≈700ms) → 重传 → 喂给对端 → 字节收齐
    crate::net::clock::net_set_now_ns(2_000_000_000);
    assert!(
        ctx.poll_egress_one(&client, iface, &guard).is_some(),
        "越过 RTO 应重传数据段"
    );
    let _ = ctx.poll_ingress(iface, &guard, 4);
    assert_eq!(child_raw.recv_available(), 5, "重传后对端应收齐 5 字节");
    let mut out = [0u8; 8];
    assert_eq!(child_raw.recv_bytes(&mut out, false), Some((5, true)));
    assert_eq!(&out[..5], b"hello");

    crate::net::clock::net_set_now_ns(0);
}
