use super::*;
use crate::net::ShutdownOutcome;
use tx_substrate::zone::Cap;

struct GracefulShutdownDriver<'a> {
    source: &'a ScriptedPacketSource,
    iface: &'a LoopbackIface,
    tcp_connected_budget: usize,
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
            tcp_connected: self.tcp_connected_budget,
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
        tcp_connected_budget: 256,
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

#[test]
fn net_delegate_poll_drives_close_body_then_peer_eof() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let (client, accepted) = prepare_connected_loopback_pair(40_199, 50_199);
    {
        let guard = tx_substrate::epoch::guard();
        clear_delegate_queue();
        assert_eq!(
            step_send_kernel_bytes(&accepted, b"hello", SendRecvFlags::empty(), &guard),
            StepOutcome::Done(5)
        );
        assert!(matches!(
            step_socket_close(&accepted, &guard),
            StepOutcome::Done(outcome) if !outcome.payload_taken
        ));
    }

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = GracefulShutdownDriver {
        source: &source,
        iface: loopback_iface(),
        tcp_connected_budget: 256,
    };
    for _ in 0..8 {
        if crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            == 0
        {
            break;
        }
        let _ = drive_one_delegate_step(&driver);
    }

    let guard = tx_substrate::epoch::guard();
    let mut body = [0_u8; 5];
    assert!(matches!(
        step_recv_kernel_bytes(
            &client,
            &mut body,
            SendRecvFlags::empty(),
            &guard
        ),
        StepOutcome::Done(outcome) if outcome.bytes == 5
    ));
    assert_eq!(&body, b"hello");
    assert_eq!(
        step_recv(&client, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
}

#[test]
fn out_of_order_fin_does_not_publish_eof_before_missing_bytes_arrive() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let (client, accepted) = prepare_connected_loopback_pair(40_201, 50_201);
    let accepted_payload = accepted.acquire_operational().expect("accepted payload");
    let accepted_raw = accepted_payload.raw_tcp_socket().expect("accepted raw tcp");
    accepted_raw.close();
    let mut future_fin = accepted_raw
        .dispatch_segment()
        .expect("accepted endpoint should dispatch FIN");
    assert_eq!(future_fin.tcp.control, smoltcp::wire::TcpControl::Fin);

    // Model packet reordering: the FIN sequence number is beyond the next
    // byte expected by the client, so smoltcp must defer it until the missing
    // stream bytes arrive.
    future_fin.tcp.seq_number = future_fin.tcp.seq_number + 8;

    let client_payload = client.acquire_operational().expect("client payload");
    let client_raw = client_payload.raw_tcp_socket().expect("client raw tcp");
    assert_eq!(
        client_raw.protocol_state(),
        smoltcp::socket::tcp::State::Established
    );

    let publish = client_raw.process_segment(&future_fin);

    assert_eq!(
        client_raw.protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
    assert!(
        !publish.recv_closed,
        "an ignored out-of-order FIN must not publish EOF"
    );
    assert!(
        !client_raw.is_recv_closed(),
        "an ignored out-of-order FIN must not become sticky EOF"
    );
}

#[test]
fn out_of_order_data_preserves_immediate_ack_until_egress_accepts_it() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let (client, accepted) = prepare_connected_loopback_pair(40_202, 50_202);
    let accepted_payload = accepted.acquire_operational().expect("accepted payload");
    let accepted_raw = accepted_payload.raw_tcp_socket().expect("accepted raw tcp");
    assert_eq!(
        accepted_raw
            .enqueue_tx_bytes(b"abcdefgh")
            .expect("queue server payload")
            .bytes,
        8
    );
    let first = accepted_raw
        .dispatch_segment()
        .expect("server payload segment");
    assert_eq!(first.payload, b"abcdefgh");

    // Deliver a copy one segment into the future. smoltcp returns an
    // immediate duplicate/SACK reply for this hole and records that ACK as
    // sent while constructing it.
    let mut future = first.clone();
    future.tcp.seq_number = future.tcp.seq_number + first.payload_len();
    let client_payload = client.acquire_operational().expect("client payload");
    let client_raw = client_payload.raw_tcp_socket().expect("client raw tcp");
    let publish = client_raw.process_segment(&future);
    assert_eq!(publish.recv_bytes_added, 0);
    assert_eq!(
        client_raw.poll_at(smoltcp::time::Instant::ZERO),
        smoltcp::socket::PollAt::Now,
        "the immediate hole ACK must remain visible as egress work"
    );

    // Backpressure must not consume the immediate reply.
    let refused = client_raw.dispatch_segment_via(smoltcp::time::Instant::ZERO, 1500, |_| false);
    assert_eq!(refused.emitted, Some(false));

    let mut hole_ack = None;
    let accepted_ack =
        client_raw.dispatch_segment_via(smoltcp::time::Instant::ZERO, 1500, |segment| {
            hole_ack = segment.tcp.ack_number;
            true
        });
    assert_eq!(accepted_ack.emitted, Some(true));
    assert_eq!(hole_ack, Some(first.tcp.seq_number));

    // Filling the gap makes both payloads contiguous and requires a new
    // cumulative immediate ACK covering all sixteen bytes.
    let publish = client_raw.process_segment(&first);
    assert_eq!(publish.recv_bytes_added, 16);
    let cumulative = client_raw
        .dispatch_segment()
        .expect("gap-filling cumulative ACK");
    assert_eq!(
        cumulative.tcp.ack_number,
        Some(first.tcp.seq_number + first.payload_len() * 2)
    );
    assert_eq!(client_raw.recv_available(), 16);
}

#[test]
fn net_delegate_timer_tick_also_drives_connected_tcp() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let (client, accepted) = prepare_connected_loopback_pair(40_200, 50_200);
    {
        let guard = tx_substrate::epoch::guard();
        clear_delegate_queue();
        assert_eq!(
            step_send_kernel_bytes(&client, b"tick", SendRecvFlags::empty(), &guard),
            StepOutcome::Done(4)
        );
    }

    // Model a supervised TCP deadline. TICK maintains the backlog and also
    // drives the established/closing lane, without touching handshakes.
    clear_delegate_queue();
    crate::net::delegate::net_delegate_kick_tick();
    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = GracefulShutdownDriver {
        source: &source,
        iface: loopback_iface(),
        tcp_connected_budget: 256,
    };
    let report = drive_one_delegate_step(&driver);
    assert!(report.runtime.tick_seen);
    assert!(!report.runtime.poll_seen);
    assert!(report.runtime.loopback.tcp_transfer_attempted >= 1);

    let guard = tx_substrate::epoch::guard();
    let mut bytes = [0_u8; 4];
    assert!(matches!(
        step_recv_kernel_bytes(&accepted, &mut bytes, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(outcome) if outcome.bytes == 4
    ));
    assert_eq!(&bytes, b"tick");
}

#[test]
fn net_delegate_budget_eventually_services_connections_after_first_window() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    // Every connection pair contributes both endpoints to the snapshot. With
    // 18 pairs, the final two pairs are beyond a 32-entry fixed scan window.
    let mut pairs = std::vec::Vec::new();
    for index in 0..18_u16 {
        pairs.push(prepare_connected_loopback_pair(
            41_000 + index,
            51_000 + index,
        ));
    }

    clear_delegate_queue();
    {
        let guard = tx_substrate::epoch::guard();
        for (client, _) in &pairs {
            assert_eq!(
                step_send_kernel_bytes(client, b"x", SendRecvFlags::empty(), &guard),
                StepOutcome::Done(1)
            );
        }
    }

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = GracefulShutdownDriver {
        source: &source,
        iface: loopback_iface(),
        tcp_connected_budget: 32,
    };
    for _ in 0..16 {
        if crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            == 0
        {
            break;
        }
        let _ = drive_one_delegate_step(&driver);
    }

    for (index, (_, accepted)) in pairs.iter().enumerate() {
        assert_eq!(
            accepted
                .acquire_operational()
                .expect("accepted payload")
                .io_snapshot()
                .recv_len,
            1,
            "connection pair {index} was starved by the fixed scan window"
        );
    }
}

#[test]
fn net_delegate_budget_delivers_concurrent_close_bodies_and_eof() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    clear_delegate_queue();

    let mut pairs = std::vec::Vec::new();
    for index in 0..40_u16 {
        pairs.push(prepare_connected_loopback_pair(
            42_000 + index,
            52_000 + index,
        ));
    }

    clear_delegate_queue();
    {
        let guard = tx_substrate::epoch::guard();
        for (_, accepted) in &pairs {
            assert_eq!(
                step_send_kernel_bytes(accepted, b"ok", SendRecvFlags::empty(), &guard),
                StepOutcome::Done(2)
            );
            assert!(matches!(
                step_socket_close(accepted, &guard),
                StepOutcome::Done(outcome) if !outcome.payload_taken
            ));
        }
    }

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = GracefulShutdownDriver {
        source: &source,
        iface: loopback_iface(),
        tcp_connected_budget: 32,
    };
    for _ in 0..64 {
        if crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            == 0
        {
            break;
        }
        let _ = drive_one_delegate_step(&driver);
    }

    let guard = tx_substrate::epoch::guard();
    for (index, (client, _)) in pairs.iter().enumerate() {
        let mut body = [0_u8; 2];
        assert!(
            matches!(
                step_recv_kernel_bytes(client, &mut body, SendRecvFlags::empty(), &guard),
                StepOutcome::Done(outcome) if outcome.bytes == 2
            ),
            "connection pair {index} did not receive its response body"
        );
        assert_eq!(&body, b"ok");
        assert_eq!(
            step_recv(client, 1, SendRecvFlags::empty(), &guard),
            StepOutcome::Done(0),
            "connection pair {index} did not observe EOF"
        );
    }
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
        tcp_connected_budget: 256,
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
            shape: YieldShape::OnWaitSource { .. },
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
