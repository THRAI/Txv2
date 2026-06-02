use super::*;

use crate::net::protocol::{
    build_icmpv4_echo_reply, build_icmpv4_echo_request, icmpv4_echo_message_len,
    parse_icmpv4_echo_payload_unchecked, parse_icmpv4_from_ipv4_bytes,
    parse_icmpv4_loopback_packet, parse_icmpv4_payload, parse_raw_icmpv4_echo_payload_unchecked,
    Icmpv4EchoPacket, Icmpv4Event, RawIcmpSocket,
};

#[test]
fn icmpv4_parse_echo_request_and_build_reply() {
    let request = Icmpv4EchoPacket {
        src: Ipv4Address::new([127, 0, 0, 2]),
        dst: Ipv4Address::LOOPBACK,
        ident: 0x1234,
        seq_no: 7,
        payload: b"ping".to_vec(),
    };

    let packet = build_icmpv4_echo_request(&request);
    assert_eq!(
        parse_icmpv4_loopback_packet(&packet),
        Icmpv4Event::EchoRequest(request.clone())
    );

    let reply = request.reply_packet();
    let packet = build_icmpv4_echo_reply(&reply);
    assert_eq!(
        parse_icmpv4_loopback_packet(&packet),
        Icmpv4Event::EchoReply(reply)
    );
}

#[test]
fn icmpv4_unchecked_echo_parser_accepts_kernel_checksum_payload() {
    let mut payload = std::vec![0u8; 16];
    payload[0] = 8;
    payload[4..6].copy_from_slice(&0x5151u16.to_be_bytes());
    payload[6..8].copy_from_slice(&7u16.to_be_bytes());
    payload[8..].fill(0xaa);

    assert_eq!(
        parse_icmpv4_echo_payload_unchecked(
            Ipv4Address::new([10, 0, 0, 2]),
            Ipv4Address::new([10, 0, 0, 1]),
            &payload,
        ),
        Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src: Ipv4Address::new([10, 0, 0, 2]),
            dst: Ipv4Address::new([10, 0, 0, 1]),
            ident: 0x5151,
            seq_no: 7,
            payload: std::vec![0xaa; 8],
        })
    );
}

#[test]
fn icmpv4_parser_accepts_busybox_pattern_echo_payload() {
    let src = Ipv4Address::new([10, 0, 0, 2]);
    let dst = Ipv4Address::new([10, 0, 0, 1]);
    let mut payload = std::vec![0xaa; 16];
    payload[0] = 8;
    payload[1] = 0;
    payload[2..4].copy_from_slice(&0u16.to_be_bytes());
    payload[4..6].copy_from_slice(&0x5151u16.to_be_bytes());
    payload[6..8].copy_from_slice(&0u16.to_be_bytes());
    payload[8..12].copy_from_slice(&0x1234_5678u32.to_ne_bytes());
    let checksum = internet_checksum(&payload);
    payload[2..4].copy_from_slice(&checksum.to_be_bytes());

    assert_eq!(
        parse_icmpv4_payload(src, dst, &payload),
        Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src,
            dst,
            ident: 0x5151,
            seq_no: 0,
            payload: payload[8..].to_vec(),
        })
    );
}

#[test]
fn raw_icmp_send_accepts_busybox_pattern_echo_code() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();
    let socket = match step_socket_create(
        ValidSocketType::validate(2, 3, 1).expect("raw icmp socket"),
        &guard,
    ) {
        StepOutcome::Done(socket) => socket,
        other => panic!("unexpected socket create outcome: {other:?}"),
    };
    let src = Ipv4Address::LOOPBACK;
    let dst = Ipv4Address::new([10, 0, 0, 1]);
    let mut payload = std::vec![0xaa; 16];
    payload[0] = 8;
    payload[2..4].copy_from_slice(&0u16.to_be_bytes());
    payload[4..6].copy_from_slice(&0x5151u16.to_be_bytes());
    payload[6..8].copy_from_slice(&0u16.to_be_bytes());
    payload[8..12].copy_from_slice(&0x1234_5678u32.to_ne_bytes());
    let checksum = internet_checksum(&payload);
    payload[2..4].copy_from_slice(&checksum.to_be_bytes());

    assert_eq!(
        parse_icmpv4_payload(src, dst, &payload),
        Icmpv4Event::Malformed
    );
    assert_eq!(
        parse_raw_icmpv4_echo_payload_unchecked(src, dst, &payload),
        Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src,
            dst,
            ident: 0x5151,
            seq_no: 0,
            payload: payload[8..].to_vec(),
        })
    );
    assert_eq!(
        step_send_to_kernel_bytes(
            &socket,
            Some(IpEndpoint::new(dst, 0)),
            &payload,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(payload.len())
    );
}

#[test]
fn raw_icmp_ipv4_recv_returns_ip_header_for_raw_socket() {
    let socket = RawIcmpSocket::new(&SocketOptionSet::for_kind(SocketKind::RawIcmp));
    let reply = Icmpv4EchoPacket {
        src: Ipv4Address::new([10, 0, 0, 1]),
        dst: Ipv4Address::new([10, 0, 0, 2]),
        ident: 0x5151,
        seq_no: 1,
        payload: b"trace".to_vec(),
    };
    let expected_len = 20 + icmpv4_echo_message_len(&reply);

    assert!(socket.ingest_rx_echo_reply(reply.clone()));
    assert_eq!(
        socket.recv_len(usize::MAX, true),
        Some((expected_len, false))
    );

    let mut out = std::vec![0u8; expected_len];
    let drain = socket.recv_bytes(&mut out, false).expect("raw reply");
    assert_eq!(drain.bytes, expected_len);
    assert_eq!(out[0] >> 4, 4);
    assert_eq!(out[0] & 0x0f, 5);
    assert_eq!(out[8], 64);
    assert_eq!(
        parse_icmpv4_from_ipv4_bytes(&out[..drain.bytes]),
        Icmpv4Event::EchoReply(reply)
    );
}

#[test]
fn loopback_iface_echo_request_becomes_echo_reply() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    let iface = loopback_iface();
    iface.clear_for_test_or_bootstrap();

    let request = Icmpv4EchoPacket {
        src: Ipv4Address::new([127, 0, 0, 2]),
        dst: Ipv4Address::LOOPBACK,
        ident: 0x4321,
        seq_no: 9,
        payload: b"loop".to_vec(),
    };
    assert!(iface.dispatch_ip(build_icmpv4_echo_request(&request)));
    assert_eq!(iface.pending_packets(), 1);

    assert_eq!(
        iface.process_icmpv4_echo_once(),
        Some(Icmpv4Event::EchoRequest(request.clone()))
    );
    assert_eq!(iface.pending_packets(), 1);

    let reply = iface.pop_ingress().expect("echo reply");
    assert_eq!(
        parse_icmpv4_loopback_packet(&reply),
        Icmpv4Event::EchoReply(request.reply_packet())
    );
}

fn internet_checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]]) as u32
        } else {
            (chunk[0] as u32) << 8
        };
        sum += word;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
