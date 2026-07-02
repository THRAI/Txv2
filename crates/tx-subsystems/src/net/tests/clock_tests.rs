use super::*;
use crate::net::clock::{net_now_instant, net_set_now_ns};

// NET_NOW_NS is process-global; the unit harness runs `--test-threads=1`
// (xtask unit / CI), so each test resets it to 0 on exit instead of locking.

#[test]
fn net_clock_bridge_converts_ns_to_micros() {
    net_set_now_ns(1_500_000_000);
    assert_eq!(
        net_now_instant(),
        smoltcp::time::Instant::from_micros(1_500_000)
    );

    net_set_now_ns(0);
    assert_eq!(net_now_instant(), smoltcp::time::Instant::from_micros(0));
}

/// The decisive P0 test: with the pre-P0 frozen `with_context` clock the
/// third dispatch never fires (retransmit timer can't expire), so this test
/// is red on the old code and green once the clock bridge is live.
#[test]
fn tcp_syn_retransmits_after_clock_advance() {
    net_set_now_ns(0);
    let socket = RawTcpSocket::new(&SocketOptionSet::default_tcp());
    socket
        .connect_endpoint(endpoint(4055), endpoint(4056))
        .expect("connect enters SynSent");

    let syn1 = socket.dispatch_segment();
    assert!(syn1.is_some(), "first dispatch must emit the SYN");

    let syn2 = socket.dispatch_segment();
    assert!(
        syn2.is_none(),
        "no time elapsed: the retransmit timer must not fire"
    );

    net_set_now_ns(2_000_000_000);
    let syn3 = socket.dispatch_segment();
    assert!(
        syn3.is_some(),
        "clock advanced past the initial RTO: smoltcp must retransmit the SYN"
    );

    net_set_now_ns(0);
}
