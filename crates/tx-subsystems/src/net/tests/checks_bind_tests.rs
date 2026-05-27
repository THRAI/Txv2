use super::*;
use crate::net::{execution, ShutdownOutcome, SockFlags};

#[test]
fn checks_reject_udp_listen_and_write_after_shutdown() {
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

    assert_eq!(
        require_socket_listen_target(&udp, 4, &guard).map(|_| ()),
        Err(Errno::EOPNOTSUPP)
    );
    assert_eq!(
        step_shutdown(&udp, SockShutdownCmd::Send, &guard),
        StepOutcome::Done(ShutdownOutcome {
            recv_shutdown: false,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 0,
            delegate_kicked: false,
        })
    );
    assert_eq!(
        require_socket_write_target(&udp, SendRecvFlags::empty(), &guard).map(|_| ()),
        Err(Errno::EPIPE)
    );
}

#[test]
fn execution_socket_create_installs_matching_payload() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let valid = ValidSocketType::validate(2, 1, 6).expect("valid tcp");
    let socket = match step_socket_create(valid, &guard) {
        StepOutcome::Done(socket) => socket,
        other => panic!("unexpected create outcome: {other:?}"),
    };

    assert_eq!(socket.kind, SocketKind::Tcp);
    let payload = socket.acquire_operational().expect("payload");
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Init)
    );
}

#[test]
fn socket_create_facade_validates_raw_args_and_preserves_flags() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let type_with_flags = 1 | SockFlags::SOCK_NONBLOCK.bits() | SockFlags::SOCK_CLOEXEC.bits();
    let output = match socket_create_facade(2, type_with_flags, 6, &guard) {
        StepOutcome::Done(output) => output,
        _ => panic!("unexpected create facade outcome"),
    };

    assert_eq!(output.handle.identity.kind, SocketKind::Tcp);
    assert!(output.handle.flags.nonblock);
    assert!(output.handle.flags.cloexec);
    assert!(matches!(
        socket_create_facade(99, 1, 0, &guard),
        StepOutcome::Err(Errno::EAFNOSUPPORT)
    ));
    assert!(matches!(
        socket_create_facade(2, 999, 0, &guard),
        StepOutcome::Err(Errno::EINVAL)
    ));
}

#[test]
fn execution_bind_updates_protocol_and_socket_table() {
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
    let local = inet(40_011);

    assert_eq!(step_bind(&tcp, local, &guard), StepOutcome::Done(()));
    let payload = tcp.acquire_operational().expect("payload");
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Bound {
            local: local.as_ip_endpoint(),
        })
    );
    let registered = SOCKET_TABLE
        .lookup_tcp_bound(local.as_ip_endpoint(), &guard)
        .expect("tcp bound entry");
    assert_eq!(registered.raw(), tcp.raw());
}

#[test]
fn socket_facade_routes_bind_listen_and_poll_to_steps() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let output = match socket_create_facade(2, 1, 6, &guard) {
        StepOutcome::Done(output) => output,
        _ => panic!("socket create facade should succeed"),
    };
    let local = inet(40_016);

    assert_eq!(
        output.handle.bind_capability(local).bind(&guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        socket_listen_facade(output.handle.listen_capability(16), &guard),
        StepOutcome::Done(())
    );
    output
        .handle
        .identity
        .readiness
        .fire_accept(AcceptWireSet::HAS_PENDING);

    let mask = match socket_poll_ready_facade(output.handle.poll_capability(PollMask::IN), &guard) {
        StepOutcome::Done(mask) => mask,
        _ => panic!("socket poll facade should succeed"),
    };
    assert!(mask.contains(PollMask::IN));
    assert!(!mask.contains(PollMask::OUT));
}

#[test]
fn execution_bind_rejects_duplicate_local_endpoint() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let first = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("first tcp");
    let second = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("second tcp");
    let local = inet(40_012);

    assert_eq!(step_bind(&first, local, &guard), StepOutcome::Done(()));
    assert_eq!(
        step_bind(&second, local, &guard),
        StepOutcome::Err(Errno::EADDRINUSE)
    );
}

#[test]
fn execution_bind_rejects_nonlocal_ipv4_address() {
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
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let nonlocal = Ipv4Address::new([10, 255, 254, 253]);

    assert_eq!(
        step_bind(&tcp, inet_addr(40_198, nonlocal), &guard),
        StepOutcome::Err(Errno::EADDRNOTAVAIL)
    );
    assert_eq!(
        step_bind(&udp, inet_addr(40_199, nonlocal), &guard),
        StepOutcome::Err(Errno::EADDRNOTAVAIL)
    );
    assert_eq!(
        step_bind(&tcp, any_inet(40_198), &guard),
        StepOutcome::Done(())
    );
}

#[test]
fn execution_tcp_bind_rejects_wildcard_exact_port_overlap() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let wildcard = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("wildcard tcp");
    let exact = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("exact tcp");
    let port = 40_196;

    assert_eq!(
        step_bind(&wildcard, any_inet(port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&exact, inet(port), &guard),
        StepOutcome::Err(Errno::EADDRINUSE)
    );
}

#[test]
fn execution_udp_bind_rejects_wildcard_exact_port_overlap() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let wildcard = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("wildcard udp");
    let exact = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("exact udp");
    let port = 40_197;

    assert_eq!(
        step_bind(&wildcard, any_inet(port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&exact, inet(port), &guard),
        StepOutcome::Err(Errno::EADDRINUSE)
    );
}

#[test]
fn execution_listen_promotes_tcp_bound_socket() {
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
    let local = inet(40_013);

    assert_eq!(step_bind(&tcp, local, &guard), StepOutcome::Done(()));
    assert_eq!(step_listen(&tcp, 4096, &guard), StepOutcome::Done(()));
    let payload = tcp.acquire_operational().expect("payload");
    assert_eq!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Listening {
            local: local.as_ip_endpoint(),
            backlog_limit: execution::SOMAXCONN_STAGING,
        })
    );
    let registered = SOCKET_TABLE
        .lookup_tcp_listener(local.as_ip_endpoint(), &guard)
        .expect("tcp listener entry");
    assert_eq!(registered.raw(), tcp.raw());
}

#[test]
fn socket_table_lookup_tcp_connection_by_four_tuple() {
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
    let key = ConnectionKey::new(endpoint(40_120), endpoint(50_120));

    SOCKET_TABLE
        .insert_tcp_connection(key, tcp.clone())
        .expect("connection insert");
    let found = SOCKET_TABLE
        .lookup_tcp_connection(key, &guard)
        .expect("connection lookup");

    assert_eq!(found.raw(), tcp.raw());
}

#[test]
fn socket_table_listener_lookup_prefers_exact_over_wildcard() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let guard = tx_substrate::epoch::guard();
    let wildcard = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("wildcard listener");
    let exact = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("exact listener");
    let port = 40_121;

    SOCKET_TABLE
        .listen_tcp(
            IpEndpoint::new(Ipv4Address::UNSPECIFIED, port),
            wildcard.clone(),
        )
        .expect("wildcard listener insert");
    SOCKET_TABLE
        .listen_tcp(IpEndpoint::new(Ipv4Address::LOOPBACK, port), exact.clone())
        .expect("exact listener insert");

    let found = SOCKET_TABLE
        .lookup_tcp_listener_addr(Ipv4Address::LOOPBACK, port, &guard)
        .expect("exact listener lookup");
    assert_eq!(found.raw(), exact.raw());

    let wildcard_found = SOCKET_TABLE
        .lookup_tcp_listener_addr(Ipv4Address::new([10, 0, 0, 7]), port, &guard)
        .expect("wildcard listener lookup");
    assert_eq!(wildcard_found.raw(), wildcard.raw());
}

#[test]
fn execution_connect_blocks_tcp_and_completes_udp() {
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
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let remote = inet(40_014);

    let wait = expect_carrier_yield(step_connect(&tcp, remote, &guard));
    assert_ne!(wait.source_id(), tcp.raw() as u64);
    assert_eq!(wait.source_id(), tcp.wait_carriers.send);
    assert!(crate::wait_source::wait_on_token(wait).is_some());
    assert!(wait.interest() & SendWireSet::SPACE.bits() != 0);
    assert!(wait.interest() & SendWireSet::BROKEN.bits() != 0);
    assert_eq!(
        tcp.acquire_operational()
            .expect("tcp payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting {
            local: IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
            remote: remote.as_ip_endpoint(),
        })
    );

    assert_eq!(step_connect(&udp, remote, &guard), StepOutcome::Done(()));
    assert_eq!(
        udp.acquire_operational()
            .expect("udp payload")
            .protocol_snapshot(),
        SocketProtocol::Udp(UdpInner::Connected {
            local: IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
            remote: remote.as_ip_endpoint(),
        })
    );
}

#[test]
fn socket_nonblocking_driver_maps_blocked_connect_to_eagain() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let output = {
        let guard = tx_substrate::epoch::guard();
        match socket_create_facade(2, 1, 6, &guard) {
            StepOutcome::Done(output) => output,
            _ => panic!("socket create facade should succeed"),
        }
    };
    let remote = inet(40_017);
    let cap = output.handle.connect_capability(remote);

    assert_eq!(
        drive_socket_nonblocking(|guard| crate::net::facade::socket_connect_facade(cap, guard)),
        StepOutcome::Err(Errno::EAGAIN)
    );
}

#[test]
fn execution_shutdown_fires_broken_readiness_bits() {
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

    assert_eq!(
        step_shutdown(&tcp, SockShutdownCmd::Both, &guard),
        StepOutcome::Done(ShutdownOutcome {
            recv_shutdown: true,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 0,
            delegate_kicked: false,
        })
    );
    let payload = tcp.acquire_operational().expect("payload");
    assert!(payload.shutdown_rd());
    assert!(payload.shutdown_wr());
    assert!(tcp.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0);
    assert!(tcp.readiness.send_wq.peek() & SendWireSet::BROKEN.bits() != 0);
}

#[test]
fn shutdown_fires_same_rawqueue_used_by_wait_token() {
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
    let token = socket_send_wait_token(&tcp);
    let mut future = crate::wait_source::wait_on_token(token).expect("send wq registered");
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(Pin::new(&mut future).poll(&mut cx), Poll::Pending));
    assert_eq!(
        step_shutdown(&tcp, SockShutdownCmd::Send, &guard),
        StepOutcome::Done(ShutdownOutcome {
            recv_shutdown: false,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 1,
            delegate_kicked: false,
        })
    );
    assert!(matches!(
        Pin::new(&mut future).poll(&mut cx),
        Poll::Ready(WaitOutcome::Ready)
    ));
}
