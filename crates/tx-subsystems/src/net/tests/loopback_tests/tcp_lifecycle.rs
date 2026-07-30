use super::*;

#[test]
fn tcp_loopback_connect_preserves_a_foreign_tuple_owner() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, _listener, local, remote) = prepare_loopback_connect(40_295, 50_295);
    let guard = tx_substrate::epoch::guard();
    let payload = client.acquire_operational().expect("client payload");
    let foreign = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("foreign tuple owner");
    let key = ConnectionKey::new(local, remote);
    payload
        .socket_table()
        .insert_tcp_connection(key, foreign.clone())
        .expect("install foreign tuple owner");

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Err(Errno::EADDRINUSE)
    ));
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Bound { local })
    );
    assert_eq!(payload.socket_error(), Some(Errno::EADDRINUSE));
    assert_eq!(
        payload
            .socket_table()
            .lookup_tcp_connection(key, &guard)
            .map(|owner| owner.raw()),
        Some(foreign.raw())
    );
    assert!(client.readiness.send_wq.peek() & SendWireSet::CONNECT_DONE.bits() != 0);
}

#[test]
fn tcp_inbound_promotion_rolls_back_connection_index_without_backlog_entry() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let local = endpoint(40_296);
    let remote = endpoint(50_296);
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    assert_eq!(
        step_bind(&listener, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let child = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("untracked inbound child");
    let payload = child.acquire_operational().expect("child payload");
    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Tcp(TcpState::Connecting { local, remote });
    });
    let (generation, _, _) = payload.tcp_flow_snapshot().expect("child TCP flow");
    let key = ConnectionKey::new(local, remote);

    assert!(matches!(
        promote_connected_stream_and_publish_accept(
            payload.socket_table(),
            &child,
            &payload,
            None,
            generation,
            &guard,
        ),
        TcpConnectedPromotion::Rejected
    ));
    assert!(
        payload
            .socket_table()
            .lookup_tcp_connection(key, &guard)
            .is_none(),
        "failed accept promotion must withdraw the child connection key"
    );
    assert_eq!(
        child.readiness.send_wq.peek() & SendWireSet::SPACE.bits(),
        0,
        "a rejected inbound child must not publish connect success"
    );
}

#[test]
fn stale_loopback_egress_cannot_dispatch_a_replacement_attempt() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, _listener, local, remote) = prepare_loopback_connect(40_297, 50_297);
    let guard = tx_substrate::epoch::guard();
    let payload = client.acquire_operational().expect("client payload");
    let stale = payload
        .active_tcp_connect_attempt()
        .expect("first connect attempt");
    start_raw_tcp_connect_for_active_attempt(&payload);

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(local.port), &guard),
        StepOutcome::Done(()),
        "AF_UNSPEC must release the old bind before the replacement attempt"
    );
    assert!(matches!(
        step_connect(&client, inet(remote.port), &guard),
        StepOutcome::Yield { .. }
    ));
    let current = payload
        .active_tcp_connect_attempt()
        .expect("replacement connect attempt");
    assert_ne!(stale, current);
    start_raw_tcp_connect_for_active_attempt(&payload);

    let iface = crate::net::namespace::initial_loopback_iface();
    while iface.pop_ingress().is_some() {}
    let mut ctx = PollContext::new_with_table(smoltcp::time::Instant::ZERO, payload.socket_table());
    assert!(
        ctx.poll_tcp_egress_one_for_flow(
            &client,
            stale.generation(),
            local,
            remote,
            iface,
            &guard,
        )
        .is_none(),
        "an old handshake must not dispatch the replacement attempt's SYN"
    );
    assert!(iface.pop_ingress().is_none());
    assert!(
        ctx.poll_egress_one(&client, iface, &guard).is_some(),
        "the replacement attempt itself remains dispatchable"
    );
    assert!(
        iface.pop_ingress().is_some(),
        "replacement SYN reaches loopback"
    );
    while iface.pop_ingress().is_some() {}

    let cleanup_called = core::cell::Cell::new(false);
    let clear_called = core::cell::Cell::new(false);
    assert_eq!(
        payload.reset_tcp_connection(
            stale.generation(),
            local,
            remote,
            |_, _| {
                cleanup_called.set(true);
                true
            },
            || clear_called.set(true),
        ),
        Err(Errno::ECANCELED),
        "a stale AF_UNSPEC observation must not reset the replacement flow"
    );
    assert!(!cleanup_called.get());
    assert!(!clear_called.get());
    assert_eq!(
        payload.active_tcp_connect_attempt(),
        Some(current),
        "generation rejection must leave the replacement flow intact"
    );
    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Done(())
    );
}

#[test]
fn tcp_unspec_disconnect_preserves_a_foreign_reverse_tuple_owner() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_298, 50_298);
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));

    let payload = client.acquire_operational().expect("client payload");
    let reverse_key = ConnectionKey::new(remote, local);
    payload
        .socket_table()
        .withdraw_tcp_connection(reverse_key)
        .expect("remove the original reverse endpoint");
    let foreign = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("foreign reverse tuple owner");
    payload
        .socket_table()
        .insert_tcp_connection(reverse_key, foreign.clone())
        .expect("install foreign reverse tuple owner");

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Done(())
    );
    assert!(payload
        .socket_table()
        .lookup_tcp_connection(ConnectionKey::new(local, remote), &guard)
        .is_none());
    assert_eq!(
        payload
            .socket_table()
            .lookup_tcp_connection(reverse_key, &guard)
            .map(|owner| owner.raw()),
        Some(foreign.raw()),
        "AF_UNSPEC must not delete a replacement reverse-key owner"
    );
}

#[test]
fn tcp_unspec_rejects_a_replacement_forward_tuple_owner() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_302, 50_302);
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let peer = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let payload = client.acquire_operational().expect("client payload");
    let table = payload.socket_table();
    let forward_key = ConnectionKey::new(local, remote);
    let reverse_key = ConnectionKey::new(remote, local);
    table
        .withdraw_tcp_connection(forward_key)
        .expect("remove the original forward endpoint");
    table
        .insert_tcp_connection(forward_key, peer.clone())
        .expect("install a replacement forward owner");

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Err(Errno::EAGAIN),
        "a replacement forward owner makes the observed source flow stale"
    );
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote })
    );
    assert_eq!(
        table
            .lookup_tcp_connection(forward_key, &guard)
            .map(|owner| owner.raw()),
        Some(peer.raw())
    );
    assert_eq!(
        table
            .lookup_tcp_connection(reverse_key, &guard)
            .map(|owner| owner.raw()),
        Some(peer.raw()),
        "the replacement's reverse endpoint must remain intact"
    );
    assert_eq!(
        table
            .lookup_tcp_bound(local, &guard)
            .map(|owner| owner.raw()),
        Some(client.raw()),
        "the stale disconnect must not partially remove the old binding"
    );
}

#[test]
fn tcp_unspec_preserves_reverse_when_its_owned_forward_is_missing() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_303, 50_303);
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let peer = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let payload = client.acquire_operational().expect("client payload");
    let table = payload.socket_table();
    let forward_key = ConnectionKey::new(local, remote);
    let reverse_key = ConnectionKey::new(remote, local);
    table
        .withdraw_tcp_connection(forward_key)
        .expect("remove the source-owned forward endpoint");

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Init)
    );
    assert!(table.lookup_tcp_connection(forward_key, &guard).is_none());
    assert_eq!(
        table
            .lookup_tcp_connection(reverse_key, &guard)
            .map(|owner| owner.raw()),
        Some(peer.raw()),
        "a missing forward entry cannot authorize removing the reverse owner"
    );
    assert!(table.lookup_tcp_bound(local, &guard).is_none());
}

#[test]
fn tcp_peer_generation_guard_rejects_same_identity_after_reconnect() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, _local, _remote) = prepare_loopback_connect(40_300, 50_300);
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let peer = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };
    let peer_payload = peer.acquire_operational().expect("peer payload");
    let (stale_generation, peer_local, peer_remote) = peer_payload
        .tcp_flow_snapshot()
        .expect("connected peer flow");

    assert_eq!(
        peer_payload.reset_tcp_connection(
            stale_generation,
            peer_local,
            peer_remote,
            |_, _| true,
            || {},
        ),
        Ok(())
    );
    let TcpConnectProgress::Started(replacement) =
        peer_payload.begin_tcp_connect(peer_local, peer_remote, |local| local, || {})
    else {
        panic!("replacement peer flow");
    };
    assert_ne!(replacement.generation(), stale_generation);

    let stale_operation_ran = core::cell::Cell::new(false);
    assert!(matches!(
        peer_payload.try_with_tcp_flow_generation(
            stale_generation,
            peer_local,
            peer_remote,
            || stale_operation_ran.set(true),
        ),
        TcpFlowGenerationTry::Stale
    ));
    assert!(!stale_operation_ran.get());
}

#[test]
fn tcp_unspec_busy_index_reservation_rolls_back_without_partial_cleanup() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_301, 50_301);
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let payload = client.acquire_operational().expect("client payload");
    let table = payload.socket_table();
    let forward_key = ConnectionKey::new(local, remote);
    let reverse_key = ConnectionKey::new(remote, local);

    let competing_reverse = table
        .reserve_tcp_connection_if_owner_for_test(reverse_key, accepted.raw())
        .expect("reserve reverse index")
        .expect("accepted peer owns reverse index");

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Err(Errno::EAGAIN)
    );
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote }),
        "a Busy reverse slot must prevent the AF_UNSPEC state commit"
    );
    drop(competing_reverse);

    assert_eq!(
        table
            .lookup_tcp_connection(forward_key, &guard)
            .map(|owner| owner.raw()),
        Some(client.raw())
    );
    assert_eq!(
        table
            .lookup_tcp_connection(reverse_key, &guard)
            .map(|owner| owner.raw()),
        Some(accepted.raw())
    );
    assert_eq!(
        table
            .lookup_tcp_bound(local, &guard)
            .map(|owner| owner.raw()),
        Some(client.raw())
    );

    let competing = table
        .reserve_tcp_connection_if_owner_for_test(forward_key, client.raw())
        .expect("reserve forward index")
        .expect("client owns forward index");

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Err(Errno::EAGAIN)
    );
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote }),
        "Busy must prevent the AF_UNSPEC state commit"
    );
    drop(competing);

    assert_eq!(
        table
            .lookup_tcp_connection(forward_key, &guard)
            .map(|owner| owner.raw()),
        Some(client.raw())
    );
    assert!(table.lookup_tcp_connection(reverse_key, &guard).is_some());
    assert_eq!(
        table
            .lookup_tcp_bound(local, &guard)
            .map(|owner| owner.raw()),
        Some(client.raw())
    );

    assert_eq!(
        step_connect(&client, KernelSockAddr::Unspec, &guard),
        StepOutcome::Done(())
    );
    assert!(table.lookup_tcp_connection(forward_key, &guard).is_none());
    assert!(table.lookup_tcp_connection(reverse_key, &guard).is_none());
    assert!(table.lookup_tcp_bound(local, &guard).is_none());
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
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            != 0,
        "freeing the receive window must kick the network delegate"
    );

    let source = ScriptedPacketSource::new(std::vec![]);
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: Some(loopback_iface()),
    };
    let accepted_payload = accepted.acquire_operational().expect("accepted payload");
    let accepted_raw = accepted_payload.raw_tcp_socket().expect("accepted raw tcp");
    let mut delegate_steps = 0;
    while accepted_raw.recv_available() < 5 {
        assert!(
            delegate_steps < 8,
            "window update must make the blocked tail readable within a bounded number of polls"
        );
        let outcome = net_delegate_step_once(&driver, &guard);
        assert!(outcome.poll_seen);
        delegate_steps += 1;
        if accepted_raw.recv_available() < 5 {
            assert!(
                crate::net::delegate::net_delegate_queue().peek()
                    & crate::net::delegate::DelegateWireSet::POLL.bits()
                    != 0,
                "an intermediate progress step must keep the delegate runnable"
            );
        }
    }

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
        // The test socket table is process-global; keep this five-port shard
        // disjoint from the fixed 50_19x ports used by neighboring cases.
        assert_eq!(
            step_bind(&client, inet(51_191 + index), &guard),
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
fn tcp_loopback_transfer_caps_total_bidirectional_egress_packets() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) =
        prepare_loopback_connect_with_client_send_buf(40_169, 50_169, 65_536);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let bytes = alloc::vec![0x5a; 65_536];
    assert_eq!(
        step_send_kernel_bytes(&client, &bytes, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(bytes.len())
    );
    assert_eq!(
        step_send_kernel_bytes(&accepted, &bytes, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(bytes.len())
    );

    let transfer = match step_tcp_loopback_transfer(&client, usize::MAX, &guard) {
        StepOutcome::Done(outcome) => outcome,
        other => panic!("unexpected bidirectional transfer outcome: {other:?}"),
    };
    assert!(transfer.bytes_moved > 0);
    assert!(
        transfer.tx_packets <= 64,
        "one transfer step must cap aggregate source+peer egress, got {} packets",
        transfer.tx_packets
    );
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

    let mut ctx =
        PollContext::new_with_table(smoltcp::time::Instant::ZERO, client_payload.socket_table());
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
    ctx = PollContext::new_with_table(
        smoltcp::time::Instant::from_millis(2_000),
        client_payload.socket_table(),
    );
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
