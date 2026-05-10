use super::*;
use crate::net::ShutdownOutcome;
use tx_substrate::zone::Cap;

struct GracefulShutdownDriver<'a> {
    source: &'a ScriptedPacketSource,
    iface: &'a LoopbackIface,
}

impl NetDelegateDriver for GracefulShutdownDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::ZERO
    }

    fn packet_source(&self) -> &dyn PacketSource {
        self.source
    }

    fn loopback_iface(&self) -> Option<&LoopbackIface> {
        Some(self.iface)
    }

    fn loopback_budget(&self) -> LoopbackPollBudget {
        LoopbackPollBudget {
            tcp_connecting: 256,
            tcp_connected: 256,
            udp_bound: 0,
            raw_icmp: 0,
            packet_budget: 32,
            tcp_transfer_bytes: 64,
        }
    }
}

#[test]
fn tcp_shutdown_write_calls_raw_close_and_kicks_delegate_poll() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let (client, _accepted) = prepare_connected_loopback_pair(40_197, 50_197);
    clear_delegate_queue();
    let guard = tx_substrate::epoch::guard();

    assert_eq!(
        step_shutdown(&client, SockShutdownCmd::Send, &guard),
        StepOutcome::Done(ShutdownOutcome {
            recv_shutdown: false,
            send_shutdown: true,
            recv_woken: 0,
            send_woken: 0,
            delegate_kicked: true,
        })
    );
    let payload = client.acquire_operational().expect("client payload");
    assert!(payload.shutdown_wr());
    assert!(payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .is_send_closed());
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            != 0
    );
}

#[test]
fn net_delegate_poll_drives_tcp_fin_to_peer_eof() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let (client, accepted) = prepare_connected_loopback_pair(40_198, 50_198);
    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = GracefulShutdownDriver {
        source: &source,
        iface: loopback_iface(),
    };
    {
        let guard = tx_substrate::epoch::guard();
        clear_delegate_queue();
        assert!(matches!(
            step_shutdown(&client, SockShutdownCmd::Send, &guard),
            StepOutcome::Done(outcome)
                if outcome.send_shutdown && outcome.delegate_kicked
        ));
    }

    let report = drive_one_delegate_step(&driver);
    let guard = tx_substrate::epoch::guard();

    assert!(report.runtime.poll_seen);
    assert!(report.runtime.loopback.tcp_transfer_attempted >= 1);
    assert!(report.runtime.loopback.tx_packets >= 1);
    let accepted_payload = accepted.acquire_operational().expect("accepted payload");
    assert!(accepted_payload.tcp_recv_closed_by_peer());
    assert!(accepted.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0);
    assert_eq!(
        step_recv(&accepted, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
    assert!(matches!(
        step_poll_ready(&accepted, &guard),
        StepOutcome::Done(mask) if mask.contains(PollMask::IN | PollMask::RDHUP)
    ));
}

fn prepare_connected_loopback_pair(
    server_port: u16,
    client_port: u16,
) -> (Cap<SocketIdentity>, Cap<SocketIdentity>) {
    let (client, listener) = prepare_loopback_connecting(server_port, client_port);
    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = GracefulShutdownDriver {
        source: &source,
        iface: loopback_iface(),
    };
    drive_one_delegate_step(&driver);
    let guard = tx_substrate::epoch::guard();
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };
    (client, accepted)
}

fn prepare_loopback_connecting(
    server_port: u16,
    client_port: u16,
) -> (Cap<SocketIdentity>, Cap<SocketIdentity>) {
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
            shape: YieldShape::OnCarrier { .. },
            ..
        }
    ));
    (client, listener)
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

fn clear_delegate_queue() {
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
}
