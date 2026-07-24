use super::*;
use std::boxed::Box;

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
    clear_delegate_queue();
    lock
}

#[test]
fn virtio_net_device_exposes_config_and_registration_shape() {
    let device = VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new([0x02, 0, 0, 0, 0, 0x34]),
            VIRTIO_NET_DEFAULT_MTU,
            VirtioNetFeatureSet::software_checksum(),
        ),
        VirtioNetQueueConfig::new(2, 2),
    );

    assert_eq!(
        device.mac_addr(),
        EthernetAddress::new([0x02, 0, 0, 0, 0, 0x34])
    );
    assert_eq!(device.mtu(), 1500);
    assert_eq!(device.queue_config().rx_capacity, 2);
    assert!(!device.config().features.checksum_offload);
}

#[test]
fn virtio_net_device_rx_queue_feeds_receive() {
    let device = VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new([0x02, 0, 0, 0, 0, 0x35]),
            1500,
            VirtioNetFeatureSet::software_checksum(),
        ),
        VirtioNetQueueConfig::new(1, 1),
    );

    let outcome = device.inject_rx_for_test_or_irq(RxFrame::new(std::vec![1, 2, 3, 4]));

    assert!(outcome.accepted);
    assert_eq!(device.rx_len(), 1);
    let frame = device.receive().expect("rx frame");
    assert_eq!(frame.as_bytes(), &[1, 2, 3, 4]);
    assert_eq!(device.rx_len(), 0);
    assert_eq!(device.stats.snapshot().rx_packets, 1);
    assert_eq!(device.stats.snapshot().rx_bytes, 4);
}

#[test]
fn virtio_net_device_tx_queue_accepts_frame_until_capacity() {
    let _lock = setup();

    let device = VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new([0x02, 0, 0, 0, 0, 0x36]),
            1500,
            VirtioNetFeatureSet::software_checksum(),
        ),
        VirtioNetQueueConfig::new(1, 1),
    );
    let guard = tx_substrate::epoch::guard();

    assert_eq!(device.transmit(&[1, 2, 3], &guard), StepOutcome::Done(()));
    assert_eq!(device.tx_inflight_len(), 1);
    assert!(matches!(
        device.transmit(&[4, 5, 6], &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));
    assert_eq!(device.tx_inflight_len(), 1);
    assert_eq!(device.stats.snapshot().tx_submitted, 1);
    assert_eq!(device.stats.snapshot().tx_busy, 1);
}

#[test]
fn virtio_tx_completion_releases_capacity_for_retry() {
    let _lock = setup();

    let device = VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new([0x02, 0, 0, 0, 0, 0x46]),
            1500,
            VirtioNetFeatureSet::software_checksum(),
        ),
        VirtioNetQueueConfig::new(1, 1),
    );
    let guard = tx_substrate::epoch::guard();

    assert_eq!(device.transmit(&[1, 2, 3], &guard), StepOutcome::Done(()));
    assert!(matches!(
        device.transmit(&[4, 5, 6], &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    let completion = device.complete_tx_for_test_or_irq(1);
    assert_eq!(completion.completed, 1);
    assert_eq!(completion.bytes, 3);
    assert_eq!(device.tx_inflight_len(), 0);
    assert_eq!(device.tx_completed_len(), 1);

    assert_eq!(device.transmit(&[4, 5, 6], &guard), StepOutcome::Done(()));
    assert_eq!(device.tx_inflight_len(), 1);
    assert_eq!(device.stats.snapshot().tx_submitted, 2);
    assert_eq!(device.stats.snapshot().tx_completed, 1);

    let completed = device.drain_completed_tx_frames_for_test_or_driver();
    assert_eq!(completed, std::vec![std::vec![1, 2, 3]]);
    assert_eq!(device.tx_completed_len(), 0);
}

#[test]
fn virtio_rx_delegate_delivers_udp_payload_to_socket() {
    let _lock = setup();

    let device = leak_virtio_device(0x37, 4, 4);
    let registration = leak_virtio_registration(device, 37);
    let local_ip = Ipv4Address::new([192, 0, 2, 2]);
    crate::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(registration, Some(local_ip))
        .expect("attach virtio test device to initial net namespace");
    let iface = EtherIface::new(
        registration,
        IfaceCommon::new(local_ip, Ipv4Address::new([255, 255, 255, 0]), 1500),
        EthernetAddress::new([0x02, 0, 0, 0, 0, 2]),
        "virt0",
    );
    let server = {
        let guard = tx_substrate::epoch::guard();
        let server = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Udp,
            SocketOptionSet::default_udp(),
        )
        .expect("server");
        assert_eq!(
            step_bind(
                &server,
                KernelSockAddr::V4(SockAddrIn::new(40_337, local_ip)),
                &guard
            ),
            StepOutcome::Done(())
        );
        server
    };
    let frame = ethernet_ipv4_frame(17, &udp_transport(50_337, 40_337, b"hello"));

    let injected = device.inject_rx_for_test_or_irq(RxFrame::new(frame));
    assert!(injected.accepted);
    assert_eq!(device.rx_len(), 1);

    let driver = VirtioEtherDelegateDriver {
        source: EtherPacketSource { iface: &iface },
    };
    crate::net::delegate::net_delegate_kick_poll_with_post(|mailbox, event| mailbox.post(event));
    let guard = tx_substrate::epoch::guard();
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(device.rx_len(), 0);
    assert_eq!(device.stats.snapshot().rx_packets, 1);
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert!(server.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
}

#[test]
fn virtio_irq_rx_available_only_fires_delegate_poll() {
    let _lock = setup();

    let device = leak_virtio_device(0x47, 2, 2);
    let injected = device.inject_rx_for_test_or_irq(RxFrame::new(std::vec![1, 2, 3]));
    assert!(injected.accepted);
    assert_eq!(device.rx_len(), 1);
    assert_eq!(delegate_ready_bits(), 0);

    let mut injected_posts = 0usize;
    let irq = device.handle_irq_with_post(VirtioNetIrqEvent::RxAvailable, |mailbox, event| {
        injected_posts += 1;
        mailbox.post(event)
    });

    assert!(irq.rx_ready);
    assert_eq!(irq.tx_completed, 0);
    assert_eq!(injected_posts, irq.poll_wakes);
    assert_eq!(device.rx_len(), 1);
    assert!(delegate_ready_bits() & crate::net::delegate::DelegateWireSet::POLL.bits() != 0);
}

#[test]
fn virtio_irq_tx_complete_releases_capacity_and_fires_delegate_poll() {
    let _lock = setup();

    let device = leak_virtio_device(0x48, 2, 1);
    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        device.transmit(&[1, 2, 3, 4], &guard),
        StepOutcome::Done(())
    );
    assert_eq!(device.tx_inflight_len(), 1);
    assert!(matches!(
        device.transmit(&[5, 6, 7, 8], &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));
    clear_delegate_queue();

    let mut injected_posts = 0usize;
    let irq = device.handle_irq_with_post(
        VirtioNetIrqEvent::TxComplete { budget: 1 },
        |mailbox, event| {
            injected_posts += 1;
            mailbox.post(event)
        },
    );

    assert_eq!(irq.tx_completed, 1);
    assert_eq!(irq.tx_completed_bytes, 4);
    assert_eq!(injected_posts, irq.poll_wakes);
    assert_eq!(device.tx_inflight_len(), 0);
    assert_eq!(device.tx_completed_len(), 1);
    assert!(delegate_ready_bits() & crate::net::delegate::DelegateWireSet::POLL.bits() != 0);
    assert_eq!(
        device.transmit(&[5, 6, 7, 8], &guard),
        StepOutcome::Done(())
    );
}

fn leak_virtio_device(
    mac_tail: u8,
    rx_capacity: usize,
    tx_capacity: usize,
) -> &'static VirtioNetDevice {
    Box::leak(Box::new(VirtioNetDevice::new(
        VirtioNetConfig::new(
            EthernetAddress::new([0x02, 0, 0, 0, 0, mac_tail]),
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

fn clear_delegate_queue() {
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
}

fn delegate_ready_bits() -> u64 {
    crate::net::delegate::net_delegate_queue().peek()
}
