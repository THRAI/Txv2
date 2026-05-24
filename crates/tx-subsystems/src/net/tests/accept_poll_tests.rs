use super::*;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    lock
}

#[test]
fn step_accept_returns_child_socket_and_clears_when_empty() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    let local = endpoint(40_138);
    let remote = endpoint(50_138);

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
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    let payload = listener.acquire_operational().expect("payload");
    assert_eq!(payload.accept_queue_len(), 1);
    assert_eq!(payload.io_snapshot().accept_pending, 1);
    assert!(listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits() != 0);

    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted,
        _ => panic!("accept should return queued child"),
    };
    assert_eq!(accepted.local, local);
    assert_eq!(accepted.peer, remote);
    assert_eq!(payload.accept_queue_len(), 0);
    assert_eq!(payload.io_snapshot().accept_pending, 0);
    assert_eq!(
        listener.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits(),
        0
    );
    assert!(matches!(
        accepted
            .child
            .acquire_operational()
            .expect("child payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local: child_local, remote: child_remote })
            if child_local == local && child_remote == remote
    ));
}

#[test]
fn step_accept_does_not_copy_ipv4_multicast_membership() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    let local = endpoint(40_139);
    let remote = endpoint(50_139);
    let multicast = Ipv4MulticastGroup::new(0, Ipv4Address::new([224, 0, 0, 0]));

    let listener_payload = listener.acquire_operational().expect("listener payload");
    assert_eq!(
        listener_payload.join_ipv4_multicast_group(multicast),
        Ok(())
    );
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
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));

    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("accept should return queued child"),
    };
    let accepted_payload = accepted.acquire_operational().expect("accepted payload");
    assert_eq!(
        accepted_payload.leave_ipv4_multicast_group(multicast),
        Err(Errno::EADDRNOTAVAIL)
    );
    assert_eq!(
        listener_payload.leave_ipv4_multicast_group(multicast),
        Ok(())
    );
}

#[test]
fn step_accept_blocks_when_queue_empty() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");

    assert_eq!(
        step_bind(&listener, inet(40_139), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let wait = expect_carrier_yield(step_accept(&listener, &guard));

    assert_eq!(wait.source_id(), listener.wait_carriers.accept);
    assert_eq!(
        wait.interest(),
        AcceptWireSet::HAS_PENDING.bits() | AcceptWireSet::BROKEN.bits()
    );
}

#[test]
fn step_process_network_events_respects_budget() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let packets = (0..(NET_EVENT_BUDGET + 3))
        .map(|_| PacketDispatch::Unsupported)
        .collect();
    let source = ScriptedPacketSource::new(packets);

    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, NET_EVENT_BUDGET);
    assert_eq!(outcome.sockets_touched, 0);
}

#[test]
fn execution_poll_reports_socket_readiness() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let local = inet(40_015);

    assert_eq!(step_bind(&udp, local, &guard), StepOutcome::Done(()));
    udp.readiness.fire_recv(RecvWireSet::HAS_DATA);

    let mask = match step_poll_ready(&udp, &guard) {
        StepOutcome::Done(mask) => mask,
        other => panic!("unexpected poll outcome: {other:?}"),
    };
    assert!(mask.contains(PollMask::IN));
    assert!(mask.contains(PollMask::OUT));

    assert!(udp.take_payload().is_some());
    let mask = match step_poll_ready(&udp, &guard) {
        StepOutcome::Done(mask) => mask,
        other => panic!("unexpected poll without payload outcome: {other:?}"),
    };
    assert!(mask.contains(PollMask::HUP));
    assert!(mask.contains(PollMask::ERR));
}

#[test]
fn execution_poll_udp_readiness_tracks_io_snapshot() {
    let _lock = setup();
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let local = endpoint(40_016);

    assert_eq!(
        step_bind(&udp, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    let payload = udp.acquire_operational().expect("udp payload");
    assert!(payload.record_recv_payload(endpoint(50_016), local, b"ready".to_vec()));
    assert_eq!(
        udp.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits(),
        0,
        "recording bytes alone must be enough for poll readiness"
    );

    let mask = match step_poll_ready(&udp, &guard) {
        StepOutcome::Done(mask) => mask,
        other => panic!("unexpected poll outcome: {other:?}"),
    };
    assert!(mask.contains(PollMask::IN));
    assert!(mask.contains(PollMask::OUT));
}
