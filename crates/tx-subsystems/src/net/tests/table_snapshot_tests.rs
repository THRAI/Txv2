use super::*;

#[test]
fn socket_table_snapshot_tcp_bound_reports_bound_connecting_client() {
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
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("client");

    assert_eq!(
        step_bind(&listener, inet(40_188), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    assert_eq!(
        step_bind(&client, inet(50_188), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(40_188), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    let snapshot = SOCKET_TABLE.snapshot_tcp_bound(&guard);

    assert!(snapshot.iter().any(|socket| socket.raw() == client.raw()));
    assert!(snapshot.iter().any(|socket| socket.raw() == listener.raw()));
}

#[test]
fn socket_table_snapshot_tcp_connections_can_report_bidirectional_keys() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let first = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("first");
    let second = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("second");
    let first_ep = endpoint(50_189);
    let second_ep = endpoint(40_189);

    SOCKET_TABLE
        .insert_tcp_connection_pair(
            ConnectionKey::new(first_ep, second_ep),
            first.clone(),
            ConnectionKey::new(second_ep, first_ep),
            second.clone(),
        )
        .expect("connection pair");

    let snapshot = SOCKET_TABLE.snapshot_tcp_connections(&guard);

    assert!(snapshot.iter().any(|socket| socket.raw() == first.raw()));
    assert!(snapshot.iter().any(|socket| socket.raw() == second.raw()));
}

#[test]
fn socket_table_snapshot_udp_bound_reports_bound_udp_socket() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp");

    assert_eq!(step_bind(&udp, inet(40_190), &guard), StepOutcome::Done(()));

    let snapshot = SOCKET_TABLE.snapshot_udp_bound(&guard);

    assert!(snapshot.iter().any(|socket| socket.raw() == udp.raw()));
}
