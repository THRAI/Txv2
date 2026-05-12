use super::*;

#[test]
fn step_accept_blocks_when_queue_empty() {
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
        step_bind(&listener, inet(40_139), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let wait = expect_carrier_yield(step_accept(&listener, &guard));

    assert_eq!(wait.carrier(), listener.wait_carriers.accept);
    assert_eq!(
        wait.interest(),
        AcceptWireSet::HAS_PENDING.bits() | AcceptWireSet::BROKEN.bits()
    );
}

#[test]
fn step_process_network_events_respects_budget() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
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
