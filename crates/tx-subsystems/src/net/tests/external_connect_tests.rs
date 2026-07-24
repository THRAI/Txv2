//! Outbound TCP connect over the real virtio-net device (OSComp finals git
//! Task2, kernel side). Verifies that a userspace-shaped `connect()` to an
//! EXTERNAL host emits a SYN, that an inbound SYN-ACK demuxed from a real wire
//! frame is fed into smoltcp, and that the handshake completes (smoltcp
//! Established + `TcpState::Connected` + `SendWireSet::SPACE` fired so the
//! blocked connect returns).
//!
//! Models on `virtio_net_device_tests::virtio_rx_delegate_delivers_udp_payload_to_socket`
//! for the device/delegate wiring and on `loopback_tests::tcp_loopback` for the
//! connect/handshake assertions.

use super::*;
use crate::net::protocol::SmoltcpTcpSegment;
use std::boxed::Box;

const LOCAL_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 2]);
const REMOTE_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 1]);
const DEVICE_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 2];
const PEER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

struct VirtioEtherDelegateDriver<'a> {
    source: EtherPacketSource<'a>,
}

impl NetDelegateDriver for VirtioEtherDelegateDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::ZERO
    }

    fn packet_source(&self) -> &dyn PacketSource {
        &self.source
    }
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    init_zones();
    let lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
    lock
}

fn leak_virtio_device(rx_capacity: usize, tx_capacity: usize) -> &'static VirtioNetDevice {
    Box::leak(Box::new(VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new(DEVICE_MAC),
            1500,
            VirtioNetFeatureSet::software_checksum(),
        ),
        VirtioNetQueueConfig::new(rx_capacity, tx_capacity),
    )))
}

fn leak_virtio_registration(
    device: &'static VirtioNetDevice,
    minor: u32,
) -> &'static NetDeviceRegistration {
    Box::leak(Box::new(NetDeviceRegistration {
        devt: DevT::new(VIRTIO_NET_STAGING_MAJOR, minor),
        name: "virtio-net-test",
        ops: device,
    }))
}

fn smoltcp_ipv4(addr: Ipv4Address) -> smoltcp::wire::Ipv4Address {
    let [a, b, c, d] = addr.octets();
    smoltcp::wire::Ipv4Address::new(a, b, c, d)
}

/// Build a checksummed TCP/IPv4/Ethernet frame (smoltcp emit computes both the
/// IP and TCP checksums, so `SmoltcpTcpSegment::parse_ipv4_packet` in the demux
/// path accepts it).
fn ethernet_tcp_frame(
    src: IpEndpoint,
    dst: IpEndpoint,
    control: smoltcp::wire::TcpControl,
    seq: i32,
    ack: Option<i32>,
) -> std::vec::Vec<u8> {
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
    let tcp_len = tcp_repr.buffer_len();
    let ip_repr = smoltcp::wire::IpRepr::Ipv4(smoltcp::wire::Ipv4Repr {
        src_addr: smoltcp_ipv4(src.addr),
        dst_addr: smoltcp_ipv4(dst.addr),
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
        &smoltcp::wire::IpAddress::Ipv4(smoltcp_ipv4(src.addr)),
        &smoltcp::wire::IpAddress::Ipv4(smoltcp_ipv4(dst.addr)),
        &checksum_caps,
    );

    let mut frame = std::vec::Vec::new();
    frame.extend_from_slice(&DEVICE_MAC); // dst MAC (must match iface ether_addr)
    frame.extend_from_slice(&PEER_MAC); // src MAC
    frame.extend_from_slice(&[0x08, 0x00]); // ethertype IPv4
    frame.extend_from_slice(&ip_bytes);
    frame
}

/// Full device-level outbound connect: bind+connect a TCP socket to an external
/// IP routed via the virtio device, inject a crafted SYN-ACK over the device RX
/// path, drive the delegate, and assert the handshake completes.
#[test]
fn external_tcp_connect_completes_handshake_from_injected_syn_ack() {
    let _lock = setup();

    let device = leak_virtio_device(4, 4);
    let registration = leak_virtio_registration(device, 71);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");
    let iface = EtherIface::new(
        registration,
        IfaceCommon::new(LOCAL_IP, Ipv4Address::new([255, 255, 255, 0]), 1500),
        EthernetAddress::new(DEVICE_MAC),
        "virt0",
    );

    let client_port = 51_271u16;
    let server_port = 41_271u16;
    let local = IpEndpoint::new(LOCAL_IP, client_port);
    let remote = IpEndpoint::new(REMOTE_IP, server_port);

    let guard = tx_substrate::epoch::guard();
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp client");
    // Bind to the device-local address so the connect local endpoint is concrete
    // (no dependency on route preferred-src selection).
    assert_eq!(
        step_bind(&client, inet_addr(client_port, LOCAL_IP), &guard),
        StepOutcome::Done(())
    );

    // connect() to the EXTERNAL remote: no in-kernel namespace owns 192.0.2.1, so
    // Change B emits the SYN into smoltcp and registers the client connection.
    assert!(matches!(
        step_connect(&client, inet_addr(server_port, REMOTE_IP), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    let client_payload = client.acquire_operational().expect("client payload");
    assert_eq!(
        client_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting { local, remote })
    );
    let client_raw = client_payload.raw_tcp_socket().expect("client raw tcp");
    // Change B drove smoltcp into SynSent.
    assert_eq!(
        client_raw.protocol_state(),
        smoltcp::socket::tcp::State::SynSent
    );
    // The client is registered under (local, remote) so the inbound SYN-ACK
    // (src=remote, dst=local) will match lookup_tcp_connection(dst, src).
    assert!(client_payload
        .socket_table()
        .lookup_tcp_connection(ConnectionKey::new(local, remote), &guard)
        .is_some());

    // Capture the client's initial send sequence number from the SYN smoltcp
    // wants to dispatch (the same call the device-TX scan makes), so the SYN-ACK
    // can ACK it correctly.
    let syn = client_raw.dispatch_segment().expect("client SYN segment");
    assert_eq!(syn.tcp.control, smoltcp::wire::TcpControl::Syn);
    assert!(syn.tcp.ack_number.is_none());
    let client_isn = syn.tcp.seq_number.0;

    // Craft and inject the SYN-ACK (server -> client), acking the client ISN.
    let syn_ack = ethernet_tcp_frame(
        remote,
        local,
        smoltcp::wire::TcpControl::Syn,
        0x4242,
        Some(client_isn.wrapping_add(1)),
    );
    let injected = device.inject_rx_for_test_or_irq(RxFrame::new(syn_ack));
    assert!(injected.accepted);
    assert_eq!(device.rx_len(), 1);

    // Drive the delegate RX path: device frame -> demux (segment populated) ->
    // process_tcp_event active-client branch (Change A) -> smoltcp advances ->
    // promote to Connected + fire SPACE (Change C).
    let driver = VirtioEtherDelegateDriver {
        source: EtherPacketSource { iface: &iface },
    };
    crate::net::delegate::net_delegate_kick_poll();
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(device.rx_len(), 0);

    // Handshake complete: smoltcp Established, protocol Connected, and the
    // blocked connect's send carrier woken.
    assert_eq!(
        client_raw.protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
    assert_eq!(
        client_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote })
    );
    assert!(client.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);
}

/// Focused check of the new active-client branch in process_tcp_event: a
/// connecting client (smoltcp SynSent) fed a real SYN-ACK segment via the
/// network-events step promotes to Connected and fires SPACE, while NOT relying
/// on the device layer. Confirms the strict gate routes Connecting clients to
/// smoltcp.process_segment (not the lossy record path).
#[test]
fn process_tcp_event_active_client_segment_promotes_to_connected() {
    let _lock = setup();

    // Attach a device so 192.0.2.2 is a local address that the client can bind.
    let device = leak_virtio_device(2, 2);
    let registration = leak_virtio_registration(device, 72);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");

    let guard = tx_substrate::epoch::guard();

    let client_port = 51_272u16;
    let server_port = 41_272u16;
    let local = IpEndpoint::new(LOCAL_IP, client_port);
    let remote = IpEndpoint::new(REMOTE_IP, server_port);

    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp client");
    assert_eq!(
        step_bind(&client, inet_addr(client_port, LOCAL_IP), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet_addr(server_port, REMOTE_IP), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    let client_payload = client.acquire_operational().expect("client payload");
    let client_raw = client_payload.raw_tcp_socket().expect("client raw tcp");
    let syn = client_raw.dispatch_segment().expect("client SYN segment");
    let client_isn = syn.tcp.seq_number.0;

    // Parse a real, checksummed SYN-ACK into a TcpPacketEvent carrying the full
    // segment (exactly what the real-device demux produces).
    let ip_bytes = {
        let frame = ethernet_tcp_frame(
            remote,
            local,
            smoltcp::wire::TcpControl::Syn,
            0x5151,
            Some(client_isn.wrapping_add(1)),
        );
        frame[14..].to_vec()
    };
    let segment = SmoltcpTcpSegment::parse_ipv4_packet(&LoopbackIpPacket::new(ip_bytes))
        .expect("parsed syn-ack segment");
    let event = TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: true,
            ack: true,
            rst: false,
        },
        std::vec::Vec::new(),
        false,
    )
    .with_segment(Some(segment));

    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(event)]);
    let outcome = match step_process_network_events(&source, &guard) {
        StepOutcome::Done(outcome) => outcome,
        _ => panic!("network step should complete"),
    };

    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(outcome.sockets_touched, 1);
    assert_eq!(
        client_raw.protocol_state(),
        smoltcp::socket::tcp::State::Established
    );
    assert_eq!(
        client_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { local, remote })
    );
    assert!(client.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0);
}

/// P2-S4 soul test: one device-TX pass drains a multi-segment send queue
/// (>=2 segments) instead of the previous one-segment-per-delegate-wake
/// shape. Establishes an external client exactly like the focused test
/// above, enqueues >2xMSS of payload, and counts frames a single
/// `step_process_device_tx_pending` pass hands the sink.
#[test]
fn device_tx_single_pass_drains_multiple_segments() {
    let _lock = setup();

    let device = leak_virtio_device(2, 2);
    let registration = leak_virtio_registration(device, 73);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");

    let guard = tx_substrate::epoch::guard();

    let client_port = 51_273u16;
    let server_port = 41_273u16;
    let local = IpEndpoint::new(LOCAL_IP, client_port);
    let remote = IpEndpoint::new(REMOTE_IP, server_port);

    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("tcp client");
    assert_eq!(
        step_bind(&client, inet_addr(client_port, LOCAL_IP), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet_addr(server_port, REMOTE_IP), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    let client_payload = client.acquire_operational().expect("client payload");
    let client_raw = client_payload.raw_tcp_socket().expect("client raw tcp");
    let syn = client_raw.dispatch_segment().expect("client SYN segment");
    let client_isn = syn.tcp.seq_number.0;

    let ip_bytes = {
        let frame = ethernet_tcp_frame(
            remote,
            local,
            smoltcp::wire::TcpControl::Syn,
            0x6161,
            Some(client_isn.wrapping_add(1)),
        );
        frame[14..].to_vec()
    };
    let segment = SmoltcpTcpSegment::parse_ipv4_packet(&LoopbackIpPacket::new(ip_bytes))
        .expect("parsed syn-ack segment");
    let event = TcpPacketEvent::new(
        remote,
        local,
        TcpPacketFlags {
            syn: true,
            ack: true,
            rst: false,
        },
        std::vec::Vec::new(),
        false,
    )
    .with_segment(Some(segment));
    let source = ScriptedPacketSource::new(std::vec![PacketDispatch::Tcp(event)]);
    assert!(matches!(
        step_process_network_events(&source, &guard),
        StepOutcome::Done(_)
    ));
    assert_eq!(
        client_raw.protocol_state(),
        smoltcp::socket::tcp::State::Established
    );

    // >2xMSS payload: the crafted SYN-ACK advertised no MSS option, so
    // smoltcp caps outgoing segments at its default remote MSS; 4000
    // bytes must split into several segments (peer window is 4096).
    let reserve = client_raw
        .enqueue_tx_bytes(&std::vec![0xa5u8; 4000])
        .expect("enqueue tx bytes");
    assert!(reserve.bytes >= 2000, "send queue too small: {}", reserve.bytes);

    struct CountingSink {
        frames: core::sync::atomic::AtomicUsize,
    }
    impl PacketTxSink for CountingSink {
        fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> PacketTxResult {
            self.frames
                .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            PacketTxResult::Accepted {
                frame_len: frame.len(),
            }
        }
    }
    let sink = CountingSink {
        frames: core::sync::atomic::AtomicUsize::new(0),
    };

    let StepOutcome::Done(tx) = step_process_device_tx_pending_in_namespace_at(
        &sink,
        crate::net::initial_net_namespace_payload(),
        smoltcp::time::Instant::from_millis(5),
        DeviceTxBudget::default(),
        &guard,
    ) else {
        panic!("device tx pass should complete");
    };
    let frames = sink.frames.load(core::sync::atomic::Ordering::Acquire);
    assert!(
        tx.tcp_packets >= 2 && frames >= 2,
        "one pass must drain multiple segments (tcp_packets={}, frames={})",
        tx.tcp_packets,
        frames
    );
}

/// P2-S5: sequential external connects must carry distinct ISNs (the
/// persistent CONTEXT_IFACE keeps advancing smoltcp's RNG; the frozen
/// throwaway-iface world dealt identical ISNs, confusing peers that hold
/// state for the previous incarnation of the tuple).
#[test]
fn sequential_connects_use_distinct_isns() {
    let _lock = setup();

    let device = leak_virtio_device(2, 2);
    let registration = leak_virtio_registration(device, 74);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");

    let guard = tx_substrate::epoch::guard();
    let remote = IpEndpoint::new(REMOTE_IP, 41_274);

    let mut isns = std::vec::Vec::new();
    for (i, client_port) in [51_274u16, 51_275u16].into_iter().enumerate() {
        let client = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Tcp,
            SocketOptionSet::default_tcp(),
        )
        .unwrap_or_else(|e| panic!("tcp client {i}: {e:?}"));
        assert_eq!(
            step_bind(&client, inet_addr(client_port, LOCAL_IP), &guard),
            StepOutcome::Done(())
        );
        assert!(matches!(
            step_connect(&client, inet_addr(41_274, REMOTE_IP), &guard),
            StepOutcome::Yield {
                shape: YieldShape::OnWaitSource { .. },
                ..
            }
        ));
        let payload = client.acquire_operational().expect("client payload");
        let raw = payload.raw_tcp_socket().expect("client raw tcp");
        let syn = raw.dispatch_segment().expect("client SYN segment");
        assert_eq!(syn.tcp.control, smoltcp::wire::TcpControl::Syn);
        assert_eq!(syn.dst_endpoint(), Some(remote));
        isns.push(syn.tcp.seq_number.0);
    }
    assert_ne!(isns[0], isns[1], "sequential connects reused the same ISN");
}

/// P2-S6 debug repro: external UDP sendto must surface a wire packet from
/// one device-TX pass (the DNS smoke path: autobind 0.0.0.0 + sendto
/// 10.0.2.3).
#[test]
fn external_udp_sendto_reaches_device_tx() {
    let _lock = setup();

    let device = leak_virtio_device(2, 2);
    let registration = leak_virtio_registration(device, 75);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");

    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    // Mirror maybe_autobind_udp_sendto: bind 0.0.0.0:ephemeral.
    assert_eq!(
        step_bind(&udp, inet(49_180), &guard),
        StepOutcome::Done(())
    );

    let dst = IpEndpoint::new(REMOTE_IP, 53);
    let outcome = step_send_to_kernel_bytes(
        &udp,
        Some(dst),
        b"dns-query-bytes",
        SendRecvFlags::empty(),
        &guard,
    );
    assert_eq!(outcome, StepOutcome::Done(15), "sendto must accept the datagram");
    let _ = dst;

    struct CountingSink {
        frames: core::sync::atomic::AtomicUsize,
    }
    impl PacketTxSink for CountingSink {
        fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> PacketTxResult {
            self.frames
                .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            PacketTxResult::Accepted {
                frame_len: frame.len(),
            }
        }
    }
    let sink = CountingSink {
        frames: core::sync::atomic::AtomicUsize::new(0),
    };
    let StepOutcome::Done(tx) = step_process_device_tx_pending_in_namespace_at(
        &sink,
        crate::net::initial_net_namespace_payload(),
        smoltcp::time::Instant::from_millis(5),
        DeviceTxBudget::default(),
        &guard,
    ) else {
        panic!("device tx pass should complete");
    };
    assert_eq!(tx.udp_failed, 0, "udp lane must not fail");
    assert!(
        tx.udp_packets >= 1,
        "udp datagram must reach the sink (attempted={}, busy={}, pending={})",
        tx.udp_attempted,
        tx.udp_busy,
        tx.udp_resolution_pending
    );
}

const LOCAL_IP6: Ipv6Address = Ipv6Address::new([
    0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15,
]);
const REMOTE_IP6: Ipv6Address = Ipv6Address::new([
    0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
]);

fn inet6_at(addr: Ipv6Address, port: u16) -> KernelSockAddr {
    KernelSockAddr::V6(SockAddrIn6::new(port, addr))
}

/// Attach the virtio test device with both a v4 and a v6 address (the v4 one
/// is what `ensure_ether_iface_for_link` keys off; the v6 one is what the
/// route/source-selection code needs).
fn attach_dual_stack_device(minor: u32) -> &'static NetDeviceRegistration {
    let device = leak_virtio_device(2, 2);
    let registration = leak_virtio_registration(device, minor);
    let namespace = crate::net::initial_net_namespace_payload();
    namespace
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");
    let ifindex = namespace
        .link_snapshot()
        .iter()
        .find(|link| link.name == registration.name)
        .expect("virtio test link")
        .ifindex;
    namespace
        .set_device_ipv6_addr_by_ifindex(
            NetAdminAuthority::for_test_or_bootstrap(),
            ifindex,
            Some(LOCAL_IP6),
            Some(64),
        )
        .expect("set virtio test device ipv6 address");
    registration
}

/// Capture what the device-TX lane actually hands the wire, and let the test
/// decide how many passes must fail before the sink accepts.
struct ScriptedSink {
    refusals_left: core::sync::atomic::AtomicUsize,
    frames: std::sync::Mutex<std::vec::Vec<std::vec::Vec<u8>>>,
}

impl ScriptedSink {
    fn new(refusals: usize) -> Self {
        Self {
            refusals_left: core::sync::atomic::AtomicUsize::new(refusals),
            frames: std::sync::Mutex::new(std::vec::Vec::new()),
        }
    }

    fn accepted(&self) -> std::vec::Vec<std::vec::Vec<u8>> {
        self.frames.lock().expect("scripted sink frames").clone()
    }
}

impl PacketTxSink for ScriptedSink {
    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> PacketTxResult {
        let left = self.refusals_left.load(core::sync::atomic::Ordering::Acquire);
        if left > 0 {
            self.refusals_left
                .store(left - 1, core::sync::atomic::Ordering::Release);
            // Exactly what the boot lane's v4-only iface answers for a unicast
            // IPv6 destination: `decide_ipv6_route` -> Unreachable.
            return PacketTxResult::Failed {
                errno: Errno::EADDRNOTAVAIL,
            };
        }
        self.frames
            .lock()
            .expect("scripted sink frames")
            .push(frame.to_vec());
        PacketTxResult::Accepted {
            frame_len: frame.len(),
        }
    }
}

/// V5-1 regression (G1a): the UDP device-TX pop is destructive, so a sink that
/// refuses the packet must NOT destroy it. Before the fix the first refusal
/// dropped the datagram permanently — and because the initial namespace is
/// scanned by TWO sinks per delegate round (the boot lane's v4-only iface
/// first), every external IPv6 UDP datagram died before the iface that could
/// route it ever ran.
///
/// Deliberately uses IPv4 so this pins the LANE, not anything v6-specific.
#[test]
fn refused_udp_datagram_is_requeued_for_the_next_pass() {
    let _lock = setup();

    let device = leak_virtio_device(2, 2);
    let registration = leak_virtio_registration(device, 76);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(LOCAL_IP))
        .expect("attach virtio test device to initial net namespace");

    let guard = tx_substrate::epoch::guard();
    let udp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("udp socket");
    assert_eq!(
        step_bind(&udp, inet_addr(49_182, LOCAL_IP), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &udp,
            Some(IpEndpoint::new(REMOTE_IP, 53)),
            b"requeue-me",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(10)
    );

    let sink = ScriptedSink::new(1);
    let pass = |now_ms: i32| {
        let StepOutcome::Done(tx) = step_process_device_tx_pending_in_namespace_at(
            &sink,
            crate::net::initial_net_namespace_payload(),
            smoltcp::time::Instant::from_millis(now_ms as i64),
            DeviceTxBudget::default(),
            &guard,
        ) else {
            panic!("device tx pass should complete");
        };
        tx
    };

    let first = pass(5);
    assert_eq!(first.udp_packets, 0, "first pass must be refused");
    assert_eq!(
        first.udp_failed, 1,
        "a sink error is still counted as a failure"
    );

    let second = pass(10);
    assert_eq!(
        second.udp_packets, 1,
        "the refused datagram must survive into the next pass \
         (attempted={}, busy={}, pending={}, failed={})",
        second.udp_attempted,
        second.udp_busy,
        second.udp_resolution_pending,
        second.udp_failed
    );
    let frames = sink.accepted();
    assert_eq!(frames.len(), 1, "exactly one packet reaches the wire");
    assert!(
        frames[0].ends_with(b"requeue-me"),
        "the requeued datagram must carry the original payload"
    );
}

/// V5-1 regression (G1c): an external IPv6 UDP datagram must leave with the
/// interface's v6 address as source. Before the fix `udp_tx_src_hint`'s V6 arm
/// was a hard `None`, smoltcp's address-less context iface fell back to
/// `Ipv6Address::LOCALHOST`, and the wire packet carried src=`::1` — which no
/// peer can answer.
#[test]
fn external_udp6_sendto_uses_the_interface_source_address() {
    let _lock = setup();

    let _registration = attach_dual_stack_device(77);
    let guard = tx_substrate::epoch::guard();

    let udp = registry::create_socket_in_namespace_with_family(
        SocketKind::Udp,
        AddressFamily::Inet6,
        SocketOptionSet::default_udp(),
        crate::net::initial_net_namespace_payload(),
    )
    .expect("udp6 socket");
    // Mirror maybe_autobind_udp_sendto for AF_INET6: bind [::]:ephemeral, i.e.
    // the socket does NOT know its own source address.
    assert_eq!(
        step_bind(&udp, inet6_at(Ipv6Address::UNSPECIFIED, 49_181), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &udp,
            Some(IpEndpoint::new_v6(REMOTE_IP6, 53)),
            b"dns6-query",
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(10)
    );

    let sink = ScriptedSink::new(0);
    let StepOutcome::Done(tx) = step_process_device_tx_pending_in_namespace_at(
        &sink,
        crate::net::initial_net_namespace_payload(),
        smoltcp::time::Instant::from_millis(5),
        DeviceTxBudget::default(),
        &guard,
    ) else {
        panic!("device tx pass should complete");
    };
    assert_eq!(
        tx.udp_packets, 1,
        "the v6 datagram must reach the sink (attempted={}, busy={}, pending={}, failed={})",
        tx.udp_attempted, tx.udp_busy, tx.udp_resolution_pending, tx.udp_failed
    );

    let frames = sink.accepted();
    assert_eq!(frames.len(), 1);
    let packet = &frames[0];
    assert_eq!(packet[0] >> 4, 6, "must be an IPv6 packet");
    assert_eq!(
        &packet[8..24],
        &LOCAL_IP6.octets(),
        "source must be the interface address, not ::1"
    );
    assert_eq!(&packet[24..40], &REMOTE_IP6.octets(), "destination");
}
