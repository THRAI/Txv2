use super::*;

use crate::net::protocol::{
    build_icmpv4_echo_reply, build_icmpv4_echo_request, parse_icmpv4_loopback_packet,
    Icmpv4EchoPacket, Icmpv4Event,
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
