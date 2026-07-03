use super::*;
use crate::net::packet::LoopbackIpPacket;
use crate::net::protocol::SmoltcpTcpSegment;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    lock
}

/// Build a checksummed TCP/IPv4 packet and parse it into the demux-shaped
/// event (segment populated) — exactly what the real-device RX path
/// produces. Hand-built flag-only events cannot enter handshakes (P2-S3).
fn tcp_segment_event(
    src: IpEndpoint,
    dst: IpEndpoint,
    control: smoltcp::wire::TcpControl,
    seq: i32,
    ack: Option<i32>,
) -> TcpPacketEvent {
    let tcp_repr = smoltcp::wire::TcpRepr {
        src_port: src.port,
        dst_port: dst.port,
        control,
        seq_number: smoltcp::wire::TcpSeqNumber(seq),
        ack_number: ack.map(smoltcp::wire::TcpSeqNumber),
        window_len: 4096,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let smol_src = {
        let [a, b, c, d] = src.addr.octets();
        smoltcp::wire::Ipv4Address::new(a, b, c, d)
    };
    let smol_dst = {
        let [a, b, c, d] = dst.addr.octets();
        smoltcp::wire::Ipv4Address::new(a, b, c, d)
    };
    let tcp_len = tcp_repr.buffer_len();
    let ip_repr = smoltcp::wire::IpRepr::Ipv4(smoltcp::wire::Ipv4Repr {
        src_addr: smol_src,
        dst_addr: smol_dst,
        next_header: smoltcp::wire::IpProtocol::Tcp,
        payload_len: tcp_len,
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let mut ip_bytes = std::vec![0u8; ip_header_len + tcp_len];
    let checksum_caps = smoltcp::phy::ChecksumCapabilities::default();
    ip_repr.emit(&mut ip_bytes[..ip_header_len], &checksum_caps);
    let mut tcp_packet = smoltcp::wire::TcpPacket::new_unchecked(&mut ip_bytes[ip_header_len..]);
    tcp_repr.emit(
        &mut tcp_packet,
        &smoltcp::wire::IpAddress::Ipv4(smol_src),
        &smoltcp::wire::IpAddress::Ipv4(smol_dst),
        &checksum_caps,
    );
    let segment = SmoltcpTcpSegment::parse_ipv4_packet(&LoopbackIpPacket::new(ip_bytes))
        .expect("parsed crafted segment");
    TcpPacketEvent::new(
        src,
        dst,
        TcpPacketFlags {
            syn: control == smoltcp::wire::TcpControl::Syn,
            ack: ack.is_some(),
            rst: false,
        },
        std::vec::Vec::new(),
        false,
    )
    .with_segment(Some(segment))
}

/// P2-S3 mirror soul test: inject a real SYN → assert a half-open child in
/// the connecting backlog with a SYN-ACK queued in smoltcp (NOT an
/// immediately-accepted fake child) → inject the final ACK → assert the
/// child is promoted to the accept queue and accept() returns it Connected.
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
    let client_isn = 0x1000;

    assert_eq!(
        step_bind(&listener, inet(local.port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    // --- SYN: half-open child, no accept entry yet ---
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(tcp_segment_event(
        remote,
        local,
        smoltcp::wire::TcpControl::Syn,
        client_isn,
        None,
    ))]);
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    let payload = listener.acquire_operational().expect("payload");
    assert_eq!(payload.accept_queue_len(), 0, "SYN alone must not accept");
    assert_eq!(payload.connecting_backlog_len(), 1, "half-open child expected");
    let child = payload
        .connecting_child(local, remote)
        .expect("connecting child");
    let child_payload = child.acquire_operational().expect("child payload");
    assert!(matches!(
        child_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting { .. })
    ));

    // The child's smoltcp state machine queued the SYN-ACK; capture its ISN
    // the same way the device-TX half-open lane would.
    let child_raw = child_payload.raw_tcp_socket().expect("child raw tcp");
    let syn_ack = child_raw.dispatch_segment().expect("queued SYN-ACK");
    assert_eq!(syn_ack.tcp.control, smoltcp::wire::TcpControl::Syn);
    assert_eq!(
        syn_ack.tcp.ack_number,
        Some(smoltcp::wire::TcpSeqNumber(client_isn.wrapping_add(1)))
    );
    let child_isn = syn_ack.tcp.seq_number.0;

    // --- final ACK: promotion to the accept queue ---
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(tcp_segment_event(
        remote,
        local,
        smoltcp::wire::TcpControl::None,
        client_isn.wrapping_add(1),
        Some(child_isn.wrapping_add(1)),
    ))]);
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    assert_eq!(payload.accept_queue_len(), 1);
    assert_eq!(payload.connecting_backlog_len(), 0);
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

    // Full real handshake (P2-S3): SYN → capture child ISN → final ACK.
    let client_isn = 0x2000;
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(tcp_segment_event(
        remote,
        local,
        smoltcp::wire::TcpControl::Syn,
        client_isn,
        None,
    ))]);
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    let child_isn = {
        let child = listener_payload
            .connecting_child(local, remote)
            .expect("connecting child");
        let child_payload = child.acquire_operational().expect("child payload");
        let child_raw = child_payload.raw_tcp_socket().expect("child raw tcp");
        child_raw
            .dispatch_segment()
            .expect("queued SYN-ACK")
            .tcp
            .seq_number
            .0
    };
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(tcp_segment_event(
        remote,
        local,
        smoltcp::wire::TcpControl::None,
        client_isn.wrapping_add(1),
        Some(child_isn.wrapping_add(1)),
    ))]);
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
