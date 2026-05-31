use super::*;
use tx_substrate::zone::Cap;

struct DelegateLoopbackDriver<'a> {
    now: smoltcp::time::Instant,
    source: &'a ScriptedPacketSource,
    iface: &'a LoopbackIface,
}

impl NetDelegateDriver for DelegateLoopbackDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        self.now
    }

    fn packet_source(&self) -> &dyn PacketSource {
        self.source
    }

    fn loopback_iface(&self) -> Option<&LoopbackIface> {
        Some(self.iface)
    }

    fn loopback_budget(&self) -> LoopbackPollBudget {
        LoopbackPollBudget {
            tcp_connecting: 8,
            tcp_connected: 8,
            udp_bound: 8,
            raw_icmp: 8,
            packet_budget: 8,
            tcp_transfer_bytes: 64,
        }
    }
}

#[test]
fn net_delegate_poll_drives_tcp_loopback_connect() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    clear_delegate_queue();
    let (client, listener, local, remote) = prepare_loopback_connecting(40_194, 50_194);
    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = DelegateLoopbackDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: loopback_iface(),
    };
    let mut task = std::boxed::Box::pin(net_delegate_task_loop(
        &driver,
        NetDelegateTaskConfig::run_steps(1),
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    let report = match task.as_mut().poll(&mut cx) {
        Poll::Ready(report) => report,
        Poll::Pending => panic!("delegate task should consume pre-fired connect poll"),
    };
    let guard = tx_substrate::epoch::guard();

    assert_eq!(report.ready_steps, 1);
    assert!(report.runtime.poll_seen);
    assert_eq!(report.runtime.loopback.tcp_connect_attempted, 1);
    assert_eq!(report.runtime.loopback.tcp_connected, 1);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(ConnectionKey::new(local, remote), &guard)
        .is_some());
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(accepted) if accepted.peer == local && accepted.local == remote
    ));
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote })
    );
}

#[test]
fn net_delegate_poll_drives_tcp_loopback_send_recv() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    clear_delegate_queue();
    let (client, listener, _local, _remote) = prepare_loopback_connecting(40_195, 50_195);
    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = DelegateLoopbackDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: loopback_iface(),
    };
    drive_one_delegate_step(&driver);
    let accepted = {
        let guard = tx_substrate::epoch::guard();
        let accepted = match step_accept(&listener, &guard) {
            StepOutcome::Done(accepted) => accepted.child,
            _ => panic!("unexpected accept outcome"),
        };
        clear_delegate_queue();
        assert_eq!(
            step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
            StepOutcome::Done(5)
        );
        accepted
    };

    let report = drive_one_delegate_step(&driver);
    let guard = tx_substrate::epoch::guard();

    assert!(report.runtime.poll_seen);
    assert_eq!(report.runtime.loopback.tcp_bytes_moved, 5);
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert_eq!(
        step_recv(&accepted, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
}

#[test]
fn net_delegate_poll_drives_udp_loopback_send_recv() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_loopback_test_state();
    clear_delegate_queue();
    let (server, _client) = {
        let guard = tx_substrate::epoch::guard();
        let server = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Udp,
            SocketOptionSet::default_udp(),
        )
        .expect("server");
        let client = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Udp,
            SocketOptionSet::default_udp(),
        )
        .expect("client");
        assert_eq!(
            step_bind(&server, inet(40_196), &guard),
            StepOutcome::Done(())
        );
        assert_eq!(
            step_bind(&client, inet(50_196), &guard),
            StepOutcome::Done(())
        );
        assert_eq!(
            step_connect(&client, inet(40_196), &guard),
            StepOutcome::Done(())
        );
        clear_delegate_queue();
        assert_eq!(
            step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
            StepOutcome::Done(5)
        );
        (server, client)
    };
    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = DelegateLoopbackDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: loopback_iface(),
    };

    let report = drive_one_delegate_step(&driver);
    let guard = tx_substrate::epoch::guard();

    assert!(report.runtime.poll_seen);
    assert_eq!(report.runtime.loopback.udp_bytes_moved, 5);
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert_eq!(
        step_recv(&server, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
}

fn drive_one_delegate_step(
    driver: &dyn NetDelegateDriver,
) -> crate::net::delegate::NetDelegateTaskReport {
    let mut task = std::boxed::Box::pin(net_delegate_task_loop(
        driver,
        NetDelegateTaskConfig::run_steps(1),
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match task.as_mut().poll(&mut cx) {
        Poll::Ready(report) => report,
        Poll::Pending => panic!("delegate task should consume pre-fired poll"),
    }
}

fn prepare_loopback_connecting(
    server_port: u16,
    client_port: u16,
) -> (
    Cap<SocketIdentity>,
    Cap<SocketIdentity>,
    IpEndpoint,
    IpEndpoint,
) {
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
        step_bind(&listener, inet(server_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));
    assert_eq!(
        step_bind(&client, inet(client_port), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(server_port), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    (
        client,
        listener,
        endpoint(client_port),
        endpoint(server_port),
    )
}

fn clear_delegate_queue() {
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
}
