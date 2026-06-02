use super::*;
use std::boxed::Box;

#[test]
fn proc_net_arp_projection_renders_resolved_pending_and_failed_entries() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");

    let device = leak_projection_device();
    let registration = leak_projection_registration(device, 38);
    let local = Ipv4Address::new([192, 0, 2, 2]);
    let iface = EtherIface::new(
        registration,
        IfaceCommon::new(local, Ipv4Address::new([255, 255, 255, 0]), 1500),
        device.mac_addr(),
        "virtp0",
    );
    let now = smoltcp::time::Instant::from_millis(10_000);
    let guard = tx_substrate::epoch::guard();

    iface.install_arp_for_test_or_bootstrap(
        Ipv4Address::new([192, 0, 2, 10]),
        EthernetAddress::new([0x02, 0, 0, 0, 0, 0x10]),
        now + smoltcp::time::Duration::from_secs(60),
    );
    iface.install_static_ndisc(
        Ipv6Address::new([0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]),
        EthernetAddress::new([0x02, 0, 0, 0, 0, 0x66]),
    );
    queue_failed_arp(
        &iface,
        local,
        Ipv4Address::new([192, 0, 2, 30]),
        now,
        &guard,
    );
    queue_pending_arp(
        &iface,
        local,
        Ipv4Address::new([192, 0, 2, 20]),
        now,
        &guard,
    );

    let text = crate::net::proc_net_arp_snapshot_text(&[&iface], now);

    assert!(text.contains("IP address"));
    assert!(text.contains("192.0.2.10"));
    assert!(text.contains("02:00:00:00:00:10"));
    assert!(text.contains("resolved"));
    assert!(text.contains("192.0.2.20"));
    assert!(text.contains("pending"));
    assert!(text.contains("192.0.2.30"));
    assert!(text.contains("failed"));
    assert!(text.contains("virtp0"));

    let neigh = crate::net::proc_net_neigh_snapshot_text(&[&iface], now);
    assert!(neigh.contains("192.0.2.10 dev virtp0 lladdr 02:00:00:00:00:10 REACHABLE"));
    assert!(neigh.contains("192.0.2.20 dev virtp0 lladdr 00:00:00:00:00:00 INCOMPLETE"));
    assert!(neigh.contains("192.0.2.30 dev virtp0 lladdr 00:00:00:00:00:00 FAILED"));
    assert!(neigh.contains("fd00:1:1:1::1 dev virtp0 lladdr 02:00:00:00:00:66 REACHABLE"));
}

#[test]
fn proc_net_dev_projection_renders_iface_stats() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");

    let device = leak_projection_device();
    let registration = leak_projection_registration(device, 39);
    let local = Ipv4Address::new([192, 0, 2, 2]);
    let peer = Ipv4Address::new([192, 0, 2, 11]);
    let iface = EtherIface::new(
        registration,
        IfaceCommon::new(local, Ipv4Address::new([255, 255, 255, 0]), 1500),
        device.mac_addr(),
        "virtp1",
    );
    let now = smoltcp::time::Instant::from_millis(20_000);
    let guard = tx_substrate::epoch::guard();
    iface.install_arp_for_test_or_bootstrap(
        peer,
        EthernetAddress::new([0x02, 0, 0, 0, 0, 0x11]),
        now + smoltcp::time::Duration::from_secs(60),
    );
    let packet = UdpTxDatagram {
        dst: IpEndpoint::new(peer, 40_339),
        payload: b"hello".to_vec(),
    }
    .emit_ipv4_packet(IpEndpoint::new(local, 50_339))
    .expect("udp packet");
    assert!(matches!(
        iface.dispatch_ip_at(packet.as_bytes(), now, &guard),
        PacketTxResult::Accepted { .. }
    ));

    let text = crate::net::proc_net_dev_snapshot_text(&[&iface]);

    assert!(text.contains("Inter-|"));
    assert!(text.contains("virtp1"));
    assert!(text.contains("1"));
    assert!(text.contains("0"));
}

fn queue_pending_arp(
    iface: &EtherIface,
    local: Ipv4Address,
    peer: Ipv4Address,
    now: smoltcp::time::Instant,
    guard: &Guard<'_>,
) {
    let packet = UdpTxDatagram {
        dst: IpEndpoint::new(peer, 40_338),
        payload: b"pending".to_vec(),
    }
    .emit_ipv4_packet(IpEndpoint::new(local, 50_338))
    .expect("udp packet");
    assert_eq!(
        iface.dispatch_ip_at(packet.as_bytes(), now, guard),
        PacketTxResult::PendingResolution { next_hop: peer }
    );
}

fn queue_failed_arp(
    iface: &EtherIface,
    local: Ipv4Address,
    peer: Ipv4Address,
    now: smoltcp::time::Instant,
    guard: &Guard<'_>,
) {
    queue_pending_arp(iface, local, peer, now, guard);
    for retry in 0..=ARP_REQUEST_RETRY_LIMIT {
        let probe_time = now + smoltcp::time::Duration::from_millis(u64::from(retry) * 1_000);
        let _ = iface.flush_pending_arp_at(probe_time, 16, guard);
    }
    assert!(iface
        .pending_arp_entry(peer)
        .is_some_and(|entry| entry.last_error == Some(Errno::EADDRNOTAVAIL)));
}

fn leak_projection_device() -> &'static VirtioNetDevice {
    Box::leak(Box::new(VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new([0x02, 0, 0, 0, 0, 0x91]),
            1500,
            VirtioNetFeatureSet::software_checksum(),
        ),
        VirtioNetQueueConfig::new(8, 16),
    )))
}

fn leak_projection_registration(
    device: &'static VirtioNetDevice,
    minor: u32,
) -> &'static NetDeviceRegistration {
    Box::leak(Box::new(NetDeviceRegistration {
        devt: DevT::new(VIRTIO_NET_STAGING_MAJOR, minor),
        name: "virtio-net-projection",
        ops: device,
    }))
}
