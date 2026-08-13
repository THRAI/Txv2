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
fn tcp_bound_and_connection_tables_exceed_legacy_256_limit_and_reuse_slots() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let netns = crate::net::create_isolated_net_namespace_for_test("tcp-table-capacity")
        .expect("net namespace")
        .payload_cap()
        .expect("net namespace payload");
    let table = netns.socket_table();
    let mut flows = alloc::vec::Vec::new();

    for offset in 0..300u16 {
        let socket = registry::create_socket_in_namespace(
            SocketKind::Tcp,
            SocketOptionSet::default_tcp(),
            netns.clone(),
        )
        .expect("tcp socket");
        let local = endpoint(20_000 + offset);
        let remote = endpoint(30_000 + offset);
        let key = ConnectionKey::new(local, remote);

        table
            .bind_tcp(local, socket.clone())
            .expect("TCP bound table must exceed the legacy 256-slot limit");
        table
            .insert_tcp_connection(key, socket.clone())
            .expect("TCP connection table must exceed the legacy 256-slot limit");
        flows.push((socket, local, key));
    }

    for (socket, local, key) in &flows {
        let bound = table
            .withdraw_tcp_bound(*local)
            .expect("withdraw TCP bound entry");
        let connection = table
            .withdraw_tcp_connection(*key)
            .expect("withdraw TCP connection entry");
        assert_eq!(bound.raw(), socket.raw());
        assert_eq!(connection.raw(), socket.raw());
    }

    let (socket, local, key) = &flows[0];
    table
        .bind_tcp(*local, socket.clone())
        .expect("released TCP bound capacity must be reusable");
    table
        .insert_tcp_connection(*key, socket.clone())
        .expect("released TCP connection capacity must be reusable");

    assert_eq!(
        table
            .lookup_tcp_bound(*local, &guard)
            .expect("reused TCP bound entry")
            .raw(),
        socket.raw()
    );
    assert_eq!(
        table
            .lookup_tcp_connection(*key, &guard)
            .expect("reused TCP connection entry")
            .raw(),
        socket.raw()
    );
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
