use super::*;
use tx_substrate::wake::MailboxEvent;

#[test]
fn net_delegate_kick_poll_with_post_uses_injected_mailbox_ref_post() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mailbox = alloc::sync::Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let _sub = crate::net::delegate::net_delegate_queue().subscribe(
        crate::net::delegate::DelegateWireSet::POLL.bits(),
        alloc::sync::Arc::downgrade(&mailbox),
        generation,
    );
    let source = crate::net::delegate::net_delegate_queue().source_id();
    let mut injected_posts = 0usize;

    let wakes = crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| {
        injected_posts += 1;
        mailbox.post(event)
    });

    assert_eq!(wakes, 1);
    assert_eq!(injected_posts, 1);
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            != 0
    );
    match mailbox.poll().expect("source fired") {
        MailboxEvent::SourceFired {
            generation: seen_generation,
            source: seen_source,
            interests,
        } => {
            assert_eq!(seen_generation, generation);
            assert_eq!(seen_source, source);
            assert_eq!(
                interests.raw(),
                crate::net::delegate::DelegateWireSet::POLL.bits()
            );
        }
        other => panic!("expected delegate SourceFired, got {other:?}"),
    }
}

#[test]
fn net_delegate_kick_tick_with_post_uses_injected_mailbox_ref_post() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mailbox = alloc::sync::Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let _sub = crate::net::delegate::net_delegate_queue().subscribe(
        crate::net::delegate::DelegateWireSet::TICK.bits(),
        alloc::sync::Arc::downgrade(&mailbox),
        generation,
    );
    let source = crate::net::delegate::net_delegate_queue().source_id();
    let mut injected_posts = 0usize;

    let wakes = crate::net::delegate::net_delegate_kick_tick_with_post(|mailbox, event| {
        injected_posts += 1;
        mailbox.post(event)
    });

    assert_eq!(wakes, 1);
    assert_eq!(injected_posts, 1);
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::TICK.bits()
            != 0
    );
    match mailbox.poll().expect("source fired") {
        MailboxEvent::SourceFired {
            generation: seen_generation,
            source: seen_source,
            interests,
        } => {
            assert_eq!(seen_generation, generation);
            assert_eq!(seen_source, source);
            assert_eq!(
                interests.raw(),
                crate::net::delegate::DelegateWireSet::TICK.bits()
            );
        }
        other => panic!("expected delegate SourceFired, got {other:?}"),
    }
}

#[test]
fn net_delegate_step_once_processes_poll_packet_source() {
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
    let local = endpoint(40_180);
    let remote = endpoint(50_180);
    assert_eq!(
        step_bind(
            &udp,
            KernelSockAddr::V4(SockAddrIn::new(local.port, local.addr)),
            &guard,
        ),
        StepOutcome::Done(())
    );
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Udp(
        UdpPacketEvent::with_payload_len(remote, local, 64),
    )]);
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: None,
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));

    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert!(!outcome.tick_seen);
    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        udp.acquire_operational()
            .expect("udp payload")
            .io_snapshot()
            .recv_len,
        64
    );
}

#[test]
fn net_delegate_step_once_processes_tick_backlog_retransmit() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_181, 50_181);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1),
        source: &source,
        iface: Some(&iface),
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    crate::net::delegate::net_delegate_kick_tick_with_post(|mailbox, event| mailbox.post(event));

    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(!outcome.poll_seen);
    assert!(outcome.tick_seen);
    assert!(outcome.backlog_retransmitted >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(iface.pending_packets(), 1);
}

#[test]
fn net_delegate_step_once_rekicks_after_loopback_progress() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    loopback_iface().clear_for_test_or_bootstrap();
    let (client, listener, _local, _remote) = prepare_loopback_connect_with_client_send_buf(
        40_192,
        50_192,
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
    let bytes = alloc::vec![0x44; TCP_CORK_AUTO_FLUSH_BYTES];
    assert_eq!(
        step_send_kernel_bytes(&client, &bytes, SendRecvFlags::MSG_MORE, &guard),
        StepOutcome::Done(bytes.len())
    );

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: Some(loopback_iface()),
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));

    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert_eq!(outcome.loopback.tcp_bytes_moved, TCP_CORK_AUTO_FLUSH_BYTES);
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::POLL.bits()
            != 0
    );
    assert_eq!(
        accepted
            .acquire_operational()
            .expect("accepted payload")
            .io_snapshot()
            .recv_len,
        TCP_CORK_AUTO_FLUSH_BYTES
    );
}

#[test]
fn net_delegate_timer_adapter_converts_smoltcp_deadline_to_reactor_ns() {
    let base = smoltcp::time::Instant::from_millis(10);
    let deadline = smoltcp::time::Instant::from_millis(15);

    assert_eq!(
        smoltcp_instant_to_reactor_deadline_ns(base, deadline, 1_000),
        Some(5_001_000)
    );
    assert_eq!(
        smoltcp_instant_to_reactor_deadline_ns(deadline, base, 1_000),
        Some(1_000)
    );
}

#[test]
fn net_delegate_reactor_timer_adapter_fires_tick_and_drives_retransmit() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_182, 50_182);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());
    let next_deadline = listener_payload
        .tcp_backlog_next_deadline()
        .expect("backlog deadline");
    let deadline_ns =
        smoltcp_instant_to_reactor_deadline_ns(smoltcp::time::Instant::ZERO, next_deadline, 0)
            .expect("reactor deadline");

    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let reactor = Reactor::new();
    let timer_registrar = reactor.deadline_registrar_handle();
    reactor.submit(async move {
        assert_eq!(
            net_delegate_wait_tick_deadline(&timer_registrar, deadline_ns).await,
            WaitOutcome::TimedOut
        );
    });
    let mut programmed = None;
    let armed = reactor.run_until_idle_with_clock(|| 0, |deadline| programmed = deadline);
    assert_eq!(armed.next_deadline_ns(), Some(deadline_ns));
    assert_eq!(programmed, Some(deadline_ns));
    assert_eq!(crate::net::delegate::net_delegate_queue().peek(), 0);

    let fired = reactor.run_until_idle_with_clock(|| deadline_ns, |_| {});
    assert_eq!(fired.timer_wakes(), 1);
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::TICK.bits()
            != 0
    );

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let driver = LoopbackDelegateDriver {
        now: next_deadline,
        source: &source,
        iface: Some(&iface),
    };
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.tick_seen);
    assert!(outcome.backlog_retransmitted >= 1);
    assert_eq!(listener_payload.connecting_backlog_len(), 1);
    assert_eq!(iface.pending_packets(), 1);
}

#[test]
fn net_delegate_task_loop_waits_for_poll_and_processes_bounded_steps() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    let guard = tx_substrate::epoch::guard();
    let local = endpoint(40_183);
    let first_remote = endpoint(50_183);
    assert_eq!(
        step_bind(
            &udp,
            KernelSockAddr::V4(SockAddrIn::new(local.port, local.addr)),
            &guard,
        ),
        StepOutcome::Done(())
    );
    drop(guard);

    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Udp(
        UdpPacketEvent::with_payload_len(first_remote, local, 64),
    )]);
    let driver = LoopbackDelegateDriver {
        now: smoltcp::time::Instant::ZERO,
        source: &source,
        iface: None,
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mut task = std::boxed::Box::pin(net_delegate_task_loop(
        &driver,
        NetDelegateTaskConfig::run_steps(1),
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));
    let report = match task.as_mut().poll(&mut cx) {
        Poll::Ready(report) => report,
        Poll::Pending => panic!("delegate task should finish after one ready step"),
    };

    assert_eq!(report.ready_steps, 1);
    assert_eq!(report.waits_ready, 1);
    assert_eq!(report.waits_failed, 0);
    assert!(report.runtime.poll_seen);
    assert_eq!(report.runtime.packets_seen, 1);
    assert_eq!(
        udp.acquire_operational()
            .expect("udp payload")
            .io_snapshot()
            .recv_len,
        64
    );
}

#[test]
fn net_delegate_task_loop_reports_deadline_refresh_from_tick() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_184, 50_185);
    let guard = tx_substrate::epoch::guard();
    let iface = LoopbackIface::new(IfaceCommon::new(
        Ipv4Address::LOOPBACK,
        Ipv4Address::new([255, 0, 0, 0]),
        1500,
    ));
    let listener_payload = listener.acquire_operational().expect("listener payload");
    let client_payload = client.acquire_operational().expect("client payload");
    client_payload
        .raw_tcp_socket()
        .expect("client raw tcp")
        .connect_endpoint(local, remote)
        .expect("client raw connect");

    let mut ctx = PollContext::new(smoltcp::time::Instant::ZERO);
    assert!(ctx.poll_egress_one(&client, &iface, &guard).is_some());
    let syn = ctx.poll_ingress(&iface, &guard, 1);
    let child = syn.created_children[0].clone();
    assert!(ctx.poll_egress_one(&child, &iface, &guard).is_some());
    assert!(iface.pop_ingress().is_some());
    drop(guard);

    let source = ScriptedPacketSource::new(std::vec::Vec::new());
    let now = smoltcp::time::Instant::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS + 1);
    let driver = LoopbackDelegateDriver {
        now,
        source: &source,
        iface: Some(&iface),
    };
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    let mut observed_deadline = None;
    let mut task = std::boxed::Box::pin(net_delegate_task_loop_with_deadline_hook(
        &driver,
        NetDelegateTaskConfig::run_steps(1),
        |deadline| observed_deadline = deadline,
    ));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
    crate::net::delegate::net_delegate_kick_tick_with_post(|mailbox, event| mailbox.post(event));
    let report = match task.as_mut().poll(&mut cx) {
        Poll::Ready(report) => report,
        Poll::Pending => panic!("delegate task should finish after one tick"),
    };

    let expected_deadline =
        now + smoltcp::time::Duration::from_millis(TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS as u64);
    drop(task);
    assert!(report.runtime.tick_seen);
    assert!(report.runtime.backlog_retransmitted >= 1);
    assert!(report.last_deadline.is_some());
    assert_eq!(observed_deadline, report.last_deadline);
    assert_eq!(
        listener_payload.tcp_backlog_next_deadline(),
        Some(expected_deadline)
    );
}

#[test]
fn tcp_loopback_cleanup_withdraws_connection_table_entries() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_171, 50_171);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let client_key = ConnectionKey::new(local, remote);
    let server_key = ConnectionKey::new(remote, local);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_some());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_some());

    let cleanup = match step_tcp_connection_cleanup(&client, &guard) {
        StepOutcome::Done(cleanup) => cleanup,
        _ => panic!("unexpected cleanup outcome"),
    };

    assert!(cleanup.was_connected);
    assert!(cleanup.local_withdrawn);
    assert!(cleanup.peer_withdrawn);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_none());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_none());
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Closed)
    );
    assert_eq!(
        step_tcp_connection_cleanup(&client, &guard),
        StepOutcome::Done(crate::net::execution::TcpConnectionCleanupOutcome::default())
    );
    assert_eq!(
        accepted
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
fn tcp_close_staging_withdraws_connection_and_publishes_broken() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let (client, listener, local, remote) = prepare_loopback_connect(40_187, 50_187);
    let guard = tx_substrate::epoch::guard();

    assert!(matches!(
        step_tcp_loopback_handshake(&client, &guard),
        StepOutcome::Done(_)
    ));
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));
    let client_key = ConnectionKey::new(local, remote);
    let server_key = ConnectionKey::new(remote, local);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_some());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_some());

    let close = match step_tcp_close_staging(&client, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("unexpected tcp close staging outcome"),
    };

    assert!(close.cleanup.was_connected);
    assert!(close.cleanup.local_withdrawn);
    assert!(close.cleanup.peer_withdrawn);
    assert!(close.recv_shutdown);
    assert!(close.send_shutdown);
    assert!(close.recv_broken_published);
    assert!(close.send_broken_published);
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(client_key, &guard)
        .is_none());
    assert!(SOCKET_TABLE
        .lookup_tcp_connection(server_key, &guard)
        .is_none());
    assert!(client.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0);
    assert!(client.readiness.send_wq.peek() & SendWireSet::BROKEN.bits() != 0);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Closed)
    );
    assert_eq!(
        step_send_kernel_bytes(&client, b"x", SendRecvFlags::empty(), &guard),
        StepOutcome::Err(Errno::EPIPE)
    );
    assert_eq!(
        step_recv(&client, 1, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(0)
    );
}
